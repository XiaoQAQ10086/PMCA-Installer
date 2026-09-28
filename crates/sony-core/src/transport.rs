//! 传输抽象：把"底层怎么送字节"与"上层说什么话"分开。
//!
//! 上层协议栈（代理消息、Sony 信封、TLS 隧道）只依赖这个 trait，
//! 因此可以：
//! - 在 Windows 上用 WPD COM 后端实现
//! - 在测试里用「脚本化假驱动」实现（不需要真相机）
//!
//! 对应原项目 `pmca/usb/driver/__init__.py:39-50` 的 `BaseMtpDriver`。

use anyhow::{bail, Result};

use crate::mtp::{PtpPacket, PTP_TYPE_COMMAND, PTP_TYPE_DATA, PTP_TYPE_RESPONSE};

/// 一台 MTP 设备的低层操作
pub trait PtpTransport {
    /// 发送一条**不带数据阶段**的命令，返回响应码
    fn send_command(&mut self, code: u16, args: &[u32]) -> Result<u16>;

    /// 发送一条**带写数据阶段**的命令，返回响应码
    fn send_write_command(&mut self, code: u16, args: &[u32], data: &[u8]) -> Result<u16>;

    /// 发送一条**带读数据阶段**的命令，返回（响应码, 数据）
    fn send_read_command(&mut self, code: u16, args: &[u32]) -> Result<(u16, Vec<u8>)>;

    /// 事务号：每条命令递增（原项目 `_writeInitialCommand` 里自增）
    fn transaction(&self) -> u32;

    fn set_transaction(&mut self, t: u32);

    /// 把参数数组编码成 PTP 命令的数据段（每个参数 4 字节小端）
    fn encode_args(&self, args: &[u32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(args.len() * 4);
        for a in args {
            out.extend_from_slice(&a.to_le_bytes());
        }
        out
    }

    /// 构造一条命令容器（供实现者复用）
    fn make_command(&mut self, code: u16, args: &[u32]) -> PtpPacket {
        let t = self.transaction().wrapping_add(1);
        self.set_transaction(t);
        PtpPacket::new(PTP_TYPE_COMMAND, code, t, self.encode_args(args))
    }

    /// 构造一条数据容器
    fn make_data(transaction: u32, code: u16, data: &[u8]) -> PtpPacket {
        PtpPacket::new(PTP_TYPE_DATA, code, transaction, data.to_vec())
    }

    /// 构造一条响应容器
    fn make_response(transaction: u32, code: u16) -> PtpPacket {
        PtpPacket::new(PTP_TYPE_RESPONSE, code, transaction, Vec::new())
    }

    /// 执行一条**索尼扩展命令**（相机在普通 MTP 模式下也支持）。
    ///
    /// 对应原项目 `pmca/usb/sony.py:126-139` 的 `sendSonyExtCommand`：
    /// ```text
    /// 1. 以命令组为参数，把「扩展命令头 + 正文（补齐到 0x2000 字节）」写给 0x9280
    /// 2. 若需要响应，则用 0x9281 读回，并按头里的 dataSize 截取正文
    /// ```
    ///
    /// `read_buffer_size == 0` 表示不读响应（`switchToAppInstaller` 就是这种）。
    fn send_sony_ext_command(
        &mut self,
        cmd: crate::mtp::SonyExtCommand,
        data: &[u8],
        read_buffer_size: usize,
    ) -> Result<Vec<u8>> {
        use crate::mtp;
        // 相机忙时要重发（原项目在这里也是一个忙等循环）
        let payload = mtp::build_sony_ext_payload(cmd, data);
        let mut rc = mtp::PTP_RC_DEVICE_BUSY;
        let mut guard = 0;
        while rc == mtp::PTP_RC_DEVICE_BUSY && guard < 50 {
            rc = self.send_write_command(
                mtp::PTP_OC_SONY_DI_EXT_CMD_WRITE,
                &[cmd.group as u32],
                &payload,
            )?;
            guard += 1;
        }
        if rc != mtp::PTP_RC_OK {
            bail!("发送索尼扩展命令失败：响应码 0x{rc:04x}");
        }
        if read_buffer_size == 0 {
            return Ok(Vec::new());
        }

        let mut rc = mtp::PTP_RC_DEVICE_BUSY;
        let mut guard = 0;
        let mut resp = Vec::new();
        while rc == mtp::PTP_RC_DEVICE_BUSY && guard < 50 {
            let (r, d) = self.send_read_command(mtp::PTP_OC_SONY_DI_EXT_CMD_READ, &[cmd.group as u32])?;
            rc = r;
            resp = d;
            guard += 1;
        }
        if rc != mtp::PTP_RC_OK {
            bail!("读取索尼扩展命令响应失败：响应码 0x{rc:04x}");
        }
        mtp::parse_sony_ext_response(&resp)
    }

    /// **让相机切换到应用安装模式。**
    ///
    /// 这是原项目 `switchToAppInstaller` 的等价实现。相机收到后会重启 USB 连接、
    /// 变成一个"支持代理消息"的设备，所以调用方必须在之后**重新枚举设备**。
    ///
    /// ⚠️ 只对支持索尼扩展命令（0x9280/0x9281/0x9282）的相机有效。
    fn switch_to_app_install_mode(&mut self) -> Result<()> {
        use crate::mtp;
        self.send_sony_ext_command(mtp::SONY_CMD_NOTIFY_SCALAR_DLMODE, &[], 0)?;
        Ok(())
    }
}

#[cfg(test)]
pub mod fake {
    //! 测试用的脚本化假驱动：按预设的响应序列回应，不需要真相机。

