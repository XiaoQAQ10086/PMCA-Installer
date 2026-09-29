//! 作者链接的图标。
//!
//! # 为什么不用图片或 SVG
//!
//! 图片要给 iced 开 `image` 特性（会拉进 `image` 解码库），
//! SVG 要开 `svg` 特性（会拉进 `resvg`，那个不小）。
//!
//! 而这三个图标都是**简单几何图形** —— 一朵云、一个音符、一台小电视。
//! 用 iced 自带的 `canvas` 直接画，**一个新依赖都不用加**，
//! 而且在任何缩放下都是清晰的矢量效果。

use iced::widget::canvas::{self, Frame, Path, Program, Stroke};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme};

// ---------------------------------------------------------------- 品牌信息

/// 作者的三个链接
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brand {
    /// 夸克网盘（放应用的地方）
    Netdisk,
    /// 抖音
    Douyin,
    /// 哔哩哔哩
    Bilibili,
}

impl Brand {
    /// 按顺序摆在标题栏里的三个按钮
    pub const ALL: [Brand; 3] = [Brand::Netdisk, Brand::Douyin, Brand::Bilibili];

    /// 按钮上写什么
    pub fn label(self) -> &'static str {
        match self {
            Brand::Netdisk => "作者网盘",
            Brand::Douyin => "作者抖音",
            Brand::Bilibili => "作者B站",
        }
    }

    /// 鼠标停上去显示的话（说明点了会去哪儿）
    pub fn tooltip(self) -> &'static str {
        match self {
            Brand::Netdisk => "作者的夸克网盘",
            Brand::Douyin => "作者的抖音主页",
            Brand::Bilibili => "作者的 B 站主页",
        }
    }

    /// 点开之后用浏览器打的地址。
    ///
    /// ⚠️ 这里**故意去掉了分享链接末尾的 `?from_tab_name=…&vid=…` 那一串**：
    /// 那些是复制链接时带上的会话参数（`vid` 甚至是某一个具体视频的编号），
    /// 留着的话别人点开可能落到某个视频上，而不是主页。
    pub fn url(self) -> &'static str {
        match self {
            Brand::Netdisk => "https://pan.quark.cn/s/d351774c9a16",
            Brand::Douyin => {
                "https://www.douyin.com/user/MS4wLjABAAAA8FdJN02m2c0E7n6Hm-vAdyBus6OxreOePrDWQHucoq7c5pmkVqBzjCu09qdjFN0b"
            }
            Brand::Bilibili => "https://space.bilibili.com/627382426",
        }
    }

    /// 图标的颜色 —— 用各家自己的品牌色，让人一眼认出来
    pub fn color(self) -> Color {
        match self {
            // 网盘：中性蓝，和"云"的直觉一致
            Brand::Netdisk => Color::from_rgb(0.153, 0.404, 0.741),
            // 抖音：官方是黑底白标；这里描边用近黑，浅色背景上看得清
            Brand::Douyin => Color::from_rgb(0.09, 0.09, 0.10),
            // B 站：官方粉 #FB7299
            Brand::Bilibili => Color::from_rgb(0.984, 0.447, 0.600),
        }
    }
}

// ---------------------------------------------------------------- 图标绘制

/// 一个可以塞进按钮里的小图标
pub struct Icon {
    brand: Brand,
}

impl Icon {
    pub fn new(brand: Brand) -> Self {
        Self { brand }
    }
}

impl<Message> Program<Message> for Icon {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        // 所有坐标都按这个边长算，这样图标在任何尺寸下都是等比缩放的
        let s = bounds.width.min(bounds.height);
        let color = self.brand.color();

        match self.brand {
            Brand::Netdisk => draw_cloud(&mut frame, s, color),
            Brand::Douyin => draw_douyin(&mut frame, s, color),
            Brand::Bilibili => draw_bilibili(&mut frame, s, color),
        }

        vec![frame.into_geometry()]
    }
}

/// 把 0..1 的相对坐标换算成实际坐标
fn pt(s: f32, x: f32, y: f32) -> Point {
    Point::new(x * s, y * s)
}

