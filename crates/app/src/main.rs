//! 临时命令行探针（Iced 界面在后续阶段替换它）。
//!
//! 用法：
//! - `cargo run -p app -- tls-golden`                   用真机录制的问候语验证 TLS 服务端
//! - `cargo run -p app -- spk <输入文件> <输出文件>`      把文件打包成 SPK（用于与 Python 逐字节比对）
//! - `cargo run -p app -- devices`                      列出所有 WPD 设备（**不需要相机**）
//! - `cargo run -p app -- camera`                       找相机、读设备信息、判断模式（需要相机）
//! - `cargo run -p app -- install <apk文件>`            把 APK 装到相机上（需要相机在应用安装模式）

use anyhow::{Context, Result};

fn main() -> Result<()> {
    // 注意：日志只在需要时初始化。启用它会影响 stdout 缓冲行为，
    // 排查 COM 问题时先关掉，避免干扰。
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .init();
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("tls-golden");
    match cmd {
        "tls-golden" => tls_golden()?,
        "spk" => {
            let input = args.get(1).context("用法：spk <输入文件> <输出文件>")?;
            let output = args.get(2).context("用法：spk <输入文件> <输出文件>")?;
            spk_pack(input, output)?;
        }
        "devices" => list_devices()?,
        "camera" => camera_info()?,
        "probe" => probe_ops()?,
        "switch" => switch_mode()?,
        "install" => {
            let apk = args.get(1).context("用法：install <apk文件>")?;
            install_app(apk)?;
        }
        other => {
            eprintln!("未知命令：{other}");
            eprintln!(
                "可用命令：tls-golden | spk <输入> <输出> | devices | camera | install <apk>"
            );
        }
    }
    Ok(())
}

/// 确保相机处于**应用安装模式**；不在就自动切过去。
///
/// 为什么需要这个：相机在重新插拔、休眠、或安装完成后会**复位回普通 MTP 模式**。
/// 所以每次安装前都要检查一遍，而不是假定它还停在安装模式。
///
/// 判定依据是相机支持的操作码：
/// - 普通 MTP 模式：有 `0x9280/0x9281/0x9282`（索尼扩展命令），没有代理消息操作码
/// - 应用安装模式：有 `0x9488/0x9489/0x948c/0x948d`（代理消息）
fn ensure_app_install_mode(dev: &mut sony_usb::WpdDevice) -> Result<()> {
    use sony_core::mtp;
    use sony_core::transport::PtpTransport;
    use std::time::{Duration, Instant};

    const PROXY_OPS: [u16; 4] = [0x9488, 0x9489, 0x948C, 0x948D];

    /// 读一次设备信息，失败时给出可读的原因
    fn read_info(dev: &sony_usb::WpdDevice) -> Result<mtp::DeviceInfo> {
        let mut t = sony_usb::WpdTransport::open(&dev.pnp_id).context("打开相机失败")?;
        let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_OPEN_SESSION, &[1]);
        let (rc, data) = PtpTransport::send_read_command(&mut t, mtp::PTP_OC_GET_DEVICE_INFO, &[])?;
        if rc != mtp::PTP_RC_OK {
            anyhow::bail!("读设备信息失败：响应码 0x{rc:04x}");
        }
        mtp::parse_device_info(&data).context("解析设备信息失败")
    }

    let info = read_info(dev)?;
    println!("  相机型号：{}", info.model);
    if info.supports_all(&PROXY_OPS) {
        println!("  当前已是应用安装模式 ✅");
        return Ok(());
    }

    println!("  当前是普通 MTP 模式，需要先切换");
    if !info.supports_all(&[mtp::PTP_OC_SONY_DI_EXT_CMD_WRITE, mtp::PTP_OC_SONY_DI_EXT_CMD_READ]) {
        anyhow::bail!(
            "这台相机在当前模式下既不支持代理消息、也不支持索尼扩展命令，无法安装应用。\n\
             支持的操作码：{:?}",
            info.operations_supported
                .iter()
                .map(|c| format!("0x{c:04x}"))
                .collect::<Vec<_>>()
        );
    }

    println!("  正在命令相机切换（相机会重启 USB 连接）…");
    {
        let mut t = sony_usb::WpdTransport::open(&dev.pnp_id).context("打开相机失败")?;
        let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_OPEN_SESSION, &[1]);
        // 即使这条命令报错，相机也可能已经切了，所以不直接失败
        if let Err(e) = PtpTransport::switch_to_app_install_mode(&mut t) {
            println!("  提示：{e}（相机可能仍会切换，继续等待）");
        }
    }

    // 等相机重新枚举成"应用安装模式"
    let deadline = Instant::now() + Duration::from_secs(20);
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
        if let Ok(info) = read_info(&d)
            && info.supports_all(&PROXY_OPS)
        {
            *dev = d;
            println!("  切换成功 ✅");
            return Ok(());
        }
    }

    anyhow::bail!(
        "等待相机切换到应用安装模式超时（20 秒）。\n\
         请拔插一次 USB 线后重试。"
    )
}

