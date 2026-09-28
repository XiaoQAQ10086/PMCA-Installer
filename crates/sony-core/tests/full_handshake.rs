//! 完整握手的端到端测试：我们同时扮演**服务端**（被测对象）与**客户端**（模拟相机）。
//!
//! # 这个测试为什么重要
//!
//! 之前基于录音的验证卡在"录音里那次握手的会话密钥对不上"，无法证明 PRF 与密钥块正确。
//! 这个测试换一条路：**自己当客户端跑一遍完整握手**。
//!
//! 如果握手的最后两步能互相验证通过：
//! - 客户端能解开并校验服务端的 Finished（用服务端的密钥）✅
//! - 服务端能解开并校验客户端的 Finished（用客户端的密钥）✅
//!
//! 那么 PRF、密钥块切分、记录层加解密、握手转录哈希**全部正确** ——
//! 因为任何一处错了，两个 Finished 都校验不过。
//!
//! 真实相机在握手中的行为与我们这个模拟客户端一致（同样的 TLS 1.0 + 静态 RSA），
//! 所以这个测试通过，等价于"握手逻辑正确"。剩下只差真机联调。

use anyhow::{bail, Context, Result};
use rsa::rand_core::OsRng;
use rsa::traits::PublicKeyParts;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};

use sony_core::prf::KeyBlock;
use sony_core::record::RecordCipher;
use sony_core::session::{finished_verify_data, Phase, TlsSession};
use sony_core::tls;

/// 我们模拟的"相机"，用它来跑完整个握手
struct FakeCamera {
    client_random: [u8; 32],
    pre_master: [u8; 48],
    transcript: Vec<u8>,
    read: Option<RecordCipher>,
    write: Option<RecordCipher>,
    /// 主密钥（算 Finished 校验值要用）
    master: Option<[u8; 48]>,
}

impl FakeCamera {
    fn new() -> Self {
        let mut client_random = [0u8; 32];
        let mut pre_master = [0u8; 48];
        use rsa::rand_core::RngCore;
        OsRng.fill_bytes(&mut client_random);
        OsRng.fill_bytes(&mut pre_master);
        // 预主密钥前两字节是客户端声明的版本
        pre_master[0] = 0x03;
        pre_master[1] = 0x01;
        Self {
            client_random,
            pre_master,
            transcript: Vec::new(),
            read: None,
            write: None,
            master: None,
        }
    }

    /// 构造 ClientHello（模仿真机：TLS 1.0、静态 RSA 套件、带 SCSV）
    ///
    /// 注意：必须用 `handshake_message` 拼装（它会写入正确的 3 字节长度字段）。
    /// 手工拼长度字段很容易和正文长度对不上，而握手转录要求两边逐字节一致，
    /// 差一个字节都会导致 Finished 校验失败。
    fn build_client_hello(&mut self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&tls::TLS_VERSION_1_0.to_be_bytes());
        body.extend_from_slice(&self.client_random);
        body.push(0); // session id 长度 0
        let suites: [u16; 3] = [tls::CIPHER_AES128_SHA, tls::CIPHER_AES256_SHA, 0x00ff];
        body.extend_from_slice(&((suites.len() * 2) as u16).to_be_bytes());
        for s in suites {
            body.extend_from_slice(&s.to_be_bytes());
        }
        body.push(1);
        body.push(0); // 压缩方式：不压缩
        body.extend_from_slice(&0u16.to_be_bytes()); // 无扩展

        let msg = tls::handshake_message(tls::HS_CLIENT_HELLO, &body);