    use super::*;
    use std::collections::VecDeque;

    /// 一次"读数据"要返回的东西
    #[derive(Debug, Clone)]
    pub struct ReadReply {
        pub code: u16,
        pub data: Vec<u8>,
    }

    /// 记录一次调用
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        Command { code: u16, args: Vec<u32> },
        Write { code: u16, args: Vec<u32>, data: Vec<u8> },
        Read { code: u16, args: Vec<u32> },
    }

    /// 假驱动：`command_replies` 按顺序给出响应码；
    /// `read_replies` 按顺序给出读数据阶段的返回。
    pub struct FakeTransport {
        pub calls: Vec<Call>,
        pub command_replies: VecDeque<u16>,
        pub read_replies: VecDeque<ReadReply>,
        transaction: u32,
    }

    impl FakeTransport {
        pub fn new() -> Self {
            Self {
                calls: Vec::new(),
                command_replies: VecDeque::new(),
                read_replies: VecDeque::new(),
                transaction: 0,
            }
        }

        /// 预设一条"不带数据"的命令的响应码
        pub fn push_command_reply(&mut self, code: u16) -> &mut Self {
            self.command_replies.push_back(code);
            self
        }

        /// 预设一条"读数据"命令的返回
        pub fn push_read_reply(&mut self, code: u16, data: Vec<u8>) -> &mut Self {
            self.read_replies.push_back(ReadReply { code, data });
            self
        }

        /// 预设若干条相同的响应码（用于忙重试的场景）
        pub fn push_command_replies(&mut self, codes: &[u16]) -> &mut Self {
            for c in codes {
                self.command_replies.push_back(*c);
            }
            self
        }
    }

    impl Default for FakeTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    impl PtpTransport for FakeTransport {
        fn send_command(&mut self, code: u16, args: &[u32]) -> Result<u16> {
            self.calls.push(Call::Command {
                code,
                args: args.to_vec(),
            });
            Ok(self.command_replies.pop_front().unwrap_or(crate::mtp::PTP_RC_OK))
        }

        fn send_write_command(&mut self, code: u16, args: &[u32], data: &[u8]) -> Result<u16> {
            self.calls.push(Call::Write {
                code,
                args: args.to_vec(),
                data: data.to_vec(),
            });
            Ok(self.command_replies.pop_front().unwrap_or(crate::mtp::PTP_RC_OK))
        }

        fn send_read_command(&mut self, code: u16, args: &[u32]) -> Result<(u16, Vec<u8>)> {
            self.calls.push(Call::Read {
                code,
                args: args.to_vec(),
            });
            match self.read_replies.pop_front() {
                Some(r) => Ok((r.code, r.data)),
                None => Ok((crate::mtp::PTP_RC_OK, Vec::new())),
            }
        }

        fn transaction(&self) -> u32 {
            self.transaction
        }

        fn set_transaction(&mut self, t: u32) {
            self.transaction = t;
        }
    }
}