/// 把 APK 安装到相机上。
///
/// 这是把前面积累的所有零件串起来的一条完整流程：
/// 打开相机 → 打招呼 → 发 XPD → 扮演 HTTPS 服务器 → 收结果。
fn install_app(apk_path: &str) -> Result<()> {
    use sony_core::market::{InstallPhase, InstallRunner, check_result};
    use sony_core::proxy::RetryPolicy;
    use sony_core::sony;
    use std::time::{Duration, Instant};

    let apk = std::fs::read(apk_path).with_context(|| format!("读取 {apk_path} 失败"))?;
    println!("=== 准备安装 ===");
    println!("应用包：{}（{} 字节）", apk_path, apk.len());
    if !apk.starts_with(b"PK") {
        println!("⚠️  这个文件看起来不是 APK（开头不是 PK）。如果是 zip 压缩的应用包可以继续。");
    }

    // 诊断落盘：真机每次插拔复位后**只有一次机会**（相机记住未完成的
    // 任务后会拒绝新的 start），所以关键信息必须写进文件才能回看。
    let log_path = "install-diag.log";
    match sony_core::market::diag_init(log_path) {
        Ok(()) => println!("诊断日志：{log_path}"),
        Err(e) => println!("（诊断日志 {log_path} 无法写入：{e}）"),
    }

    println!();
    println!("=== 正在查找相机 ===");
    let mut dev = {
        let Some(d) = sony_usb::find_sony_camera().context("枚举设备失败")? else {
            anyhow::bail!(
                "没有找到索尼相机。\n\
                 请确认：\n\
                 1. 相机已开机、USB 线已插好\n\
                 2. 没有别的程序占用相机"
            );
        };
        d
    };
    println!("找到：{}", dev.pnp_id);

    // ---- 关键一步：确保相机在应用安装模式 ----
    // 相机重新插拔/休眠后会复位回普通 MTP 模式，所以每次都要检查。
    // 普通 MTP 模式与安装模式的区别就在支持的操作码上。
    println!();
    println!("=== 检查相机模式 ===");
    ensure_app_install_mode(&mut dev)?;
    println!("找到：{}（应用安装模式）", dev.pnp_id);

    println!();
    println!("=== 正在打开相机 ===");
    let transport = sony_usb::WpdTransport::open(&dev.pnp_id).context("打开相机失败")?;

    println!("=== 正在与相机建立会话 ===");
    let mut session = sony::SonySession::new(transport, RetryPolicy::default());
    let protocols = session.handshake().context("与相机握手失败")?;
    println!(
        "相机支持：{}",
        protocols
            .iter()
            .map(|(n, id)| format!("{}(0x{id:x})", String::from_utf8_lossy(n)))
            .collect::<Vec<_>>()
            .join("、")
    );

    // 编排器负责"该发什么、收到什么怎么答"
    let mut runner = InstallRunner::new(apk, rand_server_random())?;
    // 会话已经完成握手了，编排器直接从运行阶段开始
    runner.mark_hello_done();

    println!();
    println!("=== 开始安装 ===");
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_text = String::new();

    loop {
        if Instant::now() > deadline {
            sony_core::market::trace_diag("[主循环] 超时，任务未完成");
            anyhow::bail!(
                "超时（120 秒）。相机没有完成任务。\n\
                 详细诊断已写入 {log_path}。"
            );
        }
        if runner.phase() == InstallPhase::Done || runner.phase() == InstallPhase::Failed {
            break;
        }

        // ① 把编排器要发的消息发出去
        for m in runner.take_outgoing() {
            sony_core::market::trace_diag(&format!("[主循环] 发送消息，外层类型 {}", m.msg_type));
            session.channel_mut().send(&m)?;
        }

        // ② 收一条相机的消息（收不到就稍等，NoData 是正常状态）
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
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                sony_core::market::trace_diag(&format!("[主循环] 读取消息出错：{e}"));
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        // ③ 更新进度显示
        let text = runner.progress_text();
        if text != last_text {
            println!("  {text}");
            last_text = text;
        }
    }

    println!();
    match runner.phase() {
        InstallPhase::Done => {
            if let Some(r) = runner.result() {
                check_result(r)?;
            }
            println!("✅ 安装完成");
            if let Some(info) = runner.device_report() {
                println!("相机信息：{}", info);
            }
            Ok(())
        }
        _ => anyhow::bail!(
            "安装失败：{}",
            runner.error().unwrap_or("原因未知")
        ),
    }
}

