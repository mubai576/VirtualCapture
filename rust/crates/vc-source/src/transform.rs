//! 可移植的整数几何与 BGRA→NV12 转换。
//!
//! 与 `common/stream.h` 的 `VCTransform` / `vc_bgrx_nv12` 逐字节等价：
//!
//! - 最近邻采样，全程整数除法；
//! - Y = `(77R + 150G + 29B) >> 8`；
//! - 2×2 平均后再算 U/V（BT.601 limited-range 整数系数）；
//! - `fit` 留黑边（Y=0，U=V=128），`fill` 居中裁源，`crop` 居中取输出尺寸，
//!   `stretch` 整幅缩放。
//!
//! 采样坐标不会越界：`Transform::new` 已把 `ox/rw`、`oy/rh` 限制在源内，
//! [`bgra_to_nv12`] 会再按 `pitch` 做一次长度检查。

use anyhow::{Result, bail};

/// 四种几何变换，与配置字符串一一对应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformMode {
    /// 从源中心裁出输出尺寸（要求源不小于输出）。
    Crop,
    /// 保持宽高比缩放，剩余区域留黑边。
    Fit,
    /// 保持宽高比缩放并居中裁源，填满输出。
    Fill,
    /// 不保持宽高比，整幅拉伸到输出。
    Stretch,
}

impl TransformMode {
    /// 解析配置字符串；未知值显式报错，不做默认回退。
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "crop" => Ok(Self::Crop),
            "fit" => Ok(Self::Fit),
            "fill" => Ok(Self::Fill),
            "stretch" => Ok(Self::Stretch),
            other => {
                bail!("unknown transform {other:?}; expected one of: crop, fit, fill, stretch")
            }
        }
    }

    /// 配置字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Crop => "crop",
            Self::Fit => "fit",
            Self::Fill => "fill",
            Self::Stretch => "stretch",
        }
    }
}

/// CPU 路径真正从源纹理拷出的矩形。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl CopyRect {
    pub fn valid(&self) -> bool {
        self.w >= 2 && self.h >= 2
    }
}

/// 整数采样窗口。字段含义与 `VCTransform` 相同：
/// `(ox,oy)` 是采样左上角，`(rw,rh)` 是采样区域，`(dx,dy,dw,dh)` 是输出
/// 图像中的目标矩形，`width/height` 是输出尺寸。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transform {
    pub ox: u32,
    pub oy: u32,
    pub rw: u32,
    pub rh: u32,
    pub dx: u32,
    pub dy: u32,
    pub dw: u32,
    pub dh: u32,
    pub width: u32,
    pub height: u32,
}

impl Transform {
    /// 根据源尺寸、输出尺寸和变换模式建立采样窗口。
    ///
    /// 输出宽高必须是 2..=8192 的偶数（NV12 4:2:0 与冻结的会话约束）；
    /// 源尺寸必须是 2..=16384。`crop` 在源小于输出时显式失败。
    pub fn new(
        src_width: u32,
        src_height: u32,
        width: u32,
        height: u32,
        mode: TransformMode,
    ) -> Result<Self> {
        if !(2..=8192).contains(&width) || !width.is_multiple_of(2) {
            bail!("output width must be an even value in 2..=8192 (got {width})");
        }
        if !(2..=8192).contains(&height) || !height.is_multiple_of(2) {
            bail!("output height must be an even value in 2..=8192 (got {height})");
        }
        if !(2..=16384).contains(&src_width) || !(2..=16384).contains(&src_height) {
            bail!("source size must be in 2..=16384 (got {src_width}x{src_height})");
        }

        let mut t = Self {
            ox: 0,
            oy: 0,
            rw: src_width,
            rh: src_height,
            dx: 0,
            dy: 0,
            dw: width,
            dh: height,
            width,
            height,
        };

        match mode {
            TransformMode::Crop => {
                if src_width < width || src_height < height {
                    bail!(
                        "crop source {src_width}x{src_height} is smaller than output {width}x{height}"
                    );
                }
                t.ox = (src_width - width) / 2;
                t.oy = (src_height - height) / 2;
                t.rw = width;
                t.rh = height;
            }
            TransformMode::Fit => {
                if u64::from(src_width) * u64::from(height)
                    > u64::from(src_height) * u64::from(width)
                {
                    t.dh = fit_extent(src_height, width, src_width);
                } else {
                    t.dw = fit_extent(src_width, height, src_height);
                }
                t.dx = ((width - t.dw) / 2) & !1;
                t.dy = ((height - t.dh) / 2) & !1;
            }
            TransformMode::Fill => {
                if u64::from(src_width) * u64::from(height)
                    > u64::from(src_height) * u64::from(width)
                {
                    t.rw = fit_extent(src_height, width, height);
                } else {
                    t.rh = fit_extent(src_width, height, width);
                }
                t.ox = ((src_width - t.rw) / 2) & !1;
                t.oy = ((src_height - t.rh) / 2) & !1;
            }
            TransformMode::Stretch => {}
        }
        Ok(t)
    }

