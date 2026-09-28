//! 极简 HTTP 编解码：相机与我们之间的 REST 通道上跑的就是它。
//!
//! # 为什么自己写而不用现成框架
//!
//! 相机只会说**极窄的一小撮** HTTP：固定几个 URL、HTTP/1.0 或 1.1、
//! 不需要 chunked、不需要 keep-alive、不支持 Range。原项目用 Python 的
//! `http.server`，而我们需要一个 ~100 行、可测试、无额外依赖的实现，
//! 顺带把几个"必须照抄"的怪癖固定下来。
//!
//! # 三个必须照抄的细节
//!
//! 1. **JSON 响应的 `Content-Type` 必须谎报成 SPK 的类型**
//!    （原项目注释：*This is not quite correct, it should be 'application/json',
//!    but it only works that way*）
//! 2. `Content-Length` 必须是**精确的字节数**，否则相机会一直等
//! 3. 原项目虽然写了 `Connection: Keep-Alive`，但实际用的是 HTTP/1.0，
//!    也就是**每个请求关一条连接**。我们保持这个行为（更简单也更稳）。

use anyhow::{bail, Result};

use crate::spk::MIME_TYPE as SPK_MIME;

/// 响应里 `Content-Type` 用的类型。
///
/// 对应原项目 `marketserver/constants.py`：`jsonMimeType = spk.constants.mimeType`。
pub const JSON_MIME_TYPE: &str = SPK_MIME;

/// XPD 清单的类型
pub const XPD_MIME_TYPE: &str = crate::xpd::MIME_TYPE;

/// 一个 HTTP 请求
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// 请求行里的方法，如 `POST`
    pub method: String,
    /// 请求的路径，如 `/task/start`
    pub path: String,
    /// 协议版本，如 `REST/1.0`
    pub protocol: String,
    /// 头部（键统一保留原样，比较时用不区分大小写的方式）
    pub headers: Vec<(String, String)>,
    /// 正文
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// 取一个头部（不区分大小写）
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 解析请求。`data` 是相机发来的一条 REST 正文。
    ///
    /// 头部按 latin-1 解码（相机的头部里可能有非 UTF-8 字节）。
    pub fn parse(data: &[u8]) -> Result<Self> {
        let (head, body) = split_head_body(data)?;
        let head = latin1_decode(head);
        let mut lines = head.split("\r\n");
        let first = lines
            .next()
            .ok_or_else(|| anyhow::anyhow!("HTTP 请求为空"))?;
        let mut parts = first.splitn(3, ' ');
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let protocol = parts.next().unwrap_or_default().to_string();
        if method.is_empty() || path.is_empty() {
            bail!("HTTP 请求行格式不对：{first:?}");
        }

        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        Ok(Self {
            method,
            path,
            protocol,
            headers,
            body: body.to_vec(),
        })
    }
}

/// 一个 HTTP 响应
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// `Content-Disposition` 里的文件名（可选）
    pub filename: Option<String>,
}

impl HttpResponse {
    /// 200 + 指定类型
    pub fn ok(content_type: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: content_type.into(),
            body,
            filename: None,
        }
    }

    /// JSON 响应 —— **类型会谎报成 SPK 的类型**，这是相机的硬要求
    pub fn json(body: Vec<u8>) -> Self {
        Self::ok(JSON_MIME_TYPE, body)
    }

    pub fn with_filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    /// 序列化成要发给相机的字节。
    ///
    /// **逐项对齐原项目**（Python 的 `BaseHTTPRequestHandler`）。原项目发出来的是：
    /// ```text
    /// HTTP/1.0 200 OK\r\n
    /// Server: BaseHTTP/0.6 Python/3.12.10\r\n
    /// Date: <RFC1123 时间>\r\n
    /// Connection: Keep-Alive\r\n
    /// Content-Type: <mime>\r\n
    /// Content-Length: <n>\r\n
    /// [Content-Disposition: attachment;filename="..."\r\n]
    /// \r\n
    /// ```
    ///
    /// ⚠️ 这几个头**都要照抄**。真机上出现过"相机收到安装指令后没有去
    /// 下载应用包、直接关闭连接"，而当时唯一的差异就是这里用了
    /// `Connection: close`、并且缺了 Server/Date。
    pub fn encode(&self) -> Vec<u8> {
        let mut head = format!("HTTP/1.0 {} {}\r\n", self.status, reason(self.status));
        // Python 的 send_response() 会先补这两条
        head.push_str("Server: BaseHTTP/0.6 Python/3.12.10\r\n");
        head.push_str(&format!("Date: {}\r\n", http_date_now()));
        // 再是显式加的那些头
        head.push_str("Connection: Keep-Alive\r\n");
        head.push_str(&format!("Content-Type: {}\r\n", self.content_type));
        head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        if let Some(name) = &self.filename {
            head.push_str(&format!(
                "Content-Disposition: attachment;filename=\"{name}\"\r\n"
            ));
        }
        head.push_str("\r\n");
        let mut out = latin1_encode(&head);
        out.extend_from_slice(&self.body);
        out
    }
}

/// 当前的 HTTP 日期（RFC 1123，如 `Mon, 28 Sep 2026 03:53:38 GMT`）。
///
/// 自己算而不用日期库：只需要把 Unix 时间戳转成"年月日时分秒 + 星期"，
/// 逻辑是固定的，且有测试兜着。
fn http_date_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_http_date(secs)
}

