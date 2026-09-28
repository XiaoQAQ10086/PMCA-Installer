//! 索尼"代理消息"层：相机与 PC 之间的**唯一通信管道**。
//!
//! 这一层把"一条 Sony 消息"塞进两个 PTP 命令里传出去（先送元信息，再送正文）。
//! 上面所有东西（Sony 信封、TLS 隧道、REST 请求）都建立在它之上。
//!
//! # 报文结构（对应原项目 `pmca/usb/sony.py:594-655`）
//!
//! ```text
//! InfoMsgHeader（小端，共 58 字节）
//!   offset  0 : u32   读时 0x00010000 / 写时 0
//!   offset  4 : u16   magic = 0xB481
//!   offset  6 : u16   0
//!   offset  8 : u32   dataSize      ← 正文长度，必须用它截断
//!   offset 12 : u16   读时 0x3000 / 写时 0
//!   offset 14 : 42 字节填充
//! 之后跟正文。
//!
//! MsgHeader（大端，2 字节）：u16 type
//!   0 = Sony 通用消息   1 = TCP 代理   2 = REST
//! ```
//!
//! # 两个必须照抄的细节
//!
//! 1. **忙码 `0xA489` 要重试**：相机忙时会回这个码，必须一直重发。
//!    原实现是无上限忙等；我们加了退避与总超时（否则相机死机时界面会永久卡死）。
//! 2. **`0xA488`（NoData）是正常状态**，表示"当前没有消息"，不是错误。

use anyhow::{bail, Result};

/// 代理消息元信息的魔数
pub const INFO_MAGIC: u16 = 0xB481;

/// 我们**发送**时使用的元信息头长度。
///
/// 原项目按 42 字节填充算出 58 字节，我们就照它发（相机接受）。
pub const INFO_HEADER_LEN: usize = 58;

/// **解析**相机发来的元信息头时要求的最小长度。
///
/// ⚠️ 真机实测：ILCE-6300 发来的是 **56 字节**，不是 58。
/// 收到的字节（`dataSize = 0` 表示"当前没有消息"）：
/// ```text
/// 00 00 01 00 81 b4 00 00 00 00 00 00 00 30 之后全 0
///       ↑版本    ↑魔数    ↑dataSize=0    ↑读取标记 0x3000
/// ```
/// 字段**位置**与预期完全一致，只是尾部短了 2 字节。
/// 所以解析时只要够长、且魔数到位就接受；我们该读的字段都在前 14 字节内。
pub const INFO_HEADER_MIN_LEN: usize = 56;

// ---- 索尼私有的 PTP 操作码 ----
pub const PTP_OC_GET_PROXY_MESSAGE_INFO: u16 = 0x9488;
pub const PTP_OC_GET_PROXY_MESSAGE: u16 = 0x9489;
pub const PTP_OC_SEND_PROXY_MESSAGE_INFO: u16 = 0x948C;
pub const PTP_OC_SEND_PROXY_MESSAGE: u16 = 0x948D;
pub const PTP_OC_GET_DEVICE_CAPABILITY: u16 = 0x940A;

// ---- 索尼私有的 PTP 响应码 ----
/// "暂时没有数据" —— **这是正常状态，不是错误**
pub const PTP_RC_NO_DATA: u16 = 0xA488;
/// "设备忙" —— 必须重试
pub const PTP_RC_SONY_DEVICE_BUSY: u16 = 0xA489;
pub const PTP_RC_INTERNAL_ERROR: u16 = 0xA806;
pub const PTP_RC_TOO_MUCH_DATA: u16 = 0xA809;

use crate::mtp::PTP_RC_OK;
use crate::transport::PtpTransport;

/// 一条 Sony 消息（类型 + 正文）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyMessage {
    /// 外层类型：0=通用 1=TCP 2=REST
    pub msg_type: u16,
    pub payload: Vec<u8>,
}

impl ProxyMessage {
    pub fn new(msg_type: u16, payload: Vec<u8>) -> Self {
        Self { msg_type, payload }
    }

    /// 编码成"要交给 PTP 写数据阶段"的字节：2 字节类型（大端）+ 正文
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.payload.len());
        out.extend_from_slice(&self.msg_type.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// 从读到的字节解析
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 2 {
            bail!("代理消息太短：{} 字节", data.len());
        }
        Ok(Self {
            msg_type: u16::from_be_bytes([data[0], data[1]]),
            payload: data[2..].to_vec(),
        })
    }
}

