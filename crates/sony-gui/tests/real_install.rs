//! 真正插着相机跑的端到端测试：**完全走图形界面的那条代码路径**。
//!
//! # 为什么要有这个测试
//!
//! 界面代码最麻烦的地方是"点一下试试"这种验证方式：
//!
//! - 自动化点击不可靠（窗口焦点被别的程序抢走，点了个空）
//! - 人点的时候只会点"正常路径"，点不出边界情况
//! - 而且每次都要重新插拔相机
//!
//! 所以这里**绕过鼠标事件**，直接驱动界面背后那套状态机。
//! 从"选好文件 / 点开始安装"往下，走的和真实点击**完全是同一条路**：
//!
//! ```text
//!   set_initial_apk()  ← 相当于"选好了 APK"
//!   Message::Start     ← 相当于"点了开始安装"按钮
//!        │
//!        └─► begin_install()
//!              ├─ 读文件
//!              ├─ spawn_install()      ← 真的起后台线程
//!              └─ 之后靠 Message::Tick 轮询通道
//!                    └─► on_worker()   ← 状态机消化消息
//! ```
//!
//! 只有"Windows 把鼠标点击投递给窗口"这一段没覆盖到 ——
//! 那一段是 iced 框架的事，不是我们的逻辑。
//!
//! # 怎么跑
//!
//! 默认**跳过**（`#[ignore]`），因为它需要一台真插着的相机。
//!
//! ```bash
//! set SONY_TEST_APK=C:\path\to\some.apk
//! cargo test -p sony-gui --test real_install -- --ignored --nocapture
//! ```
//!
//! 运行前请确认相机是"干净"的（上一次任务已经结束）。
//! 如果报"相机拒绝了开始任务"，把相机 USB 拔插一次再来。

use std::time::{Duration, Instant};

use sony_gui::app::{App, FontStatus, Message, Stage};
use sony_gui::fonts::{self, Fonts};

/// 整个安装最多等多久
const TIMEOUT: Duration = Duration::from_secs(180);

#[test]
#[ignore = "需要真插着相机，并且用 SONY_TEST_APK 指定一个 APK 路径"]
fn gui_install_flow_end_to_end() {
    let Ok(apk_path) = std::env::var("SONY_TEST_APK") else {
        eprintln!("跳过：没有设置 SONY_TEST_APK");
        eprintln!("用法：set SONY_TEST_APK=某个应用.apk");
        return;
    };

    // ---- 1. 建一个界面状态（和真启动时一样）----
    let mut app = App::new(Fonts::from_family(fonts::Fonts::default_family()), FontStatus::UiOnly);
    assert!(!app.has_apk(), "刚建好时不该有应用包");

    // ---- 2. 相当于"用户选好了 APK" ----
    app.set_initial_apk(apk_path.clone().into());
    assert!(app.has_apk(), "选中之后应该记住了这个文件");

    // ---- 3. 相当于"用户点了开始安装" ----
    let _ = app.update(Message::Start);
    assert_eq!(
        app.stage(),
        Stage::Preparing,
        "点了开始安装就该进入准备状态，实际：{:?}",
        app.stage()
    );

    // ---- 4. 一直轮询到结束（真实界面是每 100 毫秒 Tick 一次）----
    let deadline = Instant::now() + TIMEOUT;
    let mut last_stage = app.stage();
    while Instant::now() < deadline {
        let _ = app.update(Message::Tick);
        if matches!(app.stage(), Stage::Done | Stage::Failed) {
            break;
        }
        if app.stage() != last_stage {
            eprintln!("[测试] 状态：{:?} — {}", app.stage(), app.status_text());
            last_stage = app.stage();
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // ---- 5. 把界面日志原样打出来，失败时好定位 ----
    eprintln!();
    eprintln!("===== 界面日志 =====");
    for line in app.log_lines() {
        eprintln!("  {line}");
    }
    eprintln!("====================");
    eprintln!();

    match app.stage() {
        Stage::Done => {
            eprintln!("✅ 界面这条路径安装成功，进度 {}%", app.percent());
        }
        Stage::Failed => panic!("❌ 安装失败：{}", app.status_text()),
        other => panic!(
            "❌ 超时（{} 秒）还没结束，停在 {other:?}：{}",
            TIMEOUT.as_secs(),
            app.status_text()
        ),
    }
}

/// 没选文件时点"开始安装"不该有任何动作
///
/// 这个不需要相机，所以**不标 ignore**，每次跑测试都会执行。
#[test]
fn start_without_apk_does_nothing() {
    let mut app = App::new(Fonts::from_family(fonts::Fonts::default_family()), FontStatus::UiOnly);
    let _ = app.update(Message::Start);
    assert_eq!(app.stage(), Stage::Idle);
    assert!(app.log_lines().count() >= 1, "至少该有一条字体状态日志");
}
