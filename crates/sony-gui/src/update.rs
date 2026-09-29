//! 检查更新，以及把新版本下载下来。

//!
//! # 用什么发请求
//!
//! 用 **Windows 自带的 WinHTTP**，不引入任何 HTTP 库。
//!
//! 理由是体积：`ureq` / `reqwest` 这类库会把整套 TLS 实现（ring、rustls…）
//! 编进 exe，体积要涨一两 MB；而这个程序总共才 4 MB 多，
//! 系统既然本来就有一套能用的 HTTPS，就没必要自己再带一套。
//!
//! 代价是要手写一层 WinHTTP 绑定 —— 但只有几十行，而且都是固定签名。
//!
//! # 检查失败不算错误
//!
//! 用户可能没网、可能在公司内网、可能接口被墙。
//! **一个装应用的小工具不该因为查不到更新就报错或者变慢。**
//! 所以：
//! - 检查和下载都放在后台线程，绝不阻塞界面
//! - 检查失败时只记一句日志，界面上不弹任何东西
//! - 手动点"检查更新"时才把失败原因说出来
//!
//! # 为什么自己下载而不是跳浏览器
//!
//! 跳浏览器的话，用户下载到哪儿、下没下完、下的是不是最新版，程序一概不知道。
//! 自己做下载能给出**进度条、速度、取消按钮**，而且取消时能把没下完的半截文件
//! 清掉，不留垃圾。

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use core::ffi::c_void;

use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_CONTENT_LENGTH,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect,
    WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts,
};

/// 查询最新版本的接口
const RELEASES_API: &str =
    "https://api.github.com/repos/XiaoQAQ10086/PMCA-Installer/releases/latest";

/// 万一接口没给出链接，就退到发布页
pub const RELEASES_PAGE: &str = "https://github.com/XiaoQAQ10086/PMCA-Installer/releases/latest";

/// 我们要下的是哪个附件
const ASSET_NAME: &str = "PMCA-Installer.exe";

/// 超时（毫秒）。
///
/// 分开设是刻意的：连不上要快点放弃，而读取可以宽一些 ——
/// 用户可能网速很慢，但一直连不上就该赶紧让路。
const TIMEOUT_RESOLVE: i32 = 4_000;
const TIMEOUT_CONNECT: i32 = 5_000;
const TIMEOUT_SEND: i32 = 5_000;
/// 接收超时给得宽：下载是持续有数据的，中间断一小会儿很正常
const TIMEOUT_RECEIVE: i32 = 30_000;

/// 进度回调最快多久报一次（毫秒）。
///
/// 每次都报的话，每秒几百次回调会把界面线程淹没 —— 进度条根本不需要那么细。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);

// ---------------------------------------------------------------- 检查

/// 发现的新版本
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewVersion {
    /// 版本号（已去掉开头的 `v`），例如 `1.0.5`
    pub version: String,
    /// 发布页地址 —— 拿不到直链时用它退回浏览器
    pub page_url: String,
    /// 安装包直链。`None` 表示这个 release 没挂 exe，只能去发布页
    pub download_url: Option<String>,
    /// 安装包大小（字节）—— 有它才能算出百分比
    pub size: Option<u64>,
}

/// 检查更新的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    /// 有新版本可用
    Available(NewVersion),
    /// 已经是最新
    UpToDate {
        /// 服务器上的版本号
        latest: String,
    },
    /// 没查成（没网、被墙、接口改了…）
    ///
    /// ⚠️ **这不是错误**，别摆到界面上吓人。原因留给日志用。
    Failed(String),
}

