//! TLS 1.0 的记录层：CBC 解密 + MAC 校验。
//!
//! # TLS 1.0 的三个"年代特征"（现代实现里已经不这样了）
//!
//! 1. **隐式 IV**：CBC 的 IV 不用协商，也不随记录发送。
//!    第一条记录用密钥块里的 IV；**之后每条记录用上一条记录的最后一个密文块**
//!    （RFC 2246 §6.2.3.2）。这就是后来 BEAST 攻击的成因。
//! 2. **MAC-then-encrypt**：先算 MAC，把 `明文 || MAC || 填充` 一起加密。
//! 3. **MAC 覆盖记录头**：`HMAC(secret, seq || type || version || length || 明文)`，
//!    长度字段用的是**明文**长度、不含填充和 MAC 本身。
//!
//! 这些细节必须逐条照做，否则解密出来是垃圾。

use aes::{Aes128, Aes256};
use aes::cipher::BlockDecrypt;
use cbc::cipher::{BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::prf::KeyBlock;

type HmacSha1 = Hmac<Sha1>;

/// 支持的对称加密算法。真机实测服务端会选 **AES-256**（相机两个都提供）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkCipher {
    Aes128,
    Aes256,
}

impl BulkCipher {
    /// 从密钥长度推断算法（16 → AES-128，32 → AES-256）
    pub fn from_key_len(len: usize) -> Option<Self> {
        match len {
            16 => Some(Self::Aes128),
            32 => Some(Self::Aes256),
            _ => None,
        }
    }

    pub fn key_len(self) -> usize {
        match self {
            Self::Aes128 => 16,
            Self::Aes256 => 32,
        }
    }

    /// 在给定密钥/IV 下解密一个块（16 字节）
    fn decrypt_block(self, key: &[u8], block: &mut [u8; 16]) -> Result<(), RecordError> {
        match self {
            Self::Aes128 => {
                let c = <Aes128 as aes::cipher::KeyInit>::new_from_slice(key)
                    .map_err(|_| RecordError::BadKey)?;
                c.decrypt_block(aes::cipher::Block::<Aes128>::from_mut_slice(block));
                Ok(())
            }
            Self::Aes256 => {
                let c = <Aes256 as aes::cipher::KeyInit>::new_from_slice(key)
                    .map_err(|_| RecordError::BadKey)?;
                c.decrypt_block(aes::cipher::Block::<Aes256>::from_mut_slice(block));
                Ok(())
            }
        }
    }

    fn encrypt_buffer(self, key: &[u8], iv: &[u8], buf: &mut [u8]) -> Result<Vec<u8>, RecordError> {
        match self {
            Self::Aes128 => {
                let e = cbc::Encryptor::<Aes128>::new_from_slices(key, iv)
                    .map_err(|_| RecordError::BadKey)?;
                let n = buf.len();
                Ok(e.encrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(buf, n)
                    .map_err(|_| RecordError::BadPadding)?
                    .to_vec())
            }
            Self::Aes256 => {
                let e = cbc::Encryptor::<Aes256>::new_from_slices(key, iv)
                    .map_err(|_| RecordError::BadKey)?;
                let n = buf.len();
                Ok(e.encrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(buf, n)
                    .map_err(|_| RecordError::BadPadding)?
                    .to_vec())
            }
        }
    }
}

/// `TLS_RSA_WITH_AES_128_CBC_SHA` 的参数
pub const MAC_LEN: usize = 20;
pub const KEY_LEN: usize = 16;
pub const IV_LEN: usize = 16;

/// 一个方向的记录解密器 / 加密器
pub struct RecordCipher {
    mac_secret: Vec<u8>,
    key: Vec<u8>,
    cipher: BulkCipher,
    /// TLS 1.0 的隐式 IV：解密时是"上一条记录的最后一个密文块"
    iv: [u8; IV_LEN],
    /// 记录序号，从 0 开始，每条记录 +1（MAC 里要用）
    seq: u64,
}

impl RecordCipher {
    pub fn new(mac_secret: &[u8], key: &[u8], iv: &[u8]) -> Self {
        let cipher = BulkCipher::from_key_len(key.len()).expect("密钥长度应为 16 或 32");
        let mut iv_buf = [0u8; IV_LEN];
        iv_buf.copy_from_slice(&iv[..IV_LEN]);
        Self {
            mac_secret: mac_secret.to_vec(),
            key: key.to_vec(),
            cipher,
            iv: iv_buf,
            seq: 0,
        }
    }

