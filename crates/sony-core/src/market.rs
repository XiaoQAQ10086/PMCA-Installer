//! 假市场服务器 + 安装编排状态机。
//!
//! # 整体流程（对照原项目 `installer/__init__.py`）
//!
//! ```text
//! ① PC → 相机：Start（声明支持 TCPT / REST 两种通道）
//! ② 相机 → PC：Hello（同意开工）
//! ③ PC → 相机：REST POST /task/start  +  XPD 清单
//! ④ 相机 → PC：ProxyConnect（"帮我开一条到 www.playmemoriescameraapps.com:443 的通道"）
//! ⑤ 相机 → PC：ProxyData（里面是 TLS ClientHello 的原始字节）
//!       ↑ 我们不真的联网，而是在进程内扮演那台 HTTPS 服务器
//! ⑥ 相机 → PC：ProxyData（加密的 HTTPS 请求）
//!       → 我们回：JSON 指令"去这个地址下载并安装" / SPK 文件本体
//! ⑦ 相机 → PC：REST POST /task/progress（进度）
//! ⑧ 相机 → PC：REST POST /task/complete（结果，含设备信息）
//! ⑨ PC → 相机：Bye
//! ```
//!
//! **关键点**：相机以为自己在访问 `www.playmemoriescameraapps.com`，
//! 实际上所有 TLS 字节都通过 USB 交给了我们。原项目在注释里明确写了
//! `# Ignoring message.host` —— 连的主机名根本不用管。

use anyhow::{bail, Result};

use crate::http::{HttpRequest, HttpResponse};
use crate::proxy::ProxyMessage;
use crate::session::{Phase, TlsSession};
use crate::sony::{self, Incoming, Outgoing};
use crate::spk;
use crate::xpd;

/// 写进 XPD 清单 `TCD` 的**门户地址**。
///
/// 对应原项目 `marketserver/constants.py` 的 `baseUrl`。
pub const DEFAULT_MARKET_URL: &str = "https://www.playmemoriescameraapps.com/";

/// 安装指令里给相机的**应用包地址**。
///
/// 对应原项目 `LocalMarketServer` 的 `self.url = 'https://' + host + '/'`，
/// 其中 host 默认是 `127.0.0.1`、端口 4443。
/// 主机名其实会被相机忽略（所有 TLS 都经 USB 交给我们），但照抄原项目更稳妥。
pub const DEFAULT_SELF_URL: &str = "https://127.0.0.1/";

/// 安装过程中要报告的进度
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStatus {
    /// 阶段名（相机给的中文/英文原文）
    pub status: String,
    /// 百分比
    pub percent: u32,
    /// 总字节数
    pub total_size: u64,
}

/// 安装任务的最终结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    pub code: i32,
    pub message: String,
}

/// 假市场服务器的状态机（对应原项目 `LocalMarketServer`）
pub struct MarketServer {
    /// 要发给相机的 SPK 包
    spk: Vec<u8>,
    /// 写进 XPD 的**门户地址**（相机拿它问"下一步做什么"）
    portal_url: String,
    /// 安装指令里给相机的**应用包地址**（相机拿它下载 SPK）
    self_url: String,
    /// 相机回传的任务结果（收到一次就记下来）
    result: Option<serde_json::Value>,
}

impl MarketServer {
    /// `apk` 是要安装的应用包原始字节，`portal_url` 是写进 XPD 的门户地址。
    ///
    /// ⚠️ **门户地址与应用包地址是两个不同的 URL**，别用同一个：
    /// - XPD 的 `TCD`（门户地址）：相机拿它去请求"下一步做什么"。
    ///   原项目用 `constants.baseUrl` = `https://www.playmemoriescameraapps.com/`
    /// - 安装指令里的 SPK 地址：相机拿它去下载应用包。
    ///   原项目用**自己服务的地址** `https://127.0.0.1/`
    ///
    /// 虽然所有 TLS 都经 USB 交给我们、主机名实际上被忽略
    /// （原项目注释：`# Ignoring message.host`），两处用同一个地址也能跑通，
    /// 但既然原项目分开了，就照抄，少一个未知数。
    pub fn new(apk: Vec<u8>, portal_url: impl Into<String>) -> Result<Self> {
        let packed = spk::dump(&apk)?;
        Ok(Self {
            spk: packed.bytes,
            portal_url: portal_url.into(),
            self_url: DEFAULT_SELF_URL.to_string(),
            result: None,
        })
    }

