//! 纯整数 NV12 测试源，即现行 Rust 版 pattern 实现；与历史 C++ `sender/pattern.h`
//! 的逐字节语义一致。
//!
//! 五种图案（counter/checker/rings/noise/motion）全部只用整数运算，
//! 同一 `frame_id` 必得同一帧，便于丢帧重放和哈希对账。生成结果与
//! 历史C++图案、独立Python `rust/tests/reference/pattern_ref.py`（`gen_native`）逐字节一致：
//! 包含可变宽高、运动色块在几何边缘的裁剪规则、行内 PRNG 尾部字节规则，
//! 以及 32 位无符号回绕语义。默认产品尺寸仍为 640×640，但本模块不内置该假设。
//!
//! 输出是 packed NV12：先是 `width * height` 字节的 Y 平面，再是
//! `width * height / 2` 字节的交错 UV 平面。噪声流按小端写入，
//! 与 x64 Windows 上 C++ `memcpy(uint64_t)` 和 Python `to_bytes("little")` 一致。

use std::fmt;
use std::str::FromStr;

use vc_transport::Geometry;

/// 测试图案编号，与 C++ `vc_pattern_t` 和 28 B 包头 `pattern` 字段一致。
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pattern {
    /// 烟雾：低渐变。
    Counter = 0,
    /// 棋盘：逐帧滚动，暴露 NV12 4:2:0 错位。
    Checker = 1,
    /// 同心环：高频纹理，验证下采样/矩阵混叠。
    Rings = 2,
    /// 全帧 splitmix64 噪声：最大熵，防止隐藏压缩/作弊。
    Noise = 3,
    /// 拟游戏压力源：滚动背景 + 4 个弹跳色块 + 中央噪声窗。
    Motion = 4,
}

impl Pattern {
    /// 全部五种图案，按编号顺序排列（CLI/GUI 枚举用）。
    pub const ALL: [Self; 5] = [
        Self::Counter,
        Self::Checker,
        Self::Rings,
        Self::Noise,
        Self::Motion,
    ];

    /// 由 `pattern` 包头字节还原图案；未知编号返回 `None`。
    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::Counter),
            1 => Some(Self::Checker),
            2 => Some(Self::Rings),
            3 => Some(Self::Noise),
            4 => Some(Self::Motion),
            _ => None,
        }
    }

    /// 包头/CLI 使用的图案编号。
    pub fn id(self) -> u8 {
        self as u8
    }

    /// CLI 与日志使用的稳定小写名称。
    pub fn name(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Checker => "checker",
            Self::Rings => "rings",
            Self::Noise => "noise",
            Self::Motion => "motion",
        }
    }

    /// 生成一帧 packed NV12，写入 `output`。
    ///
    /// `output.len()` 必须恰好等于 [`Geometry::frame_bytes`]（`w*h*3/2`）。
    /// [`Geometry`] 已保证宽高为 `2..=8192` 内的偶数，因此长度不符只可能是调用方
    /// 编程错误；这里用断言而不是 `Result`，与既定签名保持一致。
    ///
    /// 所有像素运算按 32 位无符号回绕，与 C++ 原实现相同；`frame_id` 取完整
    /// 32 位，噪声/运动种子把它放进 64 位种子的高 32 位，因而不会像低 8 位
    /// 图案那样周期性重复。
    ///
    /// # Panics
    ///
    /// `output.len() != geometry.frame_bytes()` 时 panic。
    pub fn fill(self, geometry: Geometry, frame_id: u32, output: &mut [u8]) {
        let width = geometry.width() as usize;
        let height = geometry.height() as usize;
        let y_len = width * height;
        let frame_len = y_len + y_len / 2;
        assert_eq!(
            output.len(),
            frame_len,
            "pattern::fill needs an exactly sized packed NV12 output: \
             {}x{} requires {} bytes, got {}",
            geometry.width(),
            geometry.height(),
            frame_len,
            output.len()
        );
        let (y, uv) = output.split_at_mut(y_len);
        match self {
            Self::Counter => counter(frame_id, width, y, uv),
            Self::Checker => checker(frame_id, width, y, uv),
            Self::Rings => rings(frame_id, width, height, y, uv),
            Self::Noise => noise(frame_id, y, uv),
            Self::Motion => motion(frame_id, width, height, y, uv),
        }
    }
}

impl FromStr for Pattern {
    type Err = String;

