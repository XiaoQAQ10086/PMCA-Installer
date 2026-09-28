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

/// 给"WPD 找不到相机、但系统里确实有索尼 USB 设备"这种情况生成提示。
///
/// 这里要分清两种**完全不同**的原因，否则用户会被带到错误的方向：
///
/// 1. 设备是**别的模式**（海量存储器 / 电脑遥控）
///    → 相机在 Windows 里根本不是"便携设备"，改相机菜单里的 USB 设置就行
/// 2. 设备是 **MTP 模式**（本来就该被 WPD 看到），却还是没被列出来
///    → 这不是相机设置问题，多半是驱动/服务的问题，拔插或重启
pub fn describe_present_but_not_wpd(ids: &[UsbId]) -> String {
    let detail = ids
        .iter()
        .map(|id| match mode_hint(id.product) {
            Some(hint) => format!("054C:{:04X}（{hint}）", id.product),
            None => format!("054C:{:04X}（不是便携设备模式）", id.product),
        })
        .collect::<Vec<_>>()
        .join("、");

    // 只要有一个"本来就该被 WPD 看到"的设备，就说明问题出在驱动/服务那一侧
    let should_have_been_visible = ids.iter().any(|i| mode_hint(i.product).is_some());

    if should_have_been_visible {
        return format!(
            "相机插着、也是 MTP 模式，但 Windows 的「便携设备」列表里没有它。\n\
             设备：{detail}\n\
             \n\
             这说明问题不在相机设置，而在 Windows 这一侧。可以按顺序试：\n\
             ① 把相机 USB 线拔下再插上（最常见，往往一次就好）\n\
             ② 换一个 USB 口，别用 USB 集线器\n\
             ③ 关掉可能占用相机的程序（照片应用、资源管理器预览、原版 PMCA）\n\
             ④ 重启电脑（便携设备服务偶发出问题时需要）"
        );
    }

    format!(
        "检测到了索尼 USB 设备，但它没有以「便携设备（MTP）」的形式出现。\n\
         设备：{detail}\n\
         \n\
         最常见的原因是相机菜单里的 **USB 连接方式设成了「海量存储器」**。\n\
         海量存储器模式下，相机在 Windows 里就是个 U 盘，装应用用的通道根本不存在。\n\
         \n\
         改法（在相机上操作，只需改一次）：\n\
         菜单 → 设置 → USB → USB 连接 → 改成「MTP」或「自动」\n\
         改完把 USB 线拔下再插上，然后重试。\n\
         \n\
         （设成「电脑遥控」也一样装不了，改成 MTP 即可。）"
    )
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
    /// → 提示应该指向相机菜单里的 USB 设置
    #[test]
    fn unknown_pid_points_at_camera_usb_setting() {
        let msg = describe_present_but_not_wpd(&[UsbId {
            vendor: 0x054C,
            product: 0x0AAA, // 编一个：既不是 MTP 也不是应用安装模式
        }]);
        assert!(msg.contains("海量存储器"), "要说清楚是这个原因：{msg}");
        assert!(msg.contains("054C:0AAA"), "要列出实际看到的设备");
        assert!(msg.contains("MTP"), "要给出改法");
        assert!(msg.contains("菜单"), "要说明是去相机菜单里改");
    }

    /// 设备**本来就是 MTP 模式**却没被 WPD 列出来
    /// → 问题在 Windows 这一侧，不该让用户去改相机设置
    #[test]
    fn known_mtp_pid_points_at_windows_side() {
        let msg = describe_present_but_not_wpd(&[UsbId {
            vendor: 0x054C,
            product: 0x077A, // 已知的普通 MTP 模式
        }]);
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
