//! 海量存储器通道：通过 SCSI 直通发索尼私有命令。
//!
//! # 为什么需要这条路
//!
//! 索尼相机切到「应用安装模式」有**两种**触发方式，取决于机型：
//!
//! | 机型 | 相机该怎么连 | 谁来触发切换 |
//! |---|---|---|
//! | 较新（α6300 等） | MTP | 用 MTP 的扩展命令（`0x9280`） |
//! | 较老（**α6000** 等） | **海量存储器** | 用磁盘的 SCSI 直通命令 |
//!
//! 老机型在 MTP 模式下**根本不报告**那些扩展命令，所以在 MTP 那条路上
//! 无论怎么试都没用 —— 真机反馈里那句"这台相机不支持安装应用"就是这么来的，
//! 而 α6000 其实完全能装（官方机型表标着 apps 2.4）。
//!
//! # 怎么认出相机
//!
//! 对磁盘发标准的 SCSI `INQUIRY`，索尼相机在「海量存储器」模式下会回
//! 厂商 `Sony`、型号 `DSC`（摄像机是 `Camcorder`）。原项目就是按这个认的
//! （`SONY_MSC_MODELS`），比按 VID/PID 猜可靠 —— 同一个相机换模式就换 PID。
//!
//! # 两个真机摸出来的坑
//!
//! 1. **必须用「读写」方式打开卷。** 只读打开能成功，但一发 SCSI 直通
//!    就回"拒绝访问"（Win32 错误 5）。见 [`open_volume`]。
//! 2. **相机的两个分区只有一个认这条命令。** α6300 上 SD 卡分区（F:）
//!    接受、`PMHOME` 小分区（G:）拒绝。原项目是"取第一个"，
//!    这里改成**逐个试，谁接受用谁**。
//!
//! # 权限
//!
//! 对卷做 SCSI 直通可能要求管理员权限（取决于 Windows 的"可移动介质例外"
//! 是否适用）。这里不假设，而是**让调用方拿到明确的错误**：
//! [`switch_to_app_install_mode`] 会把"拒绝访问"单独识别出来。

use anyhow::{Context, Result, bail};
use core::ffi::c_void;
use std::thread::sleep;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW, GetLogicalDrives, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOVABLE;

/// `IOCTL_SCSI_PASS_THROUGH_DIRECT`
///
/// windows-sys 里没有这个常量和配套结构体，所以在本文件里自己定义。
/// 结构与 `ntddscsi.h` 一致。
const IOCTL_SCSI_PASS_THROUGH_DIRECT: u32 = 0x0004_D014;

/// 数据方向：我们要**写**给相机
const SCSI_IOCTL_DATA_OUT: u8 = 0;
/// 数据方向：我们要**读**（INQUIRY 用）
const SCSI_IOCTL_DATA_IN: u8 = 1;

/// 索尼私有命令的 SCSI 操作码
const MSC_OC_EXT_CMD: u8 = 0x7A;

/// 写方向固定补到多少字节（照原项目，见 `sony.py` 的 `writeBufferSize=0x2000`）
const EXT_CMD_WRITE_SIZE: usize = 0x2000;

/// 「设备忙」的 sense 码 —— 这不是错误，必须重试
const SENSE_DEVICE_BUSY: (u8, u8, u8) = (0x09, 0x81, 0x81);

/// 「设备忙」最多重试几次
///
/// 原项目是个 `while` 死循环，这里加上限：万一相机一直忙，
/// 宁可报错让用户拔插，也不要让界面永远转圈。
const MAX_BUSY_RETRY: usize = 50;

/// 每次重试之间等多久
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(200);

/// 切到应用安装模式用的命令号。
///
/// 对应原项目 `SONY_CMD_ScalarExtCmdPlugIn_NotifyScalarDlmode = (5, 2)`：
/// 第一个数是组号（进 CDB），第二个数进数据头。
const DLMODE_GROUP: u32 = 5;
const DLMODE_CMD: i16 = 2;

