//! 端到端安装流程测试：我们扮演**相机**，与安装编排器对话走完全程。
//!
//! # 这个测试验证什么
//!
//! 从"发 Start"到"收到安装结果"，中间所有环节都真实跑一遍：
//!
//! ```text
//! ① 我们收到 Start            → 回 Hello
//! ② 我们收到 /task/start      → 触发 TLS 隧道
//! ③ 我们发 TLS ClientHello    → 收到 ServerHello+证书+完成
//! ④ 我们发 ClientKeyExchange  → 完成握手
//! ⑤ 我们发 HTTPS POST（加密） → 收到"去下载安装"的指令
//! ⑥ 我们发 HTTPS GET（加密）  → 收到 SPK 文件
//! ⑦ 我们发 /task/complete     → 收到 Bye，任务结束
//! ```
//!
//! 这里用的 TLS 客户端是与 `tests/full_handshake.rs` 里同一套逻辑（我们自己实现，
//! 与真实相机行为一致），所以整条链路是**真跑**的，不是打桩。

use anyhow::{bail, Context, Result};
use rsa::rand_core::{OsRng, RngCore};
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use sony_core::http::HttpRequest;
use sony_core::market::{InstallPhase, InstallRunner};
use sony_core::proxy::ProxyMessage;
use sony_core::record::RecordCipher;
use sony_core::session::finished_verify_data;
use sony_core::sony::{self, Incoming};
use sony_core::tls;

/// 扮演相机的一端
struct FakeCamera {
    client_random: [u8; 32],
    pre_master: [u8; 48],
    transcript: Vec<u8>,
    read: Option<RecordCipher>,
    write: Option<RecordCipher>,
    /// 主密钥（算 Finished 校验值要用）
    master: Option<[u8; 48]>,
    socket_fd: i32,
}

impl FakeCamera {
    fn new() -> Self {
        let mut client_random = [0u8; 32];
        let mut pre_master = [0u8; 48];
        OsRng.fill_bytes(&mut client_random);
        OsRng.fill_bytes(&mut pre_master);
        pre_master[0] = 0x03;
        pre_master[1] = 0x01;
        Self {
            client_random,
            pre_master,
            transcript: Vec::new(),
            read: None,
            write: None,
            master: None,
            socket_fd: 7,
        }
    }

    /// 构造 Hello（我们是相机，主动回 Hello）
    fn hello(&mut self) -> ProxyMessage {
        let protos = sony::PROTOCOLS;
        let mut body = (protos.len() as u32).to_be_bytes().to_vec();
        for (name, id) in protos {
            body.extend_from_slice(&name);
            body.extend_from_slice(&id.to_be_bytes());
        }
        ProxyMessage::new(sony::MSG_COMMON, sony::encode_common(sony::COMMON_HELLO, &body))
    }

    /// 相机请求开一条 TLS 隧道
    fn proxy_connect(&mut self) -> ProxyMessage {
        let host = "www.playmemoriescameraapps.com";
        let mut body = Vec::new();
        body.extend_from_slice(&self.socket_fd.to_be_bytes());
        body.extend_from_slice(&443u16.to_be_bytes());
        body.extend_from_slice(&(host.len() as u32).to_be_bytes());
        body.extend_from_slice(host.as_bytes());
        ProxyMessage::new(sony::MSG_TCP, sony::encode_common(sony::TCP_PROXY_CONNECT, &body))
    }

    /// 把一段要发给"服务器"的原始 TLS 字节包成 ProxyData
    fn proxy_data(&self, data: &[u8]) -> ProxyMessage {
        let mut body = Vec::new();
        body.extend_from_slice(&self.socket_fd.to_be_bytes());
        body.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
        ProxyMessage::new(sony::MSG_TCP, sony::encode_common(sony::TCP_PROXY_DATA, &body))
    }

    /// 构造 ClientHello
    fn build_client_hello(&mut self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&tls::TLS_VERSION_1_0.to_be_bytes());
        body.extend_from_slice(&self.client_random);
        body.push(0);
        let suites: [u16; 3] = [tls::CIPHER_AES128_SHA, tls::CIPHER_AES256_SHA, 0x00ff];
        body.extend_from_slice(&((suites.len() * 2) as u16).to_be_bytes());
        for s in suites {
            body.extend_from_slice(&s.to_be_bytes());
        }
        body.push(1);
        body.push(0);
        body.extend_from_slice(&0u16.to_be_bytes());