    /// CLI 解析：只接受 [`Pattern::name`] 的精确小写名称。
    ///
    /// 与原 C++ `pat_id` 不同，未知名称不会被静默当成 motion。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "counter" => Ok(Self::Counter),
            "checker" => Ok(Self::Checker),
            "rings" => Ok(Self::Rings),
            "noise" => Ok(Self::Noise),
            "motion" => Ok(Self::Motion),
            other => Err(format!(
                "unknown pattern '{other}'; expected one of: counter, checker, rings, noise, motion"
            )),
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// splitmix64：与 pattern.h / pattern_ref.py 相同的 64 位回绕随机数。
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// 按 C++ 的写法逐段填充：先消费完整的 8 字节小端字，余数每个字节消耗一次
// splitmix64 并取低 8 位。调用方按原实现的“段”来调用，以保留行/平面边界处的
// 尾部规则（中央噪声窗每行重新开始 8 字节分组）。
fn fill_prng(state: &mut u64, out: &mut [u8]) {
    let mut chunks = out.chunks_exact_mut(8);
    for chunk in chunks.by_ref() {
        chunk.copy_from_slice(&splitmix64(state).to_le_bytes());
    }
    for byte in chunks.into_remainder() {
        *byte = splitmix64(state) as u8;
    }
}

// 烟雾渐变：Y = x + y + fid，UV 为常量 0x80 ^ (fid & 0xFF)。
fn counter(fid: u32, width: usize, y: &mut [u8], uv: &mut [u8]) {
    for (row, line) in y.chunks_exact_mut(width).enumerate() {
        let row_y = row as u32;
        for (col, pixel) in line.iter_mut().enumerate() {
            *pixel = (col as u32).wrapping_add(row_y).wrapping_add(fid) as u8;
        }
    }
    let value = 0x80u8 ^ (fid & 0xFF) as u8;
    uv.fill(value);
}

// 棋盘：Y 以 8×8 格随 fid 滚动，UV 也按帧奇偶翻转。
fn checker(fid: u32, width: usize, y: &mut [u8], uv: &mut [u8]) {
    let ox = fid & 7;
    let oy = (fid >> 3) & 7;
    for (row, line) in y.chunks_exact_mut(width).enumerate() {
        let row_y = row as u32;
        for (col, pixel) in line.iter_mut().enumerate() {
            let x = col as u32;
            let cell = ((x + ox) >> 3) + ((row_y + oy) >> 3);
            *pixel = if cell & 1 != 0 { 255 } else { 0 };
        }
    }
    for (row, line) in uv.chunks_exact_mut(width).enumerate() {
        let row_y = row as u32;
        let u_parity = row_y.wrapping_add(fid) & 1;
        let v_parity = row_y.wrapping_sub(fid) & 1;
        for (col, pair) in line.chunks_exact_mut(2).enumerate() {
            let x = (col as u32) * 2;
            let base = (x >> 3) & 1;
            pair[0] = if base ^ u_parity != 0 { 16 } else { 240 };
            pair[1] = if base ^ v_parity != 0 { 16 } else { 240 };
        }
    }
}

// 同心环：Y = ((dx²+dy²)>>4) + fid*7 + ((x^y)&15)，UV 随 fid 滚动。
fn rings(fid: u32, width: usize, height: usize, y: &mut [u8], uv: &mut [u8]) {
    let cx = (width / 2) as i32;
    let cy = (height / 2) as i32;
    for (row, line) in y.chunks_exact_mut(width).enumerate() {
        let yy = row as i32;
        let dy = yy - cy;
        for (col, pixel) in line.iter_mut().enumerate() {
            let xx = col as i32;
            let dx = xx - cx;
            let r2 = (dx * dx + dy * dy) as u32;
            let value = (r2 >> 4)
                .wrapping_add(fid.wrapping_mul(7))
                .wrapping_add(((xx ^ yy) & 15) as u32);
            *pixel = value as u8;
        }
    }
    for (row, line) in uv.chunks_exact_mut(width).enumerate() {
        let yy = row as u32;
        for (col, pair) in line.chunks_exact_mut(2).enumerate() {
            let xx = (col as u32) * 2;
            let u_src = xx
                .wrapping_mul(3)
                .wrapping_add(yy.wrapping_mul(5))
                .wrapping_add(fid.wrapping_mul(11));
            let v_src = xx
                .wrapping_mul(5)
                .wrapping_sub(yy.wrapping_mul(3))
                .wrapping_add(fid.wrapping_mul(13));
            pair[0] = (128 + ((u_src >> 2) & 127) - 64) as u8;
            pair[1] = (128 + ((v_src >> 2) & 127) - 64) as u8;
        }
    }
}

// 全帧噪声：Y 与 UV 共用同一 splitmix64 状态，UV 紧接 Y 的尾部继续。
fn noise(fid: u32, y: &mut [u8], uv: &mut [u8]) {
    // D11: XOR 而非 |，否则常量高 16 位会吞掉 fid 的部分比特。
    let mut state = ((fid as u64) << 32) ^ 0x0000_1234_5678_9ABC;
    fill_prng(&mut state, y);
    fill_prng(&mut state, uv);
}

// 拟游戏压力帧：滚动背景 + 4 个 64 像素弹跳色块 + 中央 min(256,w,h) 噪声窗。
fn motion(fid: u32, width: usize, height: usize, y: &mut [u8], uv: &mut [u8]) {
    for (row, line) in y.chunks_exact_mut(width).enumerate() {
        let base = (row >> 2) as u32;
        for (col, pixel) in line.iter_mut().enumerate() {
            *pixel = (col as u32)
                .wrapping_add(fid.wrapping_mul(3))
                .wrapping_add(base) as u8;
        }
    }

    // 色块边长：min>64 时固定 64，否则 min-1（至少 1，保证取模有意义）。
    let min_dim = width.min(height);
    let sprite = if min_dim > 64 { 64 } else { min_dim - 1 };
    for k in 0..4u32 {
        let offset_x = fid
            .wrapping_mul(5 + k * 2)
            .wrapping_add(k.wrapping_mul(173))
            % ((width - sprite) as u32);
        let offset_y =
            fid.wrapping_mul(3 + k).wrapping_add(k.wrapping_mul(311)) % ((height - sprite) as u32);
        let offset_x = offset_x as usize;
        let offset_y = offset_y as usize;
        let color = (60 + k * 50) as u8;
        let alternate = 255 - color;
        for sy in 0..sprite {
            let row_start = (offset_y + sy) * width + offset_x;
            let row = &mut y[row_start..row_start + sprite];
            for (sx, pixel) in row.iter_mut().enumerate() {
                *pixel = if (sx ^ sy) & 8 != 0 { alternate } else { color };
            }
        }
    }

    // 中央噪声窗：每行重新开始 8 字节分组，状态跨行连续（与 C++ 一致）。
    let window = if min_dim > 256 { 256 } else { min_dim };
    let x0 = (width - window) / 2;
    let y0 = (height - window) / 2;
    let mut state = ((fid as u64) << 32) ^ 0x0000_4D4F_5449_4F4E; // 'MOTION'
    for row in 0..window {
        let start = (y0 + row) * width + x0;
        fill_prng(&mut state, &mut y[start..start + window]);
    }

    for (row, line) in uv.chunks_exact_mut(width).enumerate() {
        let yy = row as u32;
        for (col, pair) in line.chunks_exact_mut(2).enumerate() {
            let xx = (col as u32) * 2;
            pair[0] = xx.wrapping_add(fid) as u8;
            pair[1] = yy.wrapping_mul(2).wrapping_sub(fid) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_transport::checksum::fnv1a64_two;

    // 独立黄金向量来源（常量不是本实现算出来的）：
    //
    // 1. 历史 `build/Release/vc-sender.exe --selftest`（C++17，含 sender/pattern.h）
    //    打印的 `VEC full`（640×640，fid 0..3）与 `VEC variable`（418×282，fid 0..1）。
    // 2. 历史用 MSVC 19.44 单独编译的 fixture（仅 include 真实 sender/pattern.h 和
    //    common/fnv1a.h），生成 2×2、100×70、60×40 及高/回绕 fid 向量。
    // 3. `rust/tests/reference/pattern_ref.py::gen_native`（独立 Python 镜像）对上述 37 个变异场景
    //    逐一复算，与 C++ fixture 输出完全一致后才写入下表。
    //
    // 哈希为 Y+UV 拼接后的 FNV-1a64，与 C++ `vc_fnv1a64_two`、
    // Python `frame_hash_yuv` 和 vc_transport::checksum::fnv1a64_two 相同。
    struct Golden {
        pattern: Pattern,
        frame_id: u32,
        width: u32,
        height: u32,
        hash: u64,
    }

    const GOLDEN: &[Golden] = &[
        // 发布尺寸 640×640，C++ `--selftest` 的 `VEC full`（含 motion）。
        Golden {
            pattern: Pattern::Counter,
            frame_id: 0,
            width: 640,
            height: 640,
            hash: 0x7A14_C003_D977_8B25,
        },
        Golden {
            pattern: Pattern::Counter,
            frame_id: 1,
            width: 640,
            height: 640,
            hash: 0x7AD8_E2A5_F6FC_2B25,
        },
        Golden {
            pattern: Pattern::Counter,
            frame_id: 2,
            width: 640,
            height: 640,
            hash: 0xB50E_0B58_72AB_2525,
        },
        Golden {
            pattern: Pattern::Counter,
            frame_id: 3,
            width: 640,
            height: 640,
            hash: 0x0850_FDA4_FB93_7F25,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 0,
            width: 640,
            height: 640,
            hash: 0x7952_84F5_7BDD_C325,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 1,
            width: 640,
            height: 640,
            hash: 0x1537_1D17_7D4C_A325,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 2,
            width: 640,
            height: 640,
            hash: 0x86AD_A300_B4D2_C325,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 3,
            width: 640,
            height: 640,
            hash: 0xD58F_5E8F_BED7_A325,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 0,
            width: 640,
            height: 640,
            hash: 0x8C95_5154_249D_36E5,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 1,
            width: 640,
            height: 640,
            hash: 0x2EF0_EA88_078C_1CE5,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 2,
            width: 640,
            height: 640,
            hash: 0x1459_F638_4B41_07A5,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 3,
            width: 640,
            height: 640,
            hash: 0x8D72_582B_3D5E_5E65,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 0,
            width: 640,
            height: 640,
            hash: 0x239D_9691_4266_0385,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 1,
            width: 640,
            height: 640,
            hash: 0x0815_4371_EDDF_387A,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 2,
            width: 640,
            height: 640,
            hash: 0x8923_954C_0E28_5F93,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 3,
            width: 640,
            height: 640,
            hash: 0x4FC0_5DF3_768A_7382,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 0,
            width: 640,
            height: 640,
            hash: 0xF6F5_3D2A_EAC3_C9DC,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 1,
            width: 640,
            height: 640,
            hash: 0x7130_F8F9_73E7_8694,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 2,
            width: 640,
            height: 640,
            hash: 0x6572_768F_E7CE_34D8,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 3,
            width: 640,
            height: 640,
            hash: 0x674E_F98C_98ED_2B65,
        },
        // 矩形 418×282（宽非 8 的倍数），C++ `--selftest` 的 `VEC variable`。
        Golden {
            pattern: Pattern::Counter,
            frame_id: 0,
            width: 418,
            height: 282,
            hash: 0x92A4_10F7_8BE5_BF59,
        },
        Golden {
            pattern: Pattern::Counter,
            frame_id: 1,
            width: 418,
            height: 282,
            hash: 0x505F_7DF4_FC2D_8833,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 0,
            width: 418,
            height: 282,
            hash: 0x07A7_19EF_C714_B1A9,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 1,
            width: 418,
            height: 282,
            hash: 0xB2B4_5D91_ECAA_AD69,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 0,
            width: 418,
            height: 282,
            hash: 0x30BB_A65D_A375_9249,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 1,
            width: 418,
            height: 282,
            hash: 0x7913_D4F7_0998_69C6,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 0,
            width: 418,
            height: 282,
            hash: 0xD95E_E5A7_9C5C_B547,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 1,
            width: 418,
            height: 282,
            hash: 0x0F4C_BEE6_DAE4_DFA3,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 0,
            width: 418,
            height: 282,
            hash: 0xD489_5A42_DF36_4ECA,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 1,
            width: 418,
            height: 282,
            hash: 0x941E_671E_689F_848A,
        },
        // 最小几何 2×2：色块 S=1、噪声窗 W2=2，覆盖所有取模/行尾边界。
        Golden {
            pattern: Pattern::Counter,
            frame_id: 0,
            width: 2,
            height: 2,
            hash: 0x7AD1_3BD8_0AE3_05C1,
        },
        Golden {
            pattern: Pattern::Counter,
            frame_id: 1,
            width: 2,
            height: 2,
            hash: 0xECFD_930B_9723_6633,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 0,
            width: 2,
            height: 2,
            hash: 0xD4B5_6CFA_26E8_95FD,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 1,
            width: 2,
            height: 2,
            hash: 0xD7AE_8CFA_296F_237D,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 0,
            width: 2,
            height: 2,
            hash: 0x6A57_85D8_01CB_65EB,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 1,
            width: 2,
            height: 2,
            hash: 0x7B93_9372_7F4E_9160,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 0,
            width: 2,
            height: 2,
            hash: 0xDF1F_9F19_6E98_F9D6,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 1,
            width: 2,
            height: 2,
            hash: 0x19A3_69A5_94EB_CC10,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 0,
            width: 2,
            height: 2,
            hash: 0xB6BB_87FB_336E_FE47,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 1,
            width: 2,
            height: 2,
            hash: 0xB6A9_E06D_8AF7_10BD,
        },
        // 噪声窗行尾边界：100×70 的 W2=70（非 8 倍数）且 S=64；
        // 60×40 的 W2=40 且 S=min-1=39。
        Golden {
            pattern: Pattern::Motion,
            frame_id: 0,
            width: 100,
            height: 70,
            hash: 0x25AE_678E_1D72_1F60,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 0,
            width: 60,
            height: 40,
            hash: 0xC07D_610E_C292_EC14,
        },
        // 高/回绕 frame_id：640×640 全部五种（含 motion）在 fid=0xFFFFFFFF；
        // motion 另取 0x80000000。
        Golden {
            pattern: Pattern::Counter,
            frame_id: 0xFFFF_FFFF,
            width: 640,
            height: 640,
            hash: 0xC8BD_D1E4_711B_C325,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 0xFFFF_FFFF,
            width: 640,
            height: 640,
            hash: 0x9C14_FCF1_C769_A325,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 0xFFFF_FFFF,
            width: 640,
            height: 640,
            hash: 0xABC3_4D1E_BFCE_05E5,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 0xFFFF_FFFF,
            width: 640,
            height: 640,
            hash: 0xD0FD_8AD8_2AF9_9D8E,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 0xFFFF_FFFF,
            width: 640,
            height: 640,
            hash: 0x3CCB_0DB4_BD75_470E,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 0x8000_0000,
            width: 640,
            height: 640,
            hash: 0x0B7F_232C_4ACE_7313,
        },
        // 100×70 全部五种在 fid=0xFFFFFFFE，motion 另取 0x80000001。
        Golden {
            pattern: Pattern::Counter,
            frame_id: 0xFFFF_FFFE,
            width: 100,
            height: 70,
            hash: 0x5F2F_B5C4_B32C_D9F9,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 0xFFFF_FFFE,
            width: 100,
            height: 70,
            hash: 0x950C_AC09_97EC_9609,
        },
        Golden {
            pattern: Pattern::Rings,
            frame_id: 0xFFFF_FFFE,
            width: 100,
            height: 70,
            hash: 0xBA68_73A5_BACA_6712,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 0xFFFF_FFFE,
            width: 100,
            height: 70,
            hash: 0xD778_4AA2_FD26_1C9E,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 0xFFFF_FFFE,
            width: 100,
            height: 70,
            hash: 0xB111_1D70_9102_267E,
        },
        Golden {
            pattern: Pattern::Motion,
            frame_id: 0x8000_0001,
            width: 100,
            height: 70,
            hash: 0xA8C8_F1B6_D1FF_415A,
        },
        // 418×282 上 fid=0xFFFFFFFF 的 checker/rings/noise 交叉校验。
        Golden {
            pattern: Pattern::Rings,
            frame_id: 0xFFFF_FFFF,
            width: 418,
            height: 282,
            hash: 0xDA13_0470_3FA3_5EBC,
        },
        Golden {
            pattern: Pattern::Checker,
            frame_id: 0xFFFF_FFFF,
            width: 418,
            height: 282,
            hash: 0x400D_256B_5B9B_3575,
        },
        Golden {
            pattern: Pattern::Noise,
            frame_id: 0xFFFF_FFFF,
            width: 418,
            height: 282,
            hash: 0xA405_1995_20EE_4EA7,
        },
    ];

    fn fill_and_hash(pattern: Pattern, width: u32, height: u32, frame_id: u32) -> u64 {
        let geometry = Geometry::new(width, height).expect("golden geometry must be valid");
        let mut frame = vec![0u8; geometry.frame_bytes()];
        pattern.fill(geometry, frame_id, &mut frame);
        let y_len = width as usize * height as usize;
        let (y, uv) = frame.split_at(y_len);
        fnv1a64_two(y, uv)
    }

    #[test]
    fn golden_vectors_match_cpp_and_python_mirror() {
        assert_eq!(GOLDEN.len(), 57);
        for case in GOLDEN {
            let got = fill_and_hash(case.pattern, case.width, case.height, case.frame_id);
            assert_eq!(
                got,
                case.hash,
                "{} fid=0x{:08X} {}x{}",
                case.pattern.name(),
                case.frame_id,
                case.width,
                case.height
            );
        }
    }

    // 2×2 全帧字节（Y 4 字节 + UV 2 字节）由真实 pattern.h fixture 的
    // `--dump` 输出抄录，作为哈希之外的逐字节校验。
    const DIRECT_2X2: &[(Pattern, u32, [u8; 4], [u8; 2])] = &[
        (Pattern::Counter, 0, [0x00, 0x01, 0x01, 0x02], [0x80, 0x80]),
        (Pattern::Counter, 1, [0x01, 0x02, 0x02, 0x03], [0x81, 0x81]),
        (Pattern::Checker, 0, [0x00, 0x00, 0x00, 0x00], [0xF0, 0xF0]),
        (Pattern::Checker, 1, [0x00, 0x00, 0x00, 0x00], [0x10, 0x10]),
        (Pattern::Rings, 0, [0x00, 0x01, 0x01, 0x00], [0x40, 0x40]),
        (Pattern::Rings, 1, [0x07, 0x08, 0x08, 0x07], [0x42, 0x43]),
        (Pattern::Noise, 0, [0x35, 0x93, 0x3B, 0x7D], [0x7A, 0x73]),
        (Pattern::Noise, 1, [0xE5, 0x06, 0xE4, 0x1E], [0x44, 0x60]),
        (Pattern::Motion, 0, [0x8B, 0x14, 0x3D, 0x50], [0x00, 0x00]),
        (Pattern::Motion, 1, [0x90, 0xA1, 0xEB, 0xD8], [0x01, 0xFF]),
    ];

    #[test]
    fn smallest_geometry_matches_byte_exact_cpp_dumps() {
        let geometry = Geometry::new(2, 2).unwrap();
        for &(pattern, frame_id, y, uv) in DIRECT_2X2 {
            let mut expected = [0u8; 6];
            expected[..4].copy_from_slice(&y);
            expected[4..].copy_from_slice(&uv);
            let mut frame = [0u8; 6];
            pattern.fill(geometry, frame_id, &mut frame);
            assert_eq!(frame, expected, "{} fid={frame_id}", pattern.name());
        }
    }

    #[test]
    fn fill_is_deterministic_for_every_pattern() {
        let geometry = Geometry::new(100, 70).unwrap();
        for pattern in Pattern::ALL {
            let mut first = vec![0u8; geometry.frame_bytes()];
            let mut second = vec![0u8; geometry.frame_bytes()];
            pattern.fill(geometry, 0xDEAD_BEEF, &mut first);
            pattern.fill(geometry, 0xDEAD_BEEF, &mut second);
            assert_eq!(first, second, "{}", pattern.name());
        }
    }

    #[test]
    #[should_panic(expected = "packed NV12 output")]
    fn fill_rejects_wrong_output_size() {
        let geometry = Geometry::new(4, 2).unwrap();
        let mut frame = vec![0u8; geometry.frame_bytes() - 1];
        Pattern::Counter.fill(geometry, 0, &mut frame);
    }

    #[test]
    fn ids_names_and_cli_parsing_are_frozen() {
        assert_eq!(Pattern::ALL.map(Pattern::id), [0, 1, 2, 3, 4]);
        assert_eq!(
            Pattern::ALL.map(Pattern::name),
            ["counter", "checker", "rings", "noise", "motion"]
        );
        for pattern in Pattern::ALL {
            assert_eq!(Pattern::from_id(pattern.id()), Some(pattern));
            assert_eq!(pattern.name().parse::<Pattern>(), Ok(pattern));
            assert_eq!(pattern.to_string(), pattern.name());
        }
        assert_eq!(Pattern::from_id(5), None);
        assert_eq!(Pattern::from_id(u8::MAX), None);
        let error = "Counter".parse::<Pattern>().unwrap_err();
        assert!(error.contains("Counter"), "{error}");
        assert!(error.contains("motion"), "{error}");
    }
}
