//! 界面配色与控件样式（Windows 11 Fluent 风格）。
//!
//! 配色直接取自 Windows 11 的 Fluent 设计规范：
//! - 窗口底色 `#F3F3F3`（比纯白柔一点，卡片才能"浮"起来）
//! - 卡片纯白 + 1 像素浅灰描边 + 8 像素圆角
//! - 强调色 `#0067C0`（Win11 默认蓝）
//!
//! 所有样式都写成 `fn(&Theme) -> Style` 的闭包形式，直接传给 `.style(...)`。

use iced::border::Border;
use iced::widget::{button, container, progress_bar};
use iced::{Background, Color, Theme};

// ---------------------------------------------------------------- 配色

/// 取色宏：写 `#RRGGBB` 比写四个小数直观得多
const fn rgb(hex: u32) -> Color {
    Color {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

/// 窗口底色
pub const WINDOW_BG: Color = rgb(0xF3F3F3);
/// 卡片底色
pub const CARD_BG: Color = rgb(0xFFFFFF);
/// 卡片描边
pub const CARD_BORDER: Color = rgb(0xE5E5E5);
/// 主要文字
pub const TEXT: Color = rgb(0x1A1A1A);
/// 次要文字（说明、标签）
pub const TEXT_WEAK: Color = rgb(0x6B6B6B);
/// 更弱的文字（提示）
pub const TEXT_FAINT: Color = rgb(0x8A8A8A);
/// 强调色（Win11 蓝）
pub const ACCENT: Color = rgb(0x0067C0);
/// 强调色悬停
pub const ACCENT_HOVER: Color = rgb(0x1A7AD1);
/// 强调色按下
pub const ACCENT_PRESSED: Color = rgb(0x005BA4);
/// 成功绿
pub const SUCCESS: Color = rgb(0x0F7B0F);
/// 警告橙
pub const WARNING: Color = rgb(0x9D5D00);
/// 错误红
pub const DANGER: Color = rgb(0xC42B1C);
/// 中性按钮底色
pub const NEUTRAL_BG: Color = rgb(0xFDFDFD);
/// 中性按钮悬停
pub const NEUTRAL_HOVER: Color = rgb(0xF5F5F5);
/// 中性按钮按下
pub const NEUTRAL_PRESSED: Color = rgb(0xEDEDED);
/// 禁用态
pub const DISABLED_BG: Color = rgb(0xE8E8E8);
/// 禁用态文字
pub const DISABLED_TEXT: Color = rgb(0xA0A0A0);

/// 圆角半径
pub const RADIUS: f32 = 8.0;
/// 小圆角
pub const RADIUS_SM: f32 = 4.0;

// ---------------------------------------------------------------- 主题

/// 构造应用主题（浅色 Fluent）
pub fn theme() -> Theme {
    Theme::custom(
        "Fluent Light".to_string(),
        iced::theme::Palette {
            background: WINDOW_BG,
            text: TEXT,
            primary: ACCENT,
            success: SUCCESS,
            warning: WARNING,
            danger: DANGER,
        },
    )
}

// ---------------------------------------------------------------- 容器样式

/// 窗口根容器：铺满底色
/// 对话框背后的半透明遮罩。
///
/// 用半透明黑而不是纯色：底下的界面还看得见，用户知道"主界面还在，
/// 只是现在被这个对话框挡住了"，不会以为程序跳到别处去了。
pub fn scrim(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Color { a: 0.45, ..Color::BLACK }.into()),
        ..container::Style::default()
    }
}

pub fn root(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(WINDOW_BG)),
        ..container::Style::default()
    }
}

/// 卡片：白底 + 浅灰描边 + 圆角
pub fn card(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(CARD_BG)),
        border: Border {
            color: CARD_BORDER,
            width: 1.0,
            radius: RADIUS.into(),
        },
        ..container::Style::default()
    }
}

/// 内嵌的浅色块（例如显示文件路径的地方）
pub fn inset(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(rgb(0xFAFAFA))),
        border: Border {
            color: rgb(0xEDEDED),
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }
}

/// 状态色圆点
pub fn dot(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            color,
            width: 0.0,
            radius: 99.0.into(),
        },
        ..container::Style::default()
    }
}

/// 成功提示条
pub fn banner_success(_theme: &Theme) -> container::Style {
    banner(rgb(0xF0F9F0), rgb(0xC3E8C3))
}

/// 错误提示条
pub fn banner_danger(_theme: &Theme) -> container::Style {
    banner(rgb(0xFDF3F2), rgb(0xF3C9C4))
}

/// 警告提示条
pub fn banner_warning(_theme: &Theme) -> container::Style {
    banner(rgb(0xFFF9F0), rgb(0xF5DFB8))
}


fn banner(bg: Color, border: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(bg)),
        border: Border {
            color: border,
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }
}

// ---------------------------------------------------------------- 按钮样式

/// 主按钮（强调色实心）
pub fn primary_button(theme: &Theme, status: button::Status) -> button::Style {
    let base = button::Style {
        background: Some(Background::Color(ACCENT)),
        text_color: Color::WHITE,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: RADIUS_SM.into(),
        },
        ..button::Style::default()
    };
    let _ = theme;
    match status {
        button::Status::Hovered => button::Style {
            background: Some(Background::Color(ACCENT_HOVER)),
            ..base
        },
        button::Status::Pressed => button::Style {
            background: Some(Background::Color(ACCENT_PRESSED)),
            ..base
        },
        button::Status::Disabled => button::Style {
            background: Some(Background::Color(DISABLED_BG)),
            text_color: DISABLED_TEXT,
            ..base
        },
        _ => base,
    }
}

/// 次按钮（白底描边）
pub fn secondary_button(_theme: &Theme, status: button::Status) -> button::Style {
    let base = button::Style {
        background: Some(Background::Color(NEUTRAL_BG)),
        text_color: TEXT,
        border: Border {
            color: rgb(0xD6D6D6),
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..button::Style::default()
    };
    match status {
        button::Status::Hovered => button::Style {
            background: Some(Background::Color(NEUTRAL_HOVER)),
            ..base
        },
        button::Status::Pressed => button::Style {
            background: Some(Background::Color(NEUTRAL_PRESSED)),
            ..base
        },
        button::Status::Disabled => button::Style {
            background: Some(Background::Color(DISABLED_BG)),
            text_color: DISABLED_TEXT,
            border: Border {
                color: rgb(0xDCDCDC),
                ..base.border
            },
            ..base
        },
        _ => base,
    }
}


// ---------------------------------------------------------------- 进度条样式

/// 进度条：圆角、强调色填充
pub fn progress(theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(rgb(0xE8E8E8)),
        bar: Background::Color(if theme.extended_palette().is_dark {
            ACCENT_HOVER
        } else {
            ACCENT
        }),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 99.0.into(),
        },
    }
}

// ---------------------------------------------------------------- 字号

/// 标题字号
pub const SIZE_TITLE: f32 = 22.0;
/// 小标题 / 按钮
pub const SIZE_HEADING: f32 = 15.0;
/// 正文字号
pub const SIZE_BODY: f32 = 13.0;
/// 说明字号
pub const SIZE_CAPTION: f32 = 12.0;
/// 日志字号
pub const SIZE_LOG: f32 = 11.5;


