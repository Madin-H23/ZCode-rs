//! AES-128-ECB 解密 + PKCS7 去填充（微信 iLink CDN 附件，`decryptWeixinCdnMedia`）。
//!
//! 依赖树里没有可用的 AES 分组密码实现（ring 只暴露 AEAD），这里按 FIPS-197 实现解密方向，
//! 用标准测试向量校验。只用于解密第三方下发的媒体，不承担任何保密职责。

const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

fn inverse_sbox() -> [u8; 256] {
    let mut inverse = [0u8; 256];
    for (index, value) in SBOX.iter().enumerate() {
        inverse[*value as usize] = index as u8;
    }
    inverse
}

fn xtime(value: u8) -> u8 {
    (value << 1) ^ if value & 0x80 != 0 { 0x1b } else { 0 }
}

fn mul(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    product
}

/// AES-128 密钥扩展：11 个轮密钥。
fn expand_key(key: &[u8; 16]) -> [[u8; 16]; 11] {
    let mut words = [[0u8; 4]; 44];
    for (index, word) in words.iter_mut().take(4).enumerate() {
        word.copy_from_slice(&key[index * 4..index * 4 + 4]);
    }
    let mut rcon = 1u8;
    for index in 4..44 {
        let mut temp = words[index - 1];
        if index % 4 == 0 {
            temp.rotate_left(1);
            for byte in temp.iter_mut() {
                *byte = SBOX[*byte as usize];
            }
            temp[0] ^= rcon;
            rcon = xtime(rcon);
        }
        for byte in 0..4 {
            words[index][byte] = words[index - 4][byte] ^ temp[byte];
        }
    }
    let mut rounds = [[0u8; 16]; 11];
    for (round, out) in rounds.iter_mut().enumerate() {
        for word in 0..4 {
            out[word * 4..word * 4 + 4].copy_from_slice(&words[round * 4 + word]);
        }
    }
    rounds
}

fn add_round_key(state: &mut [u8; 16], key: &[u8; 16]) {
    for (byte, key) in state.iter_mut().zip(key) {
        *byte ^= key;
    }
}

fn decrypt_block(block: &mut [u8; 16], rounds: &[[u8; 16]; 11], inverse: &[u8; 256]) {
    add_round_key(block, &rounds[10]);
    for round in (0..10).rev() {
        // InvShiftRows（状态按列存储：下标 = 列 * 4 + 行）。
        let copy = *block;
        for column in 0..4 {
            for row in 0..4 {
                block[column * 4 + row] = copy[((column + 4 - row) % 4) * 4 + row];
            }
        }
        for byte in block.iter_mut() {
            *byte = inverse[*byte as usize];
        }
        add_round_key(block, &rounds[round]);
        if round > 0 {
            for column in 0..4 {
                let c = &mut block[column * 4..column * 4 + 4];
                let (a0, a1, a2, a3) = (c[0], c[1], c[2], c[3]);
                c[0] = mul(a0, 14) ^ mul(a1, 11) ^ mul(a2, 13) ^ mul(a3, 9);
                c[1] = mul(a0, 9) ^ mul(a1, 14) ^ mul(a2, 11) ^ mul(a3, 13);
                c[2] = mul(a0, 13) ^ mul(a1, 9) ^ mul(a2, 14) ^ mul(a3, 11);
                c[3] = mul(a0, 11) ^ mul(a1, 13) ^ mul(a2, 9) ^ mul(a3, 14);
            }
        }
    }
}

/// AES-128-ECB 解密并去掉 PKCS7 填充；密文长度或填充非法时报错（同 Node `decipher.final()`）。
pub fn decrypt_ecb_pkcs7(data: &[u8], key: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        anyhow::bail!("wrong final block length");
    }
    let rounds = expand_key(key);
    let inverse = inverse_sbox();
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        decrypt_block(&mut block, &rounds, &inverse);
        out.extend_from_slice(&block);
    }
    let pad = *out.last().unwrap_or(&0) as usize;
    if pad == 0
        || pad > 16
        || out[out.len() - pad..]
            .iter()
            .any(|byte| *byte as usize != pad)
    {
        anyhow::bail!("bad decrypt");
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

#[cfg(test)]
pub(crate) fn encrypt_block_for_tests(block: &mut [u8; 16], key: &[u8; 16]) {
    let rounds = expand_key(key);
    add_round_key(block, &rounds[0]);
    for (round, round_key) in rounds.iter().enumerate().skip(1) {
        for byte in block.iter_mut() {
            *byte = SBOX[*byte as usize];
        }
        let copy = *block;
        for column in 0..4 {
            for row in 0..4 {
                block[column * 4 + row] = copy[((column + row) % 4) * 4 + row];
            }
        }
        if round < 10 {
            for column in 0..4 {
                let c = &mut block[column * 4..column * 4 + 4];
                let (a0, a1, a2, a3) = (c[0], c[1], c[2], c[3]);
                c[0] = mul(a0, 2) ^ mul(a1, 3) ^ a2 ^ a3;
                c[1] = a0 ^ mul(a1, 2) ^ mul(a2, 3) ^ a3;
                c[2] = a0 ^ a1 ^ mul(a2, 2) ^ mul(a3, 3);
                c[3] = mul(a0, 3) ^ a1 ^ a2 ^ mul(a3, 2);
            }
        }
        add_round_key(block, round_key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_fips_197_vector() {
        let key: [u8; 16] = std::array::from_fn(|index| index as u8);
        let mut block: [u8; 16] = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        let rounds = expand_key(&key);
        decrypt_block(&mut block, &rounds, &inverse_sbox());
        let plain: [u8; 16] = std::array::from_fn(|index| (index as u8) * 0x11);
        assert_eq!(block, plain);
        let mut again = plain;
        encrypt_block_for_tests(&mut again, &key);
        assert_eq!(again[0], 0x69);
        // 一个带 PKCS7 填充的块。
        let mut padded = *b"hello\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b";
        encrypt_block_for_tests(&mut padded, &key);
        assert_eq!(decrypt_ecb_pkcs7(&padded, &key).unwrap(), b"hello");
        assert!(decrypt_ecb_pkcs7(&padded[..15], &key).is_err());
    }
}
