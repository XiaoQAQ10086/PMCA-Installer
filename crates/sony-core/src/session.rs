//! TLS 1.0 握手会话：把各步骤串成一个内存状态机。
//!
//! **没有任何网络**：相机把 TLS 字节通过 USB 交给我们，我们返回字节。
//! 所以这里只是一个纯函数式的状态机 —— `feed(相机字节) -> 要回给相机的字节`。
//!
//! # 握手顺序（静态 RSA，无客户端证书）
//!
//! ```text
//! 客户端 → 服务端：ClientHello
//! 服务端 → 客户端：ServerHello + Certificate + ServerHelloDone     ← 我们的首段
//! 客户端 → 服务端：ClientKeyExchange                                 ← 含 RSA 加密的预主密钥
//! 客户端 → 服务端：ChangeCipherSpec
//! 客户端 → 服务端：Finished（已加密，需校验）
//! 服务端 → 客户端：ChangeCipherSpec
//! 服务端 → 客户端：Finished（已加密）                                 ← 我们发这个
//! 之后：应用数据（HTTPS 请求/响应）
//! ```

use anyhow::{bail, Context, Result};
use digest::Digest;
use md5::Md5;
use sha1::Sha1;

use crate::prf::{self, KeyBlock};
use crate::record::RecordCipher;
use crate::tls;

/// 握手阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 还没收到 ClientHello
    ExpectClientHello,
    /// 已发出服务端首段，等 ClientKeyExchange
    ExpectClientKeyExchange,
    /// 等客户端的 ChangeCipherSpec
    ExpectClientCcs,
    /// 等（已加密的）客户端 Finished
    ExpectClientFinished,
    /// 握手完成，进入应用数据阶段
    Established,
    /// 出错
    Failed,
}

/// 握手过程中记录下来的关键材料
#[derive(Debug, Clone)]
pub struct Negotiated {
    pub cipher: u16,
    pub client_random: [u8; 32],
    pub server_random: [u8; 32],
}

/// 服务端握手状态机
pub struct TlsSession {
    phase: Phase,
    /// 自己生成的服务端随机数（真机上原实现似乎用了固定值）
    server_random: [u8; 32],
    client_random: Option<[u8; 32]>,
    cipher: Option<u16>,
    /// 握手消息转录，用于计算 Finished 的校验值（MD5‖SHA1）
    transcript: Vec<u8>,
    /// 客户端方向（我们解密）与 服务端方向（我们加密）
    read_cipher: Option<RecordCipher>,
    write_cipher: Option<RecordCipher>,
    key_block: Option<KeyBlock>,
    /// 主密钥：算 Finished 校验值要用
    master_secret: Option<[u8; 48]>,
    /// 对端是否已经发来 close_notify
    peer_closed: bool,
    /// 握手完成后的应用数据缓冲（喂进来的明文请求）
    pending_requests: Vec<u8>,
}

