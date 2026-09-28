//! XPD 清单文件与商店 JSON 响应。
//!
//! # XPD 是什么
//!
//! 相机在安装流程里会向"应用商店"索要一个清单文件（扩展名 `.xpd`），
//! 里面告诉它去哪儿下载真正的应用包。我们冒充商店，于是要生成这个文件。
//!
//! # 文件内容（对应原项目 `pmca/xpd/__init__.py`）
//!
//! 就是一个 INI 文件，用 **latin-1** 编码：
//!
//! ```ini
//! [DrmTCD]
//! TCD = https://example.com/app
//! TKN = 0
//! CIC = <HMAC-SHA256 的十六进制小写>
//! ```
//!
//! - `TCD`：真正要下载的目标地址
//! - `TKN`：关联号
//! - `CIC`：`HMAC-SHA256(cicKey, TCD)` 的十六进制小写字符串
//!
//! # 必须照抄的格式细节
//!
//! 原实现用 Python 的 `configparser` 写出，它的格式是固定的：
//! `[节名]\n` + 每个键值对 `键 = 值\n` + **末尾空行**。
//! 我们逐一复刻，并用"与 Python 输出逐字节比对"来验证（见测试）。

use anyhow::{bail, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// HTTP 内容类型（**注意：必须谎报成这个，写成 application/json 相机不认**）
pub const MIME_TYPE: &str = "application/x-psn-dstartup2";

/// INI 节名
pub const SECTION_NAME: &str = "DrmTCD";

/// CIC 校验用的密钥 —— **就是下面这串字符本身（90 个 ASCII 字节）**。
///
/// 来源：`pmca/xpd/constants.py:5`，从 `ScalarAMarket.apk` 与
/// `ScalarAUsbDlApp.apk` 的 `.so` 里提取出来的。
///
/// ⚠️⚠️ **这里有个极其容易踩的坑**：原项目里写的是
/// ```python
/// cicKey = b'8595e68aa50d25dcc52b4d6e6a62af526efd7523a4cc47e212e82e979728d6f0dd02c7e4e79ddb317d56fea2bd'
/// ```
/// 这是个 **Python bytes 字面量**，内容是 90 个 ASCII 字符（`'8'`、`'5'`、`'9'`…），
/// **不是**把这段十六进制解码出来的 45 字节二进制！
///
/// 把它**当成十六进制去解码**（得到 45 字节二进制）是错的：
/// 相机收到这样的 CIC 会回
/// `{"message":"CIC hash mismatch","resultCode":300}`，安装被拒。
/// 必须直接用这 90 个 ASCII 字符当密钥。
///
/// 两个密钥算出的校验值完全不同，务必别改。
pub const CIC_KEY: &str = "8595e68aa50d25dcc52b4d6e6a62af526efd7523a4cc47e212e82e979728d6f0dd02c7e4e79ddb317d56fea2bd";

/// 供兼容旧名字使用（值是同一串字符）
pub const CIC_KEY_HEX: &str = CIC_KEY;

/// HMAC 用的密钥字节：就是上面那串字符的 ASCII 表示（90 字节）
pub fn cic_key() -> Vec<u8> {
    CIC_KEY.as_bytes().to_vec()
}

type HmacSha256 = Hmac<Sha256>;

/// 计算 CIC 校验值：`HMAC-SHA256(cicKey, data)` 的十六进制**小写**字符串
pub fn calculate_checksum(data: &[u8]) -> String {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&cic_key())
        .expect("HMAC 接受任意长度密钥");
    mac.update(data);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// XPD 清单的字段
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xpd {
    /// 要下载的目标地址
    pub tcd: String,
    /// 关联号
    pub tkn: String,
}

impl Xpd {
    pub fn new(tcd: impl Into<String>, tkn: impl Into<String>) -> Self {
        Self {
            tcd: tcd.into(),
            tkn: tkn.into(),
        }
    }

    /// CIC 校验值（对 TCD 按 **latin-1** 编码后计算）
    pub fn checksum(&self) -> String {
        calculate_checksum(&latin1_encode(&self.tcd))
    }

    /// 生成 XPD 文件内容。
    ///
    /// 字段顺序固定为 `TCD`、`TKN`、`CIC`（与原实现一致），
    /// 因为字典顺序会影响输出字节，而相机虽不校验顺序，我们仍照抄以求可比对。
    pub fn dump(&self) -> Vec<u8> {
        let text = format!(
            "[{SECTION_NAME}]\nTCD = {}\nTKN = {}\nCIC = {}\n\n",
            self.tcd,
            self.tkn,
            self.checksum()
        );
        latin1_encode(&text)
    }

    /// 解析一个 XPD 文件
    pub fn parse(data: &[u8]) -> Result<Self> {
        let text = latin1_decode(data);
        let mut tcd = None;
        let mut tkn = None;
        let mut in_section = false;
        let mut saw_section = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                in_section = line == format!("[{SECTION_NAME}]");
                if line == format!("[{SECTION_NAME}]") {
                    saw_section = true;
                }
                continue;
            }
            if !in_section {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "TCD" => tcd = Some(v.to_string()),
                "TKN" => tkn = Some(v.to_string()),
                _ => {}
            }
        }
        if !saw_section {
            bail!("XPD 里没有 [{SECTION_NAME}] 节");
        }
        Ok(Self {
            tcd: tcd.ok_or_else(|| anyhow::anyhow!("XPD 里缺少 TCD"))?,
            tkn: tkn.unwrap_or_default(),
        })
    }
}

