//! 索尼消息信封层：Common / TCP / REST 三类消息。
//!
//! 这一层坐在代理消息（[`crate::proxy`]）之上，是相机与 PC 的"会话语言"。
//!
//! # 三层信封的结构（对应原项目 `pmca/usb/sony.py:658-820`）
//!
//! 代理消息的正文 = `MsgHeader(2 字节大端类型) + 该类消息的信封 + 载荷`。
//!
//! ```text
//! 外层类型（u16 大端）
//!   0 = 通用消息（Common）  1 = TCP 代理  2 = REST
//!
//! CommonMsgHeader（大端，16 字节）
//!   u16 version = 1
//!   u32 type        0x400=Start(PC→相机) 0x401=Hello(相机→PC) 0x402=Bye(PC→相机)
//!   u32 size        从**本头起始**算起的整条长度（含头）
//!   u8[6] 填充
//!
//! TcpMsgHeader（大端，4 字节）：i32 socketFd
//!   子类型：0x501=ProxyConnect 0x502=ProxyDisconnect 0x503=ProxyData 0x504=ProxyEnd
//!
//! RestMsgHeader（大端，4 字节）：u16 type + u16 size
//!   type：0=相机→PC（In）  2=PC→相机（Out）  ⚠️ 与直觉相反
//! ```
//!
//! # 三个必须照抄的细节
//!
//! 1. **大小端混用**：信封头是大端，而代理消息的 `InfoMsgHeader` 是小端。
//! 2. `size` 是**从本头起始**算起的总长，不是载荷长度。
//! 3. 协议表是硬编码的 `[("TCPT", 1), ("REST", 0x100)]`。

use anyhow::{bail, Context, Result};

use crate::proxy::{ProxyChannel, ProxyMessage};
use crate::transport::PtpTransport;

// ---- 外层类型 ----
pub const MSG_COMMON: u16 = 0;
pub const MSG_TCP: u16 = 1;
pub const MSG_REST: u16 = 2;

// ---- Common 子类型 ----
pub const COMMON_START: u32 = 0x400;
pub const COMMON_HELLO: u32 = 0x401;
pub const COMMON_BYE: u32 = 0x402;

// ---- TCP 子类型 ----
pub const TCP_PROXY_CONNECT: u32 = 0x501;
pub const TCP_PROXY_DISCONNECT: u32 = 0x502;
pub const TCP_PROXY_DATA: u32 = 0x503;
pub const TCP_PROXY_END: u32 = 0x504;

// ---- REST 子类型 ----
/// 相机 → PC
pub const REST_IN: u16 = 0;
/// PC → 相机
pub const REST_OUT: u16 = 2;

/// 通用消息头长度
pub const COMMON_HEADER_LEN: usize = 16;
pub const COMMON_MSG_VERSION: u32 = 1;

/// 协议表（硬编码，对应 `pmca/usb/sony.py:710`）
pub const PROTOCOLS: [([u8; 4], u16); 2] = [( *b"TCPT", 0x01), (*b"REST", 0x100)];

/// 从相机收到的消息
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// 相机的协议问候（回应我们的 Start）
    Hello { protocols: Vec<([u8; 4], u16)> },
    /// 相机要求打开一条 TCP 通道（TLS 隧道就是这条）
    ProxyConnect {
        socket_fd: i32,
        host: String,
        port: u16,
    },
    ProxyDisconnect { socket_fd: i32 },
    ProxyData { socket_fd: i32, data: Vec<u8> },
    /// REST 消息（相机发来的 HTTP 请求）
    Rest { data: Vec<u8> },
    /// 相机说再见
    Bye,
}

/// 要发给相机的消息
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    Start { protocols: Vec<([u8; 4], u16)> },
    Bye,
    ProxyData { socket_fd: i32, data: Vec<u8> },
    ProxyEnd { socket_fd: i32 },
    Rest { data: Vec<u8> },
}