/// 构造发送方向的信息头（58 字节，除魔数与长度外全为 0）
pub fn encode_info_header_write(data_size: usize) -> Vec<u8> {
    let mut out = vec![0u8; INFO_HEADER_LEN];
    // offset 0：写方向为 0
    out[4..6].copy_from_slice(&INFO_MAGIC.to_le_bytes());
    // offset 6：0
    out[8..12].copy_from_slice(&(data_size as u32).to_le_bytes());
    // offset 12：写方向为 0
    out
}

/// 解析读取方向的信息头，返回正文长度
pub fn parse_info_header(data: &[u8]) -> Result<usize> {
    if data.len() < INFO_HEADER_MIN_LEN {
        let show = data.len().min(80);
        bail!(
            "代理信息头太短：{} 字节（至少需要 {INFO_HEADER_MIN_LEN} 才能取到魔数）\n  实际内容：{:02x?}",
            data.len(),
            &data[..show]
        );
    }
    let magic = u16::from_le_bytes([data[4], data[5]]);
    if magic != INFO_MAGIC {
        bail!(
            "代理信息头魔数不对：0x{magic:04x}（应为 0x{INFO_MAGIC:04x}）\n  实际内容：{:02x?}",
            &data[..data.len().min(24)]
        );
    }
    Ok(u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize)
}

/// 重试策略（可调，便于测试）
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 最多重试多少次忙码
    pub max_attempts: u32,
    /// 首次退避时长（毫秒）
    pub base_backoff_ms: u64,
    /// 退避上限（毫秒）
    pub max_backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // 原实现是无限忙等；这里 5 秒左右的总时长足够覆盖相机的正常"忙"
        Self {
            max_attempts: 50,
            base_backoff_ms: 1,
            max_backoff_ms: 200,
        }
    }
}

impl RetryPolicy {
    /// 第 `attempt` 次重试前应等待多久（指数退避，封顶）
    pub fn backoff(&self, attempt: u32) -> std::time::Duration {
        let shift = attempt.min(16);
        let ms = self
            .base_backoff_ms
            .saturating_mul(1u64 << shift)
            .min(self.max_backoff_ms);
        std::time::Duration::from_millis(ms)
    }

    /// 测试用：不真的等待
    pub fn no_delay() -> Self {
        Self {
            max_attempts: 50,
            base_backoff_ms: 0,
            max_backoff_ms: 0,
        }
    }
}

/// 代理消息通道
pub struct ProxyChannel<T: PtpTransport> {
    transport: T,
    policy: RetryPolicy,
}

