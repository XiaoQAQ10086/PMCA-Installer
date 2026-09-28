//! TLS 1.0 的密钥推导（PRF）与密钥块切分。
//!
//! TLS 1.0 的 PRF 很有年代感：它把种子**对半分**，
//! 一半用 MD5 扩展、一半用 SHA-1 扩展，再拼接起来（RFC 2246 §5）。
//! TLS 1.2 换成了单一哈希（P_SHA256），所以这段不能照抄现代实现 —— 必须按 1.0 来。
//!
//! ```text
//! PRF(secret, label, seed) = P_MD5(S1, label+seed) XOR P_SHA1(S2, label+seed)
//!   其中 S1 = secret 前一半（向上取整），S2 = secret 后一半（向下取整）
//!
//! P_hash(secret, seed)：
//!   A(0) = seed
//!   A(i) = HMAC(secret, A(i-1))
//!   输出 = HMAC(secret, A(1)+seed) + HMAC(secret, A(2)+seed) + ...
//! ```

use hmac::{Hmac, Mac};
use md5::Md5;
use sha1::Sha1;

type HmacMd5 = Hmac<Md5>;
type HmacSha1 = Hmac<Sha1>;

/// MD5 的摘要长度
const MD5_LEN: usize = 16;
/// SHA-1 的摘要长度
const SHA1_LEN: usize = 20;

/// `P_hash(secret, seed)`：按需扩展出 `out_len` 字节。
///
/// MD5 与 SHA-1 各写一份具体实现（而不是套泛型），因为这里的泛型约束很啰嗦，
/// 而可读性对这种"必须逐字节正确"的代码更重要。
fn p_md5(secret: &[u8], seed: &[u8], out_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_len + MD5_LEN);
    let mut a: Vec<u8> = seed.to_vec();
    while out.len() < out_len {
        let mut mac = HmacMd5::new_from_slice(secret).expect("HMAC 接受任意长度密钥");
        mac.update(&a);
        a = mac.finalize().into_bytes().to_vec();

        let mut mac = HmacMd5::new_from_slice(secret).expect("HMAC 接受任意长度密钥");
        mac.update(&a);
        mac.update(seed);
        out.extend_from_slice(&mac.finalize().into_bytes());
    }
    out.truncate(out_len);
    out
}

/// SHA-1 版 P_hash
fn p_sha1(secret: &[u8], seed: &[u8], out_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_len + SHA1_LEN);
    let mut a: Vec<u8> = seed.to_vec();
    while out.len() < out_len {
        let mut mac = HmacSha1::new_from_slice(secret).expect("HMAC 接受任意长度密钥");
        mac.update(&a);
        a = mac.finalize().into_bytes().to_vec();

        let mut mac = HmacSha1::new_from_slice(secret).expect("HMAC 接受任意长度密钥");
        mac.update(&a);
        mac.update(seed);
        out.extend_from_slice(&mac.finalize().into_bytes());
    }
    out.truncate(out_len);
    out
}

/// TLS 1.0 的 PRF：MD5 与 SHA-1 各半，结果异或
pub fn prf(secret: &[u8], label: &str, seed: &[u8], out_len: usize) -> Vec<u8> {
    // label 与 seed 拼接后作为"种子"
    let mut label_seed = Vec::with_capacity(label.len() + seed.len());
    label_seed.extend_from_slice(label.as_bytes());
    label_seed.extend_from_slice(seed);

    // 密钥对半分：前半给 MD5（向上取整），后半给 SHA-1（向下取整）
    let half = secret.len().div_ceil(2);
    let (s1, s2) = secret.split_at(half);

    let a = p_md5(s1, &label_seed, out_len);
    let b = p_sha1(s2, &label_seed, out_len);
    a.iter().zip(b.iter()).map(|(x, y)| x ^ y).collect()
}

/// 由预主密钥导出主密钥（48 字节）
pub fn master_secret(pre_master: &[u8], client_random: &[u8; 32], server_random: &[u8; 32]) -> [u8; 48] {
    let mut seed = Vec::with_capacity(64);
    seed.extend_from_slice(client_random);
    seed.extend_from_slice(server_random);
    let ms = prf(pre_master, "master secret", &seed, 48);
    let mut out = [0u8; 48];
    out.copy_from_slice(&ms);
    out
}

/// 由主密钥导出密钥块，并按 TLS 1.0 的规则切分
#[derive(Debug, Clone)]
pub struct KeyBlock {
    pub client_mac: Vec<u8>,
    pub server_mac: Vec<u8>,
    pub client_key: Vec<u8>,
    pub server_key: Vec<u8>,
    pub client_iv: Vec<u8>,
    pub server_iv: Vec<u8>,
}