impl Outgoing {
    /// 编码成一条代理消息
    pub fn encode(&self) -> ProxyMessage {
        match self {
            Outgoing::Start { protocols } => {
                let mut body = Vec::new();
                for (name, id) in protocols {
                    body.extend_from_slice(name);
                    body.extend_from_slice(&id.to_be_bytes());
                }
                let payload = encode_common(COMMON_START, &body);
                ProxyMessage::new(MSG_COMMON, payload)
            }
            Outgoing::Bye => {
                // ThreeValueMsg(a=0, b=0, c=0)
                let mut body = Vec::with_capacity(10);
                body.extend_from_slice(&0u16.to_be_bytes());
                body.extend_from_slice(&0u32.to_be_bytes());
                body.extend_from_slice(&0u32.to_be_bytes());
                ProxyMessage::new(MSG_COMMON, encode_common(COMMON_BYE, &body))
            }
            Outgoing::ProxyData { socket_fd, data } => {
                let mut body = Vec::with_capacity(4 + 4 + data.len());
                body.extend_from_slice(&socket_fd.to_be_bytes());
                body.extend_from_slice(&(data.len() as u32).to_be_bytes());
                body.extend_from_slice(data);
                ProxyMessage::new(MSG_TCP, encode_common(TCP_PROXY_DATA, &body))
            }
            Outgoing::ProxyEnd { socket_fd } => {
                let mut body = Vec::with_capacity(4 + 10);
                body.extend_from_slice(&socket_fd.to_be_bytes());
                // ThreeValueMsg(a=1, b=1, c=0)
                body.extend_from_slice(&1u16.to_be_bytes());
                body.extend_from_slice(&1u32.to_be_bytes());
                body.extend_from_slice(&0u32.to_be_bytes());
                ProxyMessage::new(MSG_TCP, encode_common(TCP_PROXY_END, &body))
            }
            Outgoing::Rest { data } => {
                let mut body = Vec::with_capacity(4 + data.len());
                body.extend_from_slice(&REST_OUT.to_be_bytes());
                body.extend_from_slice(&(data.len() as u16).to_be_bytes());
                body.extend_from_slice(data);
                ProxyMessage::new(MSG_REST, body)
            }
        }
    }
}

