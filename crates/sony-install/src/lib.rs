//! 把 APK 安装到索尼相机上的**完整流程**（命令行与图形界面共用）。
//!
//! 这一层是"业务编排"：把 `sony-core` 里那些协议零件（代理消息、TLS、XPD、SPK）
//! 和 `sony-usb` 里的传输层串成一条能用的流程。
//!
//! 之所以要单独抽出来：命令行工具和图形界面要用**同一套**逻辑。
//! 界面上点"安装"和命令行敲 `install`，走的是这里同一个函数 ——
//! 这样真机上验证过的东西不会有第二份实现偷偷跑偏。
//!
//! # 完整流程
//!
//! ```text
//! ① 找到相机（WPD 枚举）
//! ② 检查模式；不是"应用安装模式"就命令相机自己切过去
//! ③ 打开相机、建立会话（Start / Hello）
//! ④ 发 XPD 清单，相机接受后开始任务
//! ⑤ 在进程内扮演 HTTPS 服务器，完成 TLS 握手
//! ⑥ 相机上传设备信息 → 我们回"去下载并安装"
//! ⑦ 相机下载 SPK（我们的应用包）→ 安装 → 报告结果
//! ```

use anyhow::{Context, Result};
use sony_core::market::{InstallPhase, InstallRunner, check_result};
use sony_core::proxy::RetryPolicy;
use sony_core::sony;
use sony_usb::CameraMode;
use std::time::{Duration, Instant};

/// 代理消息模式（应用安装模式）才支持的操作码
const PROXY_OPS: [u16; 4] = [0x9488, 0x9489, 0x948C, 0x948D];

/// 安装过程的超时时间。
///
/// 相机要下载整个应用包再安装，慢一点是正常的；但也不能无限等 ——
/// 卡住时用户看到的应该是明确的错误，而不是转不完的圈。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(120);

/// 打开相机 + 握手最多试几次
const MAX_HANDSHAKE_ATTEMPTS: u32 = 6;

/// 每次重试之间等多久（相机重新枚举后需要一点时间稳定）
const HANDSHAKE_RETRY_DELAY: Duration = Duration::from_millis(700);

/// 等相机切换到「应用安装模式」最多等多久。
///
/// 这个值会**出现在给用户看的提示里**（"等待 N 秒"），
/// 所以提示语里用的是 `MODE_SWITCH_TIMEOUT.as_secs()` 而不是写死的数字 ——
/// 以后改这个常量，提示会自动跟着变，不会对不上。
const MODE_SWITCH_TIMEOUT: Duration = Duration::from_secs(25);

/// 相机当前状态
#[derive(Debug, Clone)]
pub struct CameraStatus {
    /// Windows 里的设备路径（形如 `\\?\usb#vid_054c&pid_06a8#...`）
    pub pnp_id: String,
    /// 相机型号，例如 `ILCE-6300`
    pub model: String,
    /// 相机序列号
    pub serial: String,
    /// 是否处于"应用安装模式"
    pub app_install_mode: bool,
}

impl CameraStatus {
    /// 给界面显示的模式文字
    pub fn mode_text(&self) -> &'static str {
        if self.app_install_mode {
            "应用安装模式"
        } else {
            "普通模式（只能传照片）"
        }
    }
}

/// 「检测相机」的结果。
///
/// ⚠️ 为什么不直接返回 `Result<Option<CameraStatus>>`：
/// 界面上需要**分清"没插相机"和"插着但 USB 模式不对"** ——
/// 这两种情况的卡片标题完全不同：
/// - 没插相机         → 「未找到相机」
/// - 模式不对（海量存储器）→ 「请把相机改成 MTP 模式」
///
/// 如果只回一个错误字符串，界面就只能一律显示"未找到相机"，
/// 而这对"模式不对"的用户是**完全错误的指引**。
#[derive(Debug, Clone)]
pub enum CameraProbe {
    /// 找到相机，可以直接用
    Ready(CameraStatus),
    /// 相机插着，但 USB 连接方式不对（最常见：海量存储器）
    WrongUsbMode,
    /// 插着索尼设备，但读不出设备信息（被别的程序占用 / 驱动问题）
    Unreadable(String),
    /// 没插相机
    NotFound,
}

