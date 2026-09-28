//! 手写的最小 TLS 1.0 服务端。
//!
//! # 为什么手写
//!
//! 真机实测（见 [`crate::golden::client_hello`]）表明相机**只会**：
//! - **TLS 1.0**
//! - **静态 RSA 密钥交换**：`TLS_RSA_WITH_AES_128_CBC_SHA`(0x002f) 或 `..._256_CBC_SHA`(0x0035)
//! - 不发 SNI、不发 signature_algorithms
//!
//! 这让通用 TLS 库全部不适用：`rustls` 不支持 TLS 1.0 与静态 RSA；
//! `openssl` 能用但需要 Perl 编译（本机没有）。而我们需要的配置窄到只有一种，
//! 手写反而更简单、更可控，而且**零 C 依赖** —— 最终仍是单文件 exe。
//!
//! # 实现范围（按里程碑推进）
//!
//! - [x] M1：解析 ClientHello；产出 ServerHello + Certificate + ServerHelloDone
//! - [ ] M2：解析 ClientKeyExchange，用 RSA 私钥解出预主密钥
//! - [ ] M3：TLS 1.0 PRF（MD5/SHA1 各半）导出主密钥与密钥块
//! - [ ] M4：验证客户端的 ChangeCipherSpec + Finished
//! - [ ] M5：发送服务端 ChangeCipherSpec + Finished；加解密应用数据
//!
//! # 测试基准
//!
//! 全部用**真机录制的字节**做断言，不靠自造数据。

use anyhow::{bail, Context, Result};

use crate::certs;

/// 真机实测：相机只谈 TLS 1.0
pub const TLS_VERSION_1_0: u16 = 0x0301;

// 握手消息类型
pub const HS_CLIENT_HELLO: u8 = 1;
pub const HS_SERVER_HELLO: u8 = 2;
pub const HS_CERTIFICATE: u8 = 11;
pub const HS_SERVER_KEY_EXCHANGE: u8 = 12;
pub const HS_SERVER_HELLO_DONE: u8 = 14;
pub const HS_CLIENT_KEY_EXCHANGE: u8 = 16;
pub const HS_FINISHED: u8 = 20;

// 记录类型
pub const CT_CHANGE_CIPHER_SPEC: u8 = 20;
pub const CT_ALERT: u8 = 21;
pub const CT_HANDSHAKE: u8 = 22;
pub const CT_APPLICATION_DATA: u8 = 23;

/// 我们支持的密码套件。真机实测相机只会提供这两个，服务端必须从里面挑。
pub const CIPHER_AES128_SHA: u16 = 0x002f;
pub const CIPHER_AES256_SHA: u16 = 0x0035;

/// 真机实测：原实现的服务端**优先选了 AES-256**（相机两个都提供）。
/// 我们的顺序与它保持一致，这样重建出来的 ServerHello 能与录音逐字节相同。
pub const PREFERRED_CIPHERS: [u16; 2] = [CIPHER_AES256_SHA, CIPHER_AES128_SHA];

/// 安全重协商扩展（RFC 5746）。
/// 相机在 ClientHello 里发了 `0x00ff`（SCSV）标记，服务端必须回一个空的重协商信息，
/// 否则严格的客户端会拒绝握手。
pub const EXT_RENEGOTIATION_INFO: u16 = 0xff01;

/// 解析出来的 ClientHello
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub client_version: u16,
    pub random: [u8; 32],
    pub session_id: Vec<u8>,
    pub cipher_suites: Vec<u16>,
    pub compression: Vec<u8>,
    /// 扩展 (类型, 数据)
    pub extensions: Vec<(u16, Vec<u8>)>,
}

impl ClientHello {
    /// 从**握手消息**解析。`data` 应以 `01 00 xx xx`（ClientHello 头）开头。
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 4 {
            bail!("ClientHello 太短（{} 字节）", data.len());
        }
        if data[0] != HS_CLIENT_HELLO {
            bail!("不是 ClientHello（握手类型 0x{:02x}）", data[0]);
        }
        // 握手头是 4 字节：1 字节类型 + **3 字节长度**（高字节在前）
        // ⚠️ 曾经在这里写成 data[1..3]，漏掉了最高位那个字节，
        //    导致真机问候语（长度 0x000033，最高字节为 0）被判成"长度为 0"。
        let body_len = ((data[1] as usize) << 16) | ((data[2] as usize) << 8) | data[3] as usize;
        let body = data
            .get(4..4 + body_len)
            .with_context(|| format!("ClientHello 正文被截断（声明 {body_len} 字节）"))?;

