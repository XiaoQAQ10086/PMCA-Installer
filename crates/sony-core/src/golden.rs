//! 录音器采集到的"黄金样本"。
//!
//! 这些字节是从真机（索尼 ILCE-6300）通过 USB 录下的原始数据，不是构造出来的。
//! 新实现必须能正确处理它们 —— 所有相关测试都以此为基准。
//!
//! 来源与采集脚本在本仓库**外面**的资料目录里（本机位置）：
//! `D:\AI Work\sony-app-installer-资料\ref\golden\clienthello-a6300.bin`
//! `D:\AI Work\sony-app-installer-资料\tools\pmca_recorder.py`
//!
//! # 为什么用十六进制字符串而不是字节数组
//!
//! 直接写成字节数组（`&[0x16, 0x03, ...]`）很容易在手工转写时出错 ——
//! 比如把"套件列表长度"的两个字节写反（应为 `00 06`，写成 `06 00`），
//! 长度字段就自相矛盾，解析全部失败。
//! 改成十六进制字符串后：
//! - 可以直接与权威 hex dump 逐字符比对
//! - 解析带长度自检，抄错会立刻暴露
//! - 可读性也更好

/// 相机发来的第一句问候语（ClientHello），**逐字节**来自真机录音。
///
/// 关键特征（决定了整个 TLS 方案）：
/// - 记录版本 = `0301` → **TLS 1.0**
/// - 客户端版本 = `0301` → TLS 1.0
/// - 只提供 3 个密码套件：`002f`、`0035`、`00ff`
/// - **没有 SNI 扩展**（服务端因此不能要求 SNI）
/// - **没有 signature_algorithms 扩展**
/// - 有 `session_ticket`（`0023`）扩展，长度为 0
///
/// 字节布局（便于对照）：
/// ```text
/// 16 03 01 00 37   记录头：握手 / TLS1.0 / 长度 55
/// 01 00 00 33      握手头：ClientHello / 体长 51
/// 03 01            客户端版本 TLS 1.0
/// 7d..94 (32 字节) 随机数
/// 00               session id 长度 0
/// 00 06            套件列表长度 6
/// 00 2f 00 35 00 ff 三个套件
/// 01 00            压缩方式：1 种，不压缩
/// 00 04            扩展总长 4
/// 00 23 00 00      session_ticket 扩展
/// ```
pub const CLIENT_HELLO_HEX: &str = concat!(
    // 记录头：握手 / TLS 1.0 / 长度 0x0037(55)
    "1603010037",
    // 握手头：ClientHello / 体长 0x0033(51)
    "01000033",
    // 客户端版本 TLS 1.0
    "0301",
    // 32 字节随机数（真机录音原值）
    "7d0b8a487f64fcaeff9bdf554f4ef056cd58909718059d0ba49e716fb0c36c94",
    // session id 长度 0
    "00",
    // 套件列表长度 6
    "0006",
    // 三个套件：0x002f / 0x0035 / 0x00ff
    "002f003500ff",
    // 压缩方式：1 种，不压缩
    "0100",
    // 扩展总长 4
    "0004",
    // session_ticket 扩展
    "00230000",
);

/// 解码成字节。带长度自检：抄错会立刻失败，而不是等到解析时才报莫名其妙的错。
pub fn client_hello() -> Vec<u8> {
    let compact: String = CLIENT_HELLO_HEX
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        compact.len().is_multiple_of(2),
        "十六进制字符串长度必须是偶数（当前 {}）",
        compact.len()
    );
    let bytes: Vec<u8> = (0..compact.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&compact[i..i + 2], 16).expect("十六进制解析失败"))
        .collect();
    assert_eq!(
        bytes.len(),
        60,
        "真机问候语是 60 字节，当前解码出 {} 字节 —— 多半是转写时漏了或多了字节",
        bytes.len()
    );
    bytes
}

/// 相机在 ClientHello 里声明的密码套件（按出现顺序）
pub const OFFERED_CIPHER_SUITES: [u16; 3] = [0x002f, 0x0035, 0x00ff];

/// 其中真正可用的套件（`0x00ff` 是防重协商标记，不是真套件）
pub const USABLE_CIPHER_SUITES: [u16; 2] = [0x002f, 0x0035];

#[cfg(test)]
mod tests {
    use super::*;

    /// 长度与记录头必须自洽
    #[test]
    fn length_fields_are_consistent() {
        let h = client_hello();
        assert_eq!(h[0], 0x16, "记录类型应为握手");
        assert_eq!(&h[1..3], &[0x03, 0x01], "记录版本应为 TLS 1.0");

        let record_len = u16::from_be_bytes([h[3], h[4]]) as usize;
        assert_eq!(record_len, h.len() - 5, "记录长度字段与实际字节数不一致");

        assert_eq!(h[5], 0x01, "握手类型应为 ClientHello");
        // 握手头 4 字节 = 类型(1) + 长度(3，高字节在前)
        let hs_body =
            ((h[6] as usize) << 16) | ((h[7] as usize) << 8) | h[8] as usize;
        assert_eq!(hs_body, record_len - 4, "握手体长度应为记录长度减 4");
    }

    /// 版本与套件必须与录音一致 —— 这是整个 TLS 选型的依据
    #[test]
    fn declares_tls10_and_static_rsa_only() {
        let h = client_hello();
        let client_version = u16::from_be_bytes([h[9], h[10]]);
        assert_eq!(client_version, 0x0301, "相机只谈 TLS 1.0");

        let p = 11 + 32; // 版本(2) + random(32)
        assert_eq!(h[p], 0x00, "session id 长度应为 0");
        let cs_len = u16::from_be_bytes([h[p + 1], h[p + 2]]) as usize;
        assert_eq!(cs_len, 6, "套件列表应为 6 字节（3 个套件）");

        let mut suites = Vec::new();
        for i in (0..cs_len).step_by(2) {
            suites.push(u16::from_be_bytes([h[p + 3 + i], h[p + 3 + i + 1]]));
        }
        assert_eq!(suites, OFFERED_CIPHER_SUITES.to_vec());

        for s in &suites {
            assert!(
                !(0xc000..=0xc0ff).contains(s),
                "相机不应提供 ECDHE 套件（真机实测如此）"
            );
        }
    }

    /// 不应有 SNI —— 服务端因此不能强制要求 SNI
    #[test]
    fn has_no_sni_but_has_session_ticket() {
        let h = client_hello();
        let p = 11 + 32 + 1 + 2 + 6 + 2; // 版本+random+sid长+sid+套件长+套件+压缩
        let ext_total = u16::from_be_bytes([h[p], h[p + 1]]) as usize;
        assert_eq!(ext_total, 4, "实测只有 session_ticket 一个扩展");

        let mut q = p + 2;
        let end = q + ext_total;
        let mut ext_types = Vec::new();
        while q + 4 <= end {
            let t = u16::from_be_bytes([h[q], h[q + 1]]);
            let l = u16::from_be_bytes([h[q + 2], h[q + 3]]) as usize;
            ext_types.push(t);
            q += 4 + l;
        }
        assert_eq!(ext_types, vec![0x0023], "只有 session_ticket");
        assert!(!ext_types.contains(&0x0000), "绝不能有 SNI");
    }
}
