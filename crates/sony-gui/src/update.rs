//! 检查有没有新版本。

//!
//! # 用什么发请求
//!
//! 用 **Windows 自带的 WinHTTP**，不引入任何 HTTP 库。
//!
//! 理由是体积：`ureq` / `reqwest` 这类库会把整套 TLS 实现（ring、rustls…）
//! 编进 exe，体积要涨一两 MB；而这个程序总共才 4 MB 多，
//! 系统既然本来就有一套能用的 HTTPS，就没必要自己带一套。
//!
//! 代价是要手写一层 WinHTTP 绑定 —— 但只有几十行，而且都是固定签名。
//!
//! # 检查失败不算错误
//!
//! 用户可能没网、可能在公司内网、可能接口被墙。
//! **一个装应用的小工具不该因为查不到更新就报错或者变慢。**
//! 所以：
//! - 检查放在后台线程，绝不阻塞界面
//! - 失败时只记一句日志，界面上不弹任何东西
//! - 手动点"检查更新"时才把失败原因说出来

use anyhow::{Result, bail};
use core::ffi::c_void;

use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen,
    WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts,
};

/// 查询最新版本的接口
const RELEASES_API: &str =
    "https://api.github.com/repos/XiaoQAQ10086/PMCA-Installer/releases/latest";

/// 万一接口没给出链接，就退到发布页
pub const RELEASES_PAGE: &str = "https://github.com/XiaoQAQ10086/PMCA-Installer/releases/latest";

/// 超时（毫秒）。
///
/// 分开设是刻意的：连不上要快点放弃，而读取可以宽一些 ——
/// 用户可能网速很慢，但一直连不上就该赶紧让路。
const TIMEOUT_RESOLVE: i32 = 4_000;
const TIMEOUT_CONNECT: i32 = 5_000;
const TIMEOUT_SEND: i32 = 5_000;
const TIMEOUT_RECEIVE: i32 = 8_000;

/// 检查更新的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    /// 有新版本可用
    Available {
        /// 服务器上的版本号（已去掉开头的 `v`），例如 `1.0.5`
        version: String,
        /// 下载页地址
        url: String,
    },
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

impl UpdateCheck {
    /// 这次检查有没有值得告诉用户的好消息
    pub fn has_news(&self) -> bool {
        matches!(self, UpdateCheck::Available { .. })
    }
}

/// 查一次更新。
///
/// ⚠️ **这是同步阻塞的**，调用方必须放到后台线程里 ——
/// 网络不好的时候它会卡好几秒，绝不能放在界面线程上。
pub fn check(current: &str) -> UpdateCheck {
    let body = match http_get(RELEASES_API, current) {
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
    let version = strip_v(tag);
    let url = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or(RELEASES_PAGE)
        .to_string();

    if is_newer(tag, current) {
        UpdateCheck::Available { version, url }
    } else {
        UpdateCheck::UpToDate { latest: version }
    }
}

/// `v1.0.4` → `1.0.4`
fn strip_v(tag: &str) -> String {
    tag.trim()
        .trim_start_matches(['v', 'V'])
        .to_string()
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

// ---------------------------------------------------------------- WinHTTP

/// 保证句柄一定会被关掉 —— WinHTTP 的句柄不关就是泄漏
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// 发一个 HTTPS GET，返回正文
fn http_get(url: &str, user_agent_version: &str) -> Result<String> {
    let (host, path) = split_url(url)?;
    let agent = to_wide(&format!("PMCA-Installer/{user_agent_version}"));
    let host_w = to_wide(host);
    let path_w = to_wide(path);
    let verb = to_wide("GET");

    unsafe {
        // 用 AUTOMATIC_PROXY：这样会跟随系统的代理设置。
        // 对需要挂代理才能访问 GitHub 的用户（国内挺常见），这一点很重要。
        let session = Handle(WinHttpOpen(
            agent.as_ptr(),
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

        // 状态码：404 表示这个仓库还没有任何 release
        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let ok = WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&raw mut status).cast::<c_void>(),
            &mut len,
            std::ptr::null_mut(),
        );
        if ok != 0 {
            match status {
                200 => {}
                404 => bail!("这个仓库还没有发布任何版本"),
                other => bail!("服务器返回 {other}"),
            }
        }

        // 逐块读正文
        let mut body = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let mut read: u32 = 0;
            if WinHttpReadData(
                request.0,
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len() as u32,
                &mut read,
            ) == 0
            {
                bail!("读取响应内容时断开了");
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
            // 防呆：接口返回的 JSON 只有几百字节，超过 1 MB 一定是出问题了
            if body.len() > 1 << 20 {
                bail!("响应内容异常地大");
            }
        }

        String::from_utf8(body).map_err(|e| anyhow::anyhow!("响应不是合法的 UTF-8：{e}"))
    }
}

/// 把 URL 拆成 `(主机, 路径)`。
///
/// 只认 `https://` —— 检查更新没有任何理由走明文 HTTP，
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
}