/// 构造 Common 类信封：16 字节头 + 载荷
pub fn encode_common(sub_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(COMMON_HEADER_LEN + payload.len());
    out.extend_from_slice(&(COMMON_MSG_VERSION as u16).to_be_bytes());
    out.extend_from_slice(&sub_type.to_be_bytes());
    // size = 从本头起始的总长（含头）
    out.extend_from_slice(&((COMMON_HEADER_LEN + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(&[0u8; 6]); // 填充
    out.extend_from_slice(payload);
    out
}

/// 解析一条 Common 信封，返回（子类型, 载荷）
pub fn parse_common(data: &[u8]) -> Result<(u32, Vec<u8>)> {
    if data.len() < COMMON_HEADER_LEN {
        bail!("Common 信封太短：{} 字节", data.len());
    }
    let version = u16::from_be_bytes([data[0], data[1]]);
    if version as u32 != COMMON_MSG_VERSION {
        bail!("Common 版本不支持：{version}（期望 {COMMON_MSG_VERSION}）");
    }
    let sub_type = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
    let size = u32::from_be_bytes([data[6], data[7], data[8], data[9]]) as usize;
    if size < COMMON_HEADER_LEN {
        bail!("Common 声明的长度非法：{size}");
    }
    if data.len() < size {
        bail!("Common 被截断：声明 {size} 字节，实际 {}", data.len());
    }
    Ok((sub_type, data[COMMON_HEADER_LEN..size].to_vec()))
}

/// 解析协议表：u32 个数 + 每项 4 字节名 + 2 字节 id
fn parse_protocol_list(data: &[u8]) -> Result<Vec<([u8; 4], u16)>> {
    if data.len() < 4 {
        bail!("协议表太短");
    }
    let count = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let need = 4 + count * 6;
    if data.len() < need {
        bail!("协议表被截断：声明 {count} 项，需要 {need} 字节，实际 {}", data.len());
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let p = 4 + i * 6;
        let mut name = [0u8; 4];
        name.copy_from_slice(&data[p..p + 4]);
        let id = u16::from_be_bytes([data[p + 4], data[p + 5]]);
        out.push((name, id));
    }
    Ok(out)
}

/// 解析从相机收到的一条代理消息
pub fn parse_incoming(msg: &ProxyMessage) -> Result<Incoming> {
    match msg.msg_type {
        MSG_COMMON => {
            let (sub, payload) = parse_common(&msg.payload).context("解析 Common 信封失败")?;
            match sub {
                COMMON_HELLO => Ok(Incoming::Hello {
                    protocols: parse_protocol_list(&payload).context("解析协议表失败")?,
                }),
                COMMON_BYE => Ok(Incoming::Bye),
                other => bail!("未知的 Common 子类型：0x{other:x}"),
            }
        }
        MSG_TCP => {
            let (sub, payload) = parse_common(&msg.payload).context("解析 TCP 信封失败")?;
            if payload.len() < 4 {
                bail!("TCP 消息缺少 socketFd");
            }
            let socket_fd = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let rest = &payload[4..];
            match sub {
                TCP_PROXY_CONNECT => {
                    if rest.len() < 6 {
                        bail!("ProxyConnect 缺少端口/主机长度");
                    }
                    let port = u16::from_be_bytes([rest[0], rest[1]]);
                    let host_size =
                        u32::from_be_bytes([rest[2], rest[3], rest[4], rest[5]]) as usize;
                    if rest.len() < 6 + host_size {
                        bail!("ProxyConnect 的主机名被截断");
                    }
                    let host = String::from_utf8_lossy(&rest[6..6 + host_size]).to_string();
                    Ok(Incoming::ProxyConnect {
                        socket_fd,
                        host,
                        port,
                    })
                }
                TCP_PROXY_DISCONNECT => Ok(Incoming::ProxyDisconnect { socket_fd }),
                TCP_PROXY_DATA => {
                    if rest.len() < 4 {
                        bail!("ProxyData 缺少数据长度");
                    }
                    let size = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
                    if rest.len() < 4 + size {
                        bail!("ProxyData 被截断：声明 {size} 字节");
                    }
                    Ok(Incoming::ProxyData {
                        socket_fd,
                        data: rest[4..4 + size].to_vec(),
                    })
                }
                other => bail!("未知的 TCP 子类型：0x{other:x}"),
            }
        }
        MSG_REST => {
            if msg.payload.len() < 4 {
                bail!("REST 消息太短");
            }
            let kind = u16::from_be_bytes([msg.payload[0], msg.payload[1]]);
            let size = u16::from_be_bytes([msg.payload[2], msg.payload[3]]) as usize;
            if msg.payload.len() < 4 + size {
                bail!(
                    "REST 消息被截断：声明 {size} 字节，实际只有 {}",
                    msg.payload.len() - 4
                );
            }
            // ⚠️ 真机实测（ILCE-6300）：相机**对我们请求的回复**用的方向标记也是 2，
            //    并不是原项目里那个 `REST_Out=2` 的含义。
            //    所以我们不按方向标记过滤：**任何从相机收到的 REST 正文都是数据**。
            //    原项目 `installer/__init__.py:26` 也只是把 type 解析出来、并未使用。
            let _ = kind;
            Ok(Incoming::Rest {
                data: msg.payload[4..4 + size].to_vec(),
            })
        }
        other => bail!("未知的外层消息类型：{other}"),
    }
}

/// 会话门面：把"发信封 / 收信封"包装成好用的方法
pub struct SonySession<T: PtpTransport> {
    channel: ProxyChannel<T>,
    started: bool,
}

impl<T: PtpTransport> SonySession<T> {
    pub fn new(transport: T, policy: crate::proxy::RetryPolicy) -> Self {
        Self {
            channel: ProxyChannel::new(transport, policy),
            started: false,
        }
    }

    pub fn channel(&self) -> &ProxyChannel<T> {
        &self.channel
    }

    pub fn channel_mut(&mut self) -> &mut ProxyChannel<T> {
        &mut self.channel
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// 发送 Start 并等待相机的 Hello，返回相机支持的协议表
    pub fn handshake(&mut self) -> Result<Vec<([u8; 4], u16)>> {
        let start = Outgoing::Start {
            protocols: PROTOCOLS.to_vec(),
        };
        self.channel.send(&start.encode())?;
        // 等到 Hello 为止（中间可能有无关消息）
        for _ in 0..100 {
            if let Some(msg) = self.channel.receive()? {
                match parse_incoming(&msg)? {
                    Incoming::Hello { protocols } => {
                        self.started = true;
                        return Ok(protocols);
                    }
                    other => bail!("握手时收到意外消息：{other:?}"),
                }
            }
        }
        bail!("等待相机 Hello 超时")
    }

    /// 发送一条 REST 消息
    pub fn send_rest(&mut self, data: &[u8]) -> Result<()> {
        self.channel.send(&Outgoing::Rest { data: data.to_vec() }.encode())
    }

    /// 把字节转发给相机（TLS 隧道方向：PC → 相机）
    pub fn send_proxy_data(&mut self, socket_fd: i32, data: &[u8]) -> Result<()> {
        self.channel
            .send(&Outgoing::ProxyData { socket_fd, data: data.to_vec() }.encode())
    }

    pub fn send_bye(&mut self) -> Result<()> {
        self.channel.send(&Outgoing::Bye.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{encode_info_header_write, RetryPolicy, INFO_HEADER_LEN};
    use crate::transport::fake::FakeTransport;

    fn policy() -> RetryPolicy {
        RetryPolicy::no_delay()
    }

    /// Common 信封：版本/类型/长度/填充的位置与取值
    #[test]
    fn common_envelope_layout() {
        let e = encode_common(COMMON_START, &[0xAA, 0xBB]);
        assert_eq!(e.len(), COMMON_HEADER_LEN + 2);
        assert_eq!(&e[0..2], &[0, 1], "版本 = 1");
        assert_eq!(&e[2..6], &COMMON_START.to_be_bytes(), "子类型大端");
        assert_eq!(&e[6..10], &18u32.to_be_bytes(), "size 含头");
        assert!(e[10..16].iter().all(|b| *b == 0), "6 字节填充");
        assert_eq!(&e[16..], &[0xAA, 0xBB]);
    }

    /// Common 解析：版本不对、长度不对都要报错
    #[test]
    fn common_parse_checks_version_and_size() {
        let e = encode_common(COMMON_HELLO, &[1, 2, 3]);
        let (sub, payload) = parse_common(&e).unwrap();
        assert_eq!(sub, COMMON_HELLO);
        assert_eq!(payload, vec![1, 2, 3]);

        let mut bad_ver = e.clone();
        bad_ver[0..2].copy_from_slice(&9u16.to_be_bytes());
        assert!(parse_common(&bad_ver).is_err());

        let mut bad_size = e.clone();
        bad_size[6..10].copy_from_slice(&1000u32.to_be_bytes());
        assert!(parse_common(&bad_size).is_err(), "声明比实际长应报错");
    }

    /// 协议表编解码
    #[test]
    fn protocol_list_roundtrip() {
        let mut body = Vec::new();
        for (name, id) in PROTOCOLS {
            body.extend_from_slice(&name);
            body.extend_from_slice(&id.to_be_bytes());
        }
        let mut with_count = (PROTOCOLS.len() as u32).to_be_bytes().to_vec();
        with_count.extend_from_slice(&body);
        let got = parse_protocol_list(&with_count).unwrap();
        assert_eq!(got, PROTOCOLS.to_vec());
        assert_eq!(got[0].0, *b"TCPT");
        assert_eq!(got[1].0, *b"REST");
    }

    /// 走出去的消息要能被我们自己的解析器认出来（除方向标记外）
    #[test]
    fn outgoing_tcp_data_roundtrip() {
        let msg = Outgoing::ProxyData {
            socket_fd: 3,
            data: b"TLS-bytes".to_vec(),
        }
        .encode();
        assert_eq!(msg.msg_type, MSG_TCP);
        let parsed = parse_incoming(&msg).unwrap();
        match parsed {
            Incoming::ProxyData { socket_fd, data } => {
                assert_eq!(socket_fd, 3);
                assert_eq!(data, b"TLS-bytes");
            }
            other => panic!("应是 ProxyData，实际 {other:?}"),
        }
    }

    /// 相机的 ProxyConnect：host 与 port 必须解析出来（虽然我们会忽略它们）
    #[test]
    fn parse_proxy_connect_from_camera() {
        let host = "www.playmemoriescameraapps.com";
        let mut body = Vec::new();
        body.extend_from_slice(&7i32.to_be_bytes()); // socketFd
        body.extend_from_slice(&443u16.to_be_bytes()); // port
        body.extend_from_slice(&(host.len() as u32).to_be_bytes());
        body.extend_from_slice(host.as_bytes());
        let msg = ProxyMessage::new(MSG_TCP, encode_common(TCP_PROXY_CONNECT, &body));

        match parse_incoming(&msg).unwrap() {
            Incoming::ProxyConnect {
                socket_fd,
                host: h,
                port,
            } => {
                assert_eq!(socket_fd, 7);
                assert_eq!(h, host);
                assert_eq!(port, 443, "相机以为要连 443");
            }
            other => panic!("应是 ProxyConnect，实际 {other:?}"),
        }
    }

    /// REST 方向标记：0 是"相机→PC"，2 是"PC→相机"
    #[test]
    fn rest_direction_flags() {
        // 我们发出去的用 2
        let out = Outgoing::Rest {
            data: b"POST /task/start".to_vec(),
        }
        .encode();
        assert_eq!(out.msg_type, MSG_REST);
        assert_eq!(&out.payload[0..2], &[0x00, 0x02], "PC→相机 = 2");
        // ★ 真机实测（ILCE-6300）：相机对我们请求的回复**也用方向标记 2**，
        //   所以解析时不能按方向过滤 —— 任何从相机收到的 REST 正文都是数据。
        match parse_incoming(&out).unwrap() {
            Incoming::Rest { data } => assert_eq!(data, b"POST /task/start"),
            other => panic!("方向 2 的 REST 也应能解析，实际 {other:?}"),
        }

        // 相机发来的用 0
        let mut body = Vec::new();
        body.extend_from_slice(&REST_IN.to_be_bytes());
        body.extend_from_slice(&(4u16).to_be_bytes());
        body.extend_from_slice(b"ping");
        let incoming = ProxyMessage::new(MSG_REST, body);
        match parse_incoming(&incoming).unwrap() {
            Incoming::Rest { data } => assert_eq!(data, b"ping"),
            other => panic!("应是 REST，实际 {other:?}"),
        }
    }

    /// 完整握手：发 Start、收 Hello
    #[test]
    fn session_handshake_with_fake_camera() {
        let mut t = FakeTransport::new();
        // 发 Start：写元信息 + 写正文 → 两次成功
        t.push_command_reply(crate::mtp::PTP_RC_OK);
        t.push_command_reply(crate::mtp::PTP_RC_OK);
        // 收 Hello：读元信息 → 读正文
        let mut hello_body = Vec::new();
        for (name, id) in PROTOCOLS {
            hello_body.extend_from_slice(&name);
            hello_body.extend_from_slice(&id.to_be_bytes());
        }
        let hello_payload = {
            let mut p = (PROTOCOLS.len() as u32).to_be_bytes().to_vec();
            p.extend_from_slice(&hello_body);
            p
        };
        let hello_msg = ProxyMessage::new(MSG_COMMON, encode_common(COMMON_HELLO, &hello_payload));
        t.push_read_reply(
            crate::mtp::PTP_RC_OK,
            crate::proxy::encode_info_header_write(hello_msg.encode().len()),
        );
        t.push_read_reply(crate::mtp::PTP_RC_OK, hello_msg.encode());

        let mut session = SonySession::new(t, policy());
        let protocols = session.handshake().expect("握手应成功");
        assert!(session.is_started());
        assert_eq!(protocols, PROTOCOLS.to_vec());

        // 检查我们发出去的第一条消息确实是 Start
        use crate::transport::fake::Call;
        let first_write = session
            .channel()
            .transport()
            .calls
            .iter()
            .find_map(|c| match c {
                Call::Write { code, data, .. } if *code == crate::proxy::PTP_OC_SEND_PROXY_MESSAGE => Some(data.clone()),
                _ => None,
            })
            .expect("应有正文写入");
        assert_eq!(first_write[0..2], MSG_COMMON.to_be_bytes(), "外层类型 0");
        let (sub, _) = parse_common(&first_write[2..]).unwrap();
        assert_eq!(sub, COMMON_START);
    }

    /// 信息头长度常量与代理层一致（防止两边各写一份漂移）
    #[test]
    fn info_header_len_matches_proxy_layer() {
        assert_eq!(INFO_HEADER_LEN, 58);
        assert_eq!(encode_info_header_write(0).len(), INFO_HEADER_LEN);
    }
}
