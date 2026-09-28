//! 索尼相机应用安装器 —— 图形界面（库部分）。
//!
//! 拆成 lib + bin 两个目标是**为了能测试**：
//! `main.rs` 只负责开窗口，真正的状态机、后台线程、样式都在这里，
//! 这样集成测试可以直接驱动它们（见 `tests/real_install.rs`）。
//!
//! 否则界面代码只能靠"人点一下看看"来验证 —— 而人点不出边界情况。

/// 版本号。
///
/// ⚠️ **只有这一处定义** —— 取自 `Cargo.toml` 里的 `version`。
/// 界面、`--version`、发行说明都用它，不会出现"界面写 1.0.0、
/// 实际编出来是 1.0.1"这种对不上的情况。
/// 要发新版本，只改根 `Cargo.toml` 里那一行就够了。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod app;
pub mod fonts;
pub mod theme;
pub mod worker;
