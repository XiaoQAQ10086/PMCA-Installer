//! 中文字体。
//!
//! # 为什么不在 exe 里内嵌字体
//!
//! Windows 自带的中文字体都很大（微软雅黑 18.8 MB、黑体 9.3 MB）：
//!
//! | 字体 | 大小 |
//! |---|---|
//! | msyh.ttc（微软雅黑） | 18.8 MB |
//! | simhei.ttf（黑体） | 9.3 MB |
//! | Deng.ttf（等线） | 15.6 MB |
//!
//! 内嵌任何一个都会让 exe 从 0.85 MB 膨胀到 10 MB 以上，启动也变慢 ——
//! 和"体积小、启动快"的目标直接冲突。
//!
//! 只内嵌"界面用到的那些字"也不行：用户选的 APK 文件名可能是任意中文
//! （甚至生僻字），子集化之后那些字就会变成方框。
//!
//! # 做法：启动时从系统加载
//!
//! 按优先级找一个系统里的中文字体读进来。Windows 10/11 各版本都自带微软雅黑，
//! 所以这条路非常稳；万一都没有，界面照样能开，只是中文会显示成方框，
//! 我们会在日志里提示。

/// 候选字体：(文件路径, 字体族名)
///
/// 顺序 = 优先级。微软雅黑是 Win10/11 的默认界面字体，观感最好。
const CANDIDATES: &[(&str, &str)] = &[
    (r"C:\Windows\Fonts\msyh.ttc", "Microsoft YaHei"),
    (r"C:\Windows\Fonts\msyh.ttf", "Microsoft YaHei"),
    (r"C:\Windows\Fonts\msyhl.ttc", "Microsoft YaHei Light"),
    (r"C:\Windows\Fonts\Deng.ttf", "DengXian"),
    (r"C:\Windows\Fonts\simhei.ttf", "SimHei"),
    (r"C:\Windows\Fonts\simsun.ttc", "SimSun"),
];

/// 找到的中文字体
pub struct CjkFont {
    /// 字体文件内容
    pub bytes: Vec<u8>,
    /// 字体族名（给 iced 指定默认字体用）
    pub family: &'static str,
    /// 文件路径（显示在日志里，便于排查）
    pub path: &'static str,
}

/// 从系统里找一个可用的中文字体
pub fn load_system_cjk() -> Option<CjkFont> {
    for (path, family) in CANDIDATES {
        if !std::path::Path::new(path).exists() {
            continue;
        }
        match std::fs::read(path) {
            Ok(bytes) if !bytes.is_empty() => {
                return Some(CjkFont {
                    bytes,
                    family,
                    path,
                });
            }
            _ => continue,
        }
    }
    None
}

/// 构造一个指定字体族的 iced 字体
pub fn font_of(family: &'static str) -> iced::Font {
    iced::Font {
        family: iced::font::Family::Name(family),
        ..iced::Font::default()
    }
}

/// 粗体
pub fn bold_of(family: &'static str) -> iced::Font {
    iced::Font {
        family: iced::font::Family::Name(family),
        weight: iced::font::Weight::Bold,
        ..iced::Font::default()
    }
}

/// 界面各处要用的字体集合。
///
/// 全局存一份，省得每个组件都传参 —— 字体族在程序运行期间不会变。
#[derive(Debug, Clone, Copy)]
pub struct Fonts {
    pub regular: iced::Font,
    pub bold: iced::Font,
}

impl Fonts {
    pub fn from_family(family: &'static str) -> Self {
        Self {
            regular: font_of(family),
            bold: bold_of(family),
        }
    }

    /// 没有中文字体时的兜底（用 iced 内置字体，中文会显示成方框）
    pub fn fallback() -> Self {
        Self {
            regular: iced::Font::default(),
            bold: iced::Font {
                weight: iced::font::Weight::Bold,
                ..iced::Font::default()
            },
        }
    }
}