/// 商店在"没有更多动作"时返回的空响应
///
/// ⚠️ 必须与 Python 的 `json.dumps({"actions": []})` **逐字节一致**：
/// Python 的空列表写法就是 `[]`，冒号后有一个空格。
pub fn empty_json_response() -> Vec<u8> {
    br#"{"actions": []}"#.to_vec()
}

/// 商店返回的"去下载并安装"指令
///
/// 相机收到后会去 `args` 指定的地址下载 SPK。
///
/// ⚠️ 格式必须与 Python 的 `json.dumps` **逐字节一致**：
/// - 默认分隔符是 `(', ', ': ')`，**冒号和逗号后各有一个空格**
/// - **键的顺序是插入顺序**：`command`、`args`、`attrs`
///
/// ⚠️ 不能图省事用 `serde_json::json!` + `to_vec`，有两个差异：
/// 1. 它输出紧凑格式（没有空格）
/// 2. `serde_json::Value` 内部用 `BTreeMap`，会**把键按字母序重排**
///    → `{"args":…, "attrs":…, "command":…}`
///
/// 虽然任何正常的 JSON 解析器都不该在意顺序和空白，但既然原项目产出是确定的
/// 字节，就手写拼装，彻底消除这个变量。
pub fn install_json_response(app_name: &str, spk_url: &str) -> Vec<u8> {
    let url = json_escape(spk_url);
    let name = json_escape(app_name);
    format!(
        "{{\"actions\": [{{\"command\": \"dlandinstall\", \"args\": \"{url}\", \
         \"attrs\": [{{\"attrname\": \"appname\", \"attrvalue\": \"{name}\"}}]}}]}}"
    )
    .into_bytes()
}

