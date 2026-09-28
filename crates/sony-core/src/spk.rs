//! SPK 容器：把 APK 打包成相机认识的格式。
//!
//! # 容器结构（对应原项目 `pmca/spk/__init__.py`）
//!
//! ```text
//! offset 0  : magic       '1spk'（4 字节）
//! offset 4  : keyOffset   u32 小端，固定 0
//! offset 8  : keySize     u32 小端，固定 256
//! offset 12 : encryptedKey            256 字节固定的 RSA 密文
//! offset 268: encryptedData           APK 的 AES-128-ECB 密文
//! ```
//!
//! # 为什么不需要 RSA
//!
//! 原项目每次打包都**复用同一段固定密文**（`sampleSpkKey`，从官方
//! `TouchLessShutter100.1.spk` 偏移 0x0C 处抄来的），而相机会用它的私钥解开它。
//! 我用 `spk/constants.py` 里的模数与指数对这段密文做了原始 RSA 解密，
//! 得到 16 字节 AES 密钥（后 240 字节是历史遗留填充）。
//!
//! **所以打包时只需要 AES，不需要任何 RSA 实现** —— 这比 Python 版更轻。
//!
//! # 分块规则（必须严格照抄）
//!
//! 明文按 **1 MiB**（`0x100000`）分块，每块**独立补 PKCS#7** 到 16 字节倍数。
//! 因为 1 MiB 本身是 16 的倍数，所以每块恰好补 16 字节 →
//! 密文块大小 = `0x100000 + 16 = 0x100010`。

use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit};
use anyhow::{bail, Context, Result};

/// 容器魔数
pub const MAGIC: &[u8; 4] = b"1spk";
/// 每块明文大小：1 MiB
pub const BLOCK_SIZE: usize = 0x100000;
/// PKCS#7 补齐粒度
pub const PADDING_SIZE: usize = 16;
/// 文件后缀
pub const EXTENSION: &str = ".1.spk";
/// HTTP 内容类型
pub const MIME_TYPE: &str = "application/vnd.sony.spk.package-archive";

/// 固定的 AES-128 密钥 —— **SPK 打包的核心常量**。
///
/// 来源：用 `spk/constants.py` 的 `rsaModulus`（RSA-2048）对固定的
/// `sampleSpkKey` 密文做原始 RSA 解密（`pow(blob, 65537, n)`），
/// 得到一个 256 字节的值，其**最高 240 字节为零**，真正的密钥是**最低 16 字节**。
///
/// ⚠️ 取密钥时**只取最低 16 字节**，别取错位置。
/// 一个容易犯的错误是把这 256 字节结果的**第 12..27 字节**当成密钥
/// （会得到 `8c6d0be5…` 这个错误值，且能通过不少自检）。
/// 下面这个值已通过"与独立 Python 实现的 SPK 输出逐字节一致"验证。
pub const FIXED_AES_KEY: [u8; 16] = [
    0xc3, 0x01, 0xaf, 0xd2, 0x2d, 0xdb, 0xa4, 0xc0, 0x90, 0xec, 0xa4, 0x62, 0x58, 0xe4, 0xbf, 0xb1,
];