    /// 已拷贝矩形到输出全幅的恒等映射（CPU 路径裁切后使用）。
    pub const fn identity(width: u32, height: u32) -> Self {
        Self {
            ox: 0,
            oy: 0,
            rw: width,
            rh: height,
            dx: 0,
            dy: 0,
            dw: width,
            dh: height,
            width,
            height,
        }
    }

    /// 目标坐标 → 源坐标；目标矩形外返回 `None`。
    pub fn sample(&self, x: u32, y: u32) -> Option<(u32, u32)> {
        if x < self.dx
            || y < self.dy
            || x >= self.dx.saturating_add(self.dw)
            || y >= self.dy.saturating_add(self.dh)
        {
            return None;
        }
        let sx =
            self.ox + ((u64::from(x - self.dx) * u64::from(self.rw)) / u64::from(self.dw)) as u32;
        let sy =
            self.oy + ((u64::from(y - self.dy) * u64::from(self.rh)) / u64::from(self.dh)) as u32;
        Some((sx, sy))
    }

    /// CPU 拷贝到 staging 时应使用的源矩形。
    ///
    /// `crop` 只拷中心窗口；其余模式拷整个源区域，再由采样窗口缩放
    /// （`fill` 的 `ox/oy` 是相对完整源图像的坐标，不能只拷采样区域）。
    pub fn copy_rect(
        &self,
        mode: TransformMode,
        source_width: u32,
        source_height: u32,
    ) -> CopyRect {
        match mode {
            TransformMode::Crop => CopyRect {
                x: self.ox,
                y: self.oy,
                w: self.width,
                h: self.height,
            },
            TransformMode::Fit | TransformMode::Fill | TransformMode::Stretch => CopyRect {
                x: 0,
                y: 0,
                w: source_width,
                h: source_height,
            },
        }
    }

    /// packed NV12 帧长度（Y + interleaved UV）。
    pub const fn frame_bytes(&self) -> usize {
        (self.width as usize) * (self.height as usize) * 3 / 2
    }
}

fn fit_extent(src: u32, numerator: u32, denominator: u32) -> u32 {
    let value = (u64::from(src) * u64::from(numerator) / u64::from(denominator)) as u32;
    (value & !1).max(2)
}