        let msg = tls::handshake_message(tls::HS_CLIENT_HELLO, &body);
        self.transcript.extend_from_slice(&msg);
        tls::wrap_handshake_record(&[msg])
    }

    /// 解析服务端首段，返回协商的套件
    fn parse_server_flight(&mut self, data: &[u8]) -> Result<u16> {
        for (ct, _ver, body) in tls::split_records(data) {
            if ct != tls::CT_HANDSHAKE {
                continue;
            }
            for (t, m) in sony_core::handshake::handshake_messages(&body) {
                self.transcript
                    .extend_from_slice(&sony_core::handshake::pack_handshake(t, &m));
            }
        }
        let first = tls::split_records(data)
            .into_iter()
            .find(|(ct, _, _)| *ct == tls::CT_HANDSHAKE)
            .context("服务端没发握手记录")?;
        tls::parse_server_hello_cipher(&first.2).context("没有 ServerHello")
    }

    fn build_client_key_exchange(&mut self) -> Result<Vec<u8>> {
        let key = sony_core::certs::private_key()?;
        let pub_key = RsaPublicKey::from(&key);
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

    fn derive_keys(&mut self, server_random: &[u8; 32], cipher: u16) -> Result<()> {
        let key_len = match cipher {
            tls::CIPHER_AES256_SHA => 32,
            tls::CIPHER_AES128_SHA => 16,
            other => bail!("不支持的套件 0x{other:04x}"),
        };
        let master =
            sony_core::prf::master_secret(&self.pre_master, &self.client_random, server_random);
        let kb = sony_core::prf::KeyBlock::derive(
            &master,
            &self.client_random,
            server_random,
            20,
            key_len,
            16,
        );
        self.write = Some(RecordCipher::client_to_server(&kb));
        self.read = Some(RecordCipher::server_to_client(&kb));
        self.master = Some(master);
        Ok(())
    }

    /// 客户端 CCS 与 Finished（两条独立记录）
    ///
    /// ⚠️ **CCS 不计入握手转录**（RFC 2246：Finished 只覆盖握手消息）。
    /// 记进去会让两端的哈希不一致，服务端就会判定校验值不匹配。
    fn build_client_finished(&mut self) -> Result<(Vec<u8>, Vec<u8>)> {
        // CCS 只发送，不记入转录
        let ccs = vec![tls::CT_CHANGE_CIPHER_SPEC, 0x03, 0x01, 0x00, 0x01, 0x01];

        let verify = finished_verify_data(self.master.as_ref().context("主密钥还没就绪")?, &self.transcript, "client finished");
        let msg = tls::handshake_message(tls::HS_FINISHED, &verify);
        let cipher = self.write.as_mut().context("密钥还没就绪")?;
        let enc = cipher.encrypt(tls::CT_HANDSHAKE, tls::TLS_VERSION_1_0, &msg);
        // 自己的 Finished 要记入转录：服务端的 Finished 覆盖它
        self.transcript.extend_from_slice(&msg);

        let mut fin = vec![tls::CT_HANDSHAKE, 0x03, 0x01];
        fin.extend_from_slice(&(enc.len() as u16).to_be_bytes());
        fin.extend_from_slice(&enc);

        Ok((ccs, fin))
    }

    /// 校验服务端 Finished
    fn verify_server_finished(&mut self, data: &[u8]) -> Result<()> {
        let records = tls::split_records(data);
        if records.len() < 2 {
            bail!("服务端收尾记录不足：{}", records.len());
        }
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

    /// 加密一条 HTTPS 请求
    fn encrypt_http(&mut self, http: &[u8]) -> Result<Vec<u8>> {
        let cipher = self.write.as_mut().context("密钥还没就绪")?;
        let enc = cipher.encrypt(tls::CT_APPLICATION_DATA, tls::TLS_VERSION_1_0, http);
        let mut out = vec![tls::CT_APPLICATION_DATA, 0x03, 0x01];
        out.extend_from_slice(&(enc.len() as u16).to_be_bytes());
        out.extend_from_slice(&enc);
        Ok(out)
    }

    /// 解密服务端发来的应用数据
    fn decrypt_http(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for (ct, ver, body) in tls::split_records(data) {
            if ct != tls::CT_APPLICATION_DATA {
                continue;
            }
            let cipher = self.read.as_mut().context("密钥还没就绪")?;
            out.extend_from_slice(&cipher.decrypt(ct, ver, &body)?);
        }
        Ok(out)
    }

    /// 构造一条明文 REST 消息（相机汇报用）
    fn rest(&self, http: &[u8]) -> ProxyMessage {
        let mut payload = Vec::new();
        payload.extend_from_slice(&sony::REST_IN.to_be_bytes());
        payload.extend_from_slice(&(http.len() as u16).to_be_bytes());
        payload.extend_from_slice(http);
        ProxyMessage::new(sony::MSG_REST, payload)
    }
}

/// 从编排器取出待发消息，返回其中的 TLS 数据（若有）
fn take_tls_data(runner: &mut InstallRunner) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for m in runner.take_outgoing() {
        if m.msg_type == sony::MSG_TCP
            && let Ok(Incoming::ProxyData { data, .. }) = sony::parse_incoming(&m)
        {
            out.extend_from_slice(&data);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// 跑完整个安装流程，返回（编排器, 相机收到的 SPK）
fn run_install() -> Result<(InstallRunner, Vec<u8>)> {
    let apk = vec![0x50, 0x4B, 0x03, 0x04, 0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22];
    let mut runner = InstallRunner::new(apk.clone(), [0x5Au8; 32])?;
    let mut cam = FakeCamera::new();

    // ① 编排器发 Start
    runner.start();
    let outs = runner.take_outgoing();
    assert_eq!(outs.len(), 1, "应发出 Start");
    assert_eq!(outs[0].msg_type, sony::MSG_COMMON);

    // ② 相机回 Hello
    let done = runner.poll(cam.hello())?;
    assert!(!done);
    assert_eq!(runner.phase(), InstallPhase::Running, "打完招呼应进入运行阶段");

    // 编排器现在应该发出了 /task/start
    // ⚠️ 不能用 `parse_incoming`：那是"解析收到的消息"，会拒收我们**发出去**的
    //    REST 消息（方向标记是 2 而不是 0）。这里直接按格式解析。
    let mut saw_task_start = false;
    for m in runner.take_outgoing() {
        if m.msg_type != sony::MSG_REST {
            continue;
        }
        let kind = u16::from_be_bytes([m.payload[0], m.payload[1]]);
        assert_eq!(kind, sony::REST_OUT, "我们发出去的 REST 方向应是 2");
        let size = u16::from_be_bytes([m.payload[2], m.payload[3]]) as usize;
        let req = HttpRequest::parse(&m.payload[4..4 + size])?;
        assert_eq!(req.path, "/task/start");
        assert_eq!(req.method, "POST");
        saw_task_start = true;
    }
    assert!(saw_task_start, "编排器应发起 /task/start");

    // ③ 相机要开 TLS 隧道
    runner.poll(cam.proxy_connect())?;
    let _ = runner.take_outgoing(); // 隧道打开本身不需要回消息

    // ④ TLS 握手：ClientHello
    let ch = cam.build_client_hello();
    runner.poll(cam.proxy_data(&ch))?;
    let flight = take_tls_data(&mut runner).context("应收到服务端首段")?;
    let cipher = cam.parse_server_flight(&flight)?;
    cam.derive_keys(&[0x5Au8; 32], cipher)?;

    // ⑤ ClientKeyExchange
    let kex = cam.build_client_key_exchange()?;
    runner.poll(cam.proxy_data(&kex))?;
    let _ = take_tls_data(&mut runner);

    // ⑥ CCS + Finished
    let (ccs, fin) = cam.build_client_finished()?;
    let mut both = ccs.clone();
    both.extend_from_slice(&fin);
    runner.poll(cam.proxy_data(&both))?;
    let server_tail = take_tls_data(&mut runner).context("应收到服务端完成握手")?;
    cam.verify_server_finished(&server_tail)
        .context("服务端 Finished 校验失败")?;

    // ⑦ 相机发 HTTPS POST（模拟 /task/start 已经走完，这里是相机来汇报）

    let mut post = b"POST /task/start REST/1.0\r\nContent-Type: application/json\r\n\r\n".to_vec();
    post.extend_from_slice(br#"{"accountinfo":{"signinid":"a@b.c"}}"#);
    let enc_post = cam.encrypt_http(&post)?;
    runner.poll(cam.proxy_data(&enc_post))?;
    let resp = take_tls_data(&mut runner).context("应收到 HTTPS 响应")?;
    let plain = cam.decrypt_http(&resp)?;
    let text = String::from_utf8_lossy(&plain);
    assert!(
        text.contains("dlandinstall"),
        "响应里应有安装指令，实际：{text}"
    );

    // ⑧ 相机发 HTTPS GET 下载 SPK
    let get = b"GET /app.1.spk HTTP/1.0\r\n\r\n".to_vec();
    let enc_get = cam.encrypt_http(&get)?;
    runner.poll(cam.proxy_data(&enc_get))?;
    let spk_resp = take_tls_data(&mut runner).context("应收到 SPK 响应")?;
    let spk_plain = cam.decrypt_http(&spk_resp)?;

    // 从 HTTP 响应里切出正文
    let sep = spk_plain
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("SPK 响应缺少头部/正文分隔")?;
    let spk_body = spk_plain[sep + 4..].to_vec();
    assert!(sony_core::spk::is_spk(&spk_body), "拿到的应是合法 SPK");
    assert_eq!(
        sony_core::spk::parse(&spk_body)?,
        apk,
        "SPK 解出来应等于原始 APK"
    );

    // ⑨ 相机汇报完成
    let mut complete = b"POST /task/complete REST/1.0\r\n\r\n".to_vec();
    complete.extend_from_slice(br#"{"resultCode":0,"message":"ok"}"#);
    let done = runner.poll(cam.rest(&complete))?;
    assert!(done, "任务应结束");
    assert_eq!(runner.phase(), InstallPhase::Done);

    Ok((runner, spk_body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 核心测试：完整安装流程跑通
    #[test]
    fn full_install_flow_succeeds() {
        let (runner, spk_body) = run_install().expect("安装流程应当成功");

        assert_eq!(runner.phase(), InstallPhase::Done);
        let res = runner.result().expect("应有结果");
        assert_eq!(res.code, 0);
        assert_eq!(res.message, "ok");
        assert!(sony_core::spk::is_spk(&spk_body));
        assert!(runner.progress_text().contains("安装完成"));
    }

    /// 安装过程中相机回传的设备信息要能取到
    #[test]
    fn install_captures_device_report() {
        let (runner, _) = run_install().unwrap();
        let report = runner.device_report().expect("应记录相机回传的信息");
        assert_eq!(report["accountinfo"]["signinid"], "a@b.c");
    }

    /// 换个服务端随机数也要能成功（排除碰巧）
    #[test]
    fn install_works_with_other_random() {
        // run_install 里固定用 [0x5A;32]；这里换个值再跑一遍等价流程
        let apk = vec![9u8; 64];
        let mut runner = InstallRunner::new(apk, [0x33u8; 32]).unwrap();
        let mut cam = FakeCamera::new();
        runner.start();
        let _ = runner.take_outgoing();
        runner.poll(cam.hello()).unwrap();
        let _ = runner.take_outgoing();
        runner.poll(cam.proxy_connect()).unwrap();
        let _ = runner.take_outgoing();

        let ch = cam.build_client_hello();
        runner.poll(cam.proxy_data(&ch)).unwrap();
        let flight = take_tls_data(&mut runner).unwrap();
        let cipher = cam.parse_server_flight(&flight).unwrap();
        cam.derive_keys(&[0x33u8; 32], cipher).unwrap();

        let kex = cam.build_client_key_exchange().unwrap();
        runner.poll(cam.proxy_data(&kex)).unwrap();
        let _ = take_tls_data(&mut runner);

        let (ccs, fin) = cam.build_client_finished().unwrap();
        let mut both = ccs;
        both.extend_from_slice(&fin);
        runner.poll(cam.proxy_data(&both)).unwrap();
        let tail = take_tls_data(&mut runner).unwrap();
        cam.verify_server_finished(&tail).unwrap();

        // 简单确认能正常收一条 HTTPS 响应
        let get = b"GET / HTTP/1.0\r\n\r\n".to_vec();
        let enc = cam.encrypt_http(&get).unwrap();
        runner.poll(cam.proxy_data(&enc)).unwrap();
        let resp = take_tls_data(&mut runner).unwrap();
        let plain = cam.decrypt_http(&resp).unwrap();
        assert!(String::from_utf8_lossy(&plain).contains("200 OK"));
    }

    /// 相机发 Bye 时要立刻结束，而不是继续等
    #[test]
    fn bye_ends_the_task() {
        let mut runner = InstallRunner::new(vec![1, 2, 3], [1u8; 32]).unwrap();
        runner.start();
        let _ = runner.take_outgoing();
        let mut body = (0u16).to_be_bytes().to_vec();
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        let bye = ProxyMessage::new(
            sony::MSG_COMMON,
            sony::encode_common(sony::COMMON_BYE, &body),
        );
        let done = runner.poll(bye).unwrap();
        assert!(done);
        assert_eq!(runner.phase(), InstallPhase::Failed);
    }
}