impl<T: PtpTransport> ProxyChannel<T> {
    pub fn new(transport: T, policy: RetryPolicy) -> Self {
        Self { transport, policy }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// 发送一条消息：**先送元信息，再送正文**，两步都要抗忙码
    pub fn send(&mut self, msg: &ProxyMessage) -> Result<()> {
        let body = msg.encode();
        let info = encode_info_header_write(body.len());

        let rc = self.retry_on_busy(|t| t.send_write_command(PTP_OC_SEND_PROXY_MESSAGE_INFO, &[], &info))?;
        check_ok(rc, "发送代理消息元信息")?;

        let rc = self.retry_on_busy(|t| t.send_write_command(PTP_OC_SEND_PROXY_MESSAGE, &[], &body))?;
        check_ok(rc, "发送代理消息正文")?;
        Ok(())
    }

    /// 接收一条消息。返回 `None` 表示"当前没有消息"（正常情况，调用方稍后再试）。
    pub fn receive(&mut self) -> Result<Option<ProxyMessage>> {
        // 第一步：读元信息
        let (rc, info) = self.transport.send_read_command(PTP_OC_GET_PROXY_MESSAGE_INFO, &[0])?;
        if rc == PTP_RC_NO_DATA || info.is_empty() {
            return Ok(None); // 没有消息
        }
        if rc != PTP_RC_OK {
            bail!("读取代理消息元信息失败：0x{rc:04x}");
        }
        let data_size = parse_info_header(&info)?;
        if data_size == 0 {
            return Ok(None);
        }

        // 第二步：读正文
        let (rc, body) = self.transport.send_read_command(PTP_OC_GET_PROXY_MESSAGE, &[0])?;
        if rc == PTP_RC_NO_DATA {
            return Ok(None);
        }
        if rc != PTP_RC_OK {
            bail!("读取代理消息正文失败：0x{rc:04x}");
        }
        // ⚠️ 必须按元信息里声明的长度截断：USB 传输会补齐缓冲，
        //    直接按实际收到的字节数处理会读到垃圾。
        let body = if body.len() >= data_size {
            &body[..data_size]
        } else {
            bail!(
                "代理消息正文比声明的短：声明 {data_size} 字节，实际 {} 字节",
                body.len()
            );
        };
        let msg = ProxyMessage::decode(body)?;
        tracing::debug!(
            outer_type = msg.msg_type,
            len = msg.payload.len(),
            "收到代理消息"
        );
        Ok(Some(msg))
    }

    /// 反复重试直到不再是"忙"码
    fn retry_on_busy<F>(&mut self, mut f: F) -> Result<u16>
    where
        F: FnMut(&mut T) -> Result<u16>,
    {
        for attempt in 0..self.policy.max_attempts {
            let rc = f(&mut self.transport)?;
            if rc != PTP_RC_SONY_DEVICE_BUSY && rc != PTP_RC_INTERNAL_ERROR {
                return Ok(rc);
            }
            let d = self.policy.backoff(attempt);
            if !d.is_zero() {
                std::thread::sleep(d);
            }
        }
        bail!(
            "相机持续忙碌，重试 {} 次后放弃（可能相机已死机或线缆松动）",
            self.policy.max_attempts
        )
    }
}

/// 把响应码翻译成人类可读的错误
pub fn check_ok(rc: u16, what: &str) -> Result<()> {
    if rc == PTP_RC_OK {
        return Ok(());
    }
    let hint = match rc {
        PTP_RC_SONY_DEVICE_BUSY => "相机忙",
        PTP_RC_NO_DATA => "暂时没有数据",
        PTP_RC_INTERNAL_ERROR => "相机内部错误",
        PTP_RC_TOO_MUCH_DATA => "数据过多",
        _ => "未知错误",
    };
    bail!("{what}失败：0x{rc:04x}（{hint}）");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::fake::{Call, FakeTransport};

    fn channel() -> ProxyChannel<FakeTransport> {
        ProxyChannel::new(FakeTransport::new(), RetryPolicy::no_delay())
    }

    /// 信息头必须是 58 字节，魔数与长度字段位置正确
    #[test]
    fn info_header_layout() {
        let h = encode_info_header_write(0x1234);
        assert_eq!(h.len(), INFO_HEADER_LEN);
        assert_eq!(&h[4..6], &INFO_MAGIC.to_le_bytes(), "魔数在 offset 4");
        assert_eq!(&h[8..12], &0x1234u32.to_le_bytes(), "长度在 offset 8");
        // 其余字节应为 0
        assert!(h[0..4].iter().all(|b| *b == 0));
        assert!(h[12..].iter().all(|b| *b == 0));
    }

    /// 解析信息头：魔数不对要报错，长度读对
    #[test]
    fn parse_info_header_checks_magic() {
        let h = encode_info_header_write(99);
        assert_eq!(parse_info_header(&h).unwrap(), 99);
        assert!(parse_info_header(&h[..10]).is_err(), "太短应报错");

        let mut bad = h.clone();
        bad[4] = 0x00;
        bad[5] = 0x00;
        assert!(parse_info_header(&bad).is_err(), "魔数不对应报错");
    }

    /// 消息编解码：类型是大端 2 字节
    #[test]
    fn message_roundtrip_big_endian_type() {
        let m = ProxyMessage::new(1, b"hello".to_vec());
        let e = m.encode();
        assert_eq!(&e[0..2], &[0x00, 0x01], "类型应是大端");
        assert_eq!(&e[2..], b"hello");
        assert_eq!(ProxyMessage::decode(&e).unwrap(), m);

        // 类型 0x0102 应编码成 01 02
        let m2 = ProxyMessage::new(0x0102, vec![]);
        assert_eq!(&m2.encode()[0..2], &[0x01, 0x02]);
    }

    /// 发送一条消息：应产生"元信息 + 正文"两次写命令，且顺序正确
    #[test]
    fn send_uses_two_write_commands_in_order() {
        let mut ch = channel();
        ch.transport_mut().push_command_reply(PTP_RC_OK); // 元信息
        ch.transport_mut().push_command_reply(PTP_RC_OK); // 正文

        let msg = ProxyMessage::new(2, b"POST / HTTP/1.0".to_vec());
        ch.send(&msg).unwrap();

        let calls = &ch.transport().calls;
        assert_eq!(calls.len(), 2, "应是两次写命令");
        match &calls[0] {
            Call::Write { code, data, .. } => {
                assert_eq!(*code, PTP_OC_SEND_PROXY_MESSAGE_INFO);
                assert_eq!(data.len(), INFO_HEADER_LEN);
                assert_eq!(parse_info_header(data).unwrap(), msg.encode().len());
            }
            other => panic!("第一条应是写命令，实际 {other:?}"),
        }
        match &calls[1] {
            Call::Write { code, data, .. } => {
                assert_eq!(*code, PTP_OC_SEND_PROXY_MESSAGE);
                assert_eq!(data, &msg.encode());
            }
            other => panic!("第二条应是写命令，实际 {other:?}"),
        }
    }

    /// 忙码必须重试，直到成功
    #[test]
    fn busy_code_is_retried() {
        let mut ch = channel();
        // 元信息：先忙两次再成功；正文：一次成功
        ch.transport_mut()
            .push_command_replies(&[PTP_RC_SONY_DEVICE_BUSY, PTP_RC_SONY_DEVICE_BUSY, PTP_RC_OK]);
        ch.transport_mut().push_command_reply(PTP_RC_OK);

        ch.send(&ProxyMessage::new(0, vec![1, 2, 3])).unwrap();
        // 重试了两次 + 成功一次 = 3 次写
        let writes = ch
            .transport()
            .calls
            .iter()
            .filter(|c| matches!(c, Call::Write { .. }))
            .count();
        assert_eq!(writes, 4, "元信息重试 3 次 + 正文 1 次");
    }

    /// 一直忙要能放弃（而不是永久卡死）
    #[test]
    fn gives_up_after_max_attempts() {
        let mut t = FakeTransport::new();
        t.push_command_replies(&[PTP_RC_SONY_DEVICE_BUSY; 64]);
        let mut ch = ProxyChannel::new(
            t,
            RetryPolicy {
                max_attempts: 3,
                base_backoff_ms: 0,
                max_backoff_ms: 0,
            },
        );
        let err = ch.send(&ProxyMessage::new(0, vec![])).unwrap_err();
        assert!(err.to_string().contains("持续忙碌"), "{err}");
    }

    /// NoData 是正常状态：receive 应返回 None 而不是报错
    #[test]
    fn no_data_means_no_message() {
        let mut ch = channel();
        ch.transport_mut().push_read_reply(PTP_RC_NO_DATA, vec![]);
        assert!(ch.receive().unwrap().is_none(), "NoData 应表示没有消息");
    }

    /// 正常接收一条消息
    #[test]
    fn receive_message_success() {
        let mut ch = channel();
        let msg = ProxyMessage::new(1, b"tcp-payload".to_vec());
        let info = encode_info_header_write(msg.encode().len());
        ch.transport_mut().push_read_reply(PTP_RC_OK, info);
        ch.transport_mut().push_read_reply(PTP_RC_OK, msg.encode());

        let got = ch.receive().unwrap().expect("应收到消息");
        assert_eq!(got, msg);
    }

    /// ⚠️ 正文必须按元信息声明的长度截断（USB 会补齐缓冲）
    #[test]
    fn receive_truncates_by_declared_size() {
        let mut ch = channel();
        let msg = ProxyMessage::new(2, b"short".to_vec());
        let mut padded = msg.encode();
        padded.extend_from_slice(&[0xAA; 100]); // 模拟缓冲补齐
        ch.transport_mut()
            .push_read_reply(PTP_RC_OK, encode_info_header_write(msg.encode().len()));
        ch.transport_mut().push_read_reply(PTP_RC_OK, padded);

        let got = ch.receive().unwrap().unwrap();
        assert_eq!(got, msg, "必须按声明长度截断，不能带上补齐的垃圾");
    }

    /// 声明的长度大于实际收到 → 应报错而不是静默截断
    #[test]
    fn receive_rejects_short_body() {
        let mut ch = channel();
        ch.transport_mut()
            .push_read_reply(PTP_RC_OK, encode_info_header_write(1000));
        ch.transport_mut()
            .push_read_reply(PTP_RC_OK, vec![0u8; 10]);
        assert!(ch.receive().is_err());
    }

    /// 退避必须是指数增长且封顶
    #[test]
    fn backoff_grows_and_caps() {
        let p = RetryPolicy {
            max_attempts: 10,
            base_backoff_ms: 1,
            max_backoff_ms: 20,
        };
        assert_eq!(p.backoff(0).as_millis(), 1);
        assert_eq!(p.backoff(1).as_millis(), 2);
        assert_eq!(p.backoff(2).as_millis(), 4);
        assert_eq!(p.backoff(10).as_millis(), 20, "应封顶");
        assert_eq!(p.backoff(64).as_millis(), 20, "极大值也不能溢出");
    }
}