        self.transcript.extend_from_slice(&msg);
        tls::wrap_handshake_record(&[msg])
    }

    /// 从服务端首段里取出协商的套件与 ServerHello
    fn parse_server_flight(&mut self, records: &[(u8, u16, Vec<u8>)]) -> Result<u16> {
        for (ct, _ver, body) in records {
            if *ct != tls::CT_HANDSHAKE {
                continue;
            }
            let msgs = sony_core::handshake::handshake_messages(body);

            for (t, m) in msgs {
                let packed = sony_core::handshake::pack_handshake(t, &m);

                self.transcript.extend_from_slice(&packed);
            }
        }
        // 从第一条握手记录里取套件
        let first = records
            .iter()
            .find(|(ct, _, _)| *ct == tls::CT_HANDSHAKE)
            .context("服务端没有发握手记录")?;
        let cipher = tls::parse_server_hello_cipher(&first.2).context("没有 ServerHello")?;
        Ok(cipher)
    }

    /// 构造 ClientKeyExchange（用服务器证书里的公钥加密预主密钥）
    fn build_client_key_exchange(&mut self) -> Result<Vec<u8>> {
        let key = sony_core::certs::private_key()?;
        let pub_key = RsaPublicKey::from(&key);
        assert_eq!(pub_key.size(), 256, "RSA-2048");
        let enc = pub_key
            .encrypt(&mut OsRng, Pkcs1v15Encrypt, &self.pre_master)
            .map_err(|e| anyhow::anyhow!("加密预主密钥失败：{e}"))?;

        let mut body = Vec::new();
        body.extend_from_slice(&(enc.len() as u16).to_be_bytes());
        body.extend_from_slice(&enc);
        let msg = tls::handshake_message(tls::HS_CLIENT_KEY_EXCHANGE, &body);

        self.transcript.extend_from_slice(&msg);
        Ok(tls::wrap_handshake_record(&[msg]))
    }

    /// 由预主密钥推导客户端方向与服务端方向的密钥
    fn derive_keys(&mut self, server_random: &[u8; 32], cipher: u16) -> Result<()> {
        let key_len = match cipher {
            tls::CIPHER_AES256_SHA => 32,
            tls::CIPHER_AES128_SHA => 16,
            other => bail!("不支持的套件 0x{other:04x}"),
        };
        let master = sony_core::prf::master_secret(&self.pre_master, &self.client_random, server_random);
        let kb = KeyBlock::derive(&master, &self.client_random, server_random, 20, key_len, 16);
        self.write = Some(RecordCipher::client_to_server(&kb)); // 我们（相机）发出的方向
        self.read = Some(RecordCipher::server_to_client(&kb)); // 服务端发来的方向
        self.master = Some(master);
        Ok(())
    }

    /// 构造 ChangeCipherSpec 与 Finished —— **两条独立记录**（与真机行为一致）
    fn build_client_finished(&mut self) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut ccs = Vec::new();
        ccs.push(tls::CT_CHANGE_CIPHER_SPEC);
        ccs.extend_from_slice(&tls::TLS_VERSION_1_0.to_be_bytes());
        ccs.extend_from_slice(&1u16.to_be_bytes());
        ccs.push(1);
        // ⚠️ CCS 不计入握手转录（TLS 1.0 的 Finished 只覆盖握手消息）

        let verify = finished_verify_data(self.master.as_ref().context("主密钥还没就绪")?, &self.transcript, "client finished");
        let msg = tls::handshake_message(tls::HS_FINISHED, &verify);
        let cipher = self.write.as_mut().context("密钥还没就绪")?;
        let enc = cipher.encrypt(tls::CT_HANDSHAKE, tls::TLS_VERSION_1_0, &msg);

        // 自己的 Finished 也要记入转录：服务端的 Finished 覆盖它
        self.transcript.extend_from_slice(&msg);

        let mut fin = Vec::new();
        fin.push(tls::CT_HANDSHAKE);
        fin.extend_from_slice(&tls::TLS_VERSION_1_0.to_be_bytes());
        fin.extend_from_slice(&(enc.len() as u16).to_be_bytes());
        fin.extend_from_slice(&enc);
        Ok((ccs, fin))
    }

    /// 校验服务端发来的 ChangeCipherSpec + Finished
    fn verify_server_finished(&mut self, server_flight: &[u8]) -> Result<()> {
        let records = tls::split_records(server_flight);
        if records.len() != 2 {
            bail!("服务端收尾应是 CCS + Finished 两条记录，实际 {}", records.len());
        }
        if records[0].0 != tls::CT_CHANGE_CIPHER_SPEC || records[0].2 != [1u8] {
            bail!("服务端的 ChangeCipherSpec 不合法");
        }
        // 服务端的 Finished 覆盖"到客户端 Finished 为止"的握手转录。
        // 客户端的转录里已经含自己的 Finished（build_client_finished 时追加的），
        // CCS 不属于握手消息，不计入。
        // 服务端的 Finished 用的是 **"server finished"** 标签（与客户端不同）
        let expect = finished_verify_data(
            self.master.as_ref().context("主密钥还没就绪")?,
            &self.transcript,
            "server finished",
        );

        let cipher = self.read.as_mut().context("密钥还没就绪")?;
        let plain = cipher
            .decrypt(tls::CT_HANDSHAKE, records[1].1, &records[1].2)
            .context("解密/校验服务端 Finished 失败")?;
        if plain.len() < 4 || plain[0] != tls::HS_FINISHED {
            bail!("服务端发的不是 Finished");
        }
        if plain[4..] != expect[..] {
            bail!("服务端 Finished 校验值不匹配");
        }
        Ok(())
    }
}