/// `SCSI_PASS_THROUGH_DIRECT`（x64 上 56 字节）
#[repr(C)]
#[derive(Clone, Copy)]
struct ScsiPassThroughDirect {
    length: u16,
    scsi_status: u8,
    path_id: u8,
    target_id: u8,
    lun: u8,
    cdb_length: u8,
    sense_info_length: u8,
    data_in: u8,
    data_transfer_length: u32,
    time_out_value: u32,
    data_buffer: *mut c_void,
    sense_info_offset: u32,
    cdb: [u8; 16],
}

/// `SCSI_PASS_THROUGH_DIRECT` 外面再挂上 sense 缓冲区
#[repr(C)]
struct ScsiPassThroughDirectWithBuffer {
    sptd: ScsiPassThroughDirect,
    _filler: u32,
    sense: [u8; SENSE_LEN],
}

const SENSE_LEN: usize = 32;

/// 一次 SCSI 命令的结果
#[derive(Debug, Clone, Copy)]
struct ScsiOutcome {
    /// SCSI 状态：0 表示命令执行成功
    scsi_status: u8,
    /// sense 缓冲区原始内容
    sense: [u8; SENSE_LEN],
}

impl ScsiOutcome {
    /// 解析出 `(sense key, asc, ascq)`。
    ///
    /// ⚠️ 偏移不能凭感觉取。原项目 `parseMscSense` 是：
    /// ```python
    /// buffer[2] & 0xf, buffer[12], buffer[13]
    /// ```
    /// 也就是 **key 在字节 2 的低 4 位**，asc/ascq 在 12/13。
    /// （字节 0 是 0x70 响应码，拿它当 key 就永远对不上。）
    fn sense_tuple(&self) -> (u8, u8, u8) {
        (
            self.sense[2] & 0x0F,
            self.sense[12],
            self.sense[13],
        )
    }

    fn is_ok(&self) -> bool {
        self.scsi_status == 0 && self.sense_tuple() == (0, 0, 0)
    }
}

/// 一台处于「海量存储器」模式的索尼相机
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonyMscDevice {
    /// 卷路径，形如 `\\.\F:`（注意不是 `F:\`）
    pub volume: String,
    /// `INQUIRY` 回来的型号，形如 `DSC`
    pub model: String,
}

impl SonyMscDevice {
    /// 命令相机切换到「应用安装模式」。
    ///
    /// 成功后再枚举设备，它就会变成一个 MTP 的应用安装模式设备，
    /// 后面走普通的 MTP 流程即可。
    pub fn switch_to_app_install_mode(&self) -> Result<()> {
        let mut payload = build_dlmode_payload();
        let cdb = build_ext_cmd_cdb(DLMODE_GROUP);

        let mut last: Option<(u8, u8, u8)> = None;
        for _attempt in 1..=MAX_BUSY_RETRY {
            let outcome = self
                .send_scsi(&cdb, SCSI_IOCTL_DATA_OUT, &mut payload)
                .with_context(|| format!("向 {} 发切换命令失败", self.volume))?;

            if outcome.is_ok() {
                return Ok(());
            }
            let sense = outcome.sense_tuple();
            if sense == SENSE_DEVICE_BUSY {
                last = Some(sense);
                sleep(BUSY_RETRY_DELAY);
                continue;
            }
            // 拒绝访问单独说清楚：这是**权限问题**，不是相机问题。
            // 上层可以据此提示"以管理员身份重试"，而不是让用户去折腾相机。
            if let Some(e) = access_denied_hint() {
                bail!(
                    "向 {} 发命令时被系统拒绝（{e}）。\n\
                     这通常是权限问题，需要以管理员身份运行。\n\
                     SCSI 状态 {:#04x}，sense {:02x} {:02x} {:02x}",
                    self.volume,
                    outcome.scsi_status,
                    sense.0,
                    sense.1,
                    sense.2
                );
            }
            bail!(
                "相机拒绝了切换命令（{}）。\n\
                 SCSI 状态 {:#04x}，sense {:02x} {:02x} {:02x}\n\
                 这个分区可能不是相机的主存储分区，试试另一个。",
                self.volume,
                outcome.scsi_status,
                sense.0,
                sense.1,
                sense.2
            );
        }
        bail!(
            "相机一直回报「设备忙」，重试 {} 次后放弃（最后 sense {:02x?}）。",
            MAX_BUSY_RETRY,
            last
        )
    }

