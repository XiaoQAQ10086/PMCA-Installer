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
use iced::{Alignment, Element, Length, Task};

use crate::fonts::Fonts;
use crate::theme as th;
use crate::worker::{self, ApkChoice, WorkerMsg};

/// 日志最多留多少行（防止长时间运行把内存撑起来）
const MAX_LOG_LINES: usize = 400;

// ---------------------------------------------------------------- 状态

/// 相机的检测状态。
///
/// ⚠️ 这里刻意把「没插相机」和「插着但 USB 模式不对」**分成两种状态**：
/// 后者（海量存储器）如果也显示"未找到相机"，就是在给用户**错误的指引** ——
/// 相明明插着、线也好好的，用户会去拔插线材换 USB 口，怎么试都没用。
#[derive(Debug, Clone)]
pub enum Camera {
    /// 还没检测过
    Unknown,
    /// 正在检测
    Checking,
    /// 没插相机
    Missing(String),
    /// 相机插着，但 Windows 看不到它，而且我们认不出是什么连接方式
    ///
    /// ⚠️ 标题只描述现象，**不能**写"请改成 MTP 模式"：我们只在 α6300 上
    /// 测过产品号，别的机型对不上很正常。旧版就是因此对着一个明明选了 MTP 的
    /// α6000 喊"请改成 MTP 模式"。
    ModeUnknown(String),
    /// 相机是已知的海量存储器型号，但 Windows 没看到磁盘（八成没插存储卡）
    MassStorageNoDisk(String),
    /// 相机在「海量存储器」模式 —— **这种机型就该这样，我们能自己切过去**
    ///
    /// ⚠️ 和上面两种"看不到"的状态分开是有意的：那两种要叫用户去动相机设置，
    /// 这种情况**千万不要**（α6000 改成 MTP 反而彻底没戏）。
    MassStorage(String),
    /// 插着索尼设备但读不出信息（被占用 / 驱动问题）
    Unreadable(String),
    /// 找到了，可以用
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
    /// 把运行日志复制到剪贴板
    CopyLog,
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

