//! 字体：内嵌的界面字体 + 系统字体（两层）。
//!
//! # 为什么要两层
//!
//! | 需求 | 用什么 |
//! |---|---|
//! | **界面文字**必须永远能显示 | **内嵌**一个只含界面用字的子集（58 KB） |
//! | **任意内容**（用户选的 APK 文件名可能是任意中文） | **系统字体**（几 MB，但不占 exe 体积） |
//!
//! 完整的中文字体都很大：
//!
//! | 字体 | 大小 |
//! |---|---|
//! | msyh.ttc（微软雅黑） | 18.8 MB |
//! | simsun.ttc（宋体） | 17.2 MB |
//! | Deng.ttf（等线） | 15.6 MB |
//! | simhei.ttf（黑体） | 9.3 MB |
//!
//! 内嵌任意一个，exe 都会从 4 MB 膨胀到 20 MB 以上 —— 和"体积小、启动快"直接冲突。
//!
//! 但**完全不内嵌**也有风险：万一用户机器上没有中文字体（精简版系统、
//! 非中文语言的 Windows），整个界面就全是方框，连"出了什么问题"都看不出来。
//!
//! 所以折中成两层：
//!
//! - **界面用字是固定的一小撮**（355 个字符），单独裁成一个 58 KB 的小字体内嵌。
//!   这样**界面永远能正常显示**，而且几乎不增加体积。
//! - **系统字体照常加载**，用于界面之外的任意内容（比如文件名里的生僻字）。
//!   字体引擎遇到子集里没有的字会自动回退过去。
//!
//! 子集用 `tools/make_ui_font.py` 生成，改了界面文字后重跑一次即可。

/// 内嵌的界面字体（只含界面用到的字，58 KB）
///
/// 由 `python tools/make_ui_font.py` 生成。
const UI_FONT_BYTES: &[u8] = include_bytes!("../assets/ui-font.ttf");

/// 内嵌字体的族名。
///
/// ⚠️ 必须和系统字体（`Microsoft YaHei`）**区分开**：
/// 如果两个同名字体都加载进去，它们会互相覆盖，回退就失效了。
/// 生成脚本里已经把族名改成这个。
const UI_FONT_FAMILY: &str = "InstallerUI";

/// 候选的系统字体：(文件路径, 字体族名)
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

/// 找到的系统字体
pub struct SystemFont {
    /// 字体文件内容
    pub bytes: Vec<u8>,
    /// 字体族名
    pub family: &'static str,
    /// 文件路径（写进日志，便于排查）
    pub path: &'static str,
}

/// 从系统里找一个可用的中文字体
pub fn load_system_cjk() -> Option<SystemFont> {
    for (path, family) in CANDIDATES {
        if !std::path::Path::new(path).exists() {
            continue;
        }
        match std::fs::read(path) {
            Ok(bytes) if !bytes.is_empty() => {
                return Some(SystemFont {
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

/// 系统字体的可用情况（只影响"界面之外的内容"，界面本身有内嵌字体保底）
#[derive(Debug, Clone, Copy)]
pub enum FontStatus {
    /// 内嵌界面字体 + 系统字体都可用（最理想）
    Full {
        system_name: &'static str,
        system_path: &'static str,
    },
    /// 只有内嵌的界面字体。
    /// 界面文字正常，但文件名里的生僻字可能显示成方框 —— 会在日志里说明。
    UiOnly,
}

/// 启动时准备好的字体方案
pub struct FontSetup {
    /// 要交给 iced 的所有字体数据
    pub blobs: Vec<Vec<u8>>,
    /// 界面用的字体集合
    pub ui: Fonts,
    /// 状态（写日志用）
    pub status: FontStatus,
}

/// 准备字体：内嵌的界面字体 + （如果有）系统字体
pub fn setup() -> FontSetup {
    let mut blobs = vec![UI_FONT_BYTES.to_vec()];
    let status = match load_system_cjk() {
        Some(sys) => {
            blobs.push(sys.bytes);
            FontStatus::Full {
                system_name: sys.family,
                system_path: sys.path,
            }
        }
        None => FontStatus::UiOnly,
    };
    FontSetup {
        blobs,
        // 界面一律用**内嵌**字体，保证不管系统有没有中文字体都能正常显示
        ui: Fonts::from_family(UI_FONT_FAMILY),
        status,
    }
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

    /// 界面字体的默认族名（给 iced 的 `default_font` 用）
    pub fn default_family() -> &'static str {
        UI_FONT_FAMILY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_ui_font_is_present_and_small() {
        // 字体确实内嵌进来了，而且确实是个字体文件（TrueType 魔数 0x00010000）
        assert!(UI_FONT_BYTES.len() > 10_000, "内嵌字体不该是空的");
        assert!(
            UI_FONT_BYTES.len() < 400 * 1024,
            "内嵌字体要足够小，不能把 exe 撑大"
        );
        let magic = u32::from_be_bytes([
            UI_FONT_BYTES[0],
            UI_FONT_BYTES[1],
            UI_FONT_BYTES[2],
            UI_FONT_BYTES[3],
        ]);
        assert!(
            magic == 0x0001_0000 || magic == u32::from_be_bytes(*b"true"),
            "不是合法的 TrueType 字体，魔数 = 0x{magic:08x}"
        );
    }

    #[test]
    fn setup_always_provides_the_ui_font() {
        let s = setup();
        assert!(!s.blobs.is_empty(), "至少要有内嵌的界面字体");
        // 第一个永远是内嵌的界面字体
        assert_eq!(s.blobs[0].len(), UI_FONT_BYTES.len());
        // 界面字体族名不能和系统字体撞名，否则回退会失效
        assert_ne!(UI_FONT_FAMILY, "Microsoft YaHei");
    }
}