    /// 打开卷并发一条 SCSI 直通命令
    ///
    /// `buffer` 对写方向是"要发的数据"，对读方向是"收数据的缓冲区"，
    /// 所以统一用 `&mut`。
    fn send_scsi(&self, cdb: &[u8], direction: u8, buffer: &mut [u8]) -> Result<ScsiOutcome> {
        let handle = open_volume(&self.volume)?;

        let mut with_buffer = ScsiPassThroughDirectWithBuffer {
            sptd: ScsiPassThroughDirect {
                length: core::mem::size_of::<ScsiPassThroughDirect>() as u16,
                scsi_status: 0,
                path_id: 0,
                target_id: 0,
                lun: 0,
                cdb_length: cdb.len() as u8,
                sense_info_length: SENSE_LEN as u8,
                data_in: direction,
                data_transfer_length: buffer.len() as u32,
                time_out_value: 5,
                data_buffer: buffer.as_mut_ptr().cast::<c_void>(),
                sense_info_offset: core::mem::offset_of!(ScsiPassThroughDirectWithBuffer, sense)
                    as u32,
                cdb: {
                    let mut c = [0u8; 16];
                    c[..cdb.len()].copy_from_slice(cdb);
                    c
                },
            },
            _filler: 0,
            sense: [0u8; SENSE_LEN],
        };

        let mut returned: u32 = 0;
        let in_size = core::mem::size_of::<ScsiPassThroughDirectWithBuffer>() as u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_SCSI_PASS_THROUGH_DIRECT,
                (&raw mut with_buffer).cast::<c_void>(),
                in_size,
                (&raw mut with_buffer).cast::<c_void>(),
                in_size,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        let last_error = unsafe { GetLastError() };
        unsafe { CloseHandle(handle) };

        if ok == 0 {
            // ⚠️ 错误号要在这里读出来再带上，别只留一句"调用失败"。
            //    上层要靠 5（拒绝访问）来区分"权限不够"和"相机不认"。
            bail!("DeviceIoControl 失败（Win32 错误 {last_error}）");
        }

        Ok(ScsiOutcome {
            scsi_status: with_buffer.sptd.scsi_status,
            sense: with_buffer.sense,
        })
    }

    /// 发一条标准 `INQUIRY`，拿厂商和型号
    fn inquiry(&self) -> Result<(String, String)> {
        let cdb = [0x12u8, 0x00, 0x00, 0x00, 36, 0x00];
        let mut buf = [0u8; 36];
        let outcome = self.send_scsi(&cdb, SCSI_IOCTL_DATA_IN, &mut buf)?;
        if !outcome.is_ok() {
            bail!("INQUIRY 被拒绝（SCSI 状态 {:#04x}）", outcome.scsi_status);
        }
        // 响应布局：8 字节头 + 8 字节厂商 + 16 字节型号 + 4 字节版本
        let vendor = ascii_trim(&buf[8..16]);
        let product = ascii_trim(&buf[16..32]);
        Ok((vendor, product))
    }
}

/// 列出一台里所有处于「海量存储器」模式的索尼相机（按可移动磁盘找）
///
/// 通常只会有一个，但同一个相机的**多个分区**会各出现一次，
/// 所以返回的是"候选卷"列表，调用方要逐个试。
pub fn list_sony_msc_devices() -> Vec<SonyMscDevice> {
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return out;
    }

    for i in 0..26u32 {
        if mask & (1u32 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = to_wide(&format!("{letter}:\\"));
        // 只看可移动盘：相机的海量存储器在 Windows 里就是可移动磁盘
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_REMOVABLE {
            continue;
        }

        let device = SonyMscDevice {
            volume: format!("\\\\.\\{letter}:"),
            model: String::new(),
        };
        // INQUIRY 失败很正常（比如没插介质），跳过即可
        let Ok((vendor, product)) = device.inquiry() else {
            continue;
        };
        if is_sony_camera(&vendor, &product) {
            out.push(SonyMscDevice {
                volume: device.volume,
                model: product,
            });
        }
    }
    out
}