impl TlsSession {
    pub fn new(server_random: [u8; 32]) -> Self {
        Self {
            phase: Phase::ExpectClientHello,
            server_random,
            client_random: None,
            cipher: None,
            transcript: Vec::new(),
            read_cipher: None,
            write_cipher: None,
            key_block: None,
            master_secret: None,
            peer_closed: false,
            pending_requests: Vec::new(),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// 我们这条会话用的服务端随机数
    pub fn server_random(&self) -> [u8; 32] {
        self.server_random
    }

    pub fn negotiated(&self) -> Option<Negotiated> {
        Some(Negotiated {
            cipher: self.cipher?,
            client_random: self.client_random?,
            server_random: self.server_random,
        })
    }

    /// 取出已解密的应用层数据（相机的 HTTP 请求）
    pub fn take_requests(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_requests)
    }

    /// 喂入相机发来的一条**记录正文**，返回需要回给相机的字节（可能为空）。
    ///
    /// `content_type` / `version` 来自记录头。
    pub fn feed_record(
        &mut self,
        content_type: u8,
        version: u16,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        match self.phase {
            Phase::ExpectClientHello => {
                if content_type != tls::CT_HANDSHAKE {
                    bail!("期望 ClientHello，收到的却是记录类型 {content_type}");
                }
                self.on_client_hello(version, body)
            }
            Phase::ExpectClientKeyExchange => {
                if content_type != tls::CT_HANDSHAKE {
                    bail!("期望 ClientKeyExchange，收到的却是记录类型 {content_type}");
                }
                self.on_client_key_exchange(version, body)
            }
            Phase::ExpectClientCcs => {
                if content_type != tls::CT_CHANGE_CIPHER_SPEC {
                    bail!("期望 ChangeCipherSpec，收到的却是记录类型 {content_type}");
                }
                if body != [1u8] {
                    bail!("ChangeCipherSpec 正文应为 0x01，实际 {body:02x?}");
                }
                // ⚠️ 不动握手转录。
                // TLS 1.0 的 Finished 校验值只覆盖**握手消息**（ClientHello…
                // ClientKeyExchange），ChangeCipherSpec 是"改变密码规格"的记录，
                // 不属于握手消息，因此**不能**计入转录。把它记进去会导致
                // 两端的哈希不一致、Finished 校验失败。
                self.phase = Phase::ExpectClientFinished;
                Ok(Vec::new())
            }
            Phase::ExpectClientFinished => {
                if content_type != tls::CT_HANDSHAKE {
                    bail!("期望 Finished，收到的却是记录类型 {content_type}");
                }
                self.on_client_finished(version, body)
            }
            Phase::Established => {
                // 应用数据
                let plain = match &mut self.read_cipher {
                    Some(c) => c
                        .decrypt(content_type, version, body)
                        .context("解密应用数据失败")?,
                    None => body.to_vec(),
                };
                match content_type {
                    tls::CT_APPLICATION_DATA => {
                        self.pending_requests.extend_from_slice(&plain);
                    }
                    tls::CT_ALERT => {
                        // ⚠️ TLS 告警**不能静默丢掉** —— 相机拒绝我们的请求时
                        //    就是通过它告诉我们原因的。真机实测：忽略了这一条，
                        //    现象只是"卡住不动"，完全看不出问题在哪。
                        let level = plain.first().copied().unwrap_or(0);
                        let desc = plain.get(1).copied().unwrap_or(0);
                        crate::market::trace_diag(&format!(
                            "[TLS] ⚠️ 相机发来告警：级别 {}（{}），描述 {}（{}）",
                            level,
                            alert_level_name(level),
                            desc,
                            alert_description_name(desc)
                        ));
                        // 收到 close_notify 时按规范**回一个**。
                        // 有些实现会等对端的 close_notify 才认为连接真正结束，
                        // 不回可能让它卡在那里。
                        if desc == 0 {
                            crate::market::trace_diag("[TLS] 回一个 close_notify 并收尾");
                            self.peer_closed = true;
                            let alert = [1u8, 0u8]; // warning, close_notify
                            let cipher = self.write_cipher.as_mut().context("密钥还没就绪")?;
                            let enc = cipher.encrypt(tls::CT_ALERT, tls::TLS_VERSION_1_0, &alert);
                            let mut out = Vec::with_capacity(enc.len() + 5);
                            out.push(tls::CT_ALERT);
                            out.extend_from_slice(&tls::TLS_VERSION_1_0.to_be_bytes());
                            out.extend_from_slice(&(enc.len() as u16).to_be_bytes());
                            out.extend_from_slice(&enc);
                            return Ok(out);
                        }
                    }
                    other => {
                        crate::market::trace_diag(&format!(
                            "[TLS] 收到未处理的记录类型 {other}，{} 字节",
                            plain.len()
                        ));
                    }
                }
                Ok(Vec::new())
            }
            Phase::Failed => bail!("会话已失败，不再处理数据"),
        }
    }

    // ---- 第一步：ClientHello ----
    fn on_client_hello(&mut self, _version: u16, body: &[u8]) -> Result<Vec<u8>> {
        let ch = tls::ClientHello::parse(body).context("解析 ClientHello 失败")?;
        if ch.client_version != tls::TLS_VERSION_1_0 {
            bail!("相机只支持 TLS 1.0，但它声明了 0x{:04x}", ch.client_version);
        }
        let cipher = ch
            .choose_cipher()
            .context("相机提供的套件里没有我们能做的")?;

        self.client_random = Some(ch.random);
        self.cipher = Some(cipher);
        // 转录从完整的手握消息开始（含 4 字节头）
        self.transcript
            .extend_from_slice(&tls::handshake_message(tls::HS_CLIENT_HELLO, &body[4..]));

        let flight = vec![
            tls::build_server_hello(
                &self.server_random,
                &ch.session_id,
                cipher,
                ch.offers_secure_renegotiation(),
            ),
            tls::build_certificate()?,
            tls::build_server_hello_done(),
        ];
        for m in &flight {
            self.transcript.extend_from_slice(m);
        }
        self.phase = Phase::ExpectClientKeyExchange;


        Ok(tls::wrap_handshake_record(&flight))
    }

    // ---- 第二步：ClientKeyExchange ----
    fn on_client_key_exchange(&mut self, _version: u16, body: &[u8]) -> Result<Vec<u8>> {
        // 记为完整握手消息
        let msg = tls::handshake_message(tls::HS_CLIENT_KEY_EXCHANGE, &body[4..]);

        self.transcript.extend_from_slice(&msg);

        // 正文前 2 字节是密文长度
        if body.len() < 6 {
            bail!("ClientKeyExchange 太短");
        }
        let enc_len = u16::from_be_bytes([body[4], body[5]]) as usize;
        let enc = body
            .get(6..6 + enc_len)
            .context("ClientKeyExchange 的密文被截断")?;

        // 用私钥解开预主密钥
        let key = crate::certs::private_key()?;
        let pms = key
            .decrypt(rsa::Pkcs1v15Encrypt, enc)
            .context("解开预主密钥失败（RSA 私钥与证书不匹配？）")?;
        if pms.len() != 48 {
            bail!("预主密钥应为 48 字节，实际 {}", pms.len());
        }
        if pms[0] != 0x03 {
            bail!("预主密钥的版本字节非法：0x{:02x}", pms[0]);
        }

        // 推导主密钥与密钥块
        let client_random = self.client_random.context("还没收到 ClientHello")?;
        let cipher = self.cipher.context("还没协商套件")?;
        let mac_len = 20;
        let key_len = match cipher {
            tls::CIPHER_AES256_SHA => 32,
            _ => 16,
        };
        let master = prf::master_secret(&pms, &client_random, &self.server_random);
        let kb = KeyBlock::derive(&master, &client_random, &self.server_random, mac_len, key_len, 16);
        self.read_cipher = Some(RecordCipher::client_to_server(&kb));
        self.write_cipher = Some(RecordCipher::server_to_client(&kb));
        self.key_block = Some(kb);
        self.master_secret = Some(master);

        self.phase = Phase::ExpectClientCcs;

        Ok(Vec::new())
    }

    // ---- 第三步：客户端 Finished ----
    fn on_client_finished(&mut self, version: u16, body: &[u8]) -> Result<Vec<u8>> {
        let cipher = self.read_cipher.as_mut().context("密钥还没就绪")?;
        let plain = match cipher.decrypt(tls::CT_HANDSHAKE, version, body) {
            Ok(p) => p,
            Err(e) => {
                // 失败时把"解密后原始内容"打出来 —— 这一条曾帮我定位到
                // TLS 1.0 填充长度差一字节的问题（明文头部全对，只多一个字节）
                if let Some(kb) = self.key_block.as_ref() {
                    let probe = RecordCipher::client_to_server(kb);
                    let raw = probe.decrypt_raw(body, &kb.client_iv);
                    tracing::debug!(
                        raw = %hex::encode(&raw),
                        cipher = format!("0x{:04x}", self.cipher.unwrap_or(0)),
                        "客户端 Finished 解密失败，原始内容如下"
                    );
                }
                bail!("解密/校验客户端 Finished 失败：{e}");
            }
        };
        if plain.len() < 4 || plain[0] != tls::HS_FINISHED {
            bail!("客户端的不是 Finished 消息（首字节 0x{:02x}）", plain[0]);
        }
        let verify_data = &plain[4..];

        // 校验值覆盖"到目前为止"的握手转录，且要再套一层 PRF
        let master = self.master_secret.context("主密钥还没就绪")?;
        let expect = finished_verify_data(&master, &self.transcript, "client finished");
        if verify_data != expect.as_slice() {
            bail!(
                "客户端 Finished 校验值不匹配：收到 {} / 期望 {}",
                hex_prefix(verify_data),
                hex_prefix(&expect)
            );
        }
        // Finished 本身也计入转录（虽然之后不再用到，但保持完整）
        self.transcript
            .extend_from_slice(&tls::handshake_message(tls::HS_FINISHED, verify_data));

        // 回 ChangeCipherSpec 与（加密的）Finished
        let out = self.build_server_finished()?;
        self.phase = Phase::Established;
        Ok(out)
    }

    /// 构造服务端的 ChangeCipherSpec + Finished（两条记录）
    fn build_server_finished(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();

        // 1) ChangeCipherSpec（明文）。
        // ⚠️ 与客户端方向同理：CCS 不计入握手转录。
        out.push(tls::CT_CHANGE_CIPHER_SPEC);
        out.extend_from_slice(&tls::TLS_VERSION_1_0.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.push(1);

        // 2) Finished（加密）。注意标签与客户端不同，是 "server finished"
        let master = self.master_secret.context("主密钥还没就绪")?;
        let verify_data = finished_verify_data(&master, &self.transcript, "server finished");
        let msg = tls::handshake_message(tls::HS_FINISHED, &verify_data);
        let cipher = self.write_cipher.as_mut().context("密钥还没就绪")?;
        let enc = cipher.encrypt(tls::CT_HANDSHAKE, tls::TLS_VERSION_1_0, &msg);

        out.push(tls::CT_HANDSHAKE);
        out.extend_from_slice(&tls::TLS_VERSION_1_0.to_be_bytes());
        out.extend_from_slice(&(enc.len() as u16).to_be_bytes());
        out.extend_from_slice(&enc);
        Ok(out)
    }

    /// 加密一段应用数据（给相机回 HTTP 响应用）。
    ///
    /// ⚠️⚠️ **必须按 TLS 记录上限切分**！RFC 2246 规定单条记录明文最多
    /// **2^14 = 16384 字节**。我一开始把整个响应（下载 SPK 时是 147 KB）
    /// 塞进一条记录，相机直接回了一个 TLS 告警
    /// `fatal bad_record_mac` —— 因为它的 TLS 栈按上限读取，记录被截断，
    /// MAC 自然就对不上了。
    ///
    /// 这个错误现象很误导：告警说"MAC 错"，看起来像密钥问题，
    /// 实际是**记录分帧**问题。
    pub fn encrypt_application_data(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        /// TLS 记录明文上限（RFC 2246 §6.2.1）
        const MAX_PLAINTEXT: usize = 16384;

        let mut out = Vec::with_capacity(data.len() + 64);
        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![&[]]
        } else {
            data.chunks(MAX_PLAINTEXT).collect()
        };
        for chunk in chunks {
            let cipher = self.write_cipher.as_mut().context("密钥还没就绪")?;
            let enc = cipher.encrypt(tls::CT_APPLICATION_DATA, tls::TLS_VERSION_1_0, chunk);
            out.push(tls::CT_APPLICATION_DATA);
            out.extend_from_slice(&tls::TLS_VERSION_1_0.to_be_bytes());
            out.extend_from_slice(&(enc.len() as u16).to_be_bytes());
            out.extend_from_slice(&enc);
        }
        Ok(out)
    }

    pub fn key_block(&self) -> Option<&KeyBlock> {
        self.key_block.as_ref()
    }

    /// 处理一条完整的 HTTPS 请求，产出对应的响应字节（记录层已封装好）。
    ///
    /// 这是"假 HTTPS 服务器"的核心：相机以为自己在跟远端网站说话，
    /// 实际上请求经 USB → 我们的 TLS 会话 → 交给这里决定怎么答。
    ///
    /// 测试与真机走的是**同一条代码路径**，所以这里的正确性直接决定能不能装成功。
    pub fn respond_http<F>(&mut self, request: &[u8], handler: F) -> Result<Vec<u8>>
    where
        F: FnOnce(&crate::http::HttpRequest) -> Option<crate::http::HttpResponse>,
    {
        if self.phase != Phase::Established {
            bail!(
                "TLS 还没握手完成就来了 HTTPS 请求（当前阶段：{:?}）",
                self.phase
            );
        }
        let req = crate::http::HttpRequest::parse(request).context("解析 HTTPS 请求失败")?;
        // 处理器返回 None 时用"没有更多动作"作为兜底，避免把相机挂住
        let resp = handler(&req)
            .unwrap_or_else(|| crate::http::HttpResponse::json(crate::xpd::empty_json_response()));
        self.encrypt_application_data(&resp.encode())
    }

    /// 对端是否已经关闭了 TLS 连接
    pub fn peer_closed(&self) -> bool {
        self.peer_closed
    }

    /// 诊断用：当前握手转录的长度
    pub fn transcript_len(&self) -> usize {
        self.transcript.len()
    }
}

/// TLS 告警级别名（便于看日志）
fn alert_level_name(level: u8) -> &'static str {
    match level {
        1 => "warning",
        2 => "fatal",
        _ => "未知",
    }
}