/// 把 Unix 时间戳格式化成 RFC 1123 的 HTTP 日期
fn format_http_date(unix_secs: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let days = unix_secs / 86400;
    let rem = unix_secs % 86400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 是星期四
    let weekday = ((days + 4) % 7) as usize;

    // 从 1970 起逐年扣，得到年月日
    let mut year = 1970u64;
    let mut left = days;
    loop {
        let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
        let ylen = if leap { 366 } else { 365 };
        if left < ylen {
            break;
        }
        left -= ylen;
        year += 1;
    }
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let mlen = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    while month < 12 && left >= mlen[month] {
        left -= mlen[month];
        month += 1;
    }
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[weekday],
        left + 1,
        MONTHS[month],
        year,
        hour,
        minute,
        second
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// 在第一个 `\r\n\r\n` 处切成（头部, 正文）
fn split_head_body(data: &[u8]) -> Result<(&[u8], &[u8])> {
    const SEP: &[u8] = b"\r\n\r\n";
    let pos = data
        .windows(SEP.len())
        .position(|w| w == SEP)
        .ok_or_else(|| anyhow::anyhow!("HTTP 数据里找不到头部与正文的分隔（\\r\\n\\r\\n）"))?;
    Ok((&data[..pos], &data[pos + SEP.len()..]))
}

/// Python 的 "latin-1"：每字符直接映射到一个字节
pub fn latin1_encode(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
        .collect()
}

/// latin-1 解码：每字节直接映射到一个字符
pub fn latin1_decode(b: &[u8]) -> String {
    b.iter().map(|&x| x as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析相机发来的一个真实形态的请求
    #[test]
    fn parses_camera_request() {
        let raw = b"POST /task/complete REST/1.0\r\n\
                    Content-type: application/json\r\n\
                    \r\n\
                    {\"resultCode\":0,\"message\":\"ok\"}";
        let req = HttpRequest::parse(raw).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/task/complete");
        assert_eq!(req.protocol, "REST/1.0");
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(req.header("Content-Type"), Some("application/json"));
        assert_eq!(req.body, b"{\"resultCode\":0,\"message\":\"ok\"}");
    }

    /// 没有正文的请求也要能解析（例如 /task/start 之前的一堆探测）
    #[test]
    fn parses_request_without_body() {
        let raw = b"GET / HTTP/1.0\r\n\r\n";
        let req = HttpRequest::parse(raw).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/");
        assert!(req.body.is_empty());
    }

    /// 缺少分隔符要报错，而不是把整段当正文
    #[test]
    fn rejects_malformed_request() {
        assert!(HttpRequest::parse(b"POST /x HTTP/1.0\r\n").is_err());
        assert!(HttpRequest::parse(b"\r\n\r\n").is_err());
    }

    /// ★ 响应必须谎报 Content-Type 为 SPK 的类型（相机的硬要求）
    #[test]
    fn json_response_spoofs_content_type() {
        let r = HttpResponse::json(b"{\"actions\": []}".to_vec());
        let encoded = r.encode();
        let text = latin1_decode(&encoded);
        assert!(
            text.contains(&format!("Content-Type: {SPK_MIME}")),
            "JSON 响应的类型必须是 SPK 的类型，实际：{text}"
        );
        assert!(
            !text.contains("application/json"),
            "不能出现 application/json（原项目注释明确说这样不行）"
        );
    }

    /// 响应头必须完整且 Content-Length 精确
    #[test]
    fn response_headers_are_complete() {
        let r = HttpResponse::ok("text/plain", b"hello".to_vec());
        let text = latin1_decode(&r.encode());
        assert!(text.starts_with("HTTP/1.0 200 OK\r\n"), "实际：{text}");
        // 原项目（Python BaseHTTPRequestHandler）会补 Server 和 Date
        assert!(text.contains("Server: BaseHTTP/0.6"), "缺少 Server 头：{text}");
        assert!(text.contains("Date: "), "缺少 Date 头：{text}");
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(text.contains("Connection: Keep-Alive\r\n"), "要跟原项目一样用 Keep-Alive：{text}");
        assert!(text.ends_with("\r\n\r\nhello"));
    }

    /// 带文件名的响应（相机下载 SPK 时用）
    #[test]
    fn response_with_filename() {
        let r = HttpResponse::ok(SPK_MIME, vec![1, 2, 3]).with_filename("app.1.spk");
        let text = latin1_decode(&r.encode());
        assert!(
            text.contains("Content-Disposition: attachment;filename=\"app.1.spk\"\r\n"),
            "实际：{text}"
        );
    }

    /// Content-Length 必须按**字节数**算，不能按字符数（正文可能不是 ASCII）
    #[test]
    fn content_length_counts_bytes() {
        let body = vec![0xFFu8; 300]; // 在 latin-1 下是 300 个字符
        let r = HttpResponse::ok(SPK_MIME, body);
        let text = latin1_decode(&r.encode());
        assert!(text.contains("Content-Length: 300\r\n"), "实际：{text}");
    }

    /// 我们自己生成的响应要能被自己的解析器读懂（头部/正文切分正确）
    #[test]
    fn roundtrip_through_own_parser() {
        let r = HttpResponse::ok("text/plain", b"body here".to_vec());
        let enc = r.encode();
        let (head, body) = split_head_body(&enc).unwrap();
        let head = latin1_decode(head);
        assert!(head.starts_with("HTTP/1.0 200 OK"));
        assert_eq!(body, b"body here");
    }

    /// latin-1 往返
    #[test]
    fn latin1_roundtrip() {
        let s = "abc\u{00e9}\u{00ff}";
        let b = latin1_encode(s);
        assert_eq!(b.len(), s.chars().count());
        assert_eq!(latin1_decode(&b), s);
    }
}