/// 是不是索尼相机
///
/// 判据照原项目的 `isSonyMscCamera`：厂商是 `SONY`，型号以 `DSC` 或
/// `Camcorder` 开头。用 INQUIRY 而不是 VID/PID —— 同一个相机换模式就换 PID，
/// 而 INQUIRY 的字符串稳定得多。
fn is_sony_camera(vendor: &str, product: &str) -> bool {
    vendor.eq_ignore_ascii_case("SONY")
        && (product.to_ascii_uppercase().starts_with("DSC")
            || product.to_ascii_uppercase().starts_with("CAMCORDER"))
}

// ---------------------------------------------------------------- 内部工具

/// 组装「切到应用安装模式」要写的数据。
///
/// 结构照原项目：
/// ```text
/// ExtCmdHeader { dataSize: i32 = 0, cmd: i16 = 2, direction: i16 = 0, 保留 8 字节 }
/// ```
/// 后面补零到 8192 字节 —— **必须补**，相机按固定长度收。
fn build_dlmode_payload() -> Vec<u8> {
    let mut payload = vec![0u8; EXT_CMD_WRITE_SIZE];
    payload[0..4].copy_from_slice(&0i32.to_le_bytes()); // dataSize
    payload[4..6].copy_from_slice(&DLMODE_CMD.to_le_bytes());
    payload[6..8].copy_from_slice(&0i16.to_le_bytes()); // direction
    payload
}

/// 组装索尼私有命令的 CDB：`0x7A` + 小端 32 位组号 + 7 个 0（共 12 字节）
fn build_ext_cmd_cdb(group: u32) -> [u8; 12] {
    let mut cdb = [0u8; 12];
    cdb[0] = MSC_OC_EXT_CMD;
    cdb[1..5].copy_from_slice(&group.to_le_bytes());
    cdb
}

/// 打开卷。
///
/// ⚠️ **必须带 `GENERIC_WRITE`。** 只读打开是能成功的，但接下来一发
/// SCSI 直通就会回"拒绝访问"（Win32 5）—— 真机上验过。
fn open_volume(volume: &str) -> Result<HANDLE> {
    let path = to_wide(volume);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        let err = unsafe { GetLastError() };
        if err == 5 {
            bail!(
                "打不开 {volume}：拒绝访问。\n\
                 这需要管理员权限 —— 请以管理员身份重新运行本程序。"
            );
        }
        bail!("打不开 {volume}（Win32 错误 {err}）");
    }
    Ok(handle)
}