impl CameraProbe {
    /// 这个结果该怎么显示（界面上的状态标题）
    pub fn title(&self) -> &'static str {
        match self {
            CameraProbe::Ready(_) => "已连接",
            CameraProbe::WrongUsbMode => "请把相机改成 MTP 模式",
            CameraProbe::Unreadable(_) => "相机读不出来",
            CameraProbe::NotFound => "未找到相机",
        }
    }

    /// 详细说明（界面上标题下面那行）
    pub fn detail(&self) -> Option<String> {
        match self {
            CameraProbe::Ready(s) => Some(format!(
                "{} · 序列号 {} · 安装时自动切换到应用安装模式",
                s.model, s.serial
            )),
            CameraProbe::WrongUsbMode => {
                Some(sony_usb::usbdev::WRONG_MODE_MESSAGE.to_string())
            }
            CameraProbe::Unreadable(msg) => Some(msg.clone()),
            CameraProbe::NotFound => None,
        }
    }
}

/// 安装过程中往外报告的事件
#[derive(Debug, Clone)]
pub enum Event {
    /// 一句给用户看的进度说明（例如"正在切换到应用安装模式…"）
    Step(String),
    /// 安装进度（0–100）与状态文字
    Progress { percent: u8, text: String },
}

/// 事件回调。用 `&mut dyn FnMut` 而不是泛型，调用方写起来更简单。
pub type Reporter<'a> = &'a mut dyn FnMut(Event);

/// 简单地发一句话
fn step(report: Reporter, text: impl Into<String>) {
    report(Event::Step(text.into()));
}

// ---------------------------------------------------------------- 相机

/// WPD 找不到相机时，生成**准确**的原因说明。
///
/// # 为什么不能只说"没找到相机"
///
/// 索尼相机的 USB 连接方式是可以在菜单里改的。设成「海量存储器」时，
/// 相机在 Windows 里就是个 U 盘 —— 而我们的传输层走的是 WPD（便携设备），
/// 自然一台都看不到。
///
/// 这时候如果说"没有找到索尼相机"，用户会去检查线材、开机状态、USB 口，
/// **方向完全错了**：相机明明插着，只是菜单里一个选项不对。
///
/// 所以这里再问一次系统（用 SetupAPI 按「USB」枚举器找，与设备类无关）：
/// - 找到索尼 USB 设备、且不是 MTP 模式 → 明确说是"海量存储器"的问题
/// - 找到索尼 USB 设备、但本来就是 MTP 模式 → 那是 Windows 驱动/服务的问题
/// - 什么都没找到 → 才是真的没插
fn no_camera_found() -> anyhow::Error {
    let ids = sony_usb::usbdev::list_usb_ids_of_vendor(sony_usb::SONY_VENDOR_ID);
    if ids.is_empty() {
        anyhow::anyhow!(
            "没有找到索尼相机。\n\
             请确认：\n\
             ① 相机已开机、USB 线已插好（要能传数据的线，不是只充电的线）\n\
             ② 相机菜单里的 USB 连接方式不是「海量存储器」\n\
             ③ 没有别的程序占用相机（照片应用、资源管理器预览、原版 PMCA 等）"
        )
    } else {
        // 提示语里只留"该怎么做"，设备号这类排查信息记进诊断日志 ——
        // 用户不需要看到 054C:0AAA 这种号码，但出问题时我们得能查
        sony_core::market::trace_diag(&format!(
            "[设备] WPD 看不到相机，但从 USB 枚举到的索尼设备：{}",
            ids.iter()
                .map(|i| format!("{:04X}:{:04X}", i.vendor, i.product))
                .collect::<Vec<_>>()
                .join("、")
        ));
        anyhow::anyhow!(
            "{}",
            sony_usb::usbdev::message_for(&ids)
                .unwrap_or_else(|| "没有找到索尼相机。".to_string())
        )
    }
}

