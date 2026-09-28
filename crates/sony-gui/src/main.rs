//! 索尼相机应用安装器 —— 图形界面。
//!
//! 用法：双击运行 → 选一个 APK → 点「开始安装」。
//! 相机模式是全自动切换的，用户不需要去相机菜单里做任何设置。
//!
//! # 界面技术选型上的两个决定
//!
//! 1. **渲染用纯 CPU（tiny-skia）**，不用显卡（wgpu）。
//!    这是一个小工具窗口，CPU 渲染完全够用，而且：
//!    - 不需要显卡驱动配合，老机器/虚拟机也能开
//!    - 编出来的 exe 小得多、启动更快
//!
//! 2. **中文字体从系统加载**，不内嵌。
//!    Windows 的中文字体都很大（微软雅黑 18.8 MB），
//!    内嵌会让 exe 从不到 1 MB 膨胀到 10 MB 以上。
//!    详见 `fonts.rs`。

mod app;
mod fonts;
mod theme;
mod worker;

use iced::window;
use iced::Size;

use app::{App, FontStatus, Message};
use fonts::Fonts;

fn main() -> iced::Result {
    // ---- 找一个系统中文字体 ----
    // 找不到也照常启动，只是中文会变成方框，界面上会有提示。
    let (font_bytes, fonts, font_status) = if let Some(f) = fonts::load_system_cjk() {
        let status = FontStatus::Loaded {
            name: f.family,
            path: f.path,
        };
        (Some(f.bytes), Fonts::from_family(f.family), status)
    } else {
        (None, Fonts::fallback(), FontStatus::Missing)
    };

    let mut application = iced::application(
        move || {
            let mut a = App::new(fonts, font_status);
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
    .default_font(fonts.regular)
    .window(window::Settings {
        size: Size::new(760.0, 660.0),
        min_size: Some(Size::new(620.0, 520.0)),
        resizable: true,
        ..window::Settings::default()
    })
    .subscription(|_: &App| {
        // 每 100 毫秒轮询一次后台线程。
        // 用轮询而不是回调，是因为它最简单也最不容易出错 ——
        // 界面永远不会被后台卡住。
        iced::time::every(std::time::Duration::from_millis(100)).map(|_| Message::Tick)
    })
    .antialiasing(true);

    if let Some(bytes) = font_bytes {
        application = application.font(bytes);
    }

    application.run()
}
