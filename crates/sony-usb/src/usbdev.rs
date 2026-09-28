//! 用 SetupAPI 枚举**所有**索尼 USB 设备（不只是 WPD 那一类）。
//!
//! # 为什么需要这个
//!
//! 我们的传输层走的是 Windows 的 **WPD**（便携设备）接口。
//! 但索尼相机的 USB 连接方式是可以改的：
//!
//! | 相机里的设置 | 出现在 Windows 里的样子 | 我们的 WPD 能枚举到吗 |
//! |---|---|---|
//! | MTP | WPD 设备 | ✅ 能 |
//! | 应用安装模式 | WPD 设备 | ✅ 能 |
//! | **海量存储器** | **U 盘（磁盘类）** | ❌ **不能** |
//! | 电脑遥控 | 另一种设备类 | ❌ 不能 |
//!
//! 所以在"海量存储器"模式下，用户会看到"没有找到索尼相机" ——
//! 这个提示**方向完全错了**，会让用户去检查线材、开机状态，
//! 而真正的原因只是相机菜单里的一个选项。
//!
//! 这个模块的作用就是：WPD 找不到相机时，再问一次系统
//! "有没有索尼的 USB 设备？"。
//! 如果有，就说明**相机插着、只是模式不对** —— 可以给出准确的提示。
//!
//! # 为什么不用 WPD 的 API
//!
//! WPD 只枚举"便携设备"这一类，海量存储器属于磁盘类，它看不见。
//! 所以要回到更底层：直接遍历 USB 设备节点，读它的硬件 ID
//! （形如 `USB\VID_054C&PID_077A`）。

#![cfg(windows)]

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_ALLCLASSES, DIGCF_PRESENT, SP_DEVINFO_DATA, SPDRP_HARDWAREID,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceRegistryPropertyW,
};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HWND};

/// 「USB」这个设备**枚举器**的名字（宽字符，NUL 结尾）。
///
/// # 为什么按枚举器找，而不是按设备类找
///
/// 我一开始按设备类 `GUID_DEVCLASS_USB` 找，结果**一台都找不到**
/// （真机上验证过 —— 相机明明插着，枚举结果是空的）。
///
/// 原因是：相机在 WPD 模式下，设备节点虽然叫
/// `USB\VID_054C&PID_077A\...`，但它的**安装类是「WPD」而不是「USB」**，
/// 按 USB 类去问自然问不到。
///
/// 按**枚举器**找就与类无关了：`DIGCF_ALLCLASSES` + `"USB"`
/// 能列出所有实例路径以 `USB\` 开头的设备节点，
/// 不管它是被 WPD、磁盘、还是别的驱动接管的。
const USB_ENUMERATOR: [u16; 4] = [b'U' as u16, b'S' as u16, b'B' as u16, 0];

/// 一台 USB 设备的标识
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbId {
    pub vendor: u16,
    pub product: u16,
}

/// 列出所有属于指定厂商的 USB 设备
///
/// 返回去重后的 (厂商号, 产品号) 列表。
/// 枚举失败时返回空表 —— 这只是"帮忙给个更好的提示"，
/// 不该因为它失败而把主流程搞挂。
pub fn list_usb_ids_of_vendor(vendor: u16) -> Vec<UsbId> {
    let mut out = Vec::new();

    // SAFETY: 全部是标准的 SetupAPI 调用序列：
    // 拿设备信息集 → 逐个枚举 → 读属性 → 最后销毁。
    // 所有返回的句柄都在下面被正确释放。
    unsafe {
        let set = SetupDiGetClassDevsW(
            std::ptr::null(), // 不用类 GUID，改用下面的枚举器
            USB_ENUMERATOR.as_ptr(),
            std::ptr::null_mut::<core::ffi::c_void>() as HWND,
            DIGCF_ALLCLASSES | DIGCF_PRESENT,
        );
        // SetupAPI 的句柄是按 isize 定义的，INVALID_HANDLE_VALUE = -1
        if set == -1 || set == 0 {
            return out;
        }

        let mut index = 0u32;
        loop {
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;

            if SetupDiEnumDeviceInfo(set, index, &mut info) == 0 {
                break; // 枚举完了（或出错），两种情况都停
            }
            index += 1;

            if let Some(id) = read_hardware_id(set, &mut info)
                && id.vendor == vendor
                && !out.contains(&id)
            {
                out.push(id);
            }
        }

        SetupDiDestroyDeviceInfoList(set);
    }

    out
}

