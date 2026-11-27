//! 包载荷 CRC32 与整帧 FNV-1a64。
//!
//! CRC32 保持冻结算法，与Git历史C++实现完全一致：IEEE 反射多项式 `0xEDB88320`，初值/输出异或全 1。
//! 与 C++ 一样按 8 字节分块查表，尾部逐字节处理。
//! FNV-1a64 沿用冻结初值/质数，输出小写 16 位 hex。

const CRC32_POLY: u32 = 0xEDB8_8320;
const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

const CRC32_TABLE: [[u32; 256]; 8] = {
    let mut table = [[0; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                CRC32_POLY ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[0][i] = c;
        i += 1;
    }
    let mut slice = 1;
    while slice < 8 {
        let mut i = 0;
        while i < 256 {
            let c = table[slice - 1][i];
            table[slice][i] = (c >> 8) ^ table[0][c as u8 as usize];
            i += 1;
        }
        slice += 1;
    }
    table
};

/// 包载荷 CRC32（与 `vc_crc32` 相同）。
///
/// 在 x86_64 上运行时检测 `PCLMULQDQ + SSE4.1` 并走硬件折叠路径，结果与下方
/// slicing-by-8 及逐位参考实现逐比特一致。
pub fn crc32(data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if data.len() >= 16
        && std::is_x86_feature_detected!("pclmulqdq")
        && std::is_x86_feature_detected!("sse4.1")
    {
        // SAFETY: 已通过运行时特性检测确认 CPU 支持 PCLMULQDQ 与 SSE4.1，且长度 >= 16。
        return unsafe { x86_pclmul::crc32_pclmul(data) };
    }
    crc32_slicing_from(u32::MAX, data)
}

/// 纯标量 slicing-by-8 回退实现（供单测直接与 SIMD 路径对账使用）。
#[cfg(test)]
#[inline]
fn crc32_slicing(data: &[u8]) -> u32 {
    crc32_slicing_from(u32::MAX, data)
}

#[inline]
fn crc32_slicing_from(mut c: u32, data: &[u8]) -> u32 {
    let (chunks, tail) = data.as_chunks::<8>();
    for &chunk in chunks {
        let [a, b, d, e, f, g, h, i] = (u64::from_le_bytes(chunk) ^ u64::from(c)).to_le_bytes();
        c = CRC32_TABLE[7][a as usize]
            ^ CRC32_TABLE[6][b as usize]
            ^ CRC32_TABLE[5][d as usize]
            ^ CRC32_TABLE[4][e as usize]
            ^ CRC32_TABLE[3][f as usize]
            ^ CRC32_TABLE[2][g as usize]
            ^ CRC32_TABLE[1][h as usize]
            ^ CRC32_TABLE[0][i as usize];
    }
    for &byte in tail {
        c = CRC32_TABLE[0][(c as u8 ^ byte) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(target_arch = "x86_64")]
mod x86_pclmul {
    use std::arch::x86_64::*;

    // IEEE 802.3 CRC-32 (0x04C11DB7) 在反射域的 Intel PCLMULQDQ 折叠常量与 Barrett 常量。
    const K1: u64 = 0x1_5444_2bd4; // (X^544 mod P) << 1
    const K2: u64 = 0x1_c6e4_1596; // (X^480 mod P) << 1
    const K3: u64 = 0x1_7519_97d0; // (X^160 mod P) << 1
    const K4: u64 = 0x0_ccaa_009e; // (X^96 mod P) << 1
    const K5: u64 = 0x1_63cd_6124; // (X^64 mod P) << 1
    const MU: u64 = 0x1_f701_1641; // floor(X^64 / P) 反射 33 位
    const POLY: u64 = 0x1_db71_0641; // P 反射 33 位

    #[target_feature(enable = "pclmulqdq,sse4.1")]
    #[inline]
    fn fold_128(x: __m128i, rk: __m128i, next: __m128i) -> __m128i {
        let lo = _mm_clmulepi64_si128::<0x00>(x, rk);
        let hi = _mm_clmulepi64_si128::<0x11>(x, rk);
        _mm_xor_si128(_mm_xor_si128(lo, hi), next)
    }

    #[target_feature(enable = "pclmulqdq,sse4.1")]
    pub(super) unsafe fn crc32_pclmul(data: &[u8]) -> u32 {
        debug_assert!(data.len() >= 16);
        let mut ptr = data.as_ptr();
        let mut len = data.len();

        // 首个 16 字节与 CRC 初值 0xFFFF_FFFF 异或。
        let init = _mm_cvtsi32_si128(-1i32);
        // SAFETY: 调用方保证 len >= 16，首块 16 字节在切片范围内。
        let first = unsafe { _mm_loadu_si128(ptr as *const __m128i) };
        let mut x0 = _mm_xor_si128(first, init);
        // SAFETY: ptr..ptr+16 已消费。
        ptr = unsafe { ptr.add(16) };
        len -= 16;

        // 至少还有 48 字节时展开为 4 路（每轮 64 字节），匹配 Win64 易失寄存器预算。
        if len >= 48 {
            // SAFETY: len >= 48 保证接下来的 3 个 16 字节块均在切片内。
            let mut x1 = unsafe { _mm_loadu_si128(ptr as *const __m128i) };
            let mut x2 = unsafe { _mm_loadu_si128(ptr.add(16) as *const __m128i) };
            let mut x3 = unsafe { _mm_loadu_si128(ptr.add(32) as *const __m128i) };
            ptr = unsafe { ptr.add(48) };
            len -= 48;

            let rk12 = _mm_set_epi64x(K2 as i64, K1 as i64);
            while len >= 64 {
                // SAFETY: len >= 64 保证 4 个 16 字节块均在切片内。
                let d0 = unsafe { _mm_loadu_si128(ptr as *const __m128i) };
                let d1 = unsafe { _mm_loadu_si128(ptr.add(16) as *const __m128i) };
                let d2 = unsafe { _mm_loadu_si128(ptr.add(32) as *const __m128i) };
                let d3 = unsafe { _mm_loadu_si128(ptr.add(48) as *const __m128i) };
                x0 = fold_128(x0, rk12, d0);
                x1 = fold_128(x1, rk12, d1);
                x2 = fold_128(x2, rk12, d2);
                x3 = fold_128(x3, rk12, d3);
                ptr = unsafe { ptr.add(64) };
                len -= 64;
            }

            let rk34 = _mm_set_epi64x(K4 as i64, K3 as i64);
            x0 = fold_128(x0, rk34, x1);
            x0 = fold_128(x0, rk34, x2);
            x0 = fold_128(x0, rk34, x3);
        }

        let rk34 = _mm_set_epi64x(K4 as i64, K3 as i64);
        while len >= 16 {
            // SAFETY: len >= 16 保证 16 字节块在切片内。
            let d = unsafe { _mm_loadu_si128(ptr as *const __m128i) };
            x0 = fold_128(x0, rk34, d);
            ptr = unsafe { ptr.add(16) };
            len -= 16;
        }

        // 128 -> 64 -> 32 位 Barrett 约简，输出未取反的中间状态 c。
        let x_lo_folded = _mm_clmulepi64_si128::<0x10>(x0, rk34);
        let mut x = _mm_xor_si128(x_lo_folded, _mm_srli_si128::<8>(x0));

        let mask32 = _mm_cvtsi32_si128(-1i32);
        let rk5 = _mm_set_epi64x(0, K5 as i64);
        let t = _mm_clmulepi64_si128::<0x00>(_mm_and_si128(x, mask32), rk5);
        x = _mm_xor_si128(t, _mm_srli_si128::<4>(x));

        let rk_barrett = _mm_set_epi64x(POLY as i64, MU as i64);
        let t1 = _mm_clmulepi64_si128::<0x00>(_mm_and_si128(x, mask32), rk_barrett);
        let t2 = _mm_clmulepi64_si128::<0x10>(_mm_and_si128(t1, mask32), rk_barrett);
        let c = _mm_extract_epi32::<1>(_mm_xor_si128(x, t2)) as u32;

        // SAFETY: ptr..ptr+len 是剩余未处理的 0..15 字节尾部。
        let tail = unsafe { std::slice::from_raw_parts(ptr, len) };
        super::crc32_slicing_from(c, tail)
    }
}

/// 整帧 FNV-1a64（与 `vc_fnv1a64` 相同）。
#[inline]
fn fnv1a64_step(mut h: u64, data: &[u8]) -> u64 {
    // 按 8 字节分块读取，减少逐字节访存开销，保持逐字节乘法语义。
    let (chunks, tail) = data.as_chunks::<8>();
    for &chunk in chunks {
        let w = u64::from_le_bytes(chunk);
        h = (h ^ (w & 0xFF)).wrapping_mul(FNV_PRIME);
        h = (h ^ ((w >> 8) & 0xFF)).wrapping_mul(FNV_PRIME);
        h = (h ^ ((w >> 16) & 0xFF)).wrapping_mul(FNV_PRIME);
        h = (h ^ ((w >> 24) & 0xFF)).wrapping_mul(FNV_PRIME);
        h = (h ^ ((w >> 32) & 0xFF)).wrapping_mul(FNV_PRIME);
        h = (h ^ ((w >> 40) & 0xFF)).wrapping_mul(FNV_PRIME);
        h = (h ^ ((w >> 48) & 0xFF)).wrapping_mul(FNV_PRIME);
        h = (h ^ (w >> 56)).wrapping_mul(FNV_PRIME);
    }
    for &byte in tail {
        h ^= byte as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

pub fn fnv1a64(data: &[u8]) -> u64 {
    fnv1a64_step(FNV_OFFSET, data)
}

/// 两段拼接的 FNV-1a64，等价于先拼再算，但不复制（对应 `vc_fnv1a64_two`）。
pub fn fnv1a64_two(a: &[u8], b: &[u8]) -> u64 {
    let h = fnv1a64_step(FNV_OFFSET, a);
    fnv1a64_step(h, b)
}

/// 小写 16 位 hex（对应 `vc_fnv1a64_hex`）。
pub fn fnv1a64_hex(h: u64) -> String {
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // 外部已知向量：zlib.crc32 / FNV 规范值，不是本实现的回放。
    #[test]
    fn crc32_matches_zlib_reference_vectors() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
        assert_eq!(crc32(&[0u8; 8958]), 0xE702_33A9);
    }

    #[test]
    fn crc32_slicing_matches_bitwise_reference_at_every_tail_length() {
        let bytes: Vec<u8> = (0u32..9100)
            .map(|i| (i.wrapping_mul(0x045D_9F3B).rotate_right(i % 19) >> 13) as u8)
            .collect();
        let reference = |data: &[u8]| {
            !data.iter().fold(u32::MAX, |mut crc, &byte| {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ ((crc & 1).wrapping_neg() & CRC32_POLY);
                }
                crc
            })
        };
        for offset in 0..8 {
            for len in (0..32).chain(8952..8960) {
                let data = &bytes[offset..offset + len];
                assert_eq!(crc32(data), reference(data), "offset={offset}, len={len}");
            }
        }
    }

    #[test]
    fn fnv1a64_matches_spec_vectors() {
        assert_eq!(fnv1a64(b""), 0xCBF2_9CE4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xAF63_DC4C_8601_EC8C);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_F739_67E8);
        assert_eq!(fnv1a64_hex(fnv1a64(b"foobar")), "85944171f73967e8");
    }

    #[test]
    fn two_part_hash_equals_concatenation() {
        let y = [1u8, 2, 3, 4];
        let uv = [5u8, 6];
        let mut joined = Vec::from(y);
        joined.extend_from_slice(&uv);
        assert_eq!(fnv1a64_two(&y, &uv), fnv1a64(&joined));
    }

    #[test]
    fn crc32_simd_matches_slicing_and_bitwise_across_offsets_and_lengths() {
        let bytes: Vec<u8> = (0u32..16500)
            .map(|i| (i.wrapping_mul(0x045D_9F3B).rotate_right(i % 19) >> 13) as u8)
            .collect();
        let bitwise = |data: &[u8]| {
            !data.iter().fold(u32::MAX, |mut crc, &byte| {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ ((crc & 1).wrapping_neg() & CRC32_POLY);
                }
                crc
            })
        };

        for offset in 0..16 {
            for len in (0..=200)
                .chain(1440..=1472)
                .chain(8940..=8976)
                .chain(16368..=16400)
            {
                let data = &bytes[offset..offset + len];
                let expected = crc32_slicing(data);
                assert_eq!(crc32(data), expected, "dispatch offset={offset} len={len}");
                if len <= 200 || len == 1458 || len == 8958 || len == 16384 {
                    assert_eq!(expected, bitwise(data), "bitwise offset={offset} len={len}");
                }
                #[cfg(target_arch = "x86_64")]
                if len >= 16
                    && std::is_x86_feature_detected!("pclmulqdq")
                    && std::is_x86_feature_detected!("sse4.1")
                {
                    let simd = unsafe { x86_pclmul::crc32_pclmul(data) };
                    assert_eq!(simd, expected, "pclmul offset={offset} len={len}");
                }
            }
        }
    }

    #[test]
    fn fnv1a64_and_two_part_match_naive_bytewise_across_splits_and_offsets() {
        let bytes: Vec<u8> = (0u32..128)
            .map(|i| (i.wrapping_mul(0x9E37_79B1) >> 24) as u8)
            .collect();
        let naive = |data: &[u8]| {
            let mut h = FNV_OFFSET;
            for &b in data {
                h ^= b as u64;
                h = h.wrapping_mul(FNV_PRIME);
            }
            h
        };
        for offset in 0..16 {
            for len in 0..=40 {
                let slice = &bytes[offset..offset + len];
                let expected = naive(slice);
                assert_eq!(fnv1a64(slice), expected, "offset={offset} len={len}");
                for split in 0..=len {
                    let (a, b) = slice.split_at(split);
                    assert_eq!(
                        fnv1a64_two(a, b),
                        expected,
                        "split={split} offset={offset} len={len}"
                    );
                }
            }
        }
        // 640x640 NV12 典型尺寸 (Y=409600, UV=204800) 分段等价性。
        let nv12: Vec<u8> = (0..614_400).map(|i| (i * 29 % 251) as u8).collect();
        let (y, uv) = nv12.split_at(409_600);
        assert_eq!(fnv1a64_two(y, uv), fnv1a64(&nv12));
    }
}