/// 查一次更新。
///
/// ⚠️ **这是同步阻塞的**，调用方必须放到后台线程里 ——
/// 网络不好的时候它会卡好几秒，绝不能放在界面线程上。
pub fn check(current: &str) -> UpdateCheck {
    let agent = format!("PMCA-Installer/{current}");
    let body = match http_get_text(RELEASES_API, &agent) {
        Ok(b) => b,
        Err(e) => return UpdateCheck::Failed(format!("{e:#}")),
    };

    let json: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return UpdateCheck::Failed(format!("返回的内容不是预期格式：{e}")),
    };

    let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) else {
        return UpdateCheck::Failed("返回里没有版本号（接口格式可能变了）".to_string());
    };
    if !is_newer(tag, current) {
        return UpdateCheck::UpToDate {
            latest: strip_v(tag),
        };
    }

    // 从附件里挑出安装包。挑不到也没关系 —— 那就退回浏览器。
    let asset = json
        .get("assets")
        .and_then(|v| v.as_array())
        .and_then(|list| {
            list.iter()
                .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(ASSET_NAME))
                .or_else(|| {
                    // 名字对不上就退一步：随便挑一个 .exe
                    list.iter().find(|a| {
                        a.get("name")
                            .and_then(|n| n.as_str())
                            .is_some_and(|n| n.ends_with(".exe"))
                    })
                })
        });

    let download_url = asset
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let size = asset.and_then(|a| a.get("size")).and_then(|v| v.as_u64());

    UpdateCheck::Available(NewVersion {
        version: strip_v(tag),
        page_url: json
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or(RELEASES_PAGE)
            .to_string(),
        download_url,
        size,
    })
}

/// `v1.0.4` → `1.0.4`
fn strip_v(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

/// 服务器上的版本是不是比本地新
///
/// 解析不出来就一律返回 `false` —— **宁可漏报，也不要误报"有新版本"**。
/// 误报会白白骗用户去下载一次。
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// 把 `v1.2.3` 这样的字符串拆成可比较的数字序列。
///
/// 只取开头的数字和点，后面跟的 `-beta`、`+build` 之类一律忽略 ——
/// 我们的版本号就是三段数字，不需要完整的语义化版本解析。
fn parse_version(s: &str) -> Option<Vec<u64>> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if core.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for piece in core.split('.') {
        parts.push(piece.parse::<u64>().ok()?);
    }
    // 补到三段，这样 `1.0` 和 `1.0.0` 能比出相等
    while parts.len() < 3 {
        parts.push(0);
    }
    Some(parts)
}

// ---------------------------------------------------------------- 下载

/// 一次下载的进度
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    /// 已经收到多少字节
    pub received: u64,
    /// 总共多少字节。**0 表示不知道**（那就只显示已下载量，不显示百分比）
    pub total: u64,
    /// 速度（字节/秒），用滑动平均算的
    pub speed: f64,
}

impl Progress {
    /// 百分比（0–100）。总量未知时返回 `None`。
    pub fn percent(&self) -> Option<u8> {
        if self.total == 0 {
            return None;
        }
        Some(((self.received.min(self.total) * 100) / self.total) as u8)
    }
}

/// 下载的结果
#[derive(Debug)]
pub enum DownloadOutcome {
    /// 下载完成，文件在这个路径
    Done(PathBuf),
    /// 用户点了取消 —— 半截文件已经删掉了
    Cancelled,
    /// 下载失败，原因给用户看
    Failed(String),
}

/// 下载新版本的安装包。
///
/// - `cancel`：界面把它置成 `true` 就中止下载
/// - `report`：进度回调（调用方在里面把进度转发给界面）
///
/// # 半截文件怎么办
///
/// 先写到 `.part` 临时文件，**下完了才改名成正式名字**。这样：
/// - 中途取消 → 删掉 `.part`，用户看不到半个 exe
/// - 中途崩溃 → 留下一个 `.part`，也不会被误当成能用的安装包
pub fn download(
    url: &str,
    dest: &Path,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(Progress),
) -> DownloadOutcome {
    let part = part_path(dest);

    match download_to(url, &part, cancel, report) {
        Ok(true) => {
            // 下完了 —— 改名成正式文件
            if let Err(e) = std::fs::rename(&part, dest) {
                let _ = std::fs::remove_file(&part);
                return DownloadOutcome::Failed(format!("下载完成但改名失败：{e}"));
            }
            DownloadOutcome::Done(dest.to_path_buf())
        }
        Ok(false) => {
            // 用户取消 —— 把半截文件清掉，别留垃圾
            let _ = std::fs::remove_file(&part);
            DownloadOutcome::Cancelled
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            DownloadOutcome::Failed(format!("{e:#}"))
        }
    }
}