/// 读一台设备的硬件 ID，并从中解析出 VID/PID
///
/// 硬件 ID 形如 `USB\VID_054C&PID_077A&REV_0100`。
unsafe fn read_hardware_id(
    set: windows_sys::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    info: &mut SP_DEVINFO_DATA,
) -> Option<UsbId> {
    let mut buf = [0u8; 512];
    let mut needed = 0u32;
    let mut prop_type = 0u32;

    // SAFETY: 调用方保证 set/info 有效；缓冲区是我们自己的栈数组，
    // 长度也如实传进去了。
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            info,
            SPDRP_HARDWAREID,
            &mut prop_type,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut needed,
        )
    };

    if ok == 0 {
        // 缓冲区不够大也无所谓：我们只要第一个硬件 ID，
        // 而 ERROR_INSUFFICIENT_BUFFER 时缓冲区里已经有内容了。
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        if err != ERROR_INSUFFICIENT_BUFFER && err != 0 {
            return None;
        }
        if needed == 0 {
            return None;
        }
    }

    // 是 REG_MULTI_SZ：多个字符串，双 NUL 结尾。取第一个。
    let wide: Vec<u16> = buf
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|c| *c != 0)
        .collect();
    let first = String::from_utf16_lossy(&wide);
    parse_vid_pid(&first)
}

/// 从硬件 ID 里解析 VID/PID
///
/// 输入形如 `USB\VID_054C&PID_077A&REV_0100`（大小写都可能）。
pub fn parse_vid_pid(hardware_id: &str) -> Option<UsbId> {
    let upper = hardware_id.to_ascii_uppercase();
    let vid_at = upper.find("VID_")? + 4;
    let pid_at = upper.find("PID_")? + 4;
    let vendor = u16::from_str_radix(upper.get(vid_at..vid_at + 4)?, 16).ok()?;
    let product = u16::from_str_radix(upper.get(pid_at..pid_at + 4)?, 16).ok()?;
    Some(UsbId { vendor, product })
}

/// 已知的索尼相机 PID → 模式说明。
///
/// 只列我们**实际见过**的。没见过的型号不会因此被拒绝 ——
/// 只是提示语里说不出具体是哪种模式而已。
fn mode_hint(product: u16) -> Option<&'static str> {
    match product {
        0x077A => Some("普通 MTP 模式"),
        0x06A8 => Some("应用安装模式"),
        _ => None,
    }
}

/// 索尼设备"在系统里是什么样子"的分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// 一台索尼 USB 设备都没有
    None,
    /// 找到了索尼设备，但它不是 MTP 模式（典型：海量存储器）
    WrongMode,
    /// 找到了 MTP 模式的索尼设备，却没被 WPD 列出来
    /// （说明问题在 Windows 的驱动/服务，不是相机设置）
    NotVisible,
}

/// 把「枚举到的索尼 USB 设备」归类
///
/// 这一步决定了给用户的提示方向 —— 归错了就会把用户带到错误的地方：
/// 明明是相机菜单里一个选项不对，却让人去拔插线材、换 USB 口。
pub fn classify(ids: &[UsbId]) -> Presence {
    if ids.is_empty() {
        return Presence::None;
    }
    // 有"本来就该被 WPD 看到"的设备（普通 MTP / 应用安装模式），
    // 却还是没被列出来 → 问题在 Windows 那一侧
    if ids.iter().any(|i| mode_hint(i.product).is_some()) {
        Presence::NotVisible
    } else {
        Presence::WrongMode
    }
}

/// 「USB 连接方式不对」的提示文字。
///
/// 只说该怎么做，不解释原理 —— 用户要的是"我该按哪里"，
/// 不是"为什么海量存储器不行"。
pub const WRONG_MODE_MESSAGE: &str = "请把相机的 USB 连接方式改成「MTP」。\n\
     \n\
     做法（在相机上改一次就行）：\n\
     菜单 → 设置 → USB → USB 连接 → 选「MTP」\n\
     \n\
     改完把 USB 线拔下来再插上，然后重新点「开始安装」。";

/// 「MTP 设备存在、但 WPD 看不到」的提示文字
pub fn not_visible_message(ids: &[UsbId]) -> String {
    let detail = ids
        .iter()
        .map(|id| format!("054C:{:04X}", id.product))
        .collect::<Vec<_>>()
        .join("、");
    format!(
        "相机插着、也是 MTP 模式，但 Windows 的「便携设备」列表里没有它。\n\
         设备：{detail}\n\
         \n\
         这说明问题不在相机设置，而在 Windows 这一侧。可以按顺序试：\n\
         ① 把相机 USB 线拔下再插上（最常见，往往一次就好）\n\
         ② 换一个 USB 口，别用 USB 集线器\n\
         ③ 关掉可能占用相机的程序（照片应用、资源管理器预览、原版 PMCA）\n\
         ④ 重启电脑（便携设备服务偶发出问题时需要）"
    )
}