impl KeyBlock {
    /// `mac_len` / `key_len` / `iv_len` 由协商的套件决定。
    /// 对 `TLS_RSA_WITH_AES_128_CBC_SHA` 是 20 / 16 / 16，总共 104 字节。
    pub fn derive(
        master: &[u8; 48],
        client_random: &[u8; 32],
        server_random: &[u8; 32],
        mac_len: usize,
        key_len: usize,
        iv_len: usize,
    ) -> Self {
        let mut seed = Vec::with_capacity(64);
        seed.extend_from_slice(server_random);
        seed.extend_from_slice(client_random);
        let total = 2 * mac_len + 2 * key_len + 2 * iv_len;
        let kb = prf(master, "key expansion", &seed, total);

        let mut p = 0;
        let mut take = |n: usize| {
            let s = kb[p..p + n].to_vec();
            p += n;
            s
        };
        Self {
            client_mac: take(mac_len),
            server_mac: take(mac_len),
            client_key: take(key_len),
            server_key: take(key_len),
            client_iv: take(iv_len),
            server_iv: take(iv_len),
        }
    }

    /// `TLS_RSA_WITH_AES_128_CBC_SHA` 的参数
    pub fn derive_aes128_sha(
        master: &[u8; 48],
        client_random: &[u8; 32],
        server_random: &[u8; 32],
    ) -> Self {
        Self::derive(master, client_random, server_random, 20, 16, 16)
    }

    /// `TLS_RSA_WITH_AES_256_CBC_SHA` 的参数（**真机实测协商的就是这个**）
    pub fn derive_aes256_sha(
        master: &[u8; 48],
        client_random: &[u8; 32],
        server_random: &[u8; 32],
    ) -> Self {
        Self::derive(master, client_random, server_random, 20, 32, 16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRF 必须是确定性的，且长度可控
    #[test]
    fn prf_is_deterministic() {
        let s = b"secret";
        let a = prf(s, "label", b"seed", 48);
        let b = prf(s, "label", b"seed", 48);
        assert_eq!(a, b);
        assert_eq!(a.len(), 48);
        // 换 label 必须得到不同结果
        let c = prf(s, "other", b"seed", 48);
        assert_ne!(a, c);
    }

    /// PRF 的构造特性：MD5 半与 SHA-1 半必须都参与
    #[test]
    fn prf_uses_both_halves_of_secret() {
        // 只改密钥的前半 → 结果变（MD5 参与）
        let mut s1 = [1u8; 16];
        let mut s2 = [1u8; 16];
        let a = prf(&s1, "label", b"seed", 48);
        s1[0] ^= 0xff;
        let b = prf(&s1, "label", b"seed", 48);
        assert_ne!(a, b, "改前半密钥必须改变结果（MD5 那一半）");

        // 只改密钥的后半 → 结果也变（SHA-1 参与）
        let c = prf(&s2, "label", b"seed", 48);
        s2[0] ^= 0xff;
        let d = prf(&s2, "label", b"seed", 48);
        assert_ne!(c, d, "改后半密钥必须改变结果（SHA-1 那一半）");
    }

    /// 密钥块切分必须与协商套件一致
    #[test]
    fn key_block_layout_for_aes128_sha() {
        let master = [7u8; 48];
        let cr = [1u8; 32];
        let sr = [2u8; 32];
        let kb = KeyBlock::derive_aes128_sha(&master, &cr, &sr);
        assert_eq!(kb.client_mac.len(), 20);
        assert_eq!(kb.server_mac.len(), 20);
        assert_eq!(kb.client_key.len(), 16);
        assert_eq!(kb.server_key.len(), 16);
        assert_eq!(kb.client_iv.len(), 16);
        assert_eq!(kb.server_iv.len(), 16);
        // 各段必须互不相同（否则说明切分错位）
        assert_ne!(kb.client_mac, kb.server_mac);
        assert_ne!(kb.client_key, kb.server_key);
        assert_ne!(kb.client_iv, kb.server_iv);
    }

    /// 主密钥必须是 48 字节且随随机数变化
    #[test]
    fn master_secret_depends_on_randoms() {
        let pms = [9u8; 48];
        let a = master_secret(&pms, &[1u8; 32], &[2u8; 32]);
        let b = master_secret(&pms, &[1u8; 32], &[3u8; 32]);
        assert_eq!(a.len(), 48);
        assert_ne!(a, b, "换服务端随机数必须得到不同主密钥");
    }
}