/// 将 packed BGRA（`B8G8R8A8_UNORM`，行跨度 `pitch`）转换为 packed NV12。
///
/// `dst` 前 `width*height` 字节是 Y 平面，随后 `width*height/2` 字节是
/// interleaved UV。所有采样坐标由 `transform` 给出，转换全程无浮点。
pub fn bgra_to_nv12(src: &[u8], pitch: usize, transform: &Transform, dst: &mut [u8]) -> Result<()> {
    let width = transform.width as usize;
    let height = transform.height as usize;
    if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        bail!(
            "invalid NV12 output size {}x{}",
            transform.width,
            transform.height
        );
    }
    if transform.dw == 0 || transform.dh == 0 || transform.rw == 0 || transform.rh == 0 {
        bail!("invalid transform sampling window");
    }
    let y_bytes = width * height;
    let uv_bytes = y_bytes / 2;
    if dst.len() < y_bytes + uv_bytes {
        bail!(
            "destination too small: need {} bytes, got {}",
            y_bytes + uv_bytes,
            dst.len()
        );
    }
    if pitch < 4 {
        bail!("source pitch {pitch} is too small for BGRA");
    }

    let x_end = transform.ox.saturating_add(transform.rw) as usize;
    let y_end = transform.oy.saturating_add(transform.rh) as usize;
    if x_end == 0 || y_end == 0 {
        bail!("invalid transform source window");
    }
    let required = pitch
        .checked_mul(y_end - 1)
        .and_then(|value| {
            x_end
                .checked_mul(4)
                .and_then(|row_end| value.checked_add(row_end))
        })
        .ok_or_else(|| anyhow::anyhow!("source window size overflow"))?;
    if src.len() < required {
        bail!(
            "source too small for {}x{} window: need {} bytes, got {}",
            x_end,
            y_end,
            required,
            src.len()
        );
    }

    let (y_plane, uv_tail) = dst.split_at_mut(y_bytes);
    let uv_plane = &mut uv_tail[..uv_bytes];

    for y in 0..height {
        let dy = y as u32;
        let y_row = &mut y_plane[y * width..(y + 1) * width];
        let sy = if dy >= transform.dy && dy < transform.dy.saturating_add(transform.dh) {
            Some(
                transform.oy
                    + ((u64::from(dy - transform.dy) * u64::from(transform.rh))
                        / u64::from(transform.dh)) as u32,
            )
        } else {
            None
        };
        let Some(sy) = sy else {
            y_row.fill(0);
            continue;
        };
        let row_base = sy as usize * pitch;
        for (x, out) in y_row.iter_mut().enumerate() {
            let dx = x as u32;
            if dx < transform.dx || dx >= transform.dx.saturating_add(transform.dw) {
                *out = 0;
                continue;
            }
            let sx = transform.ox
                + ((u64::from(dx - transform.dx) * u64::from(transform.rw))
                    / u64::from(transform.dw)) as u32;
            let offset = row_base + sx as usize * 4;
            let b = i32::from(src[offset]);
            let g = i32::from(src[offset + 1]);
            let r = i32::from(src[offset + 2]);
            *out = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
        }
    }

    for y in (0..height).step_by(2) {
        let uv_row = &mut uv_plane[(y / 2) * width..(y / 2 + 1) * width];
        for x in (0..width).step_by(2) {
            let mut sr = 0i32;
            let mut sg = 0i32;
            let mut sb = 0i32;
            for yy in 0..2 {
                for xx in 0..2 {
                    let (r, g, b) =
                        sample_rgb(src, pitch, transform, (x + xx) as u32, (y + yy) as u32);
                    sr += r;
                    sg += g;
                    sb += b;
                }
            }
            sr >>= 2;
            sg >>= 2;
            sb >>= 2;
            uv_row[x] = clamp8(((-43 * sr - 84 * sg + 127 * sb) >> 8) + 128);
            uv_row[x + 1] = clamp8(((127 * sr - 106 * sg - 21 * sb) >> 8) + 128);
        }
    }
    Ok(())
}

#[inline]
fn sample_rgb(src: &[u8], pitch: usize, t: &Transform, x: u32, y: u32) -> (i32, i32, i32) {
    match t.sample(x, y) {
        Some((sx, sy)) => {
            let offset = sy as usize * pitch + sx as usize * 4;
            (
                i32::from(src[offset + 2]),
                i32::from(src[offset + 1]),
                i32::from(src[offset]),
            )
        }
        None => (0, 0, 0),
    }
}