/// 生成一个随机数当服务端随机数。没有相机时用时间戳也够用。
fn rand_server_random() -> [u8; 32] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut out = [0u8; 32];
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mut seed = nanos ^ (pid << 64);
    for b in out.iter_mut() {
        // xorshift，够用了：TLS 的随机数只要求不可预测到"猜不出"，
        // 而我们这次是本地短连接、不涉及长期密钥。
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *b = (seed & 0xFF) as u8;
    }
    out
}

/// 命令相机**切换到应用安装模式**，然后等它重新出现。
///
/// 这一步对应原项目的 `switchToAppInstaller`：通过索尼私有命令
/// （0x9280 写 + 命令号 (5,2)）告诉相机去切换。相机收到后会重启 USB 连接。
///
/// 之所以能这样做：普通 MTP 模式下相机就支持 0x9280/0x9281/0x9282 这三个
/// 索尼扩展命令操作码（原项目正是靠它们切模式的）。
fn switch_mode() -> Result<()> {
    use sony_core::mtp;
    use sony_core::transport::PtpTransport;
    use std::time::{Duration, Instant};

    println!("=== 命令相机切换到应用安装模式 ===");
    let Some(dev) = sony_usb::find_sony_camera().context("没有找到索尼相机")? else {
        anyhow::bail!("没有找到索尼相机。请确认相机已开机、USB 线已插好。");
    };
    println!("找到：{}", dev.pnp_id);

    let mut t = sony_usb::WpdTransport::open(&dev.pnp_id).context("打开相机失败")?;

    // 先看当前模式：只有支持索尼扩展命令的相机才能这样切
    let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_OPEN_SESSION, &[1]);
    let (rc, data) =
        PtpTransport::send_read_command(&mut t, mtp::PTP_OC_GET_DEVICE_INFO, &[])?;
    if rc != mtp::PTP_RC_OK {
        anyhow::bail!("读设备信息失败：0x{rc:04x}");
    }
    let info = mtp::parse_device_info(&data)?;
    let can_switch = info.supports_all(&[
        mtp::PTP_OC_SONY_DI_EXT_CMD_WRITE,
        mtp::PTP_OC_SONY_DI_EXT_CMD_READ,
    ]);
    println!("相机型号：{}", info.model);
    if !can_switch {
        println!();
        println!("⚠️  这台相机（在当前模式下）不支持索尼扩展命令，无法用这个办法切换。");
        println!("    支持的操作码里有 0x9280/0x9281 才行，实际有的：");
        for op in &info.operations_supported {
            println!("      0x{op:04x}");
        }
        return Ok(());
    }
    println!("✅ 相机支持索尼扩展命令，可以尝试切换");

    println!();
    println!("=== 正在发送切换命令 ===");
    println!("（相机会重启 USB 连接，屏幕可能会闪一下或出现提示，这是正常的）");
    match PtpTransport::switch_to_app_install_mode(&mut t) {
        Ok(()) => println!("  ✅ 命令已送达"),
        Err(e) => {
            println!("  ⚠️  {e}");
            println!("     相机可能仍会切换，继续等待看看。");
        }
    }
    drop(t); // 先放手，让相机能重新枚举

    println!();
    println!("=== 等待相机重新出现（最多 20 秒）===");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut found = None;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        if let Ok(Some(d)) = sony_usb::find_sony_camera() {
            // 等它稳定一点
            std::thread::sleep(Duration::from_millis(500));
            found = Some(d);
            break;
        }
    }

    let Some(dev) = found else {
        println!("❌ 没有等到相机重新出现。");
        println!("   请拔插一次 USB 线，然后运行：cargo run -p app --bin app -- camera");
        return Ok(());
    };
    println!("✅ 相机回来了：{}", dev.pnp_id);

    // 再读一次设备信息，看模式变了没
    println!();
    println!("=== 检查切换结果 ===");
    let mut t = sony_usb::WpdTransport::open(&dev.pnp_id).context("打开相机失败")?;
    let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_OPEN_SESSION, &[1]);
    let (rc, data) =
        PtpTransport::send_read_command(&mut t, mtp::PTP_OC_GET_DEVICE_INFO, &[])?;
    if rc != mtp::PTP_RC_OK {
        println!("  读设备信息返回 0x{rc:04x}");
        return Ok(());
    }
    let info = mtp::parse_device_info(&data)?;
    println!("  厂商扩展 = {:?}", info.vendor_extension);
    println!("  支持的操作码（{} 个）：", info.operations_supported.len());
    for op in &info.operations_supported {
        let mark = match *op {
            0x9488 => "  ← 读代理消息元信息 ✅",
            0x9489 => "  ← 读代理消息正文 ✅",
            0x948c => "  ← 写代理消息元信息 ✅",
            0x948d => "  ← 写代理消息正文 ✅",
            0x9280 => "  ← 索尼扩展命令（写）",
            0x9281 => "  ← 索尼扩展命令（读）",
            0x9282 => "  ← 请求重连",
            _ => "",
        };
        println!("    0x{op:04x}{mark}");
    }

    let supports_proxy = info.supports_all(&[0x9488, 0x9489, 0x948C, 0x948D]);
    let mode = sony_usb::CameraMode::detect(&info.vendor_extension, supports_proxy);
    println!();
    println!("现在模式：{}", mode.description());
    if mode == sony_usb::CameraMode::AppInstall {
        println!();
        println!("🎉 切换成功！现在可以安装应用了：");
        println!("   cargo run -p app --bin app -- install 某个应用.apk");
    } else {
        println!();
        println!("还没有切过去。可以再跑一次本命令，或在相机菜单里手动切换。");
    }
    Ok(())
}