    /// 从密钥块里取出"客户端发来的方向"（即我们解密时用的那一半）
    pub fn client_to_server(kb: &KeyBlock) -> Self {
        Self::new(&kb.client_mac, &kb.client_key, &kb.client_iv)
    }

    /// 从密钥块里取出"我们发出去的方向"
    pub fn server_to_client(kb: &KeyBlock) -> Self {
        Self::new(&kb.server_mac, &kb.server_key, &kb.server_iv)
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// 计算 TLS 1.0 的 MAC
    fn mac(&self, content_type: u8, version: u16, plaintext: &[u8]) -> Vec<u8> {
        let mut mac = <HmacSha1 as Mac>::new_from_slice(&self.mac_secret)
            .expect("HMAC 接受任意长度密钥");
        mac.update(&self.seq.to_be_bytes());
        mac.update(&[content_type]);
        mac.update(&version.to_be_bytes());
        mac.update(&(plaintext.len() as u16).to_be_bytes());
        mac.update(plaintext);
        mac.finalize().into_bytes().to_vec()
    }

    /// 解密一条记录并校验 MAC。返回明文。
    ///
    /// `content_type` / `version` 取自**记录头**，它们是 MAC 的一部分。
    pub fn decrypt(
        &mut self,
        content_type: u8,
        version: u16,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, RecordError> {
        if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(IV_LEN) {
            return Err(RecordError::BadLength(ciphertext.len()));
        }

        // TLS 1.0 的隐式 IV：本次用上一次的 IV；解密完把 IV 更新为本条密文的最后一个块
        let next_iv = ciphertext[ciphertext.len() - IV_LEN..].to_vec();
        let mut buf = ciphertext.to_vec();

        // 自己做 CBC 链接（而不是把整段交给库），这样"IV 是上一条的最后一个密文块"
        // 这件事在代码里看得见 —— 这是 TLS 1.0 最容易被忽略的一处。
        //
        // ⚠️ 用逐块解密接口（而不是 `cbc::Decryptor` 的整段接口），
        //    因为后者会自己做 CBC 链接，而我们要自己控制隐式 IV。
        {
            let mut prev: [u8; IV_LEN] = self.iv;
            // 长度已校验为 16 的整数倍，所以剩余部分必为空
            for chunk in buf.as_chunks_mut::<IV_LEN>().0 {
                let save: [u8; IV_LEN] = *chunk;
                let mut block: [u8; IV_LEN] = save;
                self.cipher.decrypt_block(&self.key, &mut block)?;
                for (i, b) in block.iter().enumerate() {
                    chunk[i] = *b ^ prev[i];
                }
                prev = save;
            }
        }
        self.iv.copy_from_slice(&next_iv);

        // 去填充。
        //
        // ⚠️⚠️ **TLS 1.0 的填充与 PKCS#7 差一字节，这里极易写错**！
        //
        // RFC 2246（TLS 1.0）§6.2.3.2 的结构是：
        //   struct { opaque content[..]; opaque MAC[..];
        //            uint8 padding[padding_length]; uint8 padding_length; }
        // 并且明确写着：*"This length specifies the length of the padding field
        // **exclusive of the padding_length field itself**."*
        //
        // 即：`padding_length` 的值是**填充字节数，不含它自己**。
        // 所以总共要剥掉的字节数 = `padding_length + 1`。
        //
        // 真机实测（ILCE-6300）解开后长这样：
        //   `14 00 00 0c` + 12 字节校验值 + 20 字节 MAC + 12 个 `0x0b`
        //   16(明文) + 20(MAC) + 11(填充) + 1(长度字节) = 48 ✅
        //
        // ⚠️ 这里容易按 PKCS#7 的习惯只剥 `padding_length` 个字节 —— 那就错了：
        // 那个长度字节会留在明文里，导致 MAC 永远校验不过。
        // 而且现象很迷惑：明文头部完全正确，只有最后多出一个字节。
        let pad_byte = *buf.last().ok_or(RecordError::BadPadding)? as usize;
        let total_pad = pad_byte + 1;
        if total_pad > buf.len() {
            return Err(RecordError::BadPadding);
        }
        if buf[buf.len() - total_pad..]
            .iter()
            .any(|&b| b as usize != pad_byte)
        {
            return Err(RecordError::BadPadding);
        }
        let body = &buf[..buf.len() - total_pad];

        if body.len() < MAC_LEN {
            return Err(RecordError::TooShort(body.len()));
        }
        let (plain, mac_recv) = body.split_at(body.len() - MAC_LEN);
        let mac_calc = self.mac(content_type, version, plain);
        // 常量时间比较，避免计时侧信道
        let diff = mac_calc
            .iter()
            .zip(mac_recv.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        if mac_calc.len() != mac_recv.len() || diff != 0 {
            return Err(RecordError::MacMismatch);
        }

        self.seq += 1;
        Ok(plain.to_vec())
    }

    /// 诊断用：直接算一次 MAC（不改变任何状态）
    pub fn debug_mac(&self, content_type: u8, version: u16, plaintext: &[u8], seq: u64) -> Vec<u8> {
        let mut mac = <HmacSha1 as Mac>::new_from_slice(&self.mac_secret)
            .expect("HMAC 接受任意长度密钥");
        mac.update(&seq.to_be_bytes());
        mac.update(&[content_type]);
        mac.update(&version.to_be_bytes());
        mac.update(&(plaintext.len() as u16).to_be_bytes());
        mac.update(plaintext);
        mac.finalize().into_bytes().to_vec()
    }

    /// 只解密、**不去填充、不校验 MAC**（诊断专用：把中间值原样交出来）
    ///
    /// ⚠️ 注意：这个方法**会**推进隐式 IV，所以不能拿同一个对象先试解再正式解。
    pub fn decrypt_raw(&self, ciphertext: &[u8], iv: &[u8]) -> Vec<u8> {
        if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(IV_LEN) {
            return Vec::new();
        }
        let mut buf = ciphertext.to_vec();
        let mut prev: [u8; IV_LEN] = iv.try_into().unwrap_or([0u8; IV_LEN]);
        for chunk in buf.as_chunks_mut::<IV_LEN>().0 {
            let save: [u8; IV_LEN] = *chunk;
            let mut block: [u8; IV_LEN] = save;
            if self.cipher.decrypt_block(&self.key, &mut block).is_err() {
                return Vec::new();
            }
            for (i, b) in block.iter().enumerate() {
                chunk[i] = *b ^ prev[i];
            }
            prev = save;
        }
        buf
    }

    /// 只解密、去填充，**不校验 MAC**（仅用于诊断：能区分"密钥错"还是"MAC 错"）
    pub fn decrypt_ignoring_mac(
        &mut self,
        _content_type: u8,
        _version: u16,
        ciphertext: &[u8],
    ) -> Vec<u8> {
        if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(IV_LEN) {
            return Vec::new();
        }
        let next_iv = ciphertext[ciphertext.len() - IV_LEN..].to_vec();
        let mut buf = ciphertext.to_vec();
        let mut prev: [u8; IV_LEN] = self.iv;
        for chunk in buf.as_chunks_mut::<IV_LEN>().0 {
            let save: [u8; IV_LEN] = *chunk;
            let mut block: [u8; IV_LEN] = save;
            if self.cipher.decrypt_block(&self.key, &mut block).is_err() {
                return Vec::new();
            }
            for (i, b) in block.iter().enumerate() {
                chunk[i] = *b ^ prev[i];
            }
            prev = save;
        }
        self.iv.copy_from_slice(&next_iv);
        let pad_byte = *buf.last().unwrap_or(&0) as usize;
        let total_pad = pad_byte + 1;
        if total_pad > buf.len() {
            return buf;
        }
        buf[..buf.len() - total_pad].to_vec()
    }

    /// 加密一条记录
    pub fn encrypt(&mut self, content_type: u8, version: u16, plaintext: &[u8]) -> Vec<u8> {
        let mac = self.mac(content_type, version, plaintext);
        let mut body = Vec::with_capacity(plaintext.len() + MAC_LEN + IV_LEN);
        body.extend_from_slice(plaintext);
        body.extend_from_slice(&mac);

        // ⚠️ TLS 1.0 的填充：要补满到 16 的整数倍，且**填充字节数 = 长度字段值 + 1**
        //    （长度字段不含它自己，见 RFC 2246 §6.2.3.2）。
        //    所以这里补 `total_pad` 个字节，每个字节的值是 `total_pad - 1`。
        //
        //    恰好整除时也要补一整个空块：此时 total_pad = 16，
        //    每个字节的值是 15，最后一个字节也是 15（表示"后面有 15 字节填充"）。
        let total_pad = IV_LEN - (body.len() % IV_LEN);
        let pad_value = (total_pad - 1) as u8;
        body.extend(std::iter::repeat_n(pad_value, total_pad));

        let mut buf = body.clone();
        let out = self
            .cipher
            .encrypt_buffer(&self.key, &self.iv, &mut buf)
            .expect("密钥与 IV 长度由构造时保证正确");

        self.iv.copy_from_slice(&out[out.len() - IV_LEN..]);
        self.seq += 1;
        out
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecordError {
    #[error("密文长度非法：{0} 字节（必须是 16 的整数倍）")]
    BadLength(usize),
    #[error("密钥或 IV 长度不对")]
    BadKey,
    #[error("填充非法")]
    BadPadding,
    #[error("解密后的内容太短：{0} 字节")]
    TooShort(usize),
    #[error("MAC 校验失败 —— 密钥推导或数据有问题")]
    MacMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher_pair() -> (RecordCipher, RecordCipher) {
        let mac = [0x11u8; 20];
        let key = [0x22u8; 16];
        let iv = [0x33u8; 16];
        (
            RecordCipher::new(&mac, &key, &iv),
            RecordCipher::new(&mac, &key, &iv),
        )
    }

    /// 自洽性：加密再解密能还原（包括连续多条记录，验证隐式 IV 链）
    #[test]
    fn encrypt_decrypt_roundtrip_chained() {
        let (mut enc, mut dec) = cipher_pair();
        let msgs: [&[u8]; 3] = [b"hello", b"second message", b"a third one, a bit longer"];
        for (i, m) in msgs.iter().enumerate() {
            let ct = enc.encrypt(22, 0x0301, m);
            assert!(ct.len().is_multiple_of(16), "密文必须是块整数倍");
            let pt = dec
                .decrypt(22, 0x0301, &ct)
                .unwrap_or_else(|e| panic!("第 {i} 条解密失败：{e}"));
            assert_eq!(&pt, m, "第 {i} 条内容应一致");
        }
        assert_eq!(enc.seq(), 3);
        assert_eq!(dec.seq(), 3);
    }

    /// 序号必须递增：MAC 覆盖序号，所以顺序错就会校验失败
    #[test]
    fn mac_covers_sequence_number() {
        let (mut enc, mut dec) = cipher_pair();
        let ct0 = enc.encrypt(22, 0x0301, b"one");
        let _ct1 = enc.encrypt(22, 0x0301, b"two");
        // 让解密方跳过第一条，直接解第二条 → 序号不匹配 → MAC 必须失败
        let err = dec.decrypt(22, 0x0301, &_ct1).unwrap_err();
        assert_eq!(err, RecordError::MacMismatch, "序号不匹配必须被 MAC 拦住");
        let _ = ct0;
    }

    /// 改一个字节必须被 MAC 拦住
    #[test]
    fn tampered_ciphertext_fails_mac() {
        let (mut enc, mut dec) = cipher_pair();
        let mut ct = enc.encrypt(22, 0x0301, b"secret");
        ct[0] ^= 0xff;
        assert!(dec.decrypt(22, 0x0301, &ct).is_err(), "被篡改的数据必须报错");
    }

    /// 类型/版本也是 MAC 的一部分
    #[test]
    fn mac_covers_content_type_and_version() {
        let (mut enc, mut dec) = cipher_pair();
        let ct = enc.encrypt(22, 0x0301, b"x");
        assert_eq!(
            dec.decrypt(20, 0x0301, &ct).unwrap_err(),
            RecordError::MacMismatch,
            "换内容类型必须失败"
        );
        let ct2 = enc.encrypt(22, 0x0301, b"x");
        assert_eq!(
            dec.decrypt(22, 0x0302, &ct2).unwrap_err(),
            RecordError::MacMismatch,
            "换版本必须失败"
        );
    }

    /// 长度非法的输入要报错而不是 panic
    #[test]
    fn rejects_bad_length() {
        let (_e, mut dec) = cipher_pair();
        assert_eq!(dec.decrypt(22, 0x0301, &[]).unwrap_err(), RecordError::BadLength(0));
        assert_eq!(
            dec.decrypt(22, 0x0301, &[0u8; 17]).unwrap_err(),
            RecordError::BadLength(17)
        );
    }
}