    /// 生成要给相机的 XPD 清单（`TCD` 用**门户地址**）
    pub fn xpd(&self) -> Vec<u8> {
        // 关联号固定用 "0"（原项目就是写死的）
        xpd::Xpd::new(&self.portal_url, "0").dump()
    }

    /// 相机回传的结果（未收到时为 None）
    pub fn result(&self) -> Option<&serde_json::Value> {
        self.result.as_ref()
    }

    /// 处理一个 HTTPS 请求，给出响应
    ///
    /// 对应原项目 `server.py` 的 `handlePost` / `handleGet`：
    /// - `POST`：相机在汇报（含最终结果），我们回"下一步做什么"
    /// - `GET`：相机来下载 SPK
    pub fn handle(&mut self, req: &HttpRequest) -> Option<HttpResponse> {
        match req.method.as_str() {
            "POST" => {
                // 还没拿到结果、且我们有包 → 让相机去下载并安装
                let body = if self.result.is_none() && !self.spk.is_empty() {
                    xpd::install_json_response("app", &self.self_url)
                } else {
                    xpd::empty_json_response()
                };
                // 把相机汇报的内容记下来（它就是最终的设备信息）
                if let Some(v) = parse_json_loose(&req.body) {
                    self.result = Some(v);
                }
                Some(HttpResponse::json(body))
            }
            "GET" => {
                // 相机来拿 SPK
                Some(
                    HttpResponse::ok(spk::MIME_TYPE, self.spk.clone())
                        .with_filename(format!("app{}", spk::EXTENSION)),
                )
            }
            _ => None,
        }
    }
}

/// 宽松地解析 JSON。
///
/// 相机的回复/汇报里可能带**尾部填充的 0 字节或空行**（USB 传输会补齐缓冲），
/// 直接用 `serde_json` 会解析失败，于是本该触发的分支被静默跳过 ——
/// 真机上我就是这样漏掉了"相机拒绝了 start"这个信号。
fn parse_json_loose(body: &[u8]) -> Option<serde_json::Value> {
    let end = body
        .iter()
        .rposition(|b| !b.is_ascii_whitespace() && *b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    serde_json::from_slice(&body[..end]).ok()
}

/// 诊断日志文件句柄（进程级）
static DIAG_FILE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

/// 开始把诊断信息同时写入文件。
///
/// 为什么需要它：真机上**每次插拔复位后只有一次机会**（相机记住未完成的任务后
/// 会拒绝新的 start），所以关键信息必须落盘，不能只靠控制台。
pub fn diag_init(path: &str) -> std::io::Result<()> {
    let f = std::fs::File::create(path)?;
    if let Ok(mut g) = DIAG_FILE.lock() {
        *g = Some(f);
    }
    Ok(())
}

/// 写一条诊断：同时到 stderr 和日志文件（若有）
///
/// 带**毫秒时间戳** —— 真机联调时"什么时候发的什么"往往和内容一样重要
/// （能看出相机是不是在等某个超时、响应是不是太慢）。
///
/// ⚠️ 这里**不能用 `eprintln!`**。
/// 图形界面版是 GUI 子系统程序（不弹黑框），**没有控制台**，
/// 此时 stderr 句柄是无效的，而 `eprintln!` 写失败时会 **panic**
/// （`failed printing to stderr`）—— 等于"因为写不了日志把安装搞崩了"。
///
/// 所以用 `let _ = writeln!(...)`：写不出去就算了，
/// 诊断输出本来就不该影响主流程。
pub fn trace_diag(msg: &str) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() % 100_000_000)
        .unwrap_or(0);

    {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "[{ms:08}] {msg}");
    }

    if let Ok(mut g) = DIAG_FILE.lock()
        && let Some(f) = g.as_mut()
    {
        use std::io::Write;
        let _ = writeln!(f, "[{ms:08}] {msg}");
        let _ = f.flush();
    }
}

/// 编排状态机当前所处的阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    /// 刚创建，还没发 Start
    NotStarted,
    /// 已发 Start，等相机 Hello
    WaitingHello,
    /// 已打招呼，双向通信中
    Running,
    /// 相机表示任务完成
    Done,
    /// 出错了
    Failed,
}

