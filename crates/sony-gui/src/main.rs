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

/// 窗口图标：预先解码好的**原始 RGBA** 像素（128×128）。
///
/// 由 `python tools/make_icon.py <源图.png>` 生成。
///
/// # 为什么不内嵌 PNG、启动时再解码
///
/// 那样就得引入一个 PNG 解码库。而窗口图标在标题栏/任务栏最多显示到
/// 48 像素左右，128×128 足够覆盖高 DPI；原始数据 64 KB，
/// 比多一个依赖划算，而且启动时**不需要解码**、更快。
const WINDOW_ICON_RGBA: &[u8] = include_bytes!("../assets/icon-rgba.bin");

/// `WINDOW_ICON_RGBA` 的边长
const WINDOW_ICON_SIZE: u32 = 128;

/// 构造窗口图标（标题栏 + 任务栏 + Alt+Tab 用）。
///
/// ⚠️ 注意 `from_rgba` 是 `window::icon` 模块里的**自由函数**，
/// **不是** `Icon` 的方法 —— 写成 `Icon::from_rgba(...)` 编译不过。
///
/// 失败就返回 `None`：没有图标也能正常用，不该因此启动不了。
fn window_icon() -> Option<window::Icon> {
    window::icon::from_rgba(
        WINDOW_ICON_RGBA.to_vec(),
        WINDOW_ICON_SIZE,
        WINDOW_ICON_SIZE,
    )
    .ok()
}

fn main() -> iced::Result {
    // 命令行可以带一个 APK 路径
    let initial_apk = std::env::args_os().nth(1).map(PathBuf::from);
    if let Some(p) = &initial_apk {
        match p.to_string_lossy().as_ref() {
            "-h" | "--help" | "/?" => {
                print_help();
                return Ok(());
            }
            "-V" | "--version" => {
                print_version();
                return Ok(());
            }
            _ => {}
        }
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
    .title("PMCA 安装器 — 索尼相机应用安装工具")
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
        icon: window_icon(),
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

/// 往控制台输出文字（供 `--help` / `--version` 用）。
///
/// ⚠️ **不能直接用 `println!`**，这里有个很隐蔽的坑：
///
/// 发布版是 GUI 子系统程序，**进程启动时没有控制台**。
/// Rust 在那时候就已经把 stdout 定成了无效句柄 ——
/// 之后再调 `AttachConsole` 也不会让它生效（std 把它缓存住了），
/// 于是 `println!` 写出去直接失败。我们又用 `let _ =` 忽略错误，
/// 结果就是**什么都不输出**（`--help` 会一片空白）。
///
/// 所以这里绕开 std 的缓存，自己打开 `CONOUT$` 来写。
/// 双击运行时借不到控制台，打开失败 —— 静默丢弃，这是预期行为。
fn console_print(text: &str) {
    use std::io::Write;

    // 第一种情况：stdout 是可用的。
    //
    // 从 `cmd` 里启动时，cmd 会把它的标准句柄传给子进程；
    // 被重定向到文件或管道时更是如此（`PMCA-Installer.exe --version > v.txt`）。
    // 这时候直接用 stdout 就行 —— 而且**必须**用 stdout，
    // 否则重定向就失效了（写去控制台而不是文件，脚本就拿不到输出）。
    {
        let mut out = std::io::stdout();
        if out.write_all(text.as_bytes()).is_ok() && out.flush().is_ok() {
            return;
        }
    }

    // 第二种情况：stdout 用不了，但进程其实有个控制台可以"借"。
    // 打开 CONOUT$ 绕开 Rust 缓存住的无效句柄。
    // 双击运行时两种都不成立 —— 静默丢弃，这是预期行为。
    attach_parent_console();
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
        let _ = f.write_all(text.as_bytes());
        let _ = f.flush();
    }
}

/// 打印版本号
fn print_version() {
    console_print(&format!("PMCA 安装器 v{}\n", sony_gui::VERSION));
}

/// 打印用法
fn print_help() {
    console_print(&format!(
        "PMCA 安装器 v{}\n\
         索尼相机应用安装工具\n\
         \n\
         用法：\n\
         \x20 PMCA-Installer.exe [应用包路径]\n\
         \x20 PMCA-Installer.exe --version   查看版本\n\
         \n\
         不带参数直接双击运行也可以，在界面里点「选择文件…」即可。\n\
         也可以把 APK 文件直接拖进窗口，或拖到本程序的图标上。\n",
        sony_gui::VERSION
    ));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 内嵌的窗口图标数据必须与声明的尺寸对得上。
    ///
    /// 这个文件是脚本生成的，万一尺寸和常量不一致，
    /// `from_rgba` 会失败、图标静默消失 —— 加个测试盯住它。
    #[test]
    fn window_icon_data_matches_declared_size() {
        let expected = (WINDOW_ICON_SIZE * WINDOW_ICON_SIZE * 4) as usize;
        assert_eq!(
            WINDOW_ICON_RGBA.len(),
            expected,
            "icon-rgba.bin 大小不对：期望 {expected} 字节（{WINDOW_ICON_SIZE}×{WINDOW_ICON_SIZE} RGBA），\
             实际 {} 字节。改了图标请重跑 python tools/make_icon.py",
            WINDOW_ICON_RGBA.len()
        );
    }

    /// 用真实数据构造一次，确认 iced 能接受
    #[test]
    fn window_icon_can_be_built() {
        assert!(window_icon().is_some(), "内嵌的图标数据应当能构造出 Icon");
    }
}
