//! `sony-usb`：与真实相机通信的传输层。
//!
//! # 为什么不能直接用 USB
//!
//! 相机插上 Windows 后会被自动装上 MTP 驱动并**独占**。直接抓 USB 端点
//! （libusb 那套）会拿到"拒绝访问"，因为接口已经被系统驱动拿走了。
//! 所以必须走 Windows 提供的 MTP 通道 —— 也就是 WPD。
//!
//! 这也是原项目在 Windows 上最终采用的方案（`pmca/usb/driver/windows/wpd.py`）。
//!
//! # 相机有两种模式
//!
//! | 模式 | 相机菜单 | 能干什么 |
//! |---|---|---|
//! | **MTP（普通）** | USB 连接 → MTP | 只能传照片 |
//! | **应用安装模式** | 需在相机上专门切换 | 能装应用（我们要的） |
//!
//! 判定依据：设备信息里的厂商扩展字符串是否为 `sony.net/SEN_PRXY_MSG:`，
//! 以及是否支持代理消息操作码（0x9488/9/C/D）。见 [`CameraMode`]。

// 本 crate 是一层薄的 COM FFI 绑定：函数体里几乎每一行都在解引用原始指针。
// Rust 2024 默认要求 `unsafe fn` 内部再写 `unsafe {}` 块 —— 在这里那只是纯噪音，
// 而且会让"哪些是真正的边界"变得看不出来。
// 因此统一关掉这条 lint；所有不安全操作都集中在 `com.rs` 这一个文件里，便于审查。
#![allow(unsafe_op_in_unsafe_fn)]

use anyhow::Result;

pub mod ffi;
pub mod usbdev;
pub mod wpd;

pub use wpd::{WpdDevice, WpdTransport};

/// 索尼的 USB 厂商号
pub const SONY_VENDOR_ID: u16 = 0x054C;

/// 相机所处的模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraMode {
    /// 应用安装模式（我们要的）
    AppInstall,
    /// 普通 MTP 模式（只能传照片）
    PlainMtp,
    /// 无法判断（可能不是相机，或驱动异常）
    Unknown,
}

impl CameraMode {
    /// 用厂商扩展字符串与支持的操作码判断模式
    pub fn detect(vendor_extension: &str, supports_proxy_ops: bool) -> Self {
        if vendor_extension.contains("sony.net/SEN_PRXY_MSG:") || supports_proxy_ops {
            Self::AppInstall
        } else if vendor_extension.is_empty() {
            Self::PlainMtp
        } else {
            Self::Unknown
        }
    }

    /// 给用户看的中文说明
    pub fn description(self) -> &'static str {
        match self {
            Self::AppInstall => "应用安装模式（可以安装应用）",
            Self::PlainMtp => "普通 MTP 模式（只能传照片，不能装应用）",
            Self::Unknown => "无法识别的模式",
        }
    }

    /// 若不是安装模式，告诉用户该怎么办
    pub fn how_to_fix(self) -> Option<&'static str> {
        match self {
            Self::AppInstall => None,
            Self::PlainMtp => Some(
                "请把相机的 USB 连接方式改成「应用安装」模式：\n\
                 相机菜单 → 设置 → USB 连接 → 选择与「应用」相关的选项，\n\
                 然后拔插一次 USB 线。",
            ),
            Self::Unknown => Some(
                "无法确认相机模式。请确认相机已开机、USB 线已插好，\
                 且在相机菜单里选择了「应用安装」相关的 USB 连接方式。",
            ),
        }
    }
}

/// 列出所有索尼相机（过滤掉其他 WPD 设备）
pub fn list_sony_devices() -> Result<Vec<WpdDevice>> {
    let all = wpd::list_devices()?;
    Ok(all
        .into_iter()
        .filter(|d| d.vendor_id == Some(SONY_VENDOR_ID))
        .collect())
}

/// 查找第一台索尼相机
pub fn find_sony_camera() -> Result<Option<WpdDevice>> {
    Ok(list_sony_devices()?.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 厂商扩展字符串能正确判定安装模式
    #[test]
    fn detects_app_install_mode() {
        assert_eq!(
            CameraMode::detect("sony.net/SEN_PRXY_MSG:", false),
            CameraMode::AppInstall
        );
        // 即使扩展字符串为空，只要支持代理操作码也算安装模式
        assert_eq!(CameraMode::detect("", true), CameraMode::AppInstall);
    }

    /// 空扩展字符串 + 不支持代理操作码 = 普通 MTP
    #[test]
    fn detects_plain_mtp_mode() {
        assert_eq!(CameraMode::detect("", false), CameraMode::PlainMtp);
    }

    /// 有扩展字符串但不是我们认识的那个 = 未知
    #[test]
    fn detects_unknown_mode() {
        assert_eq!(
            CameraMode::detect("sony.net/SOMETHING_ELSE:", false),
            CameraMode::Unknown
        );
    }

    /// 非安装模式必须给出可操作的建议
    #[test]
    fn non_install_modes_have_instructions() {
        assert!(CameraMode::AppInstall.how_to_fix().is_none());
        let msg = CameraMode::PlainMtp.how_to_fix().unwrap();
        assert!(msg.contains("应用安装"), "建议里应提到要切模式：{msg}");
        assert!(CameraMode::Unknown.how_to_fix().is_some());
    }

    /// 描述文本是中文且不为空
    #[test]
    fn descriptions_are_present() {
        for m in [
            CameraMode::AppInstall,
            CameraMode::PlainMtp,
            CameraMode::Unknown,
        ] {
            assert!(!m.description().is_empty());
        }
    }

    /// 索尼厂商号
    #[test]
    fn sony_vendor_id() {
        assert_eq!(SONY_VENDOR_ID, 0x054C);
    }
}