/// 一次安装任务的编排器。
///
/// 它**不碰 USB**：所有收发都通过 [`InstallRunner::poll`] 交给调用方。
/// 这样同一套逻辑既能跑在真机上，也能跑在测试里的"假相机"上。
pub struct InstallRunner {
    phase: InstallPhase,
    market: MarketServer,
    session: TlsSession,
    /// 待发送的消息队列
    outgoing: Vec<ProxyMessage>,
    /// 最近一次上报的进度
    last_status: Option<TaskStatus>,
    /// 最终结果
    result: Option<TaskResult>,
    /// 相机当前那条 TLS 隧道对应的 socketFd
    active_socket: Option<i32>,
    /// 出错原因
    error: Option<String>,
    /// 相机是否已接受本次 start
    start_accepted: bool,
    /// 是否已经主动收尾过隧道（避免重复发 ProxyEnd）
    tunnel_closed: bool,

}

impl InstallRunner {
    /// `apk` 是应用包原始字节
    pub fn new(apk: Vec<u8>, server_random: [u8; 32]) -> Result<Self> {
        Ok(Self {
            phase: InstallPhase::NotStarted,
            market: MarketServer::new(apk, DEFAULT_MARKET_URL)?,
            session: TlsSession::new(server_random),
            outgoing: Vec::new(),
            last_status: None,
            result: None,
            active_socket: None,
            error: None,
            start_accepted: false,
            tunnel_closed: false,
        })
    }

    pub fn phase(&self) -> InstallPhase {
        self.phase
    }

    /// 告诉编排器："打招呼这一步已经完成了，直接进入运行阶段"。
    ///
    /// 真机上 `SonySession::handshake()` 已经替我们收发过 Start/Hello 了，
    /// 所以编排器不必再发一次，直接从"发 XPD 开始安装"往下走。
    pub fn mark_hello_done(&mut self) {
        self.phase = InstallPhase::Running;
        let xpd = self.market.xpd();
        self.outgoing
            .push(Outgoing::Rest { data: build_task_start(&xpd) }.encode());
    }

    pub fn last_status(&self) -> Option<&TaskStatus> {
        self.last_status.as_ref()
    }