        let mut p = 0usize;
        fn need(body: &[u8], p: usize, n: usize, what: &str) -> Result<()> {
            if body.len() < p + n {
                bail!("ClientHello 在「{what}」处被截断（需要 {n} 字节）");
            }
            Ok(())
        }

        need(body, p, 2, "版本")?;
        let client_version = u16::from_be_bytes([body[p], body[p + 1]]);
        p += 2;

        need(body, p, 32, "随机数")?;
        let mut random = [0u8; 32];
        random.copy_from_slice(&body[p..p + 32]);
        p += 32;

        need(body, p, 1, "session id 长度")?;
        let sid_len = body[p] as usize;
        p += 1;
        need(body, p, sid_len, "session id")?;
        let session_id = body[p..p + sid_len].to_vec();
        p += sid_len;

        need(body, p, 2, "套件列表长度")?;
        let cs_len = u16::from_be_bytes([body[p], body[p + 1]]) as usize;
        p += 2;
        need(body, p, cs_len, "套件列表")?;
        if !cs_len.is_multiple_of(2) {
            bail!("套件列表长度应为偶数，实际 {cs_len}");
        }
        let mut cipher_suites = Vec::with_capacity(cs_len / 2);
        for i in (0..cs_len).step_by(2) {
            cipher_suites.push(u16::from_be_bytes([body[p + i], body[p + i + 1]]));
        }
        p += cs_len;

        need(body, p, 1, "压缩方式长度")?;
        let comp_len = body[p] as usize;
        p += 1;
        need(body, p, comp_len, "压缩方式")?;
        let compression = body[p..p + comp_len].to_vec();
        p += comp_len;

        let mut extensions = Vec::new();
        if p + 2 <= body.len() {
            let ext_total = u16::from_be_bytes([body[p], body[p + 1]]) as usize;
            p += 2;
            let end = (p + ext_total).min(body.len());
            while p + 4 <= end {
                let et = u16::from_be_bytes([body[p], body[p + 1]]);
                let el = u16::from_be_bytes([body[p + 2], body[p + 3]]) as usize;
                p += 4;
                if p + el > end {
                    break;
                }
                extensions.push((et, body[p..p + el].to_vec()));
                p += el;
            }
        }

        Ok(Self {
            client_version,
            random,
            session_id,
            cipher_suites,
            compression,
            extensions,
        })
    }

    pub fn has_extension(&self, ext_type: u16) -> bool {
        self.extensions.iter().any(|(t, _)| *t == ext_type)
    }

    /// 相机是否声明了 SNI（真机实测：没有）
    pub fn has_sni(&self) -> bool {
        self.has_extension(0x0000)
    }

    /// 从客户端提供的套件里挑一个我们能做的。
    /// 真机只会给 `0x002f` / `0x0035`（外加防重协商标记 `0x00ff`）。
    pub fn choose_cipher(&self) -> Option<u16> {
        PREFERRED_CIPHERS
            .into_iter()
            .find(|c| self.cipher_suites.contains(c))
    }

    /// 相机是否用 `0x00ff`（SCSV）声明了"支持安全重协商"
    pub fn offers_secure_renegotiation(&self) -> bool {
        self.cipher_suites.contains(&0x00ff)
    }
}