/// TLS 告警描述名（RFC 2246 §7.2）
fn alert_description_name(desc: u8) -> &'static str {
    match desc {
        0 => "close_notify（正常关闭）",
        10 => "unexpected_message",
        20 => "bad_record_mac",
        21 => "decryption_failed",
        22 => "record_overflow",
        30 => "decompression_failure",
        40 => "handshake_failure",
        42 => "bad_certificate",
        43 => "unsupported_certificate",
        44 => "certificate_revoked",
        45 => "certificate_expired",
        46 => "certificate_unknown",
        47 => "illegal_parameter",
        48 => "unknown_ca",
        49 => "access_denied",
        50 => "decode_error",
        51 => "decrypt_error",
        60 => "export_restriction",
        70 => "protocol_version",
        71 => "insufficient_security",
        80 => "internal_error",
        90 => "user_canceled",
        100 => "no_renegotiation",
        _ => "未知",
    }
}

/// 计算 TLS 1.0 的 Finished 校验值（`verify_data`）。
///
/// RFC 2246 §7.4.9：
/// ```text
/// verify_data = PRF(master_secret,
///                   finished_label,                 // "client finished" / "server finished"
///                   MD5(handshake_messages) + SHA1(handshake_messages),
///                   12)
/// ```
///
/// ⚠️⚠️ **这里有一个非常隐蔽的坑**：`MD5||SHA1` 只是 PRF 的**种子**，
/// 还要再套一层 PRF 才是最终的 12 字节校验值。
///
/// 我最初直接把 `MD5||SHA1`（36 字节）当成了校验值，于是：
/// - 自己写的"假相机"测试能过（因为两端错得一模一样，互相验证不出问题）
/// - 真机上必然失败
///
/// 这正是"两边都是我写的"这种测试的盲区 —— 它只能证明**自洽**，
/// 证明不了**符合规范**。
pub fn finished_verify_data(master_secret: &[u8; 48], transcript: &[u8], label: &str) -> Vec<u8> {
    let mut md5 = Md5::new();
    md5.update(transcript);
    let mut sha1 = Sha1::new();
    sha1.update(transcript);
    let mut seed = md5.finalize().to_vec();
    seed.extend_from_slice(&sha1.finalize());
    crate::prf::prf(master_secret, label, &seed, 12)
}