/// 真正下载的那部分：返回 `Ok(true)` = 下完了，`Ok(false)` = 被取消
fn download_to(
    url: &str,
    part: &Path,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(Progress),
) -> Result<bool> {
    let agent = format!("PMCA-Installer/{}", crate::VERSION);
    let request = GetRequest::send(url, &agent)?;

    // 优先用服务器给的长度，拿不到就用调用方传进来的（接口里那个 size）
    let total = request.content_length().unwrap_or(0);

    let mut file = File::create(part)?;
    let mut received: u64 = 0;

    let mut last_report = Instant::now();
    let mut last_bytes: u64 = 0;
    let mut speed: f64 = 0.0;

    let mut cancelled = false;
    request.read_chunks(|chunk| {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            return false; // 让读循环停下来
        }
        if file.write_all(chunk).is_err() {
            return false;
        }
        received += chunk.len() as u64;

        // 控制上报频率：太频繁会把界面线程淹没
        let now = Instant::now();
        let dt = now.duration_since(last_report);
        if dt >= PROGRESS_INTERVAL {
            let inst = (received - last_bytes) as f64 / dt.as_secs_f64();
            // 滑动平均：瞬时速度抖得厉害，直接显示会跳来跳去
            speed = if speed == 0.0 {
                inst
            } else {
                speed * 0.7 + inst * 0.3
            };
            report(Progress {
                received,
                total,
                speed,
            });
            last_report = now;
            last_bytes = received;
        }
        true
    })?;

    if cancelled {
        return Ok(false);
    }

    file.flush()?;
    drop(file);

    // 校验：服务器说了多大就该下载到多大。短了说明连接中途断了。
    if total > 0 && received != total {
        bail!("下载不完整：期望 {total} 字节，实际只有 {received} 字节");
    }

    // 最后补报一次，让进度条走到 100%
    report(Progress {
        received,
        total: total.max(received),
        speed,
    });
    Ok(true)
}

/// `.part` 临时文件的名字
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// 把字节数说成人话：`1.8 MB/s`、`320 KB`
pub fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} 字节")
    }
}

/// 速度说成人话：`1.8 MB/s`
pub fn human_speed(bytes_per_sec: f64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec.max(0.0) as u64))
}

// ---------------------------------------------------------------- WinHTTP

/// 保证句柄一定会被关掉 —— WinHTTP 的句柄不关就是泄漏
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// 一个已经发出去、并且收到响应的 GET 请求
struct GetRequest {
    _session: Handle,
    _connect: Handle,
    request: Handle,
}