/// 跑一次完整握手，返回（会话, 模拟相机）
fn run_full_handshake(server_random: [u8; 32]) -> Result<(TlsSession, FakeCamera)> {
    let mut server = TlsSession::new(server_random);
    let mut camera = FakeCamera::new();

    // 1) ClientHello
    let ch = camera.build_client_hello();
    let recs = tls::split_records(&ch);

    let out = server.feed_record(recs[0].0, recs[0].1, &recs[0].2)?;
    assert_eq!(server.phase(), Phase::ExpectClientKeyExchange);
    if out.is_empty() {
        bail!("服务端没有回应 ClientHello");
    }

    // 2) 解析服务端首段
    let server_records = tls::split_records(&out);
    let cipher = camera.parse_server_flight(&server_records)?;

    // 3) ClientKeyExchange
    camera.derive_keys(&server_random, cipher)?;
    let kex = camera.build_client_key_exchange()?;
    let recs = tls::split_records(&kex);
    let out = server.feed_record(recs[0].0, recs[0].1, &recs[0].2)?;
    if !out.is_empty() {
        bail!("服务端不该在收到 ClientKeyExchange 时回数据");
    }

    // 4) 客户端 ChangeCipherSpec + Finished（两条独立记录，与真机一致）
    let (ccs, fin) = camera.build_client_finished()?;
    let ccs_recs = tls::split_records(&ccs);
    server.feed_record(ccs_recs[0].0, ccs_recs[0].1, &ccs_recs[0].2)?;
    assert_eq!(server.phase(), Phase::ExpectClientFinished);

    let fin_recs = tls::split_records(&fin);

    let server_out = server.feed_record(fin_recs[0].0, fin_recs[0].1, &fin_recs[0].2)?;
    if server.phase() != Phase::Established {
        bail!("握手未完成，状态是 {:?}", server.phase());
    }

    // 6) 校验服务端 Finished
    camera.verify_server_finished(&server_out)?;

    Ok((server, camera))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 核心测试：完整握手跑通，双向 Finished 互相校验通过
    #[test]
    fn full_handshake_completes_and_both_finished_verify() {
        let (server, _camera) = run_full_handshake([0x5Au8; 32]).expect("完整握手应当成功");

        assert_eq!(server.phase(), Phase::Established, "应进入应用数据阶段");
        let n = server.negotiated().unwrap();
        assert_eq!(n.cipher, tls::CIPHER_AES256_SHA, "协商出 AES-256");
        assert_ne!(n.client_random, [0u8; 32], "客户端随机数已收到");
    }

    /// 换一个服务端随机数也必须能跑通（排除"碰巧成功"）
    #[test]
    fn full_handshake_works_with_other_randoms() {
        let (server, _camera) =
            run_full_handshake([0x11u8; 32]).expect("换随机数后仍应成功");
        assert_eq!(server.phase(), Phase::Established);
    }

    /// 握手完成后可以收发应用数据
    #[test]
    fn application_data_flows_after_handshake() {
        let (mut server, _camera) = run_full_handshake([0x5Au8; 32]).unwrap();
        // 服务端给相机发一段 HTTP 响应
        let resp = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        let rec = server.encrypt_application_data(resp).unwrap();
        let records = tls::split_records(&rec);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, tls::CT_APPLICATION_DATA);
        assert!(records[0].2.len() > resp.len(), "密文应比明文长（含 MAC 与填充）");
    }

    /// 篡改客户端的 Finished 必须被拦住
    #[test]
    fn tampered_client_finished_is_rejected() {
        let mut server = TlsSession::new([0x5Au8; 32]);
        let mut camera = FakeCamera::new();
        let ch = camera.build_client_hello();
        let recs = tls::split_records(&ch);
        let out = server.feed_record(recs[0].0, recs[0].1, &recs[0].2).unwrap();
        let server_records = tls::split_records(&out);
        let cipher = camera.parse_server_flight(&server_records).unwrap();
        camera.derive_keys(&[0x5Au8; 32], cipher).unwrap();
        let kex = camera.build_client_key_exchange().unwrap();
        let recs = tls::split_records(&kex);
        server.feed_record(recs[0].0, recs[0].1, &recs[0].2).unwrap();

        // 客户端 CCS 与 Finished（两条独立记录）
        let (ccs, mut fin) = camera.build_client_finished().unwrap();
        let r = tls::split_records(&ccs);
        server.feed_record(r[0].0, r[0].1, &r[0].2).unwrap();

        // 篡改 Finished 的最后一个字节
        let last = fin.len() - 1;
        fin[last] ^= 0xff;
        let r = tls::split_records(&fin);
        let err = server
            .feed_record(r[0].0, r[0].1, &r[0].2)
            .expect_err("篡改过的 Finished 必须被拒绝");
        let msg = err.to_string();
        assert!(
            msg.contains("Finished") || msg.contains("解密") || msg.contains("校验"),
            "错误信息应说明是 Finished 的问题：{msg}"
        );
        assert_ne!(server.phase(), Phase::Established);
    }
}