/// 按分类生成提示文字
pub fn message_for(ids: &[UsbId]) -> Option<String> {
    match classify(ids) {
        Presence::None => None,
        Presence::WrongMode => Some(WRONG_MODE_MESSAGE.to_string()),
        Presence::NotVisible => Some(not_visible_message(ids)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hardware_ids() {
        assert_eq!(
            parse_vid_pid(r"USB\VID_054C&PID_077A&REV_0100"),
            Some(UsbId {
                vendor: 0x054C,
                product: 0x077A
            })
        );
        // 小写也要能认
        assert_eq!(
            parse_vid_pid(r"usb\vid_054c&pid_06a8"),
            Some(UsbId {
                vendor: 0x054C,
                product: 0x06A8
            })
        );
        // 不是 USB 硬件 ID
        assert_eq!(parse_vid_pid(r"SCSI\Disk"), None);
        assert_eq!(parse_vid_pid(""), None);
        // 长度不够不能 panic
        assert_eq!(parse_vid_pid("VID_05"), None);
    }

    /// 设备是"未知 PID"（也就是非 MTP 模式，典型就是海量存储器）
    /// → 归类为 WrongMode，提示只需要教用户**怎么改成 MTP**
    #[test]
    fn unknown_pid_is_wrong_mode() {
        let ids = [UsbId {
            vendor: 0x054C,
            product: 0x0AAA, // 编一个：既不是 MTP 也不是应用安装模式
        }];
        assert_eq!(classify(&ids), Presence::WrongMode);
        let msg = message_for(&ids).expect("这种分类必须给出提示");
        // 要教清楚怎么改
        assert!(msg.contains("MTP"), "必须给出改法：{msg}");
        assert!(msg.contains("菜单"), "要说清楚是在相机菜单里改");
        assert!(msg.contains("USB"), "要指明是 USB 连接这一项");
        // 不要解释原理，也不要露设备号 —— 用户要的是"按哪里"
        assert!(!msg.contains("海量存储器"), "不用解释为什么不行：{msg}");
        assert!(!msg.contains("0AAA"), "不该把设备号摆给用户看：{msg}");
        assert!(!msg.contains("U 盘"), "不用解释原理：{msg}");
        // 用户要求：只推荐 MTP，不要再给"自动"这个第二选项
        assert!(!msg.contains("自动"), "不要给第二个选项：{msg}");
    }

    /// 设备**本来就是 MTP 模式**却没被 WPD 列出来
    /// → 问题在 Windows 这一侧，不该让用户去改相机设置
    #[test]
    fn known_mtp_pid_points_at_windows_side() {
        let ids = [UsbId {
            vendor: 0x054C,
            product: 0x077A, // 已知的普通 MTP 模式
        }];
        assert_eq!(classify(&ids), Presence::NotVisible);
        let msg = message_for(&ids).expect("这种分类必须给出提示");
        assert!(
            msg.contains("便携设备"),
            "要说明是 Windows 没认出来：{msg}"
        );
        assert!(msg.contains("拔下再插上"), "要给出拔插的建议");
        // 这种情况不该让用户去改相机的 USB 模式 —— 方向就错了
        assert!(
            !msg.contains("海量存储器"),
            "MTP 模式下不该提海量存储器：{msg}"
        );
    }

    /// 什么都没插 → 归类为 None，不该硬凑一条提示出来
    #[test]
    fn nothing_plugged_in_gives_no_message() {
        assert_eq!(classify(&[]), Presence::None);
        assert!(message_for(&[]).is_none());
    }

    /// 什么都没插 → 分类是 None，不该编出一条提示
    #[test]
    fn no_device_gives_no_message() {
        assert_eq!(classify(&[]), Presence::None);
        assert!(message_for(&[]).is_none(), "没插就不该硬凑提示");
    }

    /// 提示里不能再出现"自动"这个可选值（用户要求只推荐 MTP）
    #[test]
    fn wrong_mode_message_only_mentions_mtp() {
        assert!(WRONG_MODE_MESSAGE.contains("MTP"));
        assert!(
            !WRONG_MODE_MESSAGE.contains("自动"),
            "只教用户改成 MTP，不要给第二个选项"
        );
    }

    /// 真机集成测试：插着相机时应该能枚举到索尼 USB 设备。
    /// 没插相机也不算失败（只是跳过）。
    #[test]
    fn can_enumerate_sony_usb_devices() {
        let ids = list_usb_ids_of_vendor(0x054C);
        if ids.is_empty() {
            println!("没枚举到索尼 USB 设备（可能没插相机），跳过");
            return;
        }
        for id in &ids {
            println!("  {:04X}:{:04X}", id.vendor, id.product);
        }
        assert!(ids.iter().all(|i| i.vendor == 0x054C));
    }
}
