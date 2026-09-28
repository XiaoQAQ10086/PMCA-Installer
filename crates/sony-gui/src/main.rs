//! 索尼相机应用安装器 —— 图形界面。
//!
//! 用法：双击运行 → 选一个 APK → 点「开始安装」。
//! 相机模式是全自动切换的，用户不需要去相机菜单里做任何设置。
//!
//! 也支持：
//! - `索尼相机应用安装器.exe 某个应用.apk`（命令行直接带路径）
//! - 把 APK 文件**拖进窗口**，或拖到程序图标上
//!
//! # 界面技术选型上的两个决定
//!
//! 1. **渲染用纯 CPU（tiny-skia）**，不用显卡（wgpu）。
//!    这是一个小工具窗口，CPU 渲染完全够用，而且：
//!    - 不需要显卡驱动配合，老机器/虚拟机也能跑
//!    - 编出来的 exe 小得多、启动更快
//!
//! 2. **中文字体分两层**：内嵌界面用字的小子集（58 KB，保证界面永远能显示）
//!    + 系统字体（覆盖文件名里的任意中文）。详见 `fonts.rs`。

// ⚠️ 关键：告诉 Windows 这是**图形程序**，不要给它开控制台窗口。
//
// 不加这一行，双击运行时系统会额外弹一个黑色 cmd 窗口 —— 很难看，
// 而且用户关掉那个黑框会把程序一起关掉。
//
// `cfg_attr(not(debug_assertions), ...)` 的意思是：
// **只有发布版**才隐藏控制台；开发时（debug）保留黑框，
// 这样 `cargo run` 还能看到 println! 的输出，方便排查问题。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use iced::window;
use iced::Size;

use sony_gui::app::{App, Message};
use sony_gui::fonts::{self, Fonts};
use sony_gui::theme;

fn main() -> iced::Result {
    // 命令行可以带一个 APK 路径
    let initial_apk = std::env::args_os().nth(1).map(PathBuf::from);
    if let Some(p) = &initial_apk
        && matches!(p.to_string_lossy().as_ref(), "-h" | "--help" | "/?")
    {
        print_help();
        return Ok(());
    }

    // ---- 准备字体：内嵌的界面字体 + （如果有）系统字体 ----
    // 界面文字永远用内嵌的那份，所以**不管系统有没有中文字体，界面都能正常显示**。
    let font_setup = fonts::setup();
    let ui_fonts = font_setup.ui;
    let font_status = font_setup.status;
    let font_blobs = font_setup.blobs;

    let apk_for_boot = initial_apk.clone();
    let mut application = iced::application(
        move || {
            let mut a = App::new(ui_fonts, font_status);
            if let Some(p) = apk_for_boot.clone() {
                a.set_initial_apk(p);
            }
            a.boot_task();
            a
        },
        App::update,
        App::view,
    )
    .title("索尼相机应用安装器")
    // ⚠️ 这几个闭包**必须写清参数类型**（`|_: &App|`）。
    //    只写 `|_|` 的话，Rust 会把生命周期推断成某一个具体的，
    //    而 iced 要求"对任意生命周期都成立"，于是报
    //    "implementation of FnOnce is not general enough"。
    .theme(|_: &App| theme::theme())
    .default_font(fonts::font_of(Fonts::default_family()))
    .window(window::Settings {
        size: Size::new(760.0, 660.0),
        min_size: Some(Size::new(620.0, 520.0)),
        resizable: true,
        ..window::Settings::default()
    })
    .subscription(|_: &App| {
        // 两条订阅：
        // 1) 每 100 毫秒轮询一次后台线程。
        //    用轮询而不是回调，是因为它最简单也最不容易出错 ——
        //    界面永远不会被后台卡住。
        // 2) 窗口事件（用来接收"文件被拖进来"）
        iced::Subscription::batch([
            iced::time::every(std::time::Duration::from_millis(100)).map(|_| Message::Tick),
            window::events().map(|(_id, event)| match event {
                window::Event::FileDropped(path) => Message::ApkDropped(path),
                _ => Message::Ignored,
            }),
        ])
    })
    .antialiasing(true);

    // 两个字体都装上：内嵌的界面字体 + 系统字体（用于文件名里的生僻字）
    for blob in font_blobs {
        application = application.font(blob);
    }

    application.run()
}

/// 打印用法。
///
/// ⚠️ 发布版是 GUI 子系统程序，**自己没有控制台**。
/// 如果是从 `cmd` 里运行的，可以"借"父进程的控制台来输出；
/// 双击运行时借不到，输出就丢弃（这是预期行为，不是错误）。
fn print_help() {
    attach_parent_console();

    use std::io::Write;
    // 用 `let _ =`：没有控制台时写不出去，不该 panic
    let _ = writeln!(std::io::stdout(), "索尼相机应用安装器");
    let _ = writeln!(std::io::stdout());
    let _ = writeln!(std::io::stdout(), "用法：");
    let _ = writeln!(std::io::stdout(), "  索尼相机应用安装器.exe [应用包路径]");
    let _ = writeln!(std::io::stdout());
    let _ = writeln!(
        std::io::stdout(),
        "不带参数直接双击运行也可以，在界面里点「选择文件…」即可。"
    );
    let _ = writeln!(
        std::io::stdout(),
        "也可以把 APK 文件直接拖进窗口，或拖到本程序的图标上。"
    );
}

/// 尝试接管父进程的控制台（仅在从 cmd/PowerShell 启动时有效）。
///
/// 自己声明 FFI 而不引入 `windows-sys`：只用一个函数，
/// 为它拉一整个依赖不划算。
#[cfg(windows)]
fn attach_parent_console() {
    // ATTACH_PARENT_PROCESS = -1（u32 全 1）
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
    }
    // 失败也无所谓（双击运行时就是这样）
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}
