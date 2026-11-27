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
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = u32::MAX;
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
}