/// 探测相机对各个（包括厂商私有的）操作码怎么回应。
///
/// 为什么要这么做：相机在设备信息里报告的"支持的操作码"列表**可能不含**
/// 厂商私有的那几个（0x9488 等），所以我们不能只看那个列表就下结论。
/// 直接试着调用，看它回什么码，才是最可靠的判断。
///
/// 这里只发**读取类**的探测，不会改动相机任何内容。
fn probe_ops() -> Result<()> {
    use sony_core::mtp;
    use sony_core::transport::PtpTransport;

    println!("=== 探测相机支持的操作码 ===");
    let Some(dev) = sony_usb::find_sony_camera().context("没有找到索尼相机")? else {
        return Ok(());
    };
    let mut t = sony_usb::WpdTransport::open(&dev.pnp_id).context("打开相机失败")?;

    // 先开会话（已开也没关系）
    let _ = PtpTransport::send_command(&mut t, mtp::PTP_OC_OPEN_SESSION, &[1]);

    // 无数据阶段的探测：这些操作码如果被调用，正常应回"没数据"而不是"不支持"
    let probes_no_data: &[(u16, &str)] = &[
        (0x9488, "读代理消息元信息"),
        (0x9489, "读代理消息正文"),
        (0x940a, "读设备能力"),
        (0x9801, "索尼私有 0x9801"),
        (0x9280, "索尼私有 0x9280"),
    ];

    println!();
    println!("【不带数据的操作码】");
    println!("  （0x2001=成功, 0xA488=暂时没数据(说明支持!), 0x2006=不支持, 其他见备注）");
    for (code, name) in probes_no_data {
        match PtpTransport::send_command(&mut t, *code, &[0]) {
            Ok(rc) => {
                let hint = match rc {
                    0x2001 => "✅ 支持",
                    0xA488 => "✅ 支持（只是当前没数据）",
                    0x2006 | 0x2013 => "❌ 不支持",
                    0x2019 => "⚠️ 设备忙",
                    0xA806 => "⚠️ 内部错误",
                    0 => "❌ 无响应",
                    _ => "❓ 未知",
                };
                println!("  0x{code:04x} {name:<20} → 0x{rc:04x}  {hint}");
            }
            Err(e) => println!("  0x{code:04x} {name:<20} → 调用失败：{e}"),
        }
    }

    // 带数据的探测：这几个是厂商私有的"读"，看能不能拿到数据
    let probes_read: &[(u16, &str)] = &[(0x9488, "读代理消息元信息"), (0x940a, "读设备能力")];
    println!();
    println!("【带数据读取的操作码】");
    for (code, name) in probes_read {
        match PtpTransport::send_read_command(&mut t, *code, &[0]) {
            Ok((rc, data)) => {
                let hint = match rc {
                    0x2001 => format!("✅ 支持，返回 {} 字节", data.len()),
                    0xA488 => "✅ 支持（当前没数据）".to_string(),
                    0x2006 | 0x2013 => "❌ 不支持".to_string(),
                    _ => format!("❓ 响应码 0x{rc:04x}，{} 字节", data.len()),
                };
                println!("  0x{code:04x} {name:<20} → {hint}");
                if !data.is_empty() {
                    let show = data.len().min(32);
                    println!("        前 {show} 字节：{:02x?}", &data[..show]);
                }
            }
            Err(e) => println!("  0x{code:04x} {name:<20} → 调用失败：{e}"),
        }
    }

    println!();
    println!("=== 结论 ===");
    println!("如果 0x9488 / 0x9489 显示「✅ 支持」，说明相机已经在应用安装模式，可以装应用。");
    println!("如果显示「❌ 不支持」，需要在相机菜单里切换 USB 连接模式。");
    Ok(())
}