    /// 处理一条消息。
    ///
    /// 返回 `Task` 是因为"复制到剪贴板"要交给 iced 去做
    /// （剪贴板是操作系统的资源，不能直接同步写）。其余消息返回 `Task::none()`。
    pub fn update(&mut self, message: Message) -> Task<Message> {
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
            Message::CopyLog => {
                // ⚠️ 先把要复制的内容取出来，**再**写"已复制"那条日志 ——
                //    否则复制出去的文本里会多出一句自言自语。
                let text = self.log_text();
                // 这里的 `self.log.len()` 在 push_log 之前求值，
                // 所以报的是**实际复制走的行数**，不含这条提示。
                self.push_log(
                    Level::Info,
                    format!("已复制到剪贴板（{} 行）", self.log.len()),
                );
                return iced::clipboard::write(text);
            }
            Message::ApkDropped(path) => {
                if !self.stage.busy() {
                    self.accept_apk(path);
                }
            }
            Message::Tick => self.drain_workers(),
            Message::Ignored => {}
        }
        Task::none()
    }

    /// 拼出要复制到剪贴板的文本。
    ///
    /// 开头带版本号：用户把这段贴出来求助时，
    /// "是哪个版本"往往是最先要问的问题，省一轮来回。
    fn log_text(&self) -> String {
        let mut out = String::with_capacity(self.log.len() * 60 + 64);
        out.push_str(&format!("PMCA 安装器 v{}\n", crate::VERSION));
        out.push_str("----------------\n");
        for (_, line) in &self.log {
            out.push_str(line);
            out.push('\n');
        }
        out
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
            WorkerMsg::Camera(Ok(probe)) => {
                use sony_install::CameraProbe as P;
                let detail = probe.detail().unwrap_or_default();
                // 排查用的设备摘要（可能为空），只写进日志
                let devices = probe.devices_hint().unwrap_or("（无）").to_string();
                match probe {
                    P::Ready(status) => {
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
                    P::MassStorage { model } => {
                        self.push_log(
                            Level::Good,
                            format!("找到相机 {model}，在海量存储器模式（安装时会自动切换）"),
                        );
                        self.camera = Camera::MassStorage(model);
                    }
                    P::ModeUnknown(_) => {
                        // ⚠️ 设备号一定要写进**运行日志**：用户报问题时点一下
                        //    「复制」就能带出来。之前只写在 install-diag.log 里，
                        //    结果用户截了图、点了复制，偏偏没有这行关键信息。
                        self.push_log(
                            Level::Warn,
                            format!(
                                "相机插着，但 Windows 看不到它，也认不出是什么连接方式（设备：{devices}）"
                            ),
                        );
                        self.camera = Camera::ModeUnknown(detail);
                    }
                    P::MassStorageNoDisk(_) => {
                        self.push_log(
                            Level::Warn,
                            format!(
                                "相机在海量存储器模式，但没看到磁盘（可能没插存储卡）（设备：{devices}）"
                            ),
                        );
                        self.camera = Camera::MassStorageNoDisk(detail);
                    }
                    P::Unreadable(msg) => {
                        self.push_log(Level::Bad, format!("相机读不出来：{msg}"));
                        self.camera = Camera::Unreadable(detail);
                    }
                    P::NotFound => {
                        let why =
                            "没有检测到索尼相机。请确认相机已开机、USB 线已插好。".to_string();
                        self.push_log(Level::Warn, &why);
                        self.camera = Camera::Missing(why);
                    }
                }
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
        // 标题行：软件名 + 右上角的版本号
        let title_row = row![
            text("PMCA 安装器")
                .size(th::SIZE_TITLE)
                .font(self.fonts.bold)
                .color(th::TEXT),
            space::horizontal(),
            text(format!("v{}", crate::VERSION))
                .size(th::SIZE_CAPTION)
                .font(self.fonts.regular)
                .color(th::TEXT_FAINT),
        ]
        .align_y(Alignment::End);

        let mut col = column![
            title_row,
            text("把 Android 应用装到索尼相机上 · 自动识别相机")
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
        // ⚠️ 关键：**「没插相机」和「模式不对」必须显示不同的标题**。
        //    如果相机设成了海量存储器却显示"未找到相机"，用户会去检查线材、
        //    换 USB 口、重启相机 —— 方向全错。真正要做的只是改相机菜单里一个选项。
        let (dot_color, title, detail) = match &self.camera {
            Camera::Unknown => (th::TEXT_FAINT, "尚未检测".to_string(), String::new()),
            Camera::Checking => (th::ACCENT, "正在检测…".to_string(), String::new()),
            Camera::Missing(why) => (th::DANGER, "未找到相机".to_string(), why.clone()),
            // 标题只描述现象、不下结论 —— 我们不知道它是什么模式，
            // 硬说"模式不对"会把用户带偏（α6000 那次就是）
            Camera::ModeUnknown(how) => (
                th::WARNING,
                "Windows 看不到相机".to_string(),
                how.clone(),
            ),
            Camera::MassStorageNoDisk(how) => (
                th::WARNING,
                "相机里可能没插存储卡".to_string(),
                how.clone(),
            ),
            // 绿色：这台相机**能用**，只是要先自动切一下。
            // 用绿色是刻意的 —— 别让用户以为出了问题要自己去折腾。
            Camera::MassStorage(how) => (
                th::SUCCESS,
                "相机在海量存储器模式".to_string(),
                how.clone(),
            ),
            Camera::Unreadable(why) => (th::DANGER, "相机读不出来".to_string(), why.clone()),
            Camera::Found(s) => (
                if s.app_install_mode {
                    th::SUCCESS
                } else {
                    th::WARNING
                },
                format!("{}  {}", s.model, s.mode_text()),
                format!("序列号 {} · 安装时自动切换到应用安装模式", s.serial),
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

        // 「复制」放在「清空」左边。
        //
        // 复制**不**受"正在安装"限制 —— 恰恰相反，
        // 安装卡住的时候最需要把日志复制出来发给别人看。
        // 只有日志为空时才禁用。
        let copy = button(
            text("复制")
                .size(th::SIZE_CAPTION)
                .font(self.fonts.regular),
        )
        .padding([4, 10])
        .style(th::secondary_button)
        .on_press_maybe((!self.log.is_empty()).then_some(Message::CopyLog));

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
                // 说明文字按用户要求去掉了，只留右下角两个按钮。
                // 用 space 把按钮顶到右边，保持原来的位置。
                row![space::horizontal(), copy, clear].align_y(Alignment::Center).spacing(6),
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
        a.on_worker(WorkerMsg::Camera(Ok(sony_install::CameraProbe::Ready(
            sony_install::CameraStatus {
                pnp_id: "x".into(),
                model: "ILCE-6300".into(),
                serial: "05130861".into(),
                app_install_mode: false,
            },
        ))));
        assert!(matches!(a.camera, Camera::Found(_)));

        a.on_worker(WorkerMsg::Camera(Ok(sony_install::CameraProbe::NotFound)));
        match &a.camera {
            Camera::Missing(why) => assert!(why.contains("没有检测到"), "要给出原因：{why}"),
            other => panic!("应是未找到状态，实际 {other:?}"),
        }
    }

    /// ⚠️ 关键回归测试：相机设成海量存储器时，
    /// ⚠️⚠️ **回归测试：认不出模式时，标题不能替用户下结论。**
    ///
    /// 真机教训：有 α6000 用户明明选了 MTP，界面却显示"请把相机改成 MTP 模式"，
    /// 因为他那台机器的产品号不在我们那张（只在 α6300 上测过的）清单里。
    #[test]
    fn unknown_mode_does_not_tell_the_user_to_switch_to_mtp() {
        let unseen = || sony_install::UnseenCamera {
            message: "某某说明".to_string(),
            devices: "054C:1234".to_string(),
        };
        let mut a = app();
        a.on_worker(WorkerMsg::Camera(Ok(sony_install::CameraProbe::ModeUnknown(
            unseen(),
        ))));

        let title = sony_install::CameraProbe::ModeUnknown(unseen()).title();
        assert!(
            !title.contains("改成 MTP"),
            "我们并不知道它是不是 MTP，标题不能替用户下结论：{title}"
        );
        assert!(title.contains("看不到"), "标题应当只描述现象：{title}");
        assert!(!title.contains("未找到"), "也不能说成没插相机：{title}");

        match &a.camera {
            Camera::ModeUnknown(_) => {}
            other => panic!("应进「认不出模式」状态，实际 {other:?}"),
        }

        // 产品号必须落进**运行日志** —— 用户点「复制」就能带出来。
        // 真机上就是缺了这一行，才没法确认 α6000 的产品号。
        assert!(
            a.log_lines().any(|l| l.contains("054C:1234")),
            "设备号要写进运行日志，否则用户没法把它发给我们"
        );
    }

    /// 已知海量存储器型号但没看到磁盘 → 标题要指向存储卡
    #[test]
    fn mass_storage_without_disk_points_at_the_card() {
        let title = sony_install::CameraProbe::MassStorageNoDisk(sony_install::UnseenCamera {
            message: String::new(),
            devices: String::new(),
        })
        .title();
        assert!(title.contains("存储卡"), "要指向存储卡：{title}");
    }

    /// 真的没插相机时，仍然要显示"未找到相机"
    #[test]
    fn really_absent_still_says_not_found() {
        assert_eq!(
            sony_install::CameraProbe::NotFound.title(),
            "未找到相机"
        );
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
        let _ = a.update(Message::Start);
        assert_eq!(a.stage, Stage::Idle, "没选文件时不该开始安装");
        assert!(a.rx.is_none(), "也不该起后台线程");
    }

    #[test]
    fn clear_log_keeps_a_marker_line() {
        let mut a = app();
        a.push_log(Level::Info, "一些旧日志");
        let _ = a.update(Message::ClearLog);
        assert_eq!(a.log.len(), 1, "清空后只留一条说明");
        assert!(a.log[0].1.contains("已清空"));
    }

    /// 复制出去的文本要带版本号。
    ///
    /// 用户把日志贴出来求助时，"是哪个版本"往往是最先要问的，
    /// 写在开头能省一轮来回。
    #[test]
    fn log_text_has_version_and_every_line() {
        let mut a = app();
        a.push_log(Level::Info, "第一条");
        a.push_log(Level::Bad, "第二条出错了");

        let t = a.log_text();
        assert!(t.contains(crate::VERSION), "开头要带版本号：\n{t}");
        assert!(t.contains("第一条"));
        assert!(t.contains("第二条出错了"));
        assert_eq!(
            t.lines().count(),
            a.log.len() + 2,
            "应当是「版本行 + 分隔线 + 每行日志」"
        );
    }

    /// ⚠️ 复制出去的内容里**不能**混进"已复制"这句提示。
    ///
    /// `update` 里是先取文本、再写提示，这个顺序不能反 ——
    /// 反了的话用户第一次复制得到的内容会以一句自言自语结尾，
    /// 贴给别人看很奇怪。
    #[test]
    fn copied_text_excludes_the_confirmation_line() {
        let mut a = app();
        a.push_log(Level::Info, "真正的内容");

        // 模拟 update 里的顺序：先取文本
        let copied = a.log_text();
        // 再写提示
        a.push_log(Level::Info, "已复制到剪贴板（1 行）");

        assert!(copied.contains("真正的内容"));
        assert!(!copied.contains("已复制"), "复制出去的内容里不该有提示：\n{copied}");
        assert!(a.log.last().unwrap().1.contains("已复制"), "界面上要有提示");
    }

    /// 日志为空时点复制不该出问题
    #[test]
    fn copy_with_empty_log_is_harmless() {
        let mut a = app();
        let _ = a.update(Message::CopyLog);
        // 空日志时 log_text 只有版本行和分隔线，不该 panic
        assert!(a.log_text().contains(crate::VERSION));
    }
}