/// 读一次相机信息（打开会话 + 读设备信息）
///
/// ⚠️ **这里故意不发 `CLOSE_SESSION`**。
///
/// 加上这一句会让后面的代理消息握手稳定报
/// `0x8007001F（连到系统上的设备没有发挥作用）`。原因有两个，都指向"别多此一举"：
/// 1. MTP 的 `CloseSession` 要带**会话号**作为参数，传空参数是畸形请求
/// 2. 设备对象析构时本来就会收尾，显式关会话反而把相机留在中间状态
///
/// 原项目也是**不关**的。
/// 所以这里保持"打开 → 读 → 让 `WpdTransport` 自己析构"。
fn read_info(pnp_id: &str) -> Result<sony_core::mtp::DeviceInfo> {
    use sony_core::mtp;
    use sony_core::transport::PtpTransport;

    let mut t = sony_usb::WpdTransport::open(pnp_id).context("打开相机失败")?;
    let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_OPEN_SESSION, &[1]);
    let (rc, data) = PtpTransport::send_read_command(&mut t, mtp::PTP_OC_GET_DEVICE_INFO, &[])?;
    if rc != mtp::PTP_RC_OK {
        anyhow::bail!("读设备信息失败：响应码 0x{rc:04x}");
    }
    mtp::parse_device_info(&data).context("解析设备信息失败")
}

/// 找相机并读它的信息。**不改变**相机状态。
///
/// - 没插相机 → `Ok(None)`
/// - 插了但读不出来 → `Err`
pub fn probe_camera() -> Result<CameraProbe> {
    let devices = sony_usb::list_sony_devices().context("枚举设备失败")?;
    if devices.is_empty() {
        return Ok(classify_absent_camera());
    }

    // 逐个试：插着多个索尼设备时（手机、随身听…）要挑出真正能当相机用的那台
    let mut last_error = None;
    for dev in devices {
        match read_info(&dev.pnp_id) {
            Ok(info) => {
                // 先算好再移动字段：`info.serial_number` 会被移走，
                // 之后就不能再借用 `info` 了
                let app_install_mode = info.supports_all(&PROXY_OPS);
                return Ok(CameraProbe::Ready(CameraStatus {
                    pnp_id: dev.pnp_id,
                    model: info.model,
                    serial: info.serial_number,
                    app_install_mode,
                }));
            }
            Err(e) => last_error = Some(e),
        }
    }

    let detail = last_error
        .map(|e| format!("{e:#}"))
        .unwrap_or_else(|| "原因未知".to_string());
    Ok(CameraProbe::Unreadable(format!(
        "插着索尼设备，但读不出设备信息。\n\
         请确认没有别的程序占用相机（照片应用、资源管理器预览、原版 PMCA）。\n\
         技术细节：{detail}"
    )))
}

/// WPD 一台都没看到时，用 USB 层再问一次，判断到底是"没插"还是"模式不对"
fn classify_absent_camera() -> CameraProbe {
    let ids = sony_usb::usbdev::list_usb_ids_of_vendor(sony_usb::SONY_VENDOR_ID);
    // 设备号这类排查信息记进诊断日志，不摆到界面上
    if !ids.is_empty() {
        sony_core::market::trace_diag(&format!(
            "[设备] WPD 看不到相机，但从 USB 枚举到的索尼设备：{}",
            ids.iter()
                .map(|i| format!("{:04X}:{:04X}", i.vendor, i.product))
                .collect::<Vec<_>>()
                .join("、")
        ));
    }
    match sony_usb::usbdev::classify(&ids) {
        sony_usb::usbdev::Presence::None => CameraProbe::NotFound,
        sony_usb::usbdev::Presence::WrongMode => CameraProbe::WrongUsbMode,
        sony_usb::usbdev::Presence::NotVisible => {
            CameraProbe::Unreadable(sony_usb::usbdev::not_visible_message(&ids))
        }
    }
}

