//! 加载捆绑的证书链与 RSA 私钥。
//!
//! 证书是**必须原样继承的资产**：相机只接受能链到它内置受信根 CA 的证书，
//! 而那张证书（`*.localtest.me`，COMODO/EssentialSSL 签发）恰好满足条件。
//! 它 2013 年就过期了，但相机不校验有效期/吊销/主机名 —— 见设计文档 §4.4。

use anyhow::{anyhow, bail, Context, Result};
use rsa::{BigUint, RsaPrivateKey, RsaPublicKey};

use crate::der;

/// 证书链（DER 拼接，每段前置 4 字节大端长度）
/// 顺序：叶证书在前，中间 CA 在后（相机要从叶链到根）
pub const CERT_CHAIN_DER: &[u8] = include_bytes!("../assets/certs.der");

/// PKCS#1 格式的 RSA 私钥（DER）
pub const PRIVATE_KEY_DER: &[u8] = include_bytes!("../assets/key.der");

/// 拆出证书链里的每一张证书（DER）
pub fn certificate_chain() -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut data = CERT_CHAIN_DER;
    while !data.is_empty() {
        if data.len() < 4 {
            bail!("certs.der 长度字段被截断");
        }
        let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let start = 4;
        let end = start + len;
        if data.len() < end {
            bail!("certs.der 内容被截断（声明 {len} 字节）");
        }
        out.push(data[start..end].to_vec());
        data = &data[end..];
    }
    if out.is_empty() {
        bail!("证书链为空");
    }
    Ok(out)
}

/// 解析 PKCS#1 私钥（DER）为 `RsaPrivateKey`
pub fn private_key() -> Result<RsaPrivateKey> {
    let seq = der::top_sequence(PRIVATE_KEY_DER).context("解析私钥 DER 失败")?;
    let items = der::sequence_items(seq).context("拆解私钥字段失败")?;
    if items.len() < 9 {
        bail!("PKCS#1 私钥字段不足（需要 9 个，得到 {}）", items.len());
    }
    let n = BigUint::from_bytes_be(items[1].as_int_be());
    let e = BigUint::from_bytes_be(items[2].as_int_be());
    let d = BigUint::from_bytes_be(items[3].as_int_be());

    // 后 5 个是 CRT 参数（p, q, dp, dq, qinv）。这里先只用 n/e/d 构造，
    // CRT 参数由 rsa crate 自己重算，避免手写解析出错。
    let _ = &items[4..9];

    RsaPrivateKey::from_components(n, e, d, vec![])
        .map_err(|err| anyhow!("构造 RSA 私钥失败：{err}"))
}

/// 对应的公钥（用于自检：验证解密能还原）
pub fn public_key() -> Result<RsaPublicKey> {
    Ok(RsaPublicKey::from(&private_key()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_has_leaf_and_intermediate() {
        let chain = certificate_chain().unwrap();
        assert_eq!(chain.len(), 2, "应为 叶证书 + 中间 CA 两张");
        // 每张都应是 DER SEQUENCE
        for c in &chain {
            assert_eq!(c[0], 0x30, "证书应为 DER SEQUENCE");
        }
        assert!(chain[0].len() > 1000);
        assert!(chain[1].len() > 1000);
    }

    #[test]
    fn private_key_is_rsa_2048() {
        use rsa::traits::PublicKeyParts;
        let key = private_key().unwrap();
        assert_eq!(key.n().bits(), 2048, "真机证书是 RSA-2048");
        assert_eq!(key.e(), &BigUint::from(65537u32));
    }

    /// 自检：用公钥加密、私钥解密能还原，证明 n/e/d 解析正确
    #[test]
    fn key_roundtrip() {
        use rsa::rand_core::OsRng;
        use rsa::traits::PublicKeyParts;
        let key = private_key().unwrap();
        assert_eq!(key.n().bits(), 2048);
        let pub_key = RsaPublicKey::from(&key);
        let msg = b"hello tls";
        let enc = pub_key
            .encrypt(&mut OsRng, rsa::Pkcs1v15Encrypt, msg)
            .unwrap();
        let dec = key
            .decrypt(rsa::Pkcs1v15Encrypt, &enc)
            .expect("解密应成功");
        assert_eq!(dec, msg, "解密结果必须与原文一致");
    }
}