/// 刚才那次失败是不是"权限不足"。
///
/// 单看返回值不够（`DeviceIoControl` 只回 0/非 0），所以直接查
/// 线程最后的错误号。
fn access_denied_hint() -> Option<String> {
    let err = unsafe { GetLastError() };
    if err == 5 {
        Some("拒绝访问".to_string())
    } else {
        None
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ascii_trim(bytes: &[u8]) -> String {
    // ⚠️ 除了空格，填充里还可能是 0 字节 —— 一并去掉，
    //    否则型号会带着一串看不见的填充字符，显示和比较都难受。
    String::from_utf8_lossy(bytes)
        .trim_matches(|c: char| c.is_whitespace() || c == '\0')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dlmode_payload_has_the_documented_header() {
        let p = build_dlmode_payload();
        assert_eq!(p.len(), EXT_CMD_WRITE_SIZE, "必须补到 8192 字节");
        assert_eq!(&p[0..4], &[0, 0, 0, 0], "dataSize = 0");
        assert_eq!(&p[4..6], &2i16.to_le_bytes(), "cmd = 2");
        assert_eq!(&p[6..8], &[0, 0], "direction = 0");
        // 头之后必须全是 0 —— 相机按固定长度收，混进垃圾会出问题
        assert!(p[8..].iter().all(|&b| b == 0), "保留区和填充必须为 0");
    }

    #[test]
    fn ext_cmd_cdb_matches_the_reference() {
        let cdb = build_ext_cmd_cdb(DLMODE_GROUP);
        // 原项目：dump8(0x7a) + dump32le(5) + 7*b'\0'
        assert_eq!(cdb[0], 0x7A);
        assert_eq!(&cdb[1..5], &5u32.to_le_bytes());
        assert!(cdb[5..].iter().all(|&b| b == 0), "后面 7 个字节必须为 0");
        assert_eq!(cdb.len(), 12);
    }

    /// ⚠️ sense 码的偏移必须照原项目，不能凭感觉。
    ///
    /// 字节 0 是 0x70（固定格式响应码），把它当 sense key 是常见错误，
    /// 会导致"设备忙"永远识别不出来、死等。
    #[test]
    fn sense_tuple_uses_the_right_offsets() {
        let mut sense = [0u8; SENSE_LEN];
        sense[0] = 0x70; // 响应码，**不是** key
        sense[2] = 0x09; // key（低 4 位有效）
        sense[12] = 0x81; // asc
        sense[13] = 0x81; // ascq
        let o = ScsiOutcome { scsi_status: 2, sense };
        assert_eq!(o.sense_tuple(), SENSE_DEVICE_BUSY, "应当识别成设备忙");
    }

    #[test]
    fn sense_key_masks_off_the_high_bits() {
        let mut sense = [0u8; SENSE_LEN];
        sense[2] = 0xF5; // 高位是"有效"标志等，key 只有低 4 位
        let o = ScsiOutcome { scsi_status: 2, sense };
        assert_eq!(o.sense_tuple().0, 0x5, "key 要取低 4 位");
    }

    #[test]
    fn ok_requires_both_status_and_sense_to_be_clean() {
        let clean = ScsiOutcome { scsi_status: 0, sense: [0u8; SENSE_LEN] };
        assert!(clean.is_ok());

        // 状态 0 但 sense 有内容 —— 不算成功
        let mut s = [0u8; SENSE_LEN];
        s[2] = 1;
        assert!(!ScsiOutcome { scsi_status: 0, sense: s }.is_ok());
        // sense 干净但状态非 0 —— 也不算成功
        assert!(!ScsiOutcome { scsi_status: 2, sense: [0u8; SENSE_LEN] }.is_ok());
    }

    #[test]
    fn recognises_sony_cameras_by_inquiry_strings() {
        assert!(is_sony_camera("Sony", "DSC"));
        assert!(is_sony_camera("SONY", "DSC USB Device"));
        assert!(is_sony_camera("sony", "Camcorder"));
        // 别的 U 盘不该被当成相机
        assert!(!is_sony_camera("Kingston", "DataTraveler"));
        assert!(!is_sony_camera("Sony", "Storage Media"));
        assert!(!is_sony_camera("", ""));
    }

    /// 结构体布局必须与 Windows 的一致，否则 `DeviceIoControl` 会收到垃圾
    #[test]
    fn scsi_struct_layout_matches_the_abi() {
        assert_eq!(
            core::mem::size_of::<ScsiPassThroughDirect>(),
            56,
            "x64 上 SCSI_PASS_THROUGH_DIRECT 是 56 字节"
        );
        assert_eq!(
            core::mem::offset_of!(ScsiPassThroughDirectWithBuffer, sense),
            60,
            "sense 缓冲区在偏移 60"
        );
        assert_eq!(
            core::mem::size_of::<ScsiPassThroughDirectWithBuffer>(),
            96,
            "整体 96 字节"
        );
    }
}