/// 列出所有 WPD 设备（不需要相机插着也能跑）
fn list_devices() -> Result<()> {
    println!("=== 正在枚举 Windows 便携设备（WPD）===");
    let devices = sony_usb::wpd::list_devices().context("枚举设备失败")?;
    if devices.is_empty() {
        println!("没有找到任何便携设备。");
        println!("（如果相机已经插上，请确认它已开机，且没有停在关机/充电状态）");
        return Ok(());
    }
    println!("共 {} 台：", devices.len());
    for (i, d) in devices.iter().enumerate() {
        let is_sony = d.vendor_id == Some(sony_usb::SONY_VENDOR_ID);
        println!();
        println!(
            "  #{} {}{}",
            i + 1,
            if is_sony { "索尼相机" } else { "其他设备" },
            if is_sony { "   ← 可以安装应用" } else { "" }
        );
        match (d.vendor_id, d.product_id) {
            (Some(v), Some(p)) => println!("     VID:PID = {v:04X}:{p:04X}"),
            _ => println!("     无法从标识里解析 VID/PID"),
        }
        println!("     标识 = {}", d.pnp_id);
    }
    Ok(())
}

/// 找相机、读设备信息、判断模式（需要相机插着）
fn camera_info() -> Result<()> {
    use sony_core::mtp;

    println!("=== 正在查找索尼相机 ===");
    let Some(dev) = sony_usb::find_sony_camera().context("枚举设备失败")? else {
        println!("没有找到索尼相机。");
        println!();
        println!("请检查：");
        println!("  1. 相机已开机");
        println!("  2. USB 线已插好（数据线，不是只有充电功能的线）");
        println!("  3. 相机菜单里 USB 连接方式选的是「应用安装」相关选项");
        println!("  4. 没有别的程序占用相机（照片应用、资源管理器预览等）");
        println!();
        println!("想先确认系统到底看到了什么设备，可以运行：cargo run -p app -- devices");
        return Ok(());
    };

    println!("找到索尼相机：{}", dev.pnp_id);
    if let (Some(v), Some(p)) = (dev.vendor_id, dev.product_id) {
        println!("VID:PID = {v:04X}:{p:04X}");
    }
    println!();

    println!("=== 正在打开相机（读取设备信息）===");
    let mut transport = sony_usb::WpdTransport::open(&dev.pnp_id).context("打开相机失败")?;

    // 打开 MTP 会话
    println!("第 1 步：打开 MTP 会话（0x1002）…");
    match sony_core::transport::PtpTransport::send_command(
        &mut transport,
        mtp::PTP_OC_OPEN_SESSION,
        &[1],
    ) {
        Ok(rc) if rc == mtp::PTP_RC_OK => println!("  ✅ 会话已打开"),
        Ok(rc) if rc == mtp::PTP_RC_SESSION_ALREADY_OPENED => println!("  ✅ 会话本来就已经打开"),
        Ok(rc) => println!("  ⚠️  返回 0x{rc:04x}，继续尝试"),
        Err(e) => println!("  ❌ {e}"),
    }

    // 读设备信息
    println!("第 2 步：读设备信息（0x1001）…");
    let (rc, data) = match sony_core::transport::PtpTransport::send_read_command(
        &mut transport,
        mtp::PTP_OC_GET_DEVICE_INFO,
        &[],
    ) {
        Ok(v) => v,
        Err(e) => {
            println!("  ❌ {e}");
            println!();
            println!("诊断建议：");
            println!("  · 上面如果是「没有返回响应码」，说明相机不接受这个操作码。");
            println!("    请确认相机菜单里 USB 连接方式选的是「应用安装」相关选项。");
            println!("  · 可以用 devices 命令看系统识别到的是哪个 PID：");
            println!("    普通 MTP 模式与安装模式是两个不同的 PID。");
            return Ok(());
        }
    };
    if rc != mtp::PTP_RC_OK {
        println!("  ❌ 相机返回 0x{rc:04x}");
        return Ok(());
    }
    println!("  ✅ 收到 {} 字节", data.len());

    let info = match mtp::parse_device_info(&data) {
        Ok(i) => i,
        Err(e) => {
            println!("  ❌ 解析设备信息失败：{e}");
            println!("     前 64 字节：{:02x?}", &data[..data.len().min(64)]);
            return Ok(());
        }
    };
    println!("  厂商   = {}", info.manufacturer);
    println!("  型号   = {}", info.model);
    println!("  序列号 = {}", info.serial_number);
    println!("  厂商扩展 = {:?}", info.vendor_extension);
    println!("  支持的操作码（{} 个）：", info.operations_supported.len());
    for op in &info.operations_supported {
        let mark = match *op {
            mtp::PTP_OC_GET_DEVICE_INFO => "  ← 读设备信息",
            0x9488 => "  ← 读代理消息元信息",
            0x9489 => "  ← 读代理消息正文",
            0x948C => "  ← 写代理消息元信息",
            0x948D => "  ← 写代理消息正文",
            0x940A => "  ← 读设备能力",
            _ => "",
        };
        println!("    0x{op:04x}{mark}");
    }

    // 判断模式
    let supports_proxy = info.supports_all(&[0x9488, 0x9489, 0x948C, 0x948D]);
    let mode = sony_usb::CameraMode::detect(&info.vendor_extension, supports_proxy);
    println!();
    println!("=== 结论 ===");
    println!("相机模式：{}", mode.description());
    match mode.how_to_fix() {
        Some(advice) => {
            println!();
            println!("⚠️  现在还不能安装应用。");
            println!("{advice}");
        }
        None => {
            println!();
            println!("✅ 相机处于应用安装模式，可以继续下一步。");
        }
    }
    Ok(())
}

