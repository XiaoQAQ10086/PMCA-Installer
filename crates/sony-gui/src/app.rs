//! 界面状态机与视图。
//!
//! 整体是 iced 的"单向数据流"结构：
//!
//! ```text
//!   Message ──► update(&mut App, Message) ──► 改状态
//!                    │
//!                    └──► view(&App) ──► 画面
//! ```
//!
//! 界面本身**不碰相机**：所有相机操作都丢给后台线程（见 `worker.rs`），
//! 界面只负责显示它报上来的结果。这样窗口永远不会卡死。

use std::sync::mpsc::{Receiver, TryRecvError};

use iced::widget::{Space, button, column, container, progress_bar, row, scrollable, space, text};
use iced::{Alignment, Element, Length};

use crate::fonts::Fonts;
use crate::theme as th;
use crate::worker::{self, ApkChoice, WorkerMsg};

/// 日志最多留多少行（防止长时间运行把内存撑起来）
const MAX_LOG_LINES: usize = 400;

// ---------------------------------------------------------------- 状态

/// 相机的检测状态
#[derive(Debug, Clone)]
pub enum Camera {
    /// 还没检测过
    Unknown,
    /// 正在检测
    Checking,
    /// 没找到（附原因）
    Missing(String),
    /// 找到了
    Found(sony_install::CameraStatus),
}

/// 安装处在哪一步
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 空闲，可以开始
    Idle,
    /// 正在准备（检测相机、切换模式等）
    Preparing,
    /// 正在安装
    Installing,
    /// 安装成功
    Done,
    /// 失败
    Failed,
}

impl Stage {
    /// 是否正在进行中（进行中要禁用所有按钮，避免同时操作相机）
    fn busy(self) -> bool {
        matches!(self, Stage::Preparing | Stage::Installing)
    }
}

/// 一行日志的级别（决定颜色）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Good,
    Warn,
    Bad,
}

// 字体状态定义在 `fonts` 模块里（那边才知道内嵌/系统两层的细节），
// 这里只是转出去，免得调用方要去两个地方找。
pub use crate::fonts::FontStatus;

pub struct App {
    pub fonts: Fonts,
    camera: Camera,
    apk: Option<ApkChoice>,
    stage: Stage,
    percent: u8,
    status: String,
    log: Vec<(Level, String)>,
    /// 安装/检测线程的消息
    rx: Option<Receiver<WorkerMsg>>,
    /// 文件选择对话框的结果
    pick_rx: Option<Receiver<Option<ApkChoice>>>,
    /// 安装成功后相机回报的完整信息
    report: Option<String>,
    font_status: FontStatus,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// 检测相机
    CheckCamera,
    /// 打开"选择应用包"对话框
    PickApk,
    /// 开始安装
    Start,
    /// 定时轮询后台线程
    Tick,
    /// 清空运行日志
    ClearLog,
    /// 用户把文件拖进了窗口
    ApkDropped(std::path::PathBuf),
    /// 不关心的窗口事件（定位、缩放等），什么都不做
    Ignored,
}

impl App {
    pub fn new(fonts: Fonts, font_status: FontStatus) -> Self {
        let mut app = Self {
            fonts,
            camera: Camera::Unknown,
            apk: None,
            stage: Stage::Idle,
            percent: 0,
            status: String::new(),
            log: Vec::new(),
            rx: None,
            pick_rx: None,
            report: None,
            font_status,
        };
        match font_status {
            FontStatus::Full {
                system_name,
                system_path,
            } => {
                app.push_log(
                    Level::Info,
                    format!("界面字体：已内嵌；系统字体：{system_name}（{system_path}）"),
                );
            }
            FontStatus::UiOnly => {
                app.push_log(
                    Level::Info,
                    "界面字体：已内嵌（界面显示不受影响）",
                );
                app.push_log(
                    Level::Warn,
                    "没有找到系统中文字体：文件名里的生僻字可能显示为方框",
                );
            }
        }
        app
    }

    /// 启动时立即检测一次相机
    pub fn boot_task(&mut self) {
        self.begin_check();
    }

    /// 预选一个应用包（命令行参数传进来的）
    ///
    /// 这样用户可以直接把 APK 拖到 exe 上，或者用
    /// `索尼相机应用安装器.exe 某个应用.apk` 打开，省掉一次点击。
    pub fn set_initial_apk(&mut self, path: std::path::PathBuf) {
        if !path.exists() {
            self.push_log(
                Level::Warn,
                format!("命令行给的文件不存在，已忽略：{}", path.display()),
            );
            return;
        }
        self.accept_apk(path);
    }