/// 网盘：一朵云
///
/// 三个叠在一起的圆 + 底下一条横杠 —— 叠着画就自然连成一朵云了。
fn draw_cloud(frame: &mut Frame, s: f32, color: Color) {
    let blobs = [
        (0.32, 0.62, 0.19), // 左
        (0.53, 0.45, 0.26), // 中间最大
        (0.72, 0.62, 0.17), // 右
    ];
    for (x, y, r) in blobs {
        frame.fill(&Path::circle(pt(s, x, y), r * s), color);
    }
    // 把三个圆的底部填平，免得下面三个缺口看起来像三个球
    frame.fill(
        &Path::rectangle(
            pt(s, 0.32, 0.62),
            Size::new(0.40 * s, 0.19 * s),
        ),
        color,
    );
}

/// 抖音：一个音符
///
/// 官方的标是个带钩的音符，这里保留最能认出来的特征：
/// 左边的音符头、右边的竖杆、顶上向右甩出去的钩。
fn draw_douyin(frame: &mut Frame, s: f32, color: Color) {
    // 竖杆
    frame.fill(
        &Path::rectangle(pt(s, 0.52, 0.14), Size::new(0.15 * s, 0.56 * s)),
        color,
    );
    // 音符头（左下那个圆）
    frame.fill(&Path::circle(pt(s, 0.38, 0.70), 0.22 * s), color);

    // 顶上的钩：从竖杆顶端向右下方甩出去
    let hook = Path::new(|b| {
        b.move_to(pt(s, 0.60, 0.21));
        b.quadratic_curve_to(pt(s, 0.70, 0.48), pt(s, 0.88, 0.52));
    });
    frame.stroke(
        &hook,
        Stroke::default().with_color(color).with_width(0.13 * s),
    );
}

/// B站：一台带天线的小电视
///
/// 两根天线 + 圆角机身（描边）+ 两只眼睛 —— 这是 B 站标最好认的部分。
fn draw_bilibili(frame: &mut Frame, s: f32, color: Color) {
    let stroke = Stroke::default().with_color(color).with_width(0.085 * s);

    // 两根天线
    let left = Path::line(pt(s, 0.26, 0.16), pt(s, 0.41, 0.32));
    let right = Path::line(pt(s, 0.74, 0.16), pt(s, 0.59, 0.32));
    frame.stroke(&left, stroke);
    frame.stroke(&right, stroke);

    // 机身：圆角矩形，只描边不填充（填充版在浅色背景上会糊成一坨黑）
    let body = Path::rounded_rectangle(
        pt(s, 0.10, 0.30),
        Size::new(0.80 * s, 0.54 * s),
        (0.14 * s).into(),
    );
    frame.stroke(&body, stroke);

    // 两只眼睛
    frame.fill(&Path::circle(pt(s, 0.36, 0.57), 0.058 * s), color);
    frame.fill(&Path::circle(pt(s, 0.64, 0.57), 0.058 * s), color);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三个链接都是 https，而且指向各自的平台
    #[test]
    fn urls_are_https_and_on_the_right_host() {
        for b in Brand::ALL {
            let url = b.url();
            assert!(url.starts_with("https://"), "{url} 不是 https");
        }
        assert!(Brand::Netdisk.url().contains("pan.quark.cn"));
        assert!(Brand::Douyin.url().contains("douyin.com/user/"));
        assert!(Brand::Bilibili.url().contains("space.bilibili.com/627382426"));
    }

    /// ⚠️ 分享链接末尾的会话参数必须去掉。
    ///
    /// 抖音那条原本带着 `?vid=7690930714358893824` —— 那是一个**具体视频**的编号，
    /// 留着的话别人点"作者抖音"会直接落到某个视频上，而不是主页。
    #[test]
    fn share_tracking_parameters_are_stripped() {
        for b in Brand::ALL {
            let url = b.url();
            assert!(!url.contains('?'), "{url} 还带着查询参数");
            assert!(!url.contains("from_tab_name"), "{url} 还带着分享参数");
            assert!(!url.contains("vid="), "{url} 还带着视频编号");
            assert!(!url.contains("spm_id_from"), "{url} 还带着跟踪参数");
        }
    }

    /// 三个按钮的文字各不相同，别复制粘贴错了
    #[test]
    fn labels_are_distinct() {
        let labels: Vec<_> = Brand::ALL.iter().map(|b| b.label()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "有重复的按钮文字：{labels:?}");
    }

    /// 每个图标都得有自己的品牌色（不然三个按钮看起来一样）
    #[test]
    fn colors_are_distinct() {
        let colors: Vec<_> = Brand::ALL.iter().map(|b| b.color()).collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "第 {i} 和第 {j} 个颜色一样");
            }
        }
    }
}