#[inline]
fn clamp8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes() {
        for mode in [
            TransformMode::Crop,
            TransformMode::Fit,
            TransformMode::Fill,
            TransformMode::Stretch,
        ] {
            assert_eq!(TransformMode::parse(mode.as_str()).unwrap(), mode);
        }
        assert!(TransformMode::parse("scaled").is_err());
    }

    #[test]
    fn crop_uses_centered_window() {
        let t = Transform::new(1920, 1080, 640, 640, TransformMode::Crop).unwrap();
        assert_eq!((t.ox, t.oy, t.rw, t.rh), (640, 220, 640, 640));
        assert_eq!(t.sample(0, 0), Some((640, 220)));
        assert_eq!(t.sample(639, 639), Some((1279, 859)));
        assert_eq!(t.copy_rect(TransformMode::Crop, 1920, 1080).w, 640);
        assert!(Transform::new(320, 200, 640, 640, TransformMode::Crop).is_err());
    }

    #[test]
    fn fit_letterboxes_and_fill_crops_source() {
        let fit = Transform::new(1920, 1080, 640, 640, TransformMode::Fit).unwrap();
        assert_eq!((fit.dx, fit.dy, fit.dw, fit.dh), (0, 140, 640, 360));
        assert_eq!(fit.sample(0, 0), None);
        assert_eq!(fit.sample(0, 139), None);
        assert_eq!(fit.sample(0, 140), Some((0, 0)));
        assert_eq!(fit.sample(639, 499), Some((1917, 1077)));
        assert_eq!(fit.sample(0, 500), None);

        let fill = Transform::new(1920, 1080, 640, 640, TransformMode::Fill).unwrap();
        assert_eq!((fill.ox, fill.oy, fill.rw, fill.rh), (420, 0, 1080, 1080));
        assert_eq!(fill.sample(0, 0), Some((420, 0)));
        assert_eq!(fill.sample(639, 639), Some((1498, 1078)));
        assert_eq!(fill.copy_rect(TransformMode::Fill, 1920, 1080).w, 1920);
    }

    #[test]
    fn stretch_is_identity_sampling() {
        let t = Transform::new(1278, 718, 418, 282, TransformMode::Stretch).unwrap();
        for (x, y) in [(0, 0), (1, 0), (417, 281), (100, 50)] {
            let (sx, sy) = t.sample(x, y).unwrap();
            assert!(sx < 1278 && sy < 718);
        }
        assert_eq!(t.sample(0, 0), Some((0, 0)));
        assert_eq!(t.sample(417, 0), Some((1274, 0)));
        assert_eq!(t.sample(0, 281), Some((0, 715)));
    }

    #[test]
    fn even_sizes_are_enforced() {
        assert!(Transform::new(640, 640, 639, 640, TransformMode::Crop).is_err());
        assert!(Transform::new(640, 640, 640, 0, TransformMode::Crop).is_err());
        assert!(Transform::new(1, 640, 2, 2, TransformMode::Crop).is_err());
    }

    fn bgra(width: usize, height: usize, pixel: impl Fn(usize, usize) -> [u8; 4]) -> Vec<u8> {
        let mut data = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                data[(y * width + x) * 4..(y * width + x + 1) * 4].copy_from_slice(&pixel(x, y));
            }
        }
        data
    }

    #[test]
    fn converts_known_colors_exactly() {
        let white = bgra(2, 2, |_, _| [255, 255, 255, 0]);
        let t = Transform::new(2, 2, 2, 2, TransformMode::Stretch).unwrap();
        let mut nv12 = vec![0u8; 2 * 2 * 3 / 2];
        bgra_to_nv12(&white, 8, &t, &mut nv12).unwrap();
        assert_eq!(&nv12[..4], &[255, 255, 255, 255]);
        assert_eq!(&nv12[4..], &[128, 128]);

        let red = bgra(2, 2, |_, _| [0, 0, 255, 0]);
        bgra_to_nv12(&red, 8, &t, &mut nv12).unwrap();
        assert_eq!(&nv12[..4], &[76, 76, 76, 76]);
        assert_eq!(&nv12[4..], &[85, 254]);
    }

    #[test]
    fn identity_conversion_matches_rectangular_source() {
        let src = bgra(4, 4, |x, y| [x as u8 * 10, y as u8 * 20, 40, 0]);
        let t = Transform::identity(4, 4);
        let mut dst = vec![0u8; 4 * 4 * 3 / 2];
        bgra_to_nv12(&src, 16, &t, &mut dst).unwrap();
        // 手工计算 Y(0,0) 与 UV(0,0)（2×2 平均）。
        let b = 0i32;
        let g = 0i32;
        let r = 40i32;
        assert_eq!(dst[0], ((77 * r + 150 * g + 29 * b) >> 8) as u8);
        let (mut sr, mut sg, mut sb) = (0i32, 0i32, 0i32);
        for y in 0..2 {
            for x in 0..2 {
                sb += (x as u8 * 10) as i32;
                sg += (y as u8 * 20) as i32;
                sr += 40;
            }
        }
        sr >>= 2;
        sg >>= 2;
        sb >>= 2;
        assert_eq!(
            dst[16],
            clamp8(((-43 * sr - 84 * sg + 127 * sb) >> 8) + 128)
        );
        assert_eq!(
            dst[17],
            clamp8(((127 * sr - 106 * sg - 21 * sb) >> 8) + 128)
        );
    }

    #[test]
    fn fit_marks_letterbox_black() {
        // 4×2 源缩放到 2×4：保持宽高比后底部两行是黑边。
        let src = bgra(4, 2, |_, _| [255, 255, 255, 0]);
        let t = Transform::new(4, 2, 2, 4, TransformMode::Fit).unwrap();
        assert_eq!((t.dw, t.dh, t.dy), (2, 2, 0));
        let mut dst = vec![0u8; 2 * 4 * 3 / 2];
        bgra_to_nv12(&src, 16, &t, &mut dst).unwrap();
        assert_eq!(&dst[..4], &[255, 255, 255, 255]);
        assert_eq!(&dst[4..8], &[0, 0, 0, 0]);
        assert_eq!(&dst[8..], &[128, 128, 128, 128]);
    }

    #[test]
    fn rejects_short_buffers() {
        let t = Transform::new(2, 2, 2, 2, TransformMode::Stretch).unwrap();
        let mut dst = [0u8; 6];
        assert!(bgra_to_nv12(&[0u8; 15], 8, &t, &mut dst).is_err());
        assert!(bgra_to_nv12(&[0u8; 16], 8, &t, &mut [0u8; 5]).is_err());
    }

    #[test]
    fn padded_pitch_is_respected_without_reading_past_last_row() {
        // 2×2 源，pitch=12：每行 8 字节像素 + 4 字节 padding（填 0xFF）。
        let mut src = vec![0xFFu8; 24];
        for y in 0..2 {
            for x in 0..2 {
                let offset = y * 12 + x * 4;
                src[offset] = 10; // B
                src[offset + 1] = 20; // G
                src[offset + 2] = 30; // R
            }
        }
        let t = Transform::new(2, 2, 2, 2, TransformMode::Stretch).unwrap();
        let mut dst = vec![0u8; 6];
        bgra_to_nv12(&src, 12, &t, &mut dst).unwrap();
        // Y=(77*30+150*20+29*10)>>8=21；UV=(121,133)。
        assert_eq!(&dst[..4], &[21, 21, 21, 21]);
        assert_eq!(&dst[4..], &[121, 133]);
        // 只需最后一行到第 2 个像素为止：12+8=20 字节即可，19 字节必须拒绝。
        assert!(bgra_to_nv12(&src[..20], 12, &t, &mut dst).is_ok());
        assert!(bgra_to_nv12(&src[..19], 12, &t, &mut dst).is_err());
    }
}