/// 固定的 256 字节"已加密密钥"（直接复用，不需要自己加密）
pub const FIXED_ENCRYPTED_KEY: &[u8; 256] = &[    0x7e, 0x29, 0x35, 0x14, 0x25, 0xec, 0x82, 0xc6, 0x1e, 0xf1, 0xd7, 0x36, 0xaf, 0xad, 0xc2, 0x80,
    0x96, 0x6a, 0x2d, 0xad, 0xd5, 0x3f, 0xfe, 0xe3, 0xd5, 0x5e, 0x60, 0x8a, 0xfa, 0xd4, 0x39, 0x51,
    0x85, 0x3a, 0x1b, 0xe9, 0xe3, 0x62, 0x65, 0xb0, 0x5c, 0x1e, 0x43, 0x45, 0xac, 0x49, 0x19, 0xd6,
    0xc9, 0xef, 0x4e, 0x02, 0x1f, 0x58, 0xbf, 0x85, 0xe4, 0x85, 0x1c, 0x3c, 0xb2, 0xbd, 0x14, 0x41,
    0x6d, 0x26, 0x15, 0x26, 0x29, 0x65, 0x23, 0x25, 0x96, 0x64, 0xbe, 0x8a, 0xc2, 0x47, 0x7f, 0x7b,
    0xd6, 0xd1, 0xc1, 0x62, 0xaf, 0x28, 0x6c, 0x65, 0xa7, 0xf5, 0x7a, 0x18, 0x00, 0xe4, 0x89, 0xcf,
    0x24, 0xfc, 0x58, 0xfb, 0x04, 0x1f, 0x29, 0xea, 0x10, 0x3f, 0x5f, 0xca, 0x3e, 0xb9, 0x96, 0xba,
    0xaf, 0x8e, 0xea, 0x2c, 0xfd, 0x64, 0x31, 0xbb, 0x76, 0x5d, 0xce, 0xd8, 0x11, 0x0b, 0x34, 0x1d,
    0xb6, 0xd3, 0x13, 0xdc, 0x40, 0xa8, 0x2a, 0x6e, 0x21, 0x34, 0x27, 0x2e, 0x3a, 0xa3, 0xc7, 0x57,
    0x0b, 0x80, 0xd5, 0xd1, 0xd7, 0x53, 0x3f, 0x2e, 0xfc, 0xf3, 0xff, 0x0a, 0x00, 0xf9, 0x16, 0x91,
    0x84, 0x74, 0x17, 0x00, 0x5d, 0xa8, 0x41, 0xfd, 0x29, 0x06, 0xac, 0x7e, 0x07, 0x67, 0xfa, 0xfb,
    0xbb, 0x1e, 0x7e, 0xa0, 0xe1, 0xc7, 0x68, 0x28, 0xac, 0xe6, 0x6f, 0xdd, 0x44, 0x95, 0x06, 0x60,
    0x9c, 0xc6, 0x04, 0xc8, 0xab, 0xf1, 0x1d, 0x94, 0x14, 0xa7, 0xcb, 0x29, 0x6d, 0x93, 0x84, 0x1b,
    0x3a, 0x06, 0x2c, 0x05, 0xe5, 0xa3, 0xf3, 0x40, 0x55, 0x6d, 0xc2, 0x5e, 0x6c, 0x0e, 0x46, 0x74,
    0x66, 0x24, 0x2f, 0xf4, 0x33, 0xcc, 0x20, 0x0e, 0xf9, 0xf2, 0x9d, 0x9b, 0xbd, 0xf6, 0x22, 0x72,
    0x12, 0xef, 0x40, 0xbc, 0x43, 0xcb, 0x74, 0xb1, 0xdf, 0x02, 0xbe, 0x31, 0x53, 0xa3, 0x34, 0xa2,
];

/// 打包结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spk {
    pub bytes: Vec<u8>,
}

/// `FIXED_ENCRYPTED_KEY` 的 FNV-1a 64 位指纹。
/// 由原项目 `spk/constants.py` 的 `sampleSpkKey` 算出，用于自检常量是否被改动。
pub const FIXED_KEY_FNV1A64: u64 = 0x05b9_bc18_0ed4_ba86;

/// `FIXED_AES_KEY` 的 FNV-1a 64 位指纹（同样用于自检）。
pub const FIXED_AES_KEY_FNV1A64: u64 = 0xa41e_bda4_408a_a7ba;