    pub fn result(&self) -> Option<&TaskResult> {
        self.result.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 相机回传的完整设备信息（安装完成时才有）
    pub fn device_report(&self) -> Option<&serde_json::Value> {
        self.market.result()
    }

    /// 启动：把 Start 消息排进发送队列
    pub fn start(&mut self) {
        self.outgoing.push(
            Outgoing::Start {
                protocols: sony::PROTOCOLS.to_vec(),
            }
            .encode(),
        );
        self.phase = InstallPhase::WaitingHello;
    }

    /// 取走待发送的消息
    pub fn take_outgoing(&mut self) -> Vec<ProxyMessage> {
        std::mem::take(&mut self.outgoing)
    }

    pub fn has_outgoing(&self) -> bool {
        !self.outgoing.is_empty()
    }

    /// 喂入一条收到的消息。返回 `true` 表示任务已结束（成功或失败）。
    pub fn poll(&mut self, msg: ProxyMessage) -> Result<bool> {
        let incoming = sony::parse_incoming(&msg)?;
        match incoming {
            Incoming::Hello { protocols } => {
                // 相机同意了，接着用 REST 发起安装任务
                if !protocols.iter().any(|(n, _)| n == b"REST") {
                    self.fail("相机不支持 REST 通道，无法继续");
                    return Ok(true);
                }
                self.phase = InstallPhase::Running;
                let xpd = self.market.xpd();
                let req = build_task_start(&xpd);
                self.outgoing
                    .push(Outgoing::Rest { data: req }.encode());
            }
            Incoming::ProxyConnect {
                socket_fd,
                host,
                port,
            } => {
                // 相机要开 TLS 隧道。原项目在这里真的去连远端；
                // 我们不联网，直接在进程内当那台服务器。
                trace_diag(&format!(
                    "[隧道] 相机请求开连接 fd={socket_fd}，目标 {host}:{port}（主机名会被忽略）"
                ));
                self.active_socket = Some(socket_fd);
                self.session = TlsSession::new(self.session.server_random());
                self.tunnel_closed = false;
            }
            Incoming::ProxyData { socket_fd, data } => {
                self.active_socket = Some(socket_fd);
                self.feed_tls(socket_fd, &data)?;
            }
            Incoming::ProxyDisconnect { socket_fd } => {
                trace_diag(&format!("[隧道] 相机断开了连接 fd={socket_fd}"));
                self.active_socket = None;
            }
            Incoming::Rest { data } => {
                self.on_rest(&data)?;
                if self.phase == InstallPhase::Done {
                    self.outgoing.push(Outgoing::Bye.encode());
                    return Ok(true);
                }
                // 失败也要立刻结束 —— 否则主循环会一直空等到超时，
                // 用户看到的是"卡住"而不是明确的错误
                if self.phase == InstallPhase::Failed {
                    return Ok(true);
                }
            }
            Incoming::Bye => {
                self.fail("相机主动断开了通信");
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 把相机发来的 TLS 字节喂给会话，并把会话产生的响应发回去
    fn feed_tls(&mut self, socket_fd: i32, data: &[u8]) -> Result<()> {
        let mut reply: Vec<u8> = Vec::new();
        let records = crate::tls::split_records(data);
        trace_diag(&format!(
            "[TLS] 收到 {} 字节 → {} 条记录，类型 {:?}",
            data.len(),
            records.len(),
            records.iter().map(|r| r.0).collect::<Vec<_>>()
        ));
        for (ct, ver, body) in records {
            let out = match self.session.feed_record(ct, ver, &body) {
                Ok(o) => o,
                Err(e) => {
                    trace_diag(&format!("[TLS] ⚠️ 处理记录失败：{e}"));
                    self.fail(&format!("TLS 处理失败：{e}"));
                    return Ok(());
                }
            };
            if !out.is_empty() {
                trace_diag(&format!("[TLS] 产出 {} 字节要回给相机", out.len()));
            }
            reply.extend_from_slice(&out);

            // 握手完成后，把已解密的应用层请求取出来处理
            if self.session.phase() == Phase::Established {
                let requests = self.session.take_requests();
                if !requests.is_empty() {
                    trace_diag(&format!("[TLS] 解密出应用层数据 {} 字节", requests.len()));
                    self.on_https(&requests, socket_fd, &mut reply)?;
                }
            }
        }
        if !reply.is_empty() {
            self.outgoing.push(
                Outgoing::ProxyData {
                    socket_fd,
                    data: reply,
                }
                .encode(),
            );
        }
        // 真机实测：相机只发了 TLS 的 close_notify，**没发隧道层的断开请求**。
        // 它可能在等我们把这条隧道收尾。补一条 ProxyEnd。
        if self.session.peer_closed() && !self.tunnel_closed {
            self.tunnel_closed = true;
            trace_diag(&format!("[隧道] 主动收尾 fd={socket_fd}（发 ProxyEnd）"));
            self.outgoing.push(Outgoing::ProxyEnd { socket_fd }.encode());
        }
        Ok(())
    }

    /// 处理一条已解密的 HTTPS 请求
    fn on_https(&mut self, request: &[u8], _socket_fd: i32, reply: &mut Vec<u8>) -> Result<()> {
        trace_diag(&format!("[HTTPS] 原始请求（{} 字节）", request.len()));
        // 完整正文分块记录 —— 相机的设备信息里可能有关键线索
        for (i, chunk) in request.chunks(400).enumerate() {
            trace_diag(&format!(
                "[HTTPS]   正文#{i}: {}",
                String::from_utf8_lossy(chunk)
            ));
        }
        let req = match HttpRequest::parse(request) {
            Ok(r) => r,
            Err(e) => {
                trace_diag(&format!("[HTTPS] ⚠️ 解析失败：{e}"));
                return Ok(());
            }
        };
        let Some(resp) = self.market.handle(&req) else {
            trace_diag(&format!("[HTTPS] ⚠️ 没有对应处理：{} {}", req.method, req.path));
            return Ok(());
        };
        trace_diag(&format!(
            "[HTTPS] → 回复 {} {} 字节，类型 {}",
            resp.status,
            resp.body.len(),
            resp.content_type
        ));
        let encoded = resp.encode();
        let encrypted = self.session.encrypt_application_data(&encoded)?;
        reply.extend_from_slice(&encrypted);
        Ok(())
    }

    /// 处理一条 REST 消息（我们用明文 REST 通道，不经 TLS）
    fn on_rest(&mut self, data: &[u8]) -> Result<()> {
        // 明文 REST 上跑的是 HTTP 形态的文本，但相机的 REST 通道
        // 在此流程里主要是 /task/progress 与 /task/complete
        let req = match HttpRequest::parse(data) {
            Ok(r) => r,
            Err(e) => {
                trace_diag(&format!("[REST] ⚠️ 解析失败（{e}），原始内容：{:?}", String::from_utf8_lossy(data)));
                return Ok(());
            }
        };
        // 相机回给我们的 HTTP 响应（例如对 /task/start 的 200 OK）也会走这里，
        // 它的第一行是 "REST/1.0 200 OK"，解析出来 method 就是 "REST/1.0"。
        // 这种不是请求，而是**响应**，要把里面的内容打出来看看（可能带错误原因）。
        if req.method.starts_with("REST/") || req.method.starts_with("HTTP/") {
            // 相机给我们的 HTTP 响应（例如对 /task/start 的 200 OK）
            trace_diag(&format!(
                "[相机回复] {} — {}",
                req.method,
                String::from_utf8_lossy(&req.body)
            ));
            // 相机可能因为"上一次任务没走完"而拒绝新的 start（真机实测：
            // resultCode 10 / "Start not accepted"）。
            //
            // 我试过自己发 `/task/complete` 去替它收尾，但相机回
            // `{"message":"Unknown request","resultCode":100}` —— 它不认我们拼的
            // 那条请求。所以这个状态**只能靠拔插相机复位**，程序清不掉。
            // 这里就把话说清楚，别让用户对着"卡住"猜。
            if !self.start_accepted
                && let Some(v) = parse_json_loose(&req.body)
            {
                let code = v["resultCode"].as_i64().unwrap_or(0);
                if code != 0 {
                    trace_diag(&format!(
                        "[恢复] 相机拒绝了 start（resultCode {code}）：{}",
                        v["message"].as_str().unwrap_or("")
                    ));
                    trace_diag(
                        "[恢复] 原因：相机里残留着上一次未完成的任务。\n\
                         [恢复] 解决：把相机 USB 拔下来再插上（复位），然后重新运行。",
                    );
                    self.fail(
                        "相机拒绝了开始任务（里面残留着上一次未完成的任务）。\n\
                         请把相机 USB 拔下来再插上（复位），然后重新运行本命令。",
                    );
                    return Ok(());
                }
                self.start_accepted = true;
                trace_diag("[恢复] 相机接受了 start");
            }
            return Ok(());
        }
        trace_diag(&format!("[REST] 请求 {} {}", req.method, req.path));
        match req.path.as_str() {
            "/task/progress" => {
                trace_diag(&format!(
                    "[REST] 进度：{}",
                    String::from_utf8_lossy(&req.body)
                ));
                if let Some(v) = parse_json_loose(&req.body) {
                    self.last_status = Some(TaskStatus {
                        status: v["status text"].as_str().unwrap_or("").to_string(),
                        percent: v["percent"].as_u64().unwrap_or(0) as u32,
                        total_size: v["total size"].as_u64().unwrap_or(0),
                    });
                }
            }
            "/task/complete" => {
                trace_diag(&format!(
                    "[REST] 完成汇报：{}",
                    String::from_utf8_lossy(&req.body)
                ));
                // ⚠️ 只取结果，**不要**调 `market.handle` —— 那会把相机先前通过
                //    HTTPS POST 上报的完整设备信息（accountinfo / deviceinfo 等）
                //    覆盖成这条只有 resultCode 的汇报。
                //    原项目里两条路径走的是不同的 handler，所以不会互相覆盖；
                //    我们这里必须显式保住先到的那份。
                let (code, message) = match parse_json_loose(&req.body) {
                    Some(v) => (
                        v["resultCode"].as_i64().unwrap_or(0) as i32,
                        v["message"].as_str().unwrap_or("").to_string(),
                    ),
                    None => (0, String::new()),
                };
                self.result = Some(TaskResult { code, message });
                self.phase = InstallPhase::Done;
            }
            _ => {}
        }
        Ok(())
    }


    fn fail(&mut self, why: &str) {
        self.phase = InstallPhase::Failed;
        self.error = Some(why.to_string());
    }

    /// 给用户看的进度文本（中文）
    pub fn progress_text(&self) -> String {
        match self.phase {
            InstallPhase::NotStarted => "准备中…".to_string(),
            InstallPhase::WaitingHello => "正在与相机建立连接…".to_string(),
            InstallPhase::Running => match &self.last_status {
                Some(s) => format!("{}（{}%）", s.status, s.percent),
                None => "正在安装…".to_string(),
            },
            InstallPhase::Done => match &self.result {
                Some(r) if r.code == 0 => "安装完成".to_string(),
                Some(r) => format!("安装失败：{}", r.message),
                None => "安装完成".to_string(),
            },
            InstallPhase::Failed => {
                format!("出错了：{}", self.error.as_deref().unwrap_or("未知原因"))
            }
        }
    }
}

/// 构造发往相机的 `/task/start` 请求。
///
/// 对应原项目 `installer/__init__.py:17-18` 的 `_buildRequest`：
/// ```text
/// POST /task/start REST/1.0\r\nContent-type: application/x-psn-dstartup2\r\n\r\n<xpd>
/// ```
pub fn build_task_start(xpd_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(xpd_data.len() + 64);
    out.extend_from_slice(b"POST /task/start REST/1.0\r\n");
    out.extend_from_slice(format!("Content-type: {}\r\n\r\n", xpd::MIME_TYPE).as_bytes());
    out.extend_from_slice(xpd_data);
    out
}

/// 校验安装任务的结果是否符合预期
pub fn check_result(r: &TaskResult) -> Result<()> {
    if r.code != 0 {
        bail!("相机报告安装失败（代码 {}）：{}", r.code, r.message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把一段 HTTP 文本包成相机方向的 REST 代理消息
    fn make_rest(http: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&sony::REST_IN.to_be_bytes());
        payload.extend_from_slice(&(http.len() as u16).to_be_bytes());
        payload.extend_from_slice(http);
        payload
    }

    fn apk() -> Vec<u8> {
        vec![0x50, 0x4B, 0x03, 0x04, 1, 2, 3, 4, 5, 6, 7, 8]
    }

    /// /task/start 请求的字节必须与原项目一致
    #[test]
    fn task_start_request_layout() {
        let x = xpd::Xpd::new(DEFAULT_MARKET_URL, "0").dump();
        let req = build_task_start(&x);

        // 头部与正文的分界
        let sep = req
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("应有头部/正文分隔");
        let head = String::from_utf8_lossy(&req[..sep]);
        assert!(
            head.starts_with("POST /task/start REST/1.0\r\n"),
            "实际头部：{head}"
        );
        assert!(
            head.contains(&format!("Content-type: {}", xpd::MIME_TYPE)),
            "实际头部：{head}"
        );
        assert_eq!(&req[sep + 4..], &x[..], "正文应该是完整的 XPD 内容");
    }

    /// POST 请求：第一次回"去下载安装"，之后回"没有更多动作"
    #[test]
    fn market_post_returns_install_then_empty() {
        let mut m = MarketServer::new(apk(), DEFAULT_MARKET_URL).unwrap();

        // 第一次：相机汇报，我们让它去下载
        let req = HttpRequest::parse(
            b"POST /task/start REST/1.0\r\nContent-type: application/json\r\n\r\n{\"a\":1}",
        )
        .unwrap();
        let resp = m.handle(&req).unwrap();
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("dlandinstall"), "第一次应回安装指令：{body}");

        // 第二次：已经有结果了，回空
        let resp = m.handle(&req).unwrap();
        let body = String::from_utf8_lossy(&resp.body);
        assert_eq!(body, "{\"actions\": []}", "第二次应回空动作");
    }

    /// GET 请求：把 SPK 发给相机，类型与文件名都要对
    #[test]
    fn market_get_returns_spk() {
        let mut m = MarketServer::new(apk(), DEFAULT_MARKET_URL).unwrap();
        let req = HttpRequest::parse(b"GET /app.1.spk HTTP/1.0\r\n\r\n").unwrap();
        let resp = m.handle(&req).unwrap();
        assert_eq!(resp.content_type, spk::MIME_TYPE);
        assert_eq!(resp.filename.as_deref(), Some("app.1.spk"));
        assert!(spk::is_spk(&resp.body), "返回的应该是合法的 SPK 容器");
        assert!(
            resp.body.len() > apk().len(),
            "SPK 比 APK 大（有头部与填充）"
        );
    }

    /// XPD 清单要能被原项目的解析器读回来
    #[test]
    fn market_xpd_is_parseable() {
        let m = MarketServer::new(apk(), DEFAULT_MARKET_URL).unwrap();
        let x = xpd::Xpd::parse(&m.xpd()).unwrap();
        assert_eq!(x.tcd, DEFAULT_MARKET_URL);
        assert_eq!(x.tkn, "0");
    }

    /// 编排器：Start 消息要排进队列
    #[test]
    fn runner_start_queues_start_message() {
        let mut r = InstallRunner::new(apk(), [1u8; 32]).unwrap();
        assert_eq!(r.phase(), InstallPhase::NotStarted);
        r.start();
        assert_eq!(r.phase(), InstallPhase::WaitingHello);
        let out = r.take_outgoing();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].msg_type, sony::MSG_COMMON);
        assert!(!r.has_outgoing(), "取走之后队列应为空");
    }

    /// 收到 Hello 后要主动发起 /task/start
    #[test]
    fn runner_hello_triggers_task_start() {
        let mut r = InstallRunner::new(apk(), [1u8; 32]).unwrap();
        r.start();
        let _ = r.take_outgoing();

        // 手工构造一条 Hello（协议表：u32 个数 + 每项 4 字节名 + 2 字节 id）
        let protos: [([u8; 4], u16); 2] = [(*b"TCPT", 0x01), (*b"REST", 0x100)];
        let mut body = (protos.len() as u32).to_be_bytes().to_vec();
        for (name, id) in protos {
            body.extend_from_slice(&name);
            body.extend_from_slice(&id.to_be_bytes());
        }
        let msg = ProxyMessage::new(
            sony::MSG_COMMON,
            sony::encode_common(sony::COMMON_HELLO, &body),
        );

        let done = r.poll(msg).unwrap();
        assert!(!done);
        assert_eq!(r.phase(), InstallPhase::Running);

        let out = r.take_outgoing();
        assert_eq!(out.len(), 1, "应发起 /task/start");
        assert_eq!(out[0].msg_type, sony::MSG_REST);
    }

    /// 相机不支持 REST 时要立刻失败，而不是挂住
    #[test]
    fn runner_fails_if_no_rest_support() {
        let mut r = InstallRunner::new(apk(), [1u8; 32]).unwrap();
        r.start();
        let _ = r.take_outgoing();

        // 只声明 TCPT
        let mut body = (1u32).to_be_bytes().to_vec();
        body.extend_from_slice(b"TCPT");
        body.extend_from_slice(&1u16.to_be_bytes());
        let msg = ProxyMessage::new(
            sony::MSG_COMMON,
            sony::encode_common(sony::COMMON_HELLO, &body),
        );
        let done = r.poll(msg).unwrap();
        assert!(done, "应立刻结束");
        assert_eq!(r.phase(), InstallPhase::Failed);
        assert!(r.error().unwrap().contains("REST"));
    }

    /// /task/complete 要能取到结果并结束
    #[test]
    fn runner_completes_on_task_complete() {
        let mut r = InstallRunner::new(apk(), [1u8; 32]).unwrap();
        r.start();
        let _ = r.take_outgoing();
        r.phase = InstallPhase::Running;

        let body = br#"{"resultCode":0,"message":"success"}"#;
        // REST 消息里装的是一条 HTTP 形态的请求
        let mut req = b"POST /task/complete REST/1.0\r\n\r\n".to_vec();
        req.extend_from_slice(body);
        let mut payload = Vec::new();
        payload.extend_from_slice(&sony::REST_IN.to_be_bytes());
        payload.extend_from_slice(&(req.len() as u16).to_be_bytes());
        payload.extend_from_slice(&req);
        let msg = ProxyMessage::new(sony::MSG_REST, payload);

        let done = r.poll(msg).unwrap();
        assert!(done, "应结束");
        assert_eq!(r.phase(), InstallPhase::Done);
        let res = r.result().unwrap();
        assert_eq!(res.code, 0);
        assert_eq!(res.message, "success");
        assert!(r.progress_text().contains("安装完成"));
    }

    /// /task/progress 要能更新进度
    #[test]
    fn runner_tracks_progress() {
        let mut r = InstallRunner::new(apk(), [1u8; 32]).unwrap();
        r.start();
        let _ = r.take_outgoing();
        r.phase = InstallPhase::Running;

        // JSON 里含中文，所以用普通字符串再转成字节
        let json = r#"{"status":"downloading","status text":"正在下载","percent":42,"total size":1000}"#
            .as_bytes();
        let mut req = b"POST /task/progress REST/1.0\r\n\r\n".to_vec();
        req.extend_from_slice(json);
        let mut payload = Vec::new();
        payload.extend_from_slice(&sony::REST_IN.to_be_bytes());
        payload.extend_from_slice(&(req.len() as u16).to_be_bytes());
        payload.extend_from_slice(&req);

        r.poll(ProxyMessage::new(sony::MSG_REST, payload)).unwrap();
        let s = r.last_status().unwrap();
        assert_eq!(s.status, "正在下载");
        assert_eq!(s.percent, 42);
        assert_eq!(s.total_size, 1000);
        assert!(r.progress_text().contains("正在下载"));
    }

    /// 相机拒绝 start（resultCode 10）时，应**明确失败并提示拔插复位**。
    ///
    /// 这个状态**只能靠用户拔插相机**，程序清不掉：真机上试过自动发
    /// `/task/complete` 替它收尾，相机回
    /// `{"message":"Unknown request","resultCode":100}` —— 它不认那条请求。
    /// 测试就固定这个行为。
    #[test]
    fn runner_fails_clearly_when_start_rejected() {
        let mut r = InstallRunner::new(apk(), [1u8; 32]).unwrap();
        r.start();
        let _ = r.take_outgoing();
        r.phase = InstallPhase::Running;

        // 相机回一条 "Start not accepted"
        let body = br#"{"message":"Start not accepted","resultCode":10}"#;
        let mut req = b"REST/1.0 200 OK\r\n\r\n".to_vec();
        req.extend_from_slice(body);
        let payload = make_rest(&req);

        let done = r.poll(ProxyMessage::new(sony::MSG_REST, payload)).unwrap();
        assert!(done, "被拒绝时应立刻结束（不要空等）");
        assert_eq!(r.phase(), InstallPhase::Failed);
        let msg = r.error().unwrap();
        assert!(msg.contains("拔"), "错误信息要告诉用户拔插相机：{msg}");
        assert!(r.take_outgoing().is_empty(), "不该再发无效请求");
    }

    /// 相机接受 start 后，不应再做恢复动作
    #[test]
    fn runner_marks_start_accepted() {
        let mut r = InstallRunner::new(apk(), [1u8; 32]).unwrap();
        r.start();
        let _ = r.take_outgoing();
        r.phase = InstallPhase::Running;

        let body = br#"{"message":"Start accepted","resultCode":0}"#;
        let mut req = b"REST/1.0 200 OK\r\n\r\n".to_vec();
        req.extend_from_slice(body);
        let mut payload = Vec::new();
        payload.extend_from_slice(&sony::REST_IN.to_be_bytes());
        payload.extend_from_slice(&(req.len() as u16).to_be_bytes());
        payload.extend_from_slice(&req);

        r.poll(ProxyMessage::new(sony::MSG_REST, payload)).unwrap();
        assert!(r.take_outgoing().is_empty(), "接受后不该再发东西");
        assert_eq!(r.phase(), InstallPhase::Running, "不该变成失败");
    }

    /// 结果码非 0 时 check_result 要报错
    #[test]
    fn check_result_rejects_nonzero() {
        assert!(check_result(&TaskResult {
            code: 0,
            message: String::new()
        })
        .is_ok());
        let e = check_result(&TaskResult {
            code: 7,
            message: "boom".into(),
        })
        .unwrap_err();
        assert!(e.to_string().contains("boom"));
    }
}