/// JSON 字符串转义（与 Python `json.dumps` 的行为一致）
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            // Python 默认 ensure_ascii=True，非 ASCII 会转成 \uXXXX
            c if (c as u32) > 0x7f => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf).iter() {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Python 的 "latin-1"（ISO-8859-1）：每个字符直接映射到一个字节
pub fn latin1_encode(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
        .collect()
}

/// latin-1 解码：每个字节直接映射到一个字符
pub fn latin1_decode(b: &[u8]) -> String {
    b.iter().map(|&x| x as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CIC 密钥必须与 Python 源文件一致
    #[test]
    fn cic_key_matches_source() {
        // 源文件里是 90 个 ASCII 字符
        assert_eq!(CIC_KEY.len(), 90);
        assert!(CIC_KEY.chars().all(|c| c.is_ascii_hexdigit()));
        // ★ 密钥**就是这串字符本身**，不是解码后的 45 字节
        assert_eq!(cic_key().len(), 90, "密钥是 90 字节的 ASCII 字符串");
        assert_eq!(cic_key(), CIC_KEY.as_bytes());
        // 与源文件逐字节一致（防止抄错）
        assert!(CIC_KEY.starts_with("8595e68aa50d25dcc52b4d6e6a62af52"));
        assert!(CIC_KEY.ends_with("4e79ddb317d56fea2bd"));
    }

    /// ★ 回归测试：CIC 必须与**原项目跑出来的值完全一致**。
    ///
    /// 这组数据是用 `pmca/xpd` 的原始实现（`configparser` + `HMAC` + `SHA256`）跑出来的，
    /// 输入 TCD = `https://www.playmemoriescameraapps.com/`、TKN = `0`。
    ///
    /// 踩过的坑：把 `cicKey = b'85…'` 当成十六进制解码成 45 字节，
    /// 算出的 CIC 是 `c418bdfa…`，相机直接回
    /// `{"message":"CIC hash mismatch","resultCode":300}`。
    #[test]
    fn cic_matches_original_implementation() {
        assert_eq!(
            calculate_checksum(b"https://www.playmemoriescameraapps.com/"),
            "281ae67024e41a14e76299d2ebee8d6e5c03bf527bdc38c0633a45a0d1988d37",
            "CIC 与原项目不一致 —— 相机会拒绝安装"
        );
        // 反例：解码成 45 字节会得到另一个值，绝不能是它
        let wrong_key = hex::decode(CIC_KEY).unwrap();
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&wrong_key).unwrap();
        mac.update(b"https://www.playmemoriescameraapps.com/");
        let wrong: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_ne!(wrong, "281ae67024e41a14e76299d2ebee8d6e5c03bf527bdc38c0633a45a0d1988d37");
    }

    /// 校验和是 64 个十六进制小写字符
    #[test]
    fn checksum_is_hex_sha256() {
        let c = calculate_checksum(b"https://example.com/app");
        assert_eq!(c.len(), 64);
        assert!(c.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(c.chars().all(|ch| !ch.is_ascii_uppercase()), "必须是小写");
    }

    /// XPD 文件的骨架必须与 Python configparser 的输出一致
    #[test]
    fn xpd_layout_matches_configparser() {
        let x = Xpd::new("https://example.com/app", "0");
        let text = latin1_decode(&x.dump());
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("[DrmTCD]"));
        assert_eq!(lines.next(), Some("TCD = https://example.com/app"));
        assert_eq!(lines.next(), Some("TKN = 0"));
        let cic_line = lines.next().unwrap();
        assert!(cic_line.starts_with("CIC = "), "实际：{cic_line}");
        assert_eq!(cic_line.len(), "CIC = ".len() + 64);
        // configparser 写完会多一个空行
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), None);
    }

    /// 解析回读：dump 出来的必须能 parse 回来
    #[test]
    fn xpd_roundtrip() {
        let x = Xpd::new("https://www.playmemoriescameraapps.com/app/1", "42");
        let back = Xpd::parse(&x.dump()).unwrap();
        assert_eq!(back, x);
    }

    /// 缺少 [DrmTCD] 节要报错
    #[test]
    fn xpd_parse_rejects_missing_section() {
        assert!(Xpd::parse(b"[Other]\nTCD = x\n").is_err());
        assert!(Xpd::parse(b"[DrmTCD]\nTKN = 0\n").is_err(), "缺 TCD 应报错");
    }

    /// 非 ASCII 字符不能导致 panic（latin-1 映射）
    #[test]
    fn xpd_handles_non_ascii() {
        let x = Xpd::new("https://例子.com", "0");
        let bytes = x.dump();
        assert!(!bytes.is_empty());
        // latin-1 编码下每个字符 1 字节，超出范围的用 '?' 代替
        assert!(bytes.contains(&b'?'));
        assert_eq!(Xpd::parse(&bytes).unwrap().tcd, "https://??.com");
    }

    /// 空动作响应：必须是紧凑 JSON（相机对空格敏感）
    #[test]
    fn empty_response_is_compact() {
        assert_eq!(empty_json_response(), br#"{"actions": []}"#.to_vec());
    }

    /// 安装指令的 JSON 结构
    #[test]
    fn install_response_structure() {
        let raw = install_json_response("My App", "https://x/y.1.spk");
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["actions"][0]["command"], "dlandinstall");
        assert_eq!(v["actions"][0]["args"], "https://x/y.1.spk");
        assert_eq!(v["actions"][0]["attrs"][0]["attrname"], "appname");
        assert_eq!(v["actions"][0]["attrs"][0]["attrvalue"], "My App");
        // ★ 必须与 Python 的 json.dumps 逐字节一致（冒号/逗号后有空格）
        let s = String::from_utf8_lossy(&raw);
        assert_eq!(
            s,
            r#"{"actions": [{"command": "dlandinstall", "args": "https://x/y.1.spk", "attrs": [{"attrname": "appname", "attrvalue": "My App"}]}]}"#,
            "必须与 Python json.dumps 的输出一致"
        );
        assert!(!s.contains('\n'), "不该有换行：{s}");
    }

    /// MIME 类型必须是那个"谎报"的值
    #[test]
    fn mime_type_is_the_spoofed_one() {
        assert_eq!(MIME_TYPE, "application/x-psn-dstartup2");
        assert_ne!(MIME_TYPE, "application/json");
    }

    /// latin-1 编解码往返
    #[test]
    fn latin1_roundtrip() {
        let s = "abc\u{00e9}\u{00ff}";
        let b = latin1_encode(s);
        assert_eq!(b.len(), s.chars().count(), "每字符一字节");
        assert_eq!(latin1_decode(&b), s);
    }

    /// ★ 黄金样本：必须与 **Python configparser 的真实输出逐字节一致**。
    ///
    /// 这组字节是用 `pmca/xpd` 的原始实现（configparser + HMAC + SHA256）跑出来的，
    /// 输入为 `Xpd::new("https://example.com/app", "0")`。
    /// 只要这个测试通过，就说明我们的 XPD 与原项目产出**完全一样**。
    #[test]
    fn matches_python_configparser_byte_for_byte() {
        let expected = concat!(
            "[DrmTCD]\n",
            "TCD = https://example.com/app\n",
            "TKN = 0\n",
            "CIC = b5715dbf4c4059627181bec028e2a412635e4d565c57b9b99d947c91318a61e6\n",
            "\n",
        );
        let x = Xpd::new("https://example.com/app", "0");
        let got = latin1_decode(&x.dump());
        assert_eq!(
            got, expected,
            "\nXPD 输出与 Python configparser 不一致\n实际：{got:?}\n期望：{expected:?}"
        );
    }

    /// 第二个黄金样本（更长的 URL），同样来自 Python 原始实现
    #[test]
    fn matches_python_configparser_second_sample() {
        let x = Xpd::new("https://www.playmemoriescameraapps.com/app/1", "42");
        let got = x.dump();
        let text = latin1_decode(&got);
        assert!(text.starts_with("[DrmTCD]\nTCD = https://www.playmemoriescameraapps.com/app/1\nTKN = 42\nCIC = "));
        assert!(text.ends_with("\n\n"));
        // CIC 值也必须一致
        assert!(
            text.contains("CIC = 5fbdddc15a192831eddfa12edfb04c557f83ad2c28145480834f75ab6a5c5be4"),
            "CIC 校验值不一致：{text}"
        );
        assert_eq!(got.len(), 141, "与 Python 输出的长度一致");
    }
}