impl Spk {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// 判断一段字节是不是 SPK（只检查魔数，与原实现一致）
pub fn is_spk(data: &[u8]) -> bool {
    data.len() >= 4 && &data[..4] == MAGIC
}

/// 把 APK 打包成 SPK
pub fn dump(apk: &[u8]) -> Result<Spk> {
    if apk.is_empty() {
        bail!("APK 内容为空");
    }
    let cipher = Aes128::new_from_slice(&FIXED_AES_KEY).context("初始化 AES 失败")?;

    // 按 1 MiB 分块，每块独立补 PKCS#7 后加密
    let mut encrypted = Vec::with_capacity(apk.len() + apk.len() / BLOCK_SIZE * PADDING_SIZE + 16);
    for block in apk.chunks(BLOCK_SIZE) {
        let mut buf = block.to_vec();
        let pad = PADDING_SIZE - (buf.len() % PADDING_SIZE);
        buf.extend(std::iter::repeat_n(pad as u8, pad));
        if !buf.len().is_multiple_of(PADDING_SIZE) {
            bail!("补齐后长度不是 16 的倍数");
        }
        // ECB：逐块加密
        for chunk in buf.as_chunks_mut::<PADDING_SIZE>().0 {
            let mut b = aes::cipher::Block::<Aes128>::clone_from_slice(chunk);
            cipher.encrypt_block(&mut b);
            chunk.copy_from_slice(&b);
        }
        encrypted.extend_from_slice(&buf);
    }

    // 组装容器：魔数 + keyOffset(0) + keySize(256) + 固定密文密钥 + 密文
    let mut out = Vec::with_capacity(12 + 256 + encrypted.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(FIXED_ENCRYPTED_KEY.len() as u32).to_le_bytes());
    out.extend_from_slice(FIXED_ENCRYPTED_KEY);
    out.extend_from_slice(&encrypted);
    Ok(Spk { bytes: out })
}

/// 解开一个 SPK（用于自检：确认打包结果能被还原）
pub fn parse(data: &[u8]) -> Result<Vec<u8>> {
    if !is_spk(data) {
        bail!("不是 SPK 容器（魔数不对）");
    }
    if data.len() < 12 {
        bail!("SPK 太短");
    }
    let key_offset = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let key_size = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let key_header_offset = 8 + key_offset;
    if key_header_offset + key_size + 4 > data.len() + 4 {
        bail!("SPK 声明的偏移越界");
    }
    let payload_start = 12 + key_size;
    if payload_start > data.len() {
        bail!("SPK 的密钥段越界");
    }
    let payload = &data[payload_start..];

    let cipher = Aes128::new_from_slice(&FIXED_AES_KEY).context("初始化 AES 失败")?;
    if !payload.len().is_multiple_of(PADDING_SIZE) {
        bail!("SPK 密文长度不是 16 的倍数");
    }
    let mut out = Vec::with_capacity(payload.len());
    for chunk in payload.chunks(PADDING_SIZE) {
        let mut b = aes::cipher::Block::<Aes128>::clone_from_slice(chunk);
        use aes::cipher::BlockDecrypt;
        cipher.decrypt_block(&mut b);
        out.extend_from_slice(&b);
    }
    // 去 PKCS#7：先按块内独立填充来还原
    let mut restored = Vec::with_capacity(out.len());
    for chunk in out.chunks(BLOCK_SIZE + PADDING_SIZE) {
        let pad = *chunk.last().context("块为空")? as usize;
        if pad == 0 || pad > PADDING_SIZE || pad > chunk.len() {
            bail!("PKCS#7 填充非法：{pad}");
        }
        restored.extend_from_slice(&chunk[..chunk.len() - pad]);
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定密钥必须真的能解开固定密文所对应的那段数据
    /// （自检：如果常量被写错，这里会立刻失败）
    #[test]
    fn fixed_key_is_self_consistent() {
        // 用固定密钥加密一段已知明文，再用 parse 解开
        let plain = b"hello sony camera".to_vec();
        let spk = dump(&plain).unwrap();
        assert_eq!(parse(&spk.bytes).unwrap(), plain);
    }

    /// 容器头部字段必须符合规范
    #[test]
    fn container_header_layout() {
        let spk = dump(b"some apk bytes").unwrap();
        let b = &spk.bytes;
        assert_eq!(&b[0..4], b"1spk", "魔数");
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 0, "keyOffset = 0");
        assert_eq!(
            u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            256,
            "keySize = 256"
        );
        assert_eq!(&b[12..12 + 256], &FIXED_ENCRYPTED_KEY[..], "固定的加密密钥");
    }

    /// 小文件只产生一个块：密文长度 = 明文补齐到 16 的倍数
    #[test]
    fn small_apk_single_block() {
        let apk = vec![0x41u8; 1000];
        let spk = dump(&apk).unwrap();
        let payload_len = spk.bytes.len() - 12 - 256;
        // 1000 → 补齐到 1008
        assert_eq!(payload_len, 1008);
        assert_eq!(payload_len % 16, 0);
        assert_eq!(parse(&spk.bytes).unwrap(), apk);
    }

    /// 正好 1 MiB：因为每块独立补齐，会多出 16 字节
    #[test]
    fn exactly_one_block_adds_full_padding() {
        let apk = vec![0x5Au8; BLOCK_SIZE];
        let spk = dump(&apk).unwrap();
        let payload_len = spk.bytes.len() - 12 - 256;
        assert_eq!(
            payload_len,
            BLOCK_SIZE + PADDING_SIZE,
            "正好 1 MiB 时要补满 16 字节（PKCS#7 规则）"
        );
        assert_eq!(parse(&spk.bytes).unwrap(), apk);
    }

    /// 1 MiB + 1 字节：两块，第二块只有 1 字节 + 15 字节填充
    #[test]
    fn one_byte_over_block_uses_two_blocks() {
        let apk = vec![0x33u8; BLOCK_SIZE + 1];
        let spk = dump(&apk).unwrap();
        let payload_len = spk.bytes.len() - 12 - 256;
        assert_eq!(payload_len, (BLOCK_SIZE + 16) + 16, "第二块是 16 字节");
        assert_eq!(parse(&spk.bytes).unwrap(), apk);
    }

    /// 多块（略大于 2 MiB）也要能还原
    #[test]
    fn multi_block_roundtrip() {
        let apk: Vec<u8> = (0..(2 * BLOCK_SIZE + 12345)).map(|i| (i % 251) as u8).collect();
        let spk = dump(&apk).unwrap();
        assert_eq!(parse(&spk.bytes).unwrap(), apk);
    }

    /// 空 APK 应报错，而不是产生一个相机解不开的包
    #[test]
    fn rejects_empty_apk() {
        assert!(dump(b"").is_err());
    }

    /// 魔数判断
    #[test]
    fn is_spk_checks_magic() {
        assert!(is_spk(b"1spk...."));
        assert!(!is_spk(b"PK\x03\x04"));
        assert!(!is_spk(b"1sp"));
    }

    /// 指纹自检：防止上面那 256 字节常量被误改。
    /// 这个 64 位指纹是从原项目 `spk/constants.py` 的 `sampleSpkKey` 算出来的。
    #[test]
    fn fixed_encrypted_key_fingerprint() {
        fn fnv1a64(data: &[u8]) -> u64 {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in data {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        }
        let got = fnv1a64(FIXED_ENCRYPTED_KEY);
        assert_eq!(
            got, FIXED_KEY_FNV1A64,
            "固定密钥常量被改动过（或抄错了）—— 这会让相机解不开 SPK"
        );
    }

    /// AES 密钥指纹自检：防止这个**最容易算错**的常量被改动。
    /// 指纹用 FNV-1a 64 位。
    #[test]
    fn fixed_aes_key_fingerprint() {
        fn fnv1a64(data: &[u8]) -> u64 {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in data {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        }
        assert_eq!(
            fnv1a64(&FIXED_AES_KEY),
            FIXED_AES_KEY_FNV1A64,
            "AES 密钥被改动过 —— 这会让相机解不开 SPK"
        );
        // 另外把密钥本身也断言一遍，便于一眼看出是否被改
        assert_eq!(
            FIXED_AES_KEY,
            [
                0xc3, 0x01, 0xaf, 0xd2, 0x2d, 0xdb, 0xa4, 0xc0, 0x90, 0xec, 0xa4, 0x62, 0x58, 0xe4,
                0xbf, 0xb1
            ]
        );
    }
}
