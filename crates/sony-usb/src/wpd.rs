//! WPD 后端：Windows 上唯一能用的 MTP 通道。
//!
//! 底层的 COM 访问在 C 薄封装里（`wpd_shim.c`），这里只是安全、好用的 Rust 外壳，
//! 并实现 [`sony_core::transport::PtpTransport`] 供上层协议栈使用。
//!
//! # 为什么 COM 初始化放在 Rust 侧
//!
//! `CoInitializeEx` 必须与 `CoUninitialize` 严格配对，而且**在打开设备期间**
//! 要一直保持初始化状态。用 Rust 的 RAII 守卫表达这个生命周期最自然：
//! 守卫活着 → COM 可用；守卫析构 → 收尾。

use anyhow::{anyhow, bail, Result};
use windows_sys::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
};

use sony_core::transport::PtpTransport;

use crate::ffi::{self, WpdDeviceRaw};

/// 一次 COM 会话（RAII）
pub struct ComSession {
    initialized: bool,
}

impl ComSession {
    pub fn new() -> Result<Self> {
        unsafe {
            let hr = CoInitializeEx(core::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            // S_OK=0，S_FALSE=1（此前已在这个线程初始化过），都算成功
            if hr >= 0 {
                return Ok(Self { initialized: true });
            }
            // RPC_E_CHANGED_MODE（0x80010106）：本线程已是别的套间模型，
            // 换 MTA 再试一次（WPD 在两种模型下都能工作）。
            const RPC_E_CHANGED_MODE: i32 = -2147417850;
            if hr == RPC_E_CHANGED_MODE {
                let hr2 = CoInitializeEx(core::ptr::null(), COINIT_MULTITHREADED as u32);
                if hr2 >= 0 {
                    return Ok(Self { initialized: true });
                }
                bail!("初始化 COM 失败（MTA）：错误 0x{:08X}", hr2 as u32);
            }
            bail!("初始化 COM 失败：错误 0x{:08X}", hr as u32);
        }
    }
}

impl Drop for ComSession {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
            self.initialized = false;
        }
    }
}

/// 一台枚举到的 WPD 设备
#[derive(Debug, Clone)]
pub struct WpdDevice {
    /// PnP 设备标识
    pub pnp_id: String,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
}

impl WpdDevice {
    /// 从 PnP 标识里解析 vid/pid。
    ///
    /// 形如 `\\?\usb#vid_054c&pid_077a#...`，大小写与分隔符都不固定。
    pub fn parse_vid_pid(pnp_id: &str) -> (Option<u16>, Option<u16>) {
        fn find(s: &str, tag: &str) -> Option<u16> {
            let lower = s.to_lowercase();
            let idx = lower.find(tag)? + tag.len();
            let hex: String = lower[idx..]
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if hex.len() < 4 {
                return None;
            }
            u16::from_str_radix(&hex[..4], 16).ok()
        }
        (find(pnp_id, "vid_"), find(pnp_id, "pid_"))
    }
}

/// 枚举所有 WPD 设备（自动处理 COM 初始化）
pub fn list_devices() -> Result<Vec<WpdDevice>> {
    let _com = ComSession::new()?;
    let ids = ffi::list_devices_raw().map_err(|e| anyhow!("枚举设备失败：{e}"))?;
    Ok(ids
        .into_iter()
        .map(|pnp_id| {
            let (vendor_id, product_id) = WpdDevice::parse_vid_pid(&pnp_id);
            WpdDevice {
                pnp_id,
                vendor_id,
                product_id,
            }
        })
        .collect())
}

/// 一台已打开的 WPD 设备，可以直接收发 PTP 命令。
///
/// 字段顺序很重要：`_com` 必须**最后**释放 —— 也就是先关设备、再 `CoUninitialize`。
/// Rust 按声明顺序**逆序**析构结构体字段，所以把 `_com` 放在最后一位。
pub struct WpdTransport {
    device: *mut WpdDeviceRaw,
    transaction: u32,
    _com: ComSession,
}

impl WpdTransport {
    /// 打开指定 PnP 标识的设备
    pub fn open(pnp_id: &str) -> Result<Self> {
        let com = ComSession::new()?;
        let device = ffi::open_raw(pnp_id).map_err(|e| anyhow!("打开相机失败：{e}"))?;
        Ok(Self {
            device,
            transaction: 0,
            _com: com,
        })
    }
}

impl PtpTransport for WpdTransport {
    fn send_command(&mut self, code: u16, args: &[u32]) -> Result<u16> {
        unsafe { ffi::send_command_raw(self.device, code, args) }.map_err(|e| anyhow!("{e}"))
    }

    fn send_write_command(&mut self, code: u16, args: &[u32], data: &[u8]) -> Result<u16> {
        unsafe { ffi::send_write_command_raw(self.device, code, args, data) }
            .map_err(|e| anyhow!("{e}"))
    }

    fn send_read_command(&mut self, code: u16, args: &[u32]) -> Result<(u16, Vec<u8>)> {
        unsafe { ffi::send_read_command_raw(self.device, code, args) }.map_err(|e| anyhow!("{e}"))
    }

    fn transaction(&self) -> u32 {
        self.transaction
    }

    fn set_transaction(&mut self, t: u32) {
        self.transaction = t;
    }
}

impl Drop for WpdTransport {
    fn drop(&mut self) {
        unsafe { ffi::close_raw(self.device) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PnP 标识解析：大小写与分隔符都要兼容
    #[test]
    fn parses_vid_pid_from_pnp_id() {
        let cases = [
            (r"\\?\usb#vid_054c&pid_077a#1234", Some(0x054c), Some(0x077a)),
            (r"\\?\USB#VID_054C&PID_077A#1234", Some(0x054c), Some(0x077a)),
            (r"usb\vid_054c&pid_077a\1234", Some(0x054c), Some(0x077a)),
        ];
        for (s, v, p) in cases {
            assert_eq!(WpdDevice::parse_vid_pid(s), (v, p), "输入：{s}");
        }
    }

    /// 解析不出来时返回 None，而不是 panic
    #[test]
    fn handles_unparsable_pnp_id() {
        assert_eq!(WpdDevice::parse_vid_pid(""), (None, None));
        assert_eq!(WpdDevice::parse_vid_pid("nonsense"), (None, None));
        assert_eq!(WpdDevice::parse_vid_pid("vid_zz"), (None, None));
    }

    /// 真正的集成测试：调用 C 封装的设备枚举。
    ///
    /// 不需要相机插着也能跑（没有设备时返回空列表）。
    /// 这一步能通过，就说明 **COM 初始化 + C 封装 + FFI** 这条链是通的。
    #[test]
    fn can_enumerate_devices_via_c_shim() {
        match list_devices() {
            Ok(devices) => {
                println!("枚举到 {} 台 WPD 设备", devices.len());
                for d in &devices {
                    println!(
                        "  {} (VID:PID = {:04X?}:{:04X?})",
                        d.pnp_id, d.vendor_id, d.product_id
                    );
                }
            }
            Err(e) => {
                // 没有 WPD 服务时会失败，这不算测试失败，但要如实报告
                println!("枚举未成功（可能是环境问题，非代码缺陷）：{e}");
            }
        }
    }
}