/// 找相机；如果不在应用安装模式，就**命令相机自己切过去**。
///
/// 为什么要自动切：相机在重新插拔、休眠、安装完成后都会**复位回普通 MTP 模式**。
/// 所以每次安装前都要检查一遍，而不是假定它还停在安装模式。
/// 这样用户就完全不需要去相机菜单里翻设置。
///
/// 判定依据是相机支持的操作码：
/// - 普通 MTP 模式：有 `0x9280/0x9281/0x9282`（索尼扩展命令），没有代理消息操作码
/// - 应用安装模式：有 `0x9488/0x9489/0x948c/0x948d`（代理消息）
pub fn find_and_prepare_camera(report: Reporter) -> Result<CameraStatus> {
    use sony_core::mtp;
    use sony_core::transport::PtpTransport;

    let devices = sony_usb::list_sony_devices().context("枚举设备失败")?;
    if devices.is_empty() {
        return Err(no_camera_found());
    }
    step(report, "找到索尼设备，正在确认哪一台是相机…");

    // ⚠️ 为什么要**逐个试**而不是直接拿第一台：
    //    我们只按"厂商 = 索尼"筛设备。如果用户同时插着索尼手机、随身听、
    //    读卡器之类的东西，第一台未必是相机。
    //    真正的相机会回应「读设备信息」，用这个来确认。
    let mut found: Option<(sony_usb::WpdDevice, mtp::DeviceInfo)> = None;
    let mut last_error: Option<anyhow::Error> = None;
    for dev in devices {
        match read_info(&dev.pnp_id) {
            Ok(info) => {
                found = Some((dev, info));
                break;
            }
            Err(e) => {
                sony_core::market::trace_diag(&format!(
                    "[设备] {} 不是相机或读不出来：{e}",
                    dev.pnp_id
                ));
                last_error = Some(e);
            }
        }
    }

    let Some((dev, info)) = found else {
        let detail = last_error
            .map(|e| format!("\n最后一次尝试的原因：{e:#}"))
            .unwrap_or_default();
        anyhow::bail!(
            "找到了索尼设备，但都不是相机（或者读不出设备信息）。\n\
             请确认插的是相机，并且没有别的程序占用它。{detail}"
        );
    };

    step(report, format!("相机：{}（{}）", info.model, info.serial_number));

    // 已经在应用安装模式 → 直接可用
    if info.supports_all(&PROXY_OPS) {
        return Ok(CameraStatus {
            pnp_id: dev.pnp_id,
            model: info.model,
            serial: info.serial_number,
            app_install_mode: true,
        });
    }

    // ---- 不在安装模式，试着让相机自己切过去 ----
    //
    // 能不能切，取决于相机支不支持索尼的扩展命令。
    // 不支持就说明**这台相机根本装不了应用**（不是操作问题），
    // 这时候要把话说清楚，别让用户以为是哪里插错了。
    if !info.supports_all(&[
        mtp::PTP_OC_SONY_DI_EXT_CMD_WRITE,
        mtp::PTP_OC_SONY_DI_EXT_CMD_READ,
    ]) {
        // ⚠️ 这里**不能说**"这台相机不支持安装应用"。
        //
        // 真机教训：有用户拿 ILCE-6000 来试，看到的就是旧版这句"不支持"，
        // 以为自己的相机买错了。实际上 α6000 完全能装应用
        // （官方机型表标着 apps 2.4，社区还有人给它做了 Doom），
        // 只是它必须用「海量存储器」模式，靠一条磁盘命令切换过去 ——
        // 而本工具目前只认 MTP，看不到那种连接方式。
        //
        // 所以要把两种可能性都说清楚，别冤枉相机，也别给错方向。
        anyhow::bail!(
            "这台相机（{}）没有回应「安装应用」需要的命令。\n\
             \n\
             ⚠️ 但这**不一定**说明它装不了应用。\n\
             \n\
             有一类机型（比如 α6000）需要把相机的 USB 连接方式设成\n\
             「海量存储器」，再由电脑发一条磁盘命令把它切过去。\n\
             那套做法本工具还没实现，所以看不到这类相机。\n\
             \n\
             如果你要装到这类相机上，可以先用原版工具：\n\
             https://github.com/ma1co/Sony-PMCA-RE/releases\n\
             \n\
             另外，采用较新架构的机型（比如 α7 III）确实装不了，\n\
             那种情况怎么设置都不行。\n\
             \n\
             相机报告的操作码：{}",
            info.model,
            info.operations_supported
                .iter()
                .map(|c| format!("0x{c:04x}"))
                .collect::<Vec<_>>()
                .join("、")
        );
    }

    step(report, "正在命令相机切换到「应用安装模式」…（相机会重连一次）");
    {
        let mut t = sony_usb::WpdTransport::open(&dev.pnp_id).context("打开相机失败")?;
        let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_OPEN_SESSION, &[1]);
        // 即使这条命令报错，相机也可能已经切了，所以不直接失败。
        //
        // ⚠️ 这里**不要**补 `CLOSE_SESSION`：MTP 的这个命令要带会话号参数，
        //    传空参数是畸形请求。见 `read_info` 上的说明。
        if let Err(e) = PtpTransport::switch_to_app_install_mode(&mut t) {
            step(report, format!("提示：{e}（相机可能仍会切换，继续等待）"));
        }
    }

    // 等相机重新枚举成"应用安装模式"
    //
    // 顺便记一件事：**相机有没有因为这条命令而重新枚举**。
    // 这个信息能区分两种完全不同的情况：
    // - 重新枚举了，但没进安装模式 → 相机支持，只是这次没成，拔插重试即可
    // - 压根没重新枚举         → 相机不理这条命令，多半是**不支持装应用**
    let old_pnp_id = dev.pnp_id.clone();
    let mut re_enumerated = false;
    let deadline = Instant::now() + MODE_SWITCH_TIMEOUT;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        let devices = match sony_usb::list_sony_devices() {
            Ok(d) => d,
            Err(_) => continue,
        };
        for d in devices {
            if d.pnp_id == old_pnp_id {
                continue; // 还是老设备，还没切完
            }
            re_enumerated = true;
            // 设备换了，确认一下新模式
            std::thread::sleep(Duration::from_millis(400));
            if let Ok(info) = read_info(&d.pnp_id)
                && info.supports_all(&PROXY_OPS)
            {
                step(report, "切换成功");
                // 刚重新枚举出来的设备还需要一点点时间才稳定，
                // 否则紧接着开会话可能拿到 0x8007001F
                std::thread::sleep(Duration::from_millis(600));
                return Ok(CameraStatus {
                    pnp_id: d.pnp_id,
                    model: info.model,
                    serial: info.serial_number,
                    app_install_mode: true,
                });
            }
        }
    }

    if re_enumerated {
        anyhow::bail!(
            "相机切换了模式，但没能进入「应用安装模式」。\n\
             请把相机 USB 线拔下再插上，然后重试。"
        );
    }
    anyhow::bail!(
        "相机没有响应「切换到应用安装模式」的命令（等待 {} 秒）。\n\
         这通常说明**这台相机不支持安装应用**。\n\
         相机型号：{}\n\
         能装应用的机型需要支持 PlayMemories Camera Apps。\n\
         如果你的相机确实支持，请把 USB 线拔下再插上后重试。",
        MODE_SWITCH_TIMEOUT.as_secs(),
        info.model
    )
}