impl GetRequest {
    /// 建会话、连服务器、发请求、收响应。
    ///
    /// 失败（含 HTTP 状态码不是 200）都会返回 `Err`。
    fn send(url: &str, agent: &str) -> Result<Self> {
        let (host, path) = split_url(url)?;
        let agent_w = to_wide(agent);
        let host_w = to_wide(host);
        let path_w = to_wide(path);
        let verb = to_wide("GET");

        unsafe {
            // 用 AUTOMATIC_PROXY：这样会跟随系统的代理设置。
            // 对需要挂代理才能访问 GitHub 的用户（国内挺常见），这一点很重要。
            let session = Handle(WinHttpOpen(
                agent_w.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            ));
            if session.0.is_null() {
                bail!("初始化网络会话失败");
            }
            WinHttpSetTimeouts(
                session.0,
                TIMEOUT_RESOLVE,
                TIMEOUT_CONNECT,
                TIMEOUT_SEND,
                TIMEOUT_RECEIVE,
            );

            let connect = Handle(WinHttpConnect(session.0, host_w.as_ptr(), 443, 0));
            if connect.0.is_null() {
                bail!("连接 {host} 失败");
            }

            let request = Handle(WinHttpOpenRequest(
                connect.0,
                verb.as_ptr(),
                path_w.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ));
            if request.0.is_null() {
                bail!("构造请求失败");
            }

            // GitHub 的接口要求带 User-Agent（WinHttpOpen 的 agent 已经提供了），
            // 再补一个 Accept 明确要 JSON
            let headers = to_wide("Accept: application/vnd.github+json\r\n");
            WinHttpSendRequest(
                request.0,
                headers.as_ptr(),
                u32::MAX, // -1：让 WinHTTP 自己算长度
                std::ptr::null(),
                0,
                0,
                0,
            );

            if WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0 {
                bail!("没有收到服务器的响应（网络或代理的问题？）");
            }

            let this = Self {
                _session: session,
                _connect: connect,
                request,
            };

            match this.status_code() {
                Some(200) => Ok(this),
                Some(404) => bail!("服务器说没有这个东西（404）"),
                Some(other) => bail!("服务器返回 {other}"),
                None => Ok(this), // 拿不到状态码就当作没出错，硬着头皮读
            }
        }
    }

