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

/// 读一次相机信息（打开会话 + 读设备信息 + 关会话）
fn read_info(pnp_id: &str) -> Result<sony_core::mtp::DeviceInfo> {
    use sony_core::mtp;
    use sony_core::transport::PtpTransport;

    let mut t = sony_usb::WpdTransport::open(pnp_id).context("打开相机失败")?;
    let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_OPEN_SESSION, &[1]);
    let (rc, data) = PtpTransport::send_read_command(&mut t, mtp::PTP_OC_GET_DEVICE_INFO, &[])?;
    let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_CLOSE_SESSION, &[]);
    if rc != mtp::PTP_RC_OK {
        anyhow::bail!("读设备信息失败：响应码 0x{rc:04x}");
    }
    mtp::parse_device_info(&data).context("解析设备信息失败")
}

/// 找相机并读它的信息。**不改变**相机状态。
///
/// - 没插相机 → `Ok(None)`
/// - 插了但读不出来 → `Err`
pub fn probe_camera() -> Result<Option<CameraStatus>> {
    let Some(dev) = sony_usb::find_sony_camera().context("枚举设备失败")? else {
        return Ok(None);
    };
    let info = read_info(&dev.pnp_id)?;
    Ok(Some(CameraStatus {
        pnp_id: dev.pnp_id,
        model: info.model.clone(),
        serial: info.serial_number.clone(),
        app_install_mode: info.supports_all(&PROXY_OPS),
    }))
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

    let Some(mut dev) = sony_usb::find_sony_camera().context("枚举设备失败")? else {
        anyhow::bail!(
            "没有找到索尼相机。\n\
             请确认：\n\
             ① 相机已开机、USB 线已插好\n\
             ② 没有别的程序（比如原版 PMCA）占用相机"
        );
    };
    step(report, "找到相机，正在读取信息…");

    let info = read_info(&dev.pnp_id)?;
    if info.supports_all(&PROXY_OPS) {
        return Ok(CameraStatus {
            pnp_id: dev.pnp_id,
            model: info.model,
            serial: info.serial_number,
            app_install_mode: true,
        });
    }

    // 不在安装模式 —— 先确认它有办法切过去
    if !info.supports_all(&[
        mtp::PTP_OC_SONY_DI_EXT_CMD_WRITE,
        mtp::PTP_OC_SONY_DI_EXT_CMD_READ,
    ]) {
        anyhow::bail!(
            "这台相机在当前模式下既不支持代理消息、也不支持索尼扩展命令，无法安装应用。\n\
             支持的操作码：{:?}",
            info.operations_supported
                .iter()
                .map(|c| format!("0x{c:04x}"))
                .collect::<Vec<_>>()
        );
    }

    step(report, "正在命令相机切换到「应用安装模式」…（相机会重连一次）");
    {
        let mut t = sony_usb::WpdTransport::open(&dev.pnp_id).context("打开相机失败")?;
        let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_OPEN_SESSION, &[1]);
        // 即使这条命令报错，相机也可能已经切了，所以不直接失败
        if let Err(e) = PtpTransport::switch_to_app_install_mode(&mut t) {
            step(report, format!("提示：{e}（相机可能仍会切换，继续等待）"));
        }
        let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_CLOSE_SESSION, &[]);
    }

    // 等相机重新枚举成"应用安装模式"
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(Some(d)) = sony_usb::find_sony_camera() else {
            continue;
        };
        if d.pnp_id == dev.pnp_id {
            continue; // 还是老设备，还没切完
        }
        // 设备换了，确认一下新模式
        std::thread::sleep(Duration::from_millis(400));
        if let Ok(info) = read_info(&d.pnp_id)
            && info.supports_all(&PROXY_OPS)
        {
            dev = d;
            step(report, "切换成功");
            // 刚重新枚举出来的设备还需要一点点时间才稳定，
            // 否则紧接着开会话可能拿到 0x8007001F
            std::thread::sleep(Duration::from_millis(600));
            return Ok(CameraStatus {
                pnp_id: dev.pnp_id,
                model: info.model,
                serial: info.serial_number,
                app_install_mode: true,
            });
        }
    }

    anyhow::bail!(
        "等待相机切换到应用安装模式超时（25 秒）。\n\
         请把相机 USB 线拔下再插上，然后重试。"
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