// ---------------------------------------------------------------- 安装

/// 安装成功后的结果
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    /// 相机最后回报的完整设备信息（JSON），里面有已安装应用列表
    pub device_report: Option<String>,
}

/// 把 APK 装到相机上。
///
/// `apk` 是应用包的原始字节。整个过程通过 `report` 往外报进度。
///
/// # 关于"相机拒绝开始任务"
///
/// 如果相机里残留着上一次**未完成**的任务，它会拒绝新的开始
/// （`Start not accepted` / resultCode 10）。
/// 这时候**只能靠拔插 USB 线复位**：我们试过自己发 `/task/complete` 替它收尾，
/// 相机回 `{"message":"Unknown request","resultCode":100}`，不认。
/// 所以这里直接把话说明白，而不是让用户对着"卡住"猜。
pub fn install(apk: &[u8], app_name: &str, report: Reporter) -> Result<InstallOutcome> {
    // 诊断落盘：真机每次拔插复位后**只有一次机会**，
    // 所以关键信息必须写进文件才能事后回看。
    let log_path = "install-diag.log";
    if sony_core::market::diag_init(log_path).is_ok() {
        sony_core::market::trace_diag(&format!(
            "[开始] 应用包 {} 字节，输出名 {app_name}",
            apk.len()
        ));
    }

    if !apk.starts_with(b"PK") {
        step(report, "提示：这个文件开头不是 PK，可能不是 APK 文件");
    }

    // ① 相机就绪
    let status = find_and_prepare_camera(report)?;
    sony_core::market::trace_diag(&format!(
        "[开始] 相机 {}（{}），模式：{}",
        status.model,
        status.serial,
        status.mode_text()
    ));

    // ② 打开并握手
    //
    // ⚠️ 这里**必须能重试**：相机刚切换完模式、USB 连接刚刚重新枚举出来，
    //    有时候还没完全就绪就开会话，会拿到
    //    `0x8007001F（连到系统上的设备没有发挥作用）`。
    //    这不是真故障，等几百毫秒再来一次就好。
    //    没有重试的话，用户看到的就是"刚点安装就报错"，体验很差。
    step(report, "正在打开相机并建立会话…");
    let mut attempt = 0;
    let (mut session, _protocols) = loop {
        attempt += 1;
        match sony_usb::WpdTransport::open(&status.pnp_id) {
            Ok(transport) => {
                let mut s = sony::SonySession::new(transport, RetryPolicy::default());
                match s.handshake() {
                    Ok(p) => break (s, p),
                    Err(e) if attempt < MAX_HANDSHAKE_ATTEMPTS => {
                        sony_core::market::trace_diag(&format!(
                            "[握手] 第 {attempt} 次失败：{e}，稍后重试"
                        ));
                        step(report, format!("相机还没准备好，正在重试（第 {attempt} 次）…"));
                    }
                    Err(e) => return Err(e).context("与相机握手失败"),
                }
            }
            Err(e) if attempt < MAX_HANDSHAKE_ATTEMPTS => {
                sony_core::market::trace_diag(&format!(
                    "[握手] 第 {attempt} 次打开失败：{e}，稍后重试"
                ));
                step(report, format!("相机还没准备好，正在重试（第 {attempt} 次）…"));
            }
            Err(e) => return Err(e).context("打开相机失败"),
        }
        std::thread::sleep(HANDSHAKE_RETRY_DELAY);
    };

    // ③ 编排器负责"该发什么、收到什么怎么答"
    let mut runner = InstallRunner::new(apk.to_vec(), rand_server_random())?;
    // 会话已经完成握手了，编排器直接从运行阶段开始
    runner.mark_hello_done();

    step(report, "正在安装…");
    let deadline = Instant::now() + INSTALL_TIMEOUT;
    let mut last_text = String::new();

    loop {
        if Instant::now() > deadline {
            sony_core::market::trace_diag("[主循环] 超时，任务未完成");
            anyhow::bail!(
                "超时（{} 秒）。相机没有完成任务。\n\
                 详细诊断已写入 {log_path}。",
                INSTALL_TIMEOUT.as_secs()
            );
        }
        if matches!(runner.phase(), InstallPhase::Done | InstallPhase::Failed) {
            break;
        }

        // 把编排器要发的消息发出去
        for m in runner.take_outgoing() {
            sony_core::market::trace_diag(&format!("[主循环] 发送消息，外层类型 {}", m.msg_type));
            session.channel_mut().send(&m)?;
        }

        // 收一条相机的消息（收不到就稍等，NoData 是正常状态）
        match session.channel_mut().receive() {
            Ok(Some(msg)) => {
                sony_core::market::trace_diag(&format!(
                    "[主循环] 收到消息，外层类型 {}，正文 {} 字节",
                    msg.msg_type,
                    msg.payload.len()
                ));
                if let Err(e) = runner.poll(msg) {
                    sony_core::market::trace_diag(&format!("[主循环] 处理消息出错：{e}"));
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                sony_core::market::trace_diag(&format!("[主循环] 读取消息出错：{e}"));
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        // 进度
        //
        // 注意：编排器在收尾时会报一次"安装完成"，但那不是真正的进度值
        // （解析不出百分比）。所以**正在收尾时不再往外报**，
        // 由下面 Done 分支统一报一次 100%，避免出现"完成（0%）"这种误导数字。
        let finishing = matches!(runner.phase(), InstallPhase::Done | InstallPhase::Failed);
        let text = runner.progress_text();
        if text != last_text && !finishing {
            sony_core::market::trace_diag(&format!("[进度] {text}"));
            report(Event::Progress {
                percent: percent_from_text(&text),
                text: text.clone(),
            });
            last_text = text;
        }

        // 编排器进到失败（例如相机拒绝 start）要立刻停，不要空等到超时
        if runner.phase() == InstallPhase::Failed {
            break;
        }
    }

    match runner.phase() {
        InstallPhase::Done => {
            if let Some(r) = runner.result() {
                check_result(r)?;
            }
            report(Event::Progress {
                percent: 100,
                text: "安装完成".into(),
            });
            Ok(InstallOutcome {
                device_report: runner.device_report().map(|v| v.to_string()),
            })
        }
        _ => anyhow::bail!("{}", runner.error().unwrap_or("原因未知")),
    }
}

/// 从进度文字里猜一个百分比。
///
/// 相机的进度汇报里带 `"percent":N`，编排器把它转成了人话。
/// 这里只做一层"尽力而为"的提取：提取不到就返回 0，不影响流程。
fn percent_from_text(text: &str) -> u8 {
    // 形如 "正在下载（45%）"
    if let Some(start) = text.rfind('（')
        && let Some(end) = text[start..].find('%')
    {
        let digits: String = text[start + '（'.len_utf8()..start + end]
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if let Ok(v) = digits.parse::<u8>() {
            return v.min(100);
        }
    }
    0
}

/// 生成一个随机数当服务端随机数。没有相机时用时间戳也够用。
fn rand_server_random() -> [u8; 32] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut out = [0u8; 32];
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let pid = std::process::id() as u64;
    seed ^= pid << 17;
    // xorshift64*：够随机，且不引入依赖
    for chunk in out.chunks_mut(8) {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        let v = seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let bytes = v.to_le_bytes();
        let n = chunk.len().min(8);
        chunk[..n].copy_from_slice(&bytes[..n]);
    }
    out
}

/// 把相机当前模式转成给用户看的说明（供界面直接用）
pub fn mode_hint(mode: CameraMode) -> &'static str {
    match mode {
        CameraMode::AppInstall => "应用安装模式",
        CameraMode::PlainMtp => "普通模式（只能传照片）",
        CameraMode::Unknown => "未知模式",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_extraction_works() {
        assert_eq!(percent_from_text("正在下载（45%）"), 45);
        assert_eq!(percent_from_text("正在下载（100%）"), 100);
        assert_eq!(percent_from_text("上传中（7%）"), 7);
        // 提取不到就返回 0，而不是 panic
        assert_eq!(percent_from_text("正在安装…"), 0);
        assert_eq!(percent_from_text(""), 0);
        // 超过 100 会被夹住
        assert_eq!(percent_from_text("（250%）"), 100);
    }

    #[test]
    fn camera_status_text_is_chinese() {
        let s = CameraStatus {
            pnp_id: "x".into(),
            model: "ILCE-6300".into(),
            serial: "1".into(),
            app_install_mode: true,
        };
        assert_eq!(s.mode_text(), "应用安装模式");
        let s = CameraStatus {
            app_install_mode: false,
            ..s
        };
        assert!(s.mode_text().contains("普通模式"));
    }
}