    /// 接受一个应用包（文件选择、拖放、命令行参数三条路都走这里）
    fn accept_apk(&mut self, path: std::path::PathBuf) {
        let choice = ApkChoice::from_path(path);
        self.push_log(
            Level::Info,
            format!("已选择 {}（{}）", choice.name, human_size(choice.size)),
        );
        self.apk = Some(choice);
        // 换包之后把上一次的安装结果清掉，免得混淆
        if matches!(self.stage, Stage::Done | Stage::Failed) {
            self.reset();
        }
    }

    fn push_log(&mut self, level: Level, text: impl Into<String>) {
        self.log.push((level, text.into()));
        if self.log.len() > MAX_LOG_LINES {
            let drop = self.log.len() - MAX_LOG_LINES;
            self.log.drain(..drop);
        }
    }

    // ------------------------------------------------------------ 动作

    fn begin_check(&mut self) {
        self.camera = Camera::Checking;
        self.push_log(Level::Info, "正在检测相机…");
        self.rx = Some(worker::spawn_probe());
    }

    fn begin_pick(&mut self) {
        if self.pick_rx.is_some() {
            return;
        }
        self.pick_rx = Some(worker::spawn_pick_apk());
    }

    fn begin_install(&mut self) {
        // 先把要用的信息取出来（后面要改 self，不能一直借着）
        let Some((path, name, size)) = self
            .apk
            .as_ref()
            .map(|a| (a.path.clone(), a.name.clone(), a.size))
        else {
            return;
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                self.stage = Stage::Failed;
                self.status = format!("读取文件失败：{e}");
                self.push_log(Level::Bad, format!("读取 {} 失败：{e}", path.display()));
                return;
            }
        };
        self.stage = Stage::Preparing;
        self.percent = 0;
        self.status = "正在准备…".into();
        self.report = None;
        self.push_log(
            Level::Info,
            format!("开始安装 {name}（{}）", human_size(size)),
        );
        self.rx = Some(worker::spawn_install(bytes, name));
    }

    /// 把界面恢复到"可以再装一次"的状态（不清日志）
    fn reset(&mut self) {
        self.stage = Stage::Idle;
        self.percent = 0;
        self.status.clear();
        self.report = None;
    }

    // ------------------------------------------------------------ 消息处理

    pub fn update(&mut self, message: Message) {
        match message {
            Message::CheckCamera => {
                if !self.stage.busy() {
                    self.begin_check();
                }
            }
            Message::PickApk => {
                if !self.stage.busy() {
                    self.begin_pick();
                }
            }
            Message::Start => {
                if !self.stage.busy() {
                    self.begin_install();
                }
            }
            Message::ClearLog => {
                self.log.clear();
                self.push_log(Level::Info, "日志已清空");
            }
            Message::ApkDropped(path) => {
                if !self.stage.busy() {
                    self.accept_apk(path);
                }
            }
            Message::Tick => self.drain_workers(),
            Message::Ignored => {}
        }
    }

    /// 把后台线程已经报上来的消息全部取出来（不阻塞）
    fn drain_workers(&mut self) {
        // ---- 文件选择 ----
        let mut picked: Option<Option<ApkChoice>> = None;
        if let Some(rx) = &self.pick_rx {
            match rx.try_recv() {
                Ok(v) => picked = Some(v),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => picked = Some(None),
            }
        }
        if let Some(v) = picked {
            self.pick_rx = None;
            // 用户取消时 v 是 None，什么都不做
            if let Some(choice) = v {
                self.accept_apk(choice.path);
            }
        }
        // ---- 相机 / 安装线程 ----
        let mut msgs = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(m) => msgs.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for m in msgs {
            self.on_worker(m);
        }
        if disconnected {
            self.rx = None;
            // 线程没了但状态还停在"进行中"，说明出了意外
            if self.stage.busy() {
                self.stage = Stage::Failed;
                self.status = "操作意外中断（后台线程已退出）".into();
                self.push_log(Level::Bad, "操作意外中断：后台线程已退出");
            }
        }
    }

    fn on_worker(&mut self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::Camera(Ok(Some(status))) => {
                self.push_log(
                    Level::Good,
                    format!(
                        "找到相机 {}（{}），当前：{}",
                        status.model,
                        status.serial,
                        status.mode_text()
                    ),
                );
                self.camera = Camera::Found(status);
            }
            WorkerMsg::Camera(Ok(None)) => {
                let why = "没有检测到索尼相机。请确认相机已开机、USB 线已插好。".to_string();
                self.push_log(Level::Warn, &why);
                self.camera = Camera::Missing(why);
            }
            WorkerMsg::Camera(Err(e)) => {
                self.push_log(Level::Bad, format!("检测相机失败：{e}"));
                self.camera = Camera::Missing(e);
            }
            WorkerMsg::Step(s) => {
                self.push_log(Level::Info, &s);
                if self.stage == Stage::Preparing {
                    self.status = s.clone();
                }
            }
            WorkerMsg::Progress { percent, text } => {
                self.percent = percent;
                self.status = text.clone();
                if self.stage == Stage::Preparing {
                    self.stage = Stage::Installing;
                }
                self.push_log(Level::Info, text);
            }
            WorkerMsg::Finished(Ok(outcome)) => {
                self.stage = Stage::Done;
                self.percent = 100;
                self.status = "安装完成".into();
                self.rx = None;
                self.push_log(Level::Good, "✅ 安装完成：相机已报告成功");
                if let Some(r) = &outcome.device_report {
                    // 只把关键字段摘出来，完整 JSON 太长
                    self.push_log(Level::Info, format!("相机信息：{}", summarize(r)));
                }
                self.report = outcome.device_report;
                // 安装完成后相机会复位回普通模式。
                // 这里**不**自动重新检测：刚装完相机还在复位中，立刻去开会话
                // 有可能失败，界面上就会跳出"未找到相机"吓到用户。
                // 反正真正安装时 `sony-install` 会自己重新检测并切模式，
                // 所以显示暂时旧一点不影响任何功能。
                self.camera = Camera::Unknown;
                self.push_log(
                    Level::Info,
                    "相机已复位（下次安装会自动重新检测并切换模式）",
                );
            }
            WorkerMsg::Finished(Err(e)) => {
                self.stage = Stage::Failed;
                self.status = first_line(&e);
                self.rx = None;
                self.push_log(Level::Bad, format!("安装失败：{e}"));
            }
        }
    }

    // ------------------------------------------------------------ 只读查询
    //
    // 给集成测试用的（也方便以后加"导出诊断信息"之类的功能）。
    // 故意只暴露**只读**视图：外部改不了状态，状态只能通过 `update` 变 ——
    // 单向数据流不能被绕过。

    /// 当前处在哪一步
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// 当前状态文字
    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// 当前进度
    pub fn percent(&self) -> u8 {
        self.percent
    }

    /// 日志内容（从旧到新）
    pub fn log_lines(&self) -> impl Iterator<Item = &str> {
        self.log.iter().map(|(_, s)| s.as_str())
    }

    /// 是否已经选好了应用包
    pub fn has_apk(&self) -> bool {
        self.apk.is_some()
    }

    // ------------------------------------------------------------ 视图

    pub fn view(&self) -> Element<'_, Message> {
        let mut body = column![self.header()].spacing(12).width(Length::Fill);

        body = body.push(self.camera_card());
        body = body.push(self.apk_card());
        body = body.push(self.action_card());
        body = body.push(self.log_card());

        container(body)
            .padding(18)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(th::root)
            .into()
    }

    /// 顶部标题
    fn header(&self) -> Element<'_, Message> {
        let mut col = column![
            text("索尼相机应用安装器")
                .size(th::SIZE_TITLE)
                .font(self.fonts.bold)
                .color(th::TEXT),
            text("把 Android 应用装到索尼相机上 · 全自动识别相机")
                .size(th::SIZE_CAPTION)
                .font(self.fonts.regular)
                .color(th::TEXT_WEAK),
        ]
        .spacing(2);

        if matches!(self.font_status, FontStatus::UiOnly) {
            col = col.push(banner(
                self.fonts,
                th::banner_warning,
                th::WARNING,
                "系统里没有中文字体。界面显示不受影响，但文件名里的生僻字可能显示为方框。",
            ));
        }
        col.into()
    }

    /// ① 相机
    fn camera_card(&self) -> Element<'_, Message> {
        let (dot_color, title, detail) = match &self.camera {
            Camera::Unknown => (th::TEXT_FAINT, "尚未检测".to_string(), String::new()),
            Camera::Checking => (th::ACCENT, "正在检测…".to_string(), String::new()),
            Camera::Missing(why) => (th::DANGER, "未找到相机".to_string(), why.clone()),
            Camera::Found(s) => (
                if s.app_install_mode {
                    th::SUCCESS
                } else {
                    th::WARNING
                },
                format!("{}  {}", s.model, s.mode_text()),
                format!("序列号 {}　·　安装时会自动切换到应用安装模式", s.serial),
            ),
        };

        let dot = container(Space::new().width(10).height(10)).style(th::dot(dot_color));

        let mut info = column![row![
            dot,
            text(title)
                .size(th::SIZE_BODY)
                .font(self.fonts.bold)
                .color(th::TEXT)
        ]
        .spacing(8)
        .align_y(Alignment::Center)]
        .spacing(3);
        if !detail.is_empty() {
            info = info.push(
                text(detail)
                    .size(th::SIZE_CAPTION)
                    .font(self.fonts.regular)
                    .color(th::TEXT_WEAK),
            );
        }

        let refresh = button(
            text("重新检测")
                .size(th::SIZE_CAPTION)
                .font(self.fonts.regular),
        )
        .padding([6, 12])
        .style(th::secondary_button)
        .on_press_maybe((!self.stage.busy()).then_some(Message::CheckCamera));

        card(
            "① 相机",
            self.fonts,
            row![info, space::horizontal(), refresh]
                .align_y(Alignment::Center)
                .into(),
        )
    }

    /// ② 应用包
    fn apk_card(&self) -> Element<'_, Message> {
        let body: Element<'_, Message> = match &self.apk {
            Some(a) => row![
                column![
                    text(&a.name)
                        .size(th::SIZE_BODY)
                        .font(self.fonts.bold)
                        .color(th::TEXT),
                    text(format!("文件大小 {}", human_size(a.size)))
                        .size(th::SIZE_CAPTION)
                        .font(self.fonts.regular)
                        .color(th::TEXT_WEAK),
                ]
                .spacing(2)
                .width(Length::FillPortion(2)),
                container(
                    text(a.path.display().to_string())
                        .size(th::SIZE_CAPTION)
                        .font(self.fonts.regular)
                        .color(th::TEXT_WEAK),
                )
                .padding([4, 8])
                .width(Length::FillPortion(3))
                .style(th::inset),
                button(
                    text("换一个")
                        .size(th::SIZE_CAPTION)
                        .font(self.fonts.regular)
                )
                .padding([6, 12])
                .style(th::secondary_button)
                .on_press_maybe((!self.stage.busy()).then_some(Message::PickApk)),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into(),
            None => row![
                text("还没有选择应用包")
                    .size(th::SIZE_BODY)
                    .font(self.fonts.regular)
                    .color(th::TEXT_FAINT)
                    .width(Length::Fill),
                button(
                    text("选择文件…")
                        .size(th::SIZE_CAPTION)
                        .font(self.fonts.regular)
                )
                .padding([6, 12])
                .style(th::secondary_button)
                .on_press_maybe((!self.stage.busy()).then_some(Message::PickApk)),
            ]
            .align_y(Alignment::Center)
            .into(),
        };

        card("② 应用包（APK 文件）", self.fonts, body)
    }

    /// ③ 安装动作 + 进度
    fn action_card(&self) -> Element<'_, Message> {
        let can_start = self.apk.is_some() && !self.stage.busy();

        // 按钮文案随状态变化。
        // 结束（成功/失败）之后按钮直接变成"再来一次"，**一步就能重装** ——
        // 不需要先点"重置"再点"开始安装"，少一次点击。
        let button_label = match self.stage {
            Stage::Preparing => "正在准备…",
            Stage::Installing => "正在安装…",
            Stage::Done => "再装一次",
            Stage::Failed => "重试安装",
            Stage::Idle => "开始安装",
        };
        let button_msg = can_start.then_some(Message::Start);

        let start = button(
            text(button_label)
                .size(th::SIZE_HEADING)
                .font(self.fonts.bold),
        )
        .padding([10, 28])
        .style(th::primary_button)
        .on_press_maybe(button_msg);

        let mut col = column![row![start].align_y(Alignment::Center)].spacing(10);

        if self.stage.busy() {
            col = col.push(
                progress_bar(0.0..=100.0, self.percent as f32)
                    .girth(8.0)
                    .length(Length::Fill)
                    .style(th::progress),
            );
            if !self.status.is_empty() {
                col = col.push(
                    text(format!("{}　{}%", self.status, self.percent))
                        .size(th::SIZE_CAPTION)
                        .font(self.fonts.regular)
                        .color(th::TEXT_WEAK),
                );
            }
        } else if self.stage == Stage::Done {
            col = col.push(banner(
                self.fonts,
                th::banner_success,
                th::SUCCESS,
                "✅ 安装完成：相机已报告成功。可以拔掉相机，在相机的应用菜单里打开它。",
            ));
        } else if self.stage == Stage::Failed {
            col = col.push(banner(
                self.fonts,
                th::banner_danger,
                th::DANGER,
                &self.status,
            ));
            // 相机里残留任务时会拒绝开始 —— 这是最常见的一种失败，单独给指引
            if self.status.contains("拒绝") {
                col = col.push(
                    text("请把相机 USB 线拔下再插上（复位），然后点「重试安装」。")
                        .size(th::SIZE_CAPTION)
                        .font(self.fonts.regular)
                        .color(th::TEXT_WEAK),
                );
            }
        }

        card("③ 安装", self.fonts, col.into())
    }

    /// ④ 日志
    fn log_card(&self) -> Element<'_, Message> {
        let mut lines = column![].spacing(1).width(Length::Fill);
        if self.log.is_empty() {
            lines = lines.push(
                text("（这里会显示安装过程的详细信息）")
                    .size(th::SIZE_LOG)
                    .font(self.fonts.regular)
                    .color(th::TEXT_FAINT),
            );
        }
        for (level, line) in &self.log {
            let color = match level {
                Level::Info => th::TEXT_WEAK,
                Level::Good => th::SUCCESS,
                Level::Warn => th::WARNING,
                Level::Bad => th::DANGER,
            };
            lines = lines.push(
                text(line)
                    .size(th::SIZE_LOG)
                    .font(self.fonts.regular)
                    .color(color),
            );
        }

        let area = scrollable(lines)
            .height(Length::Fixed(150.0))
            .width(Length::Fill)
            .anchor_bottom();

        let clear = button(
            text("清空")
                .size(th::SIZE_CAPTION)
                .font(self.fonts.regular),
        )
        .padding([4, 10])
        .style(th::secondary_button)
        .on_press_maybe(
            (!self.log.is_empty() && !self.stage.busy()).then_some(Message::ClearLog),
        );

        card(
            "④ 运行日志",
            self.fonts,
            column![
                row![
                    text("安装过程的详细信息都在这里，出问题时可以对照排查")
                        .size(th::SIZE_CAPTION)
                        .font(self.fonts.regular)
                        .color(th::TEXT_FAINT)
                        .width(Length::Fill),
                    clear,
                ]
                .align_y(Alignment::Center),
                area,
            ]
            .spacing(6)
            .into(),
        )
    }
}

