//! 后台工作线程。
//!
//! # 为什么必须有它
//!
//! 相机通信是一件**又慢又可能失败**的事：切换模式要等相机重新枚举（几秒），
//! 安装要等相机下载整个应用包（几十秒）。如果这些跑在界面线程上，
//! 窗口会**直接卡死**——用户看到的是"程序未响应"。
//!
//! # 怎么通信
//!
//! ```text
//!   界面线程                          工作线程
//!      │                                 │
//!      │  spawn_install(apk)             │
//!      ├───────── 建通道 ───────────────►│
//!      │                                 │  跑安装流程
//!      │  ◄──── WorkerMsg::Step ─────────┤  （每有进展就发一条）
//!      │  ◄──── WorkerMsg::Progress ─────┤
//!      │  ◄──── WorkerMsg::Finished ─────┤
//!      │                                 │
//!   定时（约 10 次/秒）用 try_recv 取消息，不阻塞
//! ```
//!
//! 用定时轮询而不是 `Task::perform`，是因为轮询最简单、也最不容易出错：
//! 界面永远不会被后台卡住，消息晚几十毫秒到也无所谓。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use sony_install::{CameraProbe, Event, InstallOutcome};

/// 用户在对话框里选中的应用包
#[derive(Debug, Clone)]
pub struct ApkChoice {
    pub path: PathBuf,
    /// 文件名（给界面显示）
    pub name: String,
    /// 字节数
    pub size: u64,
}

impl ApkChoice {
    /// 从路径构造（文件选择、拖放、命令行参数三条路都走这里）
    pub fn from_path(path: PathBuf) -> Self {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self { path, name, size }
    }
}

/// 让用户挑一个应用包。
///
/// 对话框跑在**后台线程**上：Windows 的原生打开对话框是阻塞式的，
/// 直接在界面线程里调用会让窗口停止重绘（看起来像卡死）。
pub fn spawn_pick_apk() -> Receiver<Option<ApkChoice>> {
    let (tx, rx) = channel();
    let _ = std::thread::Builder::new()
        .name("选择文件".into())
        .spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("选择要安装的应用包")
                .add_filter("Android 应用包", &["apk"])
                .add_filter("所有文件", &["*"])
                .pick_file()
                .map(ApkChoice::from_path);
            let _ = tx.send(picked);
        });
    rx
}



/// 工作线程 → 界面线程的消息
#[derive(Debug)]
pub enum WorkerMsg {
    /// 相机检测的结果
    Camera(Result<CameraProbe, String>),
    /// 一句进度说明
    Step(String),
    /// 安装进度
    Progress { percent: u8, text: String },
    /// 安装结束：成功给结果，失败给原因
    Finished(Result<InstallOutcome, String>),
    /// 检查更新的结果
    Update(crate::update::UpdateCheck),
    /// 下载进度
    DownloadProgress(crate::update::Progress),
    /// 下载结束
    DownloadDone(crate::update::DownloadOutcome),
}

/// 检测相机（**不改变**相机状态）。返回一个可以轮询的接收端。
pub fn spawn_probe() -> Receiver<WorkerMsg> {
    let (tx, rx) = channel();
    let _ = std::thread::Builder::new()
        .name("相机检测".into())
        .spawn(move || {
            let result = sony_install::probe_camera().map_err(|e| format!("{e:#}"));
            let _ = tx.send(WorkerMsg::Camera(result));
        });
    rx
}

/// 查一次有没有新版本。
///
/// ⚠️ 必须放在后台线程：网络不好的时候会卡好几秒到十几秒
/// （DNS 慢、代理慢、连不上要等超时），放在界面线程上窗口直接卡死。
///
/// `current` 是本程序的版本号，用来跟服务器上的比。
pub fn spawn_update_check(current: String) -> Receiver<WorkerMsg> {
    let (tx, rx) = channel();
    let _ = std::thread::Builder::new()
        .name("检查更新".into())
        .spawn(move || {
            let result = crate::update::check(&current);
            let _ = tx.send(WorkerMsg::Update(result));
        });
    rx
}

/// 下载新版本的安装包。
///
/// `cancel` 是界面和这个线程共享的旗子 —— 用户点「取消下载」时界面把它置起来，
/// 下载循环下一块数据就会停，**并且把没下完的半截文件删掉**。
///
/// 进度回调故意做成"合并上报"：`update::download` 内部按固定间隔才回调一次，
/// 免得每秒几百条消息把界面线程淹掉。
pub fn spawn_download(
    url: String,
    dest: std::path::PathBuf,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Receiver<WorkerMsg> {
    let (tx, rx) = channel();
    let _ = std::thread::Builder::new()
        .name("下载更新".into())
        .spawn(move || {
            let tx_progress = tx.clone();
            let mut report = move |p: crate::update::Progress| {
                let _ = tx_progress.send(WorkerMsg::DownloadProgress(p));
            };
            let outcome = crate::update::download(&url, &dest, &cancel, &mut report);
            let _ = tx.send(WorkerMsg::DownloadDone(outcome));
        });
    rx
}
/// 开始安装。安装过程的所有进展都会通过返回的接收端送出来。
pub fn spawn_install(apk: Vec<u8>, app_name: String) -> Receiver<WorkerMsg> {
    let (tx, rx) = channel();

    let _ = std::thread::Builder::new()
        .name("安装".into())
        .spawn(move || {
            let tx_progress = tx.clone();
            let mut report = move |ev: Event| {
                let msg = match ev {
                    Event::Step(s) => WorkerMsg::Step(s),
                    Event::Progress { percent, text } => WorkerMsg::Progress { percent, text },
                };
                let _ = tx_progress.send(msg);
            };

            // 用 catch_unwind 兜底：底层是 COM 和 USB，万一哪儿 panic 了，
            // 界面不能永远停在"正在安装…"转圈 —— 必须给用户一句明确的话。
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sony_install::install(&apk, &app_name, &mut report)
            }));

            let msg = match outcome {
                Ok(Ok(o)) => WorkerMsg::Finished(Ok(o)),
                Ok(Err(e)) => WorkerMsg::Finished(Err(format!("{e:#}"))),
                Err(_) => WorkerMsg::Finished(Err(
                    "安装过程中出现了意外错误（内部异常）。\n\
                     请拔插一次相机 USB 线后重试；若反复出现，请把 install-diag.log 发给开发者。"
                        .into(),
                )),
            };
            let _ = tx.send(msg);
        });

    rx
}
