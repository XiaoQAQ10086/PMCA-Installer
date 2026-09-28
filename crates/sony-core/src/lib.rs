//! `sony-core`：与 UI 无关的核心逻辑。
//!
//! 设计约束（见设计文档 §5.1）：
//! - **不依赖 `iced`**：业务逻辑必须能脱离 GUI 运行与测试
//! - 协议字节解析全部有单元测试，基准是**真机录音**（见 [`golden`]）
//!
//! 已完成：TLS 1.0 服务端（含完整握手）、MTP 容器、索尼代理消息、
//! Sony 信封、SPK 打包、XPD 清单、商店 JSON 响应。

pub mod certs;
pub mod der;
pub mod golden;
pub mod golden_handshake;
pub mod handshake;
pub mod http;
pub mod market;
pub mod mtp;
pub mod prf;
pub mod proxy;
pub mod record;
pub mod session;
pub mod sony;
pub mod spk;
pub mod tls;
pub mod transport;
pub mod xpd;