// ---------------------------------------------------------------- 小组件

/// 统一的卡片外壳：标题 + 内容
fn card<'a>(title: &'a str, fonts: Fonts, body: Element<'a, Message>) -> Element<'a, Message> {
    container(
        column![
            text(title)
                .size(th::SIZE_CAPTION)
                .font(fonts.bold)
                .color(th::TEXT_WEAK),
            body,
        ]
        .spacing(8),
    )
    .padding(14)
    .width(Length::Fill)
    .style(th::card)
    .into()
}

/// 提示条
fn banner<'a>(
    fonts: Fonts,
    style: fn(&iced::Theme) -> iced::widget::container::Style,
    color: iced::Color,
    body: &'a str,
) -> Element<'a, Message> {
    container(
        text(body)
            .size(th::SIZE_CAPTION)
            .font(fonts.regular)
            .color(color),
    )
    .padding([8, 10])
    .width(Length::Fill)
    .style(style)
    .into()
}

// ---------------------------------------------------------------- 工具

/// 把字节数变成好读的大小
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} 字节")
    }
}

/// 取多行文本的第一行（用在状态行上）
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

/// 从相机回报的 JSON 里摘出关键信息，避免日志里出现一坨长 JSON
fn summarize(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    let model = v["deviceinfo"]["name"].as_str().unwrap_or("?");
    let fw = v["deviceinfo"]["fwversion"].as_str().unwrap_or("?");
    let count = v["applications"].as_array().map(|a| a.len()).unwrap_or(0);
    format!("型号 {model}，固件 {fw}，已安装 {count} 个应用")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sony_install::InstallOutcome;

    fn app() -> App {
        App::new(Fonts::from_family("x"), FontStatus::UiOnly)
    }

    #[test]
    fn human_size_reads_naturally() {
        assert_eq!(human_size(512), "512 字节");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(147_452), "144.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn first_line_stops_at_newline() {
        assert_eq!(first_line("第一行\n第二行"), "第一行");
        assert_eq!(first_line("只有一行"), "只有一行");
    }

    #[test]
    fn summarize_picks_key_fields() {
        let json = r#"{"deviceinfo":{"name":"ILCE-6300","fwversion":"2.01"},
                       "applications":[{"name":"a"},{"name":"b"}]}"#;
        let s = summarize(json);
        assert!(s.contains("ILCE-6300"));
        assert!(s.contains("2.01"));
        assert!(s.contains("2 个应用"));
        // 不是 JSON 就原样返回，不能 panic
        assert_eq!(summarize("随便一串"), "随便一串");
    }

    #[test]
    fn log_is_capped() {
        let mut a = app();
        for i in 0..(MAX_LOG_LINES + 50) {
            a.push_log(Level::Info, format!("第 {i} 行"));
        }
        assert_eq!(a.log.len(), MAX_LOG_LINES, "日志行数要有上限");
        // 保留的应该是**最新的**那些
        assert!(
            a.log
                .last()
                .unwrap()
                .1
                .contains(&format!("第 {} 行", MAX_LOG_LINES + 49))
        );
    }

    // ---------------- 状态机 ----------------

    #[test]
    fn progress_moves_preparing_to_installing() {
        let mut a = app();
        a.stage = Stage::Preparing;
        a.on_worker(WorkerMsg::Progress {
            percent: 45,
            text: "正在下载（45%）".into(),
        });
        assert_eq!(a.stage, Stage::Installing, "第一次进度应把状态推进到安装中");
        assert_eq!(a.percent, 45);
        assert_eq!(a.status, "正在下载（45%）");
    }

    #[test]
    fn success_ends_in_done() {
        let mut a = app();
        a.stage = Stage::Installing;
        a.on_worker(WorkerMsg::Finished(Ok(InstallOutcome {
            device_report: None,
        })));
        assert_eq!(a.stage, Stage::Done);
        assert_eq!(a.percent, 100);
        assert!(
            matches!(a.camera, Camera::Unknown),
            "装完相机状态应标为待重检"
        );
    }

    #[test]
    fn failure_keeps_only_first_line_in_status() {
        let mut a = app();
        a.stage = Stage::Installing;
        a.on_worker(WorkerMsg::Finished(Err(
            "相机拒绝了开始任务（里面残留着上一次未完成的任务）。\n请把相机 USB 拔下来再插上。"
                .into(),
        )));
        assert_eq!(a.stage, Stage::Failed);
        // 状态行只放第一行，完整信息在日志里
        assert!(a.status.contains("相机拒绝了开始任务"));
        assert!(!a.status.contains("拔下来"), "状态行不该塞进整个多行文本");
    }

    #[test]
    fn camera_found_and_missing_update_state() {
        let mut a = app();
        a.on_worker(WorkerMsg::Camera(Ok(Some(sony_install::CameraStatus {
            pnp_id: "x".into(),
            model: "ILCE-6300".into(),
            serial: "05130861".into(),
            app_install_mode: false,
        }))));
        assert!(matches!(a.camera, Camera::Found(_)));

        a.on_worker(WorkerMsg::Camera(Ok(None)));
        match &a.camera {
            Camera::Missing(why) => assert!(why.contains("没有检测到"), "要给出原因：{why}"),
            other => panic!("应是未找到状态，实际 {other:?}"),
        }
    }

    #[test]
    fn step_messages_do_not_advance_stage() {
        // "正在准备…"这类说明不该把状态推进到 Installing，
        // 只有真正的进度汇报才推进
        let mut a = app();
        a.stage = Stage::Preparing;
        a.on_worker(WorkerMsg::Step("正在命令相机切换到「应用安装模式」…".into()));
        assert_eq!(a.stage, Stage::Preparing);
        assert!(a.status.contains("应用安装模式"));
    }

    #[test]
    fn cannot_start_without_an_apk() {
        let mut a = app();
        a.update(Message::Start);
        assert_eq!(a.stage, Stage::Idle, "没选文件时不该开始安装");
        assert!(a.rx.is_none(), "也不该起后台线程");
    }

    #[test]
    fn clear_log_keeps_a_marker_line() {
        let mut a = app();
        a.push_log(Level::Info, "一些旧日志");
        a.update(Message::ClearLog);
        assert_eq!(a.log.len(), 1, "清空后只留一条说明");
        assert!(a.log[0].1.contains("已清空"));
    }
}