/// 把若干握手消息打包成一条 TLS 记录
pub fn wrap_handshake_record(messages: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for m in messages {
        body.extend_from_slice(m);
    }
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(CT_HANDSHAKE);
    out.extend_from_slice(&TLS_VERSION_1_0.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// 构造一条握手消息（4 字节头 + 正文）
pub fn handshake_message(msg_type: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.push(msg_type);
    let len = body.len() as u32;
    out.extend_from_slice(&len.to_be_bytes()[1..]); // 3 字节长度
    out.extend_from_slice(body);
    out
}

/// 构造 ServerHello。
///
/// 真机实测的服务端回应结构（逐字节核对过）：
/// ```text
/// 02 00 00 2d            握手头：ServerHello / 体长 45
/// 03 01                  版本 TLS 1.0
/// <32 字节服务端随机数>
/// 00                     session id 长度 0
/// 00 35                  套件 TLS_RSA_WITH_AES_256_CBC_SHA
/// 00                     压缩方式：不压缩
/// 00 05                  扩展总长 5
/// ff 01 00 01 00         安全重协商扩展（空）
/// ```
pub fn build_server_hello(
    server_random: &[u8; 32],
    session_id: &[u8],
    cipher: u16,
    secure_renegotiation: bool,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&TLS_VERSION_1_0.to_be_bytes()); // 服务端也回 TLS 1.0
    body.extend_from_slice(server_random);
    body.push(session_id.len() as u8);
    body.extend_from_slice(session_id);
    body.extend_from_slice(&cipher.to_be_bytes());
    body.push(0); // 压缩方式：不压缩

    if secure_renegotiation {
        // ff 01 00 01 00：类型 ff01、长度 1、内容"重协商信息长度 = 0"
        body.extend_from_slice(&5u16.to_be_bytes()); // 扩展总长
        body.extend_from_slice(&EXT_RENEGOTIATION_INFO.to_be_bytes());
        body.extend_from_slice(&1u16.to_be_bytes()); // 该扩展的长度
        body.push(0); // 重协商信息长度 = 0
    } else {
        body.extend_from_slice(&0u16.to_be_bytes()); // 无扩展
    }
    handshake_message(HS_SERVER_HELLO, &body)
}

/// 构造 Certificate（整条证书链放在一条消息里，这是 TLS 1.0 的做法）
pub fn build_certificate() -> Result<Vec<u8>> {
    let chain = certs::certificate_chain()?;
    let mut items = Vec::new();
    for der in &chain {
        items.extend_from_slice(&(der.len() as u32).to_be_bytes()[1..]); // 3 字节长度
        items.extend_from_slice(der);
    }
    let mut body = Vec::with_capacity(3 + items.len());
    body.extend_from_slice(&(items.len() as u32).to_be_bytes()[1..]);
    body.extend_from_slice(&items);
    Ok(handshake_message(HS_CERTIFICATE, &body))
}

/// 构造 ServerHelloDone（空正文）
pub fn build_server_hello_done() -> Vec<u8> {
    handshake_message(HS_SERVER_HELLO_DONE, &[])
}

/// 把字节流切成 TLS 记录：(类型, 版本, 正文)
pub fn split_records(mut data: &[u8]) -> Vec<(u8, u16, Vec<u8>)> {
    let mut out = Vec::new();
    while data.len() >= 5 {
        let ctype = data[0];
        let ver = u16::from_be_bytes([data[1], data[2]]);
        let len = u16::from_be_bytes([data[3], data[4]]) as usize;
        if data.len() < 5 + len {
            break;
        }
        out.push((ctype, ver, data[5..5 + len].to_vec()));
        data = &data[5 + len..];
    }
    out
}

/// 从 ServerHello 正文里取出协商的套件
pub fn parse_server_hello_cipher(body: &[u8]) -> Option<u16> {
    if body.len() < 4 || body[0] != HS_SERVER_HELLO {
        return None;
    }
    let mut p = 4 + 2 + 32;
    if body.len() <= p {
        return None;
    }
    let sid_len = body[p] as usize;
    p += 1 + sid_len;
    if body.len() < p + 2 {
        return None;
    }
    Some(u16::from_be_bytes([body[p], body[p + 1]]))
}

/// 在握手消息序列里查找某个类型
pub fn contains_handshake_type(data: &[u8], want: u8) -> bool {
    let mut p = 0usize;
    while p + 4 <= data.len() {
        let t = data[p];
        let len = u32::from_be_bytes([0, data[p + 1], data[p + 2], data[p + 3]]) as usize;
        if t == want {
            return true;
        }
        p += 4 + len;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::client_hello;

    /// 里程碑 M1：必须能正确解析**真机**问候语
    #[test]
    fn parses_golden_client_hello() {
        let raw = client_hello();
        let ch = ClientHello::parse(&raw[5..]).expect("应能解析真机问候语");

        assert_eq!(ch.client_version, TLS_VERSION_1_0, "相机只谈 TLS 1.0");
        assert_eq!(ch.session_id.len(), 0, "真机没有 session id");
        assert_eq!(
            ch.cipher_suites,
            vec![CIPHER_AES128_SHA, CIPHER_AES256_SHA, 0x00ff],
            "真机提供的套件顺序"
        );
        assert_eq!(ch.compression, vec![0]);
        assert!(!ch.has_sni(), "真机不发 SNI —— 服务端不能要求 SNI");
        assert!(ch.has_extension(0x0023), "真机有 session_ticket");
    }

    /// 必须能从真机提供的套件里挑出一个我们支持的
    #[test]
    fn chooses_supported_cipher_for_golden_client() {
        let raw = client_hello();
        let ch = ClientHello::parse(&raw[5..]).unwrap();
        let chosen = ch.choose_cipher().expect("应能挑出套件");
        // 真机实测：原实现的服务端优先选 AES-256（0x0035）
        assert_eq!(chosen, CIPHER_AES256_SHA, "优先 AES-256（与真机一致）");
        assert!(ch.cipher_suites.contains(&chosen), "必须来自客户端列表");
    }

    /// M1 的产出：ServerHello + Certificate + ServerHelloDone 必须结构正确
    #[test]
    fn builds_valid_server_flight() {
        let raw = client_hello();
        let ch = ClientHello::parse(&raw[5..]).unwrap();
        let cipher = ch.choose_cipher().unwrap();

        let server_random = [0xAAu8; 32];
        let flight = vec![
            build_server_hello(
                &server_random,
                &ch.session_id,
                cipher,
                ch.offers_secure_renegotiation(),
            ),
            build_certificate().unwrap(),
            build_server_hello_done(),
        ];
        let record = wrap_handshake_record(&flight);

        let records = split_records(&record);
        assert_eq!(records.len(), 1, "三条握手消息可以放在一条记录里");
        let (ctype, ver, body) = &records[0];
        assert_eq!(*ctype, CT_HANDSHAKE);
        assert_eq!(*ver, TLS_VERSION_1_0);

        assert_eq!(body[0], HS_SERVER_HELLO);
        let sh_len = u32::from_be_bytes([0, body[1], body[2], body[3]]) as usize;
        assert_eq!(
            parse_server_hello_cipher(body),
            Some(cipher),
            "ServerHello 里必须是协商好的套件"
        );

        let mut p = 4 + sh_len;
        assert_eq!(body[p], HS_CERTIFICATE, "第二条应是 Certificate");
        let cert_len = u32::from_be_bytes([0, body[p + 1], body[p + 2], body[p + 3]]) as usize;
        p += 4 + cert_len;
        assert_eq!(body[p], HS_SERVER_HELLO_DONE, "第三条应是 ServerHelloDone");
        assert_eq!(body.len(), p + 4, "不应有多余字节");
    }

    /// 静态 RSA 绝不能发 ServerKeyExchange（发了就说明协商成了 DHE）
    #[test]
    fn no_server_key_exchange_for_static_rsa() {
        let raw = client_hello();
        let ch = ClientHello::parse(&raw[5..]).unwrap();
        let cipher = ch.choose_cipher().unwrap();
        let flight = vec![
            build_server_hello(&[1u8; 32], &[], cipher, false),
            build_certificate().unwrap(),
            build_server_hello_done(),
        ];
        let record = wrap_handshake_record(&flight);
        let (_, _, body) = &split_records(&record)[0];
        assert!(
            !contains_handshake_type(body, HS_SERVER_KEY_EXCHANGE),
            "静态 RSA 不应出现 ServerKeyExchange"
        );
    }

    /// 证书链必须完整（叶 + 中间），否则相机链不到受信根
    #[test]
    fn certificate_message_contains_full_chain() {
        let msg = build_certificate().unwrap();
        assert_eq!(msg[0], HS_CERTIFICATE);
        let total = u32::from_be_bytes([0, msg[1], msg[2], msg[3]]) as usize;
        assert_eq!(total + 4, msg.len(), "长度字段与正文一致");

        let list_len = u32::from_be_bytes([0, msg[4], msg[5], msg[6]]) as usize;
        assert_eq!(list_len + 3, total);
        let c1 = u32::from_be_bytes([0, msg[7], msg[8], msg[9]]) as usize;
        let p2 = 10 + c1;
        let c2 = u32::from_be_bytes([0, msg[p2], msg[p2 + 1], msg[p2 + 2]]) as usize;
        assert_eq!(10 + c1 + 3 + c2, msg.len(), "应恰好包含两张证书");
    }
}