/// 把文件打包成 SPK，并打印长度与哈希（供与 Python 实现比对）
fn spk_pack(input: &str, output: &str) -> Result<()> {
    let data = std::fs::read(input).with_context(|| format!("读取 {input} 失败"))?;
    println!("输入 {} 字节", data.len());
    let spk = sony_core::spk::dump(&data)?;
    std::fs::write(output, &spk.bytes).with_context(|| format!("写入 {output} 失败"))?;
    println!("SPK  {} 字节", spk.len());
    println!("指纹 {}", fnv1a64(&spk.bytes));
    Ok(())
}

/// 简单的 FNV-1a 64 位哈希，用于跨语言比对（不引入额外依赖）
fn fnv1a64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// 用真机录制的 ClientHello 走一遍服务端第一段（M1）
fn tls_golden() -> Result<()> {
    use sony_core::golden;
    use sony_core::tls;

    println!("=== TLS 服务端自检（真机录制的问候语）===");
    let raw = golden::client_hello();
    println!("问候语：{} 字节", raw.len());

    let ch = tls::ClientHello::parse(&raw[5..])?;
    println!("客户端版本   ：0x{:04x}", ch.client_version);
    println!(
        "提供的套件   ：{:?}",
        ch.cipher_suites
            .iter()
            .map(|c| format!("0x{c:04x}"))
            .collect::<Vec<_>>()
    );
    println!("带 SNI 扩展  ：{}", ch.has_sni());
    println!(
        "扩展列表     ：{:?}",
        ch.extensions
            .iter()
            .map(|(t, l)| format!("0x{t:04x}({})", l.len()))
            .collect::<Vec<_>>()
    );

    let cipher = ch
        .choose_cipher()
        .ok_or_else(|| anyhow::anyhow!("客户端提供的套件里没有我们能做的"))?;
    println!("协商套件     ：0x{cipher:04x}");

    let server_random = [0x11u8; 32];
    let flight = vec![
        tls::build_server_hello(
            &server_random,
            &ch.session_id,
            cipher,
            ch.offers_secure_renegotiation(),
        ),
        tls::build_certificate()?,
        tls::build_server_hello_done(),
    ];
    let record = tls::wrap_handshake_record(&flight);
    println!("服务端首段   ：{} 字节", record.len());

    let records = tls::split_records(&record);
    for (i, (ct, ver, body)) in records.iter().enumerate() {
        println!("  记录#{i} 类型={ct} 版本=0x{ver:04x} 长度={}", body.len());
    }
    let (_, _, body) = &records[0];
    let found = tls::parse_server_hello_cipher(body);
    println!(
        "ServerHello 里的套件：{:?}",
        found.map(|c| format!("0x{c:04x}"))
    );

    let chain = sony_core::certs::certificate_chain()?;
    println!(
        "证书链       ：{} 张（{}）",
        chain.len(),
        chain
            .iter()
            .map(|c| c.len().to_string())
            .collect::<Vec<_>>()
            .join(" + ")
    );
    let key = sony_core::certs::private_key()?;
    use rsa::traits::PublicKeyParts;
    println!("私钥         ：RSA-{} bit", key.n().bits());

    println!();
    println!("✅ 能解析真机问候语并产出结构正确的服务端首段");
    Ok(())
}