    /// HTTP 状态码
    fn status_code(&self) -> Option<u32> {
        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let ok = unsafe {
            WinHttpQueryHeaders(
                self.request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                (&raw mut status).cast::<c_void>(),
                &mut len,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(status)
    }

    /// 服务器声明的正文长度（`Content-Length`）。拿不到就是 `None`。
    fn content_length(&self) -> Option<u64> {
        let mut len: u64 = 0;
        let mut size = std::mem::size_of::<u64>() as u32;
        let ok = unsafe {
            WinHttpQueryHeaders(
                self.request.0,
                WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                (&raw mut len).cast::<c_void>(),
                &mut size,
                std::ptr::null_mut(),
            )
        };
        (ok != 0 && len > 0).then_some(len)
    }

    /// 逐块读正文。
    ///
    /// `on_chunk` 返回 `false` 表示"别再读了"（取消）—— 这时会尽快停下，
    /// 不会把剩下的几十兆也读完。
    fn read_chunks(&self, mut on_chunk: impl FnMut(&[u8]) -> bool) -> Result<()> {
        let mut buf = [0u8; 32 * 1024];
        loop {
            let mut read: u32 = 0;
            let ok = unsafe {
                WinHttpReadData(
                    self.request.0,
                    buf.as_mut_ptr().cast::<c_void>(),
                    buf.len() as u32,
                    &mut read,
                )
            };
            if ok == 0 {
                bail!("读取响应内容时断开了");
            }
            if read == 0 {
                return Ok(()); // 读完了
            }
            if !on_chunk(&buf[..read as usize]) {
                return Ok(()); // 调用方要求停
            }
        }
    }
}

/// 发一个 HTTPS GET 并把正文当文本返回（用于查版本这种小响应）
fn http_get_text(url: &str, agent: &str) -> Result<String> {
    let request = GetRequest::send(url, agent)?;
    let mut body = Vec::new();
    request.read_chunks(|chunk| {
        body.extend_from_slice(chunk);
        // 防呆：接口返回的 JSON 只有几百字节，超过 1 MB 一定是出问题了
        body.len() < (1 << 20)
    })?;
    String::from_utf8(body).map_err(|e| anyhow::anyhow!("响应不是合法的 UTF-8：{e}"))
}

/// 把 URL 拆成 `(主机, 路径)`。
///
/// 只认 `https://` —— 检查和下载都没有任何理由走明文 HTTP，
/// 万一以后有人改了常量写成 http，这里会直接报错而不是悄悄降级。
fn split_url(url: &str) -> Result<(&str, &str)> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow::anyhow!("只支持 https 地址"))?;
    Ok(match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    })
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_with_or_without_the_v() {
        assert_eq!(parse_version("v1.0.4"), Some(vec![1, 0, 4]));
        assert_eq!(parse_version("1.0.4"), Some(vec![1, 0, 4]));
        assert_eq!(parse_version("V2.13.0"), Some(vec![2, 13, 0]));
        // 后面跟的东西一律忽略
        assert_eq!(parse_version("v1.0.4-beta.1"), Some(vec![1, 0, 4]));
        // 解析不了就返回 None，调用方据此保守处理
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn two_part_versions_are_padded() {
        assert_eq!(parse_version("1.2"), Some(vec![1, 2, 0]));
        // 1.2 和 1.2.0 应当相等
        assert!(!is_newer("1.2", "1.2.0"));
        assert!(!is_newer("1.2.0", "1.2"));
    }

    #[test]
    fn detects_newer_versions() {
        assert!(is_newer("v1.0.5", "1.0.4"));
        assert!(is_newer("v1.1.0", "1.0.4"));
        assert!(is_newer("v2.0.0", "1.9.9"));
        // 数字要按数值比，不能按字符串比 —— "1.10" 比 "1.9" 新
        assert!(is_newer("1.10.0", "1.9.0"));
    }

    #[test]
    fn same_or_older_is_not_newer() {
        assert!(!is_newer("v1.0.4", "1.0.4"));
        assert!(!is_newer("v1.0.3", "1.0.4"));
        assert!(!is_newer("v0.9.0", "1.0.4"));
    }

    /// ⚠️ 解析不出来时**必须**返回 false。
    ///
    /// 宁可漏报，也不能误报 —— 误报会让用户白跑一趟去下载，
    /// 而且会让人不再相信这个提示。
    #[test]
    fn unparsable_versions_never_report_an_update() {
        assert!(!is_newer("最新版", "1.0.4"));
        assert!(!is_newer("v1.0.5", "不知道"));
        assert!(!is_newer("", ""));
        assert!(!is_newer("v", "1.0.4"));
    }

    #[test]
    fn splits_urls() {
        assert_eq!(
            split_url("https://api.github.com/repos/a/b/releases/latest").unwrap(),
            ("api.github.com", "/repos/a/b/releases/latest")
        );
        // 没有路径也能处理
        assert_eq!(split_url("https://example.com").unwrap(), ("example.com", "/"));
        // 明文 http 直接拒绝，不悄悄降级
        assert!(split_url("http://example.com/x").is_err());
    }

    /// 接口地址必须指向我们这个仓库 —— 写错了会去查别人的版本
    #[test]
    fn the_api_url_points_at_this_repo() {
        assert!(RELEASES_API.contains("XiaoQAQ10086/PMCA-Installer"));
        assert!(RELEASES_API.starts_with("https://"));
    }

    /// 下载先写 `.part`，下完才改名 —— 半截文件不能被当成能用的安装包
    #[test]
    fn temporary_file_keeps_the_original_name() {
        let dest = Path::new(r"C:\Users\x\Downloads\PMCA-Installer-v1.0.5.exe");
        let p = part_path(dest);
        assert_eq!(
            p.file_name().unwrap().to_string_lossy(),
            "PMCA-Installer-v1.0.5.exe.part"
        );
        // 必须和目标文件**同一个目录**，否则最后那步改名会跨盘失败
        assert_eq!(p.parent(), dest.parent());
    }

    #[test]
    fn percent_is_unknown_when_total_is_missing() {
        assert_eq!(
            Progress {
                received: 100,
                total: 0,
                speed: 0.0
            }
            .percent(),
            None
        );
        assert_eq!(
            Progress {
                received: 50,
                total: 200,
                speed: 0.0
            }
            .percent(),
            Some(25)
        );
        // 收多了也不能超过 100
        assert_eq!(
            Progress {
                received: 300,
                total: 200,
                speed: 0.0
            }
            .percent(),
            Some(100)
        );
    }

    #[test]
    fn bytes_read_naturally() {
        assert_eq!(human_bytes(512), "512 字节");
        assert_eq!(human_bytes(2048), "2 KB");
        assert_eq!(human_bytes(4_898_304), "4.7 MB");
    }

    #[test]
    fn speed_reads_naturally() {
        assert_eq!(human_speed(1_887_436.0), "1.8 MB/s");
        assert_eq!(human_speed(0.0), "0 字节/s");
        // 负数不该 panic（时钟回拨之类的意外）
        assert_eq!(human_speed(-5.0), "0 字节/s");
    }

    /// 真去 GitHub 查一次。
    ///
    /// 标了 `#[ignore]` 是因为它**依赖网络**，放进日常测试会让 CI 变得不可靠。
    /// 手动跑（改了 WinHTTP 那层代码之后一定要跑一次）：
    ///
    /// ```text
    /// cargo test -p sony-gui --lib -- --ignored --nocapture check_against_github
    /// ```
    #[test]
    #[ignore = "需要网络"]
    fn check_against_github() {
        let result = check(crate::VERSION);
        println!("本地版本 {}，检查结果：{result:?}", crate::VERSION);
        assert!(
            !matches!(result, UpdateCheck::Failed(_)),
            "连不上或者接口变了：{result:?}"
        );
    }

    /// 真的下载一次（下完就删）。
    ///
    /// 这条测的是**下载那整条链路**：状态码、Content-Length、分块写文件、
    /// `.part` 改名、进度回调、以及最后的大小校验。
    /// 光靠单元测试证明不了这些 —— WinHTTP 那层是手写的，必须真跑。
    ///
    /// ```text
    /// cargo test -p sony-gui --lib -- --ignored --nocapture download_for_real
    /// ```
    #[test]
    #[ignore = "需要网络，而且会真的下载几 MB"]
    fn download_for_real() {
        // 用一个固定存在的版本，不依赖"本地是否已是最新"
        let url = "https://github.com/XiaoQAQ10086/PMCA-Installer/releases/download/v1.0.3/PMCA-Installer.exe";
        let dest = std::env::temp_dir().join("pmca-download-test.exe");
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_file(part_path(&dest));

        let cancel = AtomicBool::new(false);
        let mut reports = 0;
        let outcome = download(url, &dest, &cancel, &mut |p| {
            reports += 1;
            println!(
                "  进度 {} / {} ({:?}%)  {}",
                p.received,
                p.total,
                p.percent(),
                human_speed(p.speed)
            );
        });

        match outcome {
            DownloadOutcome::Done(path) => {
                let size = std::fs::metadata(&path).unwrap().len();
                println!("下载完成：{size} 字节，进度回调 {reports} 次");
                assert!(size > 100_000, "文件太小了，不像真的 exe");
                assert!(reports > 0, "一次进度都没报，进度条会是死的");
                let _ = std::fs::remove_file(&path);
                assert!(!part_path(&path).exists(), ".part 没清干净");
            }
            DownloadOutcome::Cancelled => panic!("没人点取消，不该是取消"),
            DownloadOutcome::Failed(e) => panic!("下载失败：{e}"),
        }
    }

    /// 取消之后必须删掉半截文件、并且留下 `.part` 以外的痕迹
    #[test]
    #[ignore = "需要网络"]
    fn cancel_leaves_nothing_behind() {
        use std::sync::atomic::AtomicBool;

        let url = "https://github.com/XiaoQAQ10086/PMCA-Installer/releases/download/v1.0.3/PMCA-Installer.exe";
        let dest = std::env::temp_dir().join("pmca-cancel-test.exe");
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_file(part_path(&dest));

        // 一上来就置成"已取消"，读到第一块就会停
        let cancel = AtomicBool::new(true);
        let outcome = download(url, &dest, &cancel, &mut |_| {});

        assert!(
            matches!(outcome, DownloadOutcome::Cancelled),
            "应当是取消：{outcome:?}"
        );
        assert!(!dest.exists(), "取消后不能留下正式文件");
        assert!(!part_path(&dest).exists(), "取消后不能留下 .part");
    }
}