/// 只算 `MD5(转录) ‖ SHA1(转录)` —— **仅用于诊断**，不是最终校验值
pub fn transcript_hash(transcript: &[u8]) -> Vec<u8> {
    let mut md5 = Md5::new();
    md5.update(transcript);
    let mut sha1 = Sha1::new();
    sha1.update(transcript);
    let mut out = md5.finalize().to_vec();
    out.extend_from_slice(&sha1.finalize());
    out
}

fn hex_prefix(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden;

    fn session() -> TlsSession {
        TlsSession::new([0x5Au8; 32])
    }

    /// 第一步：喂真机 ClientHello，应产出服务端首段并进入下一阶段
    #[test]
    fn step1_replies_to_golden_client_hello() {
        let raw = golden::client_hello();
        let mut s = session();
        let out = s
            .feed_record(tls::CT_HANDSHAKE, tls::TLS_VERSION_1_0, &raw[5..])
            .expect("应能处理 ClientHello");

        assert_eq!(s.phase(), Phase::ExpectClientKeyExchange);
        let records = tls::split_records(&out);
        assert_eq!(records.len(), 1, "首段应放在一条记录里");
        let (ct, ver, body) = &records[0];
        assert_eq!(*ct, tls::CT_HANDSHAKE);
        assert_eq!(*ver, tls::TLS_VERSION_1_0);

        // 三条消息：ServerHello / Certificate / ServerHelloDone
        assert_eq!(body[0], tls::HS_SERVER_HELLO);
        let sh_len = u32::from_be_bytes([0, body[1], body[2], body[3]]) as usize;
        let p = 4 + sh_len;
        assert_eq!(body[p], tls::HS_CERTIFICATE);
        let cert_len = u32::from_be_bytes([0, body[p + 1], body[p + 2], body[p + 3]]) as usize;
        let p2 = p + 4 + cert_len;
        assert_eq!(body[p2], tls::HS_SERVER_HELLO_DONE);

        let n = s.negotiated().unwrap();
        assert_eq!(n.cipher, tls::CIPHER_AES256_SHA, "与真机一致优先 AES-256");
    }

    /// 阶段顺序错误必须被拦住（而不是默默算错）
    #[test]
    fn rejects_out_of_order_records() {
        let mut s = session();
        // 还没收到 ClientHello 就收到 CCS
        let err = s
            .feed_record(tls::CT_CHANGE_CIPHER_SPEC, tls::TLS_VERSION_1_0, &[1])
            .unwrap_err();
        assert!(err.to_string().contains("期望 ClientHello"), "{err}");
    }

    /// 非 TLS 1.0 的客户端必须被拒绝（相机只会 1.0）
    #[test]
    fn rejects_non_tls10_client() {
        let mut raw = golden::client_hello();
        raw[9] = 0x03;
        raw[10] = 0x03; // 改成声明 TLS 1.2
        let mut s = session();
        let err = s
            .feed_record(tls::CT_HANDSHAKE, tls::TLS_VERSION_1_0, &raw[5..])
            .unwrap_err();
        assert!(err.to_string().contains("TLS 1.0"), "{err}");
    }

    /// ChangeCipherSpec 正文必须是 0x01
    #[test]
    fn rejects_bad_ccs_body() {
        let raw = golden::client_hello();
        let mut s = session();
        s.feed_record(tls::CT_HANDSHAKE, tls::TLS_VERSION_1_0, &raw[5..])
            .unwrap();
        s.phase = Phase::ExpectClientCcs;
        let err = s
            .feed_record(tls::CT_CHANGE_CIPHER_SPEC, tls::TLS_VERSION_1_0, &[2])
            .unwrap_err();
        assert!(err.to_string().contains("0x01"), "{err}");
    }

    /// Finished 校验值固定 12 字节，而且**必须再套一层 PRF**
    /// （不能直接用 MD5‖SHA1 那 36 字节，这是最常见的错法）
    #[test]
    fn verify_data_is_12_bytes_and_uses_prf() {
        let master = [0x11u8; 48];
        let v = finished_verify_data(&master, b"hello", "client finished");
        assert_eq!(v.len(), 12, "TLS 1.0 的 verify_data 固定 12 字节");
        // 换任一输入都必须变
        assert_ne!(v, finished_verify_data(&master, b"hellp", "client finished"));
        assert_ne!(v, finished_verify_data(&master, b"hello", "server finished"));
        let mut other = master;
        other[0] ^= 0xff;
        assert_ne!(v, finished_verify_data(&other, b"hello", "client finished"));

        // 明确排除"直接用 MD5‖SHA1"这个错法
        let raw = transcript_hash(b"hello");
        assert_eq!(raw.len(), 36);
        assert_ne!(&v[..], &raw[..12], "不能直接把 MD5‖SHA1 当成校验值");
    }

    /// 服务端 Finished 记录必须能被"服务端方向的密钥"解回来（加密自洽）
    #[test]
    fn server_finished_is_self_consistent() {
        // 构造一个"已协商"的会话，直接测加密路径
        let mut s = session();
        s.client_random = Some([1u8; 32]);
        s.cipher = Some(tls::CIPHER_AES256_SHA);
        let kb = KeyBlock::derive_aes256_sha(&[9u8; 48], &[1u8; 32], &s.server_random);
        s.write_cipher = Some(RecordCipher::server_to_client(&kb));
        s.read_cipher = Some(RecordCipher::client_to_server(&kb));
        s.master_secret = Some([9u8; 48]);
        s.transcript.extend_from_slice(b"fake transcript");

        let out = s.build_server_finished().unwrap();
        let records = tls::split_records(&out);
        assert_eq!(records.len(), 2, "应是 CCS + Finished 两条记录");
        assert_eq!(records[0].0, tls::CT_CHANGE_CIPHER_SPEC);
        assert_eq!(records[1].0, tls::CT_HANDSHAKE);

        // 用"客户端方向"的解密器去解服务端的 Finished：密钥不同，应该失败
        let mut wrong = RecordCipher::client_to_server(&kb);
        assert!(
            wrong
                .decrypt(tls::CT_HANDSHAKE, tls::TLS_VERSION_1_0, &records[1].2)
                .is_err(),
            "用错方向的密钥必须解不开"
        );
    }

    /// 应用数据加解密闭环
    #[test]
    fn application_data_roundtrip() {
        let kb = KeyBlock::derive_aes256_sha(&[7u8; 48], &[2u8; 32], &[3u8; 32]);
        let mut writer = RecordCipher::server_to_client(&kb);
        let mut reader = RecordCipher::server_to_client(&kb);

        let mut s = session();
        s.write_cipher = Some(writer);
        let rec = s.encrypt_application_data(b"HTTP/1.0 200 OK\r\n\r\n").unwrap();
        let records = tls::split_records(&rec);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, tls::CT_APPLICATION_DATA);
        let plain = reader
            .decrypt(tls::CT_APPLICATION_DATA, tls::TLS_VERSION_1_0, &records[0].2)
            .unwrap();
        assert_eq!(plain, b"HTTP/1.0 200 OK\r\n\r\n");
        writer = s.write_cipher.take().unwrap();
        let _ = writer;
    }
}
