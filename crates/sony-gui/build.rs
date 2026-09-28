//! 构建脚本：把图标嵌进 exe。
//!
//! # 为什么需要这个脚本
//!
//! Windows 的程序图标**不是代码**，而是一段"资源"（resource）：
//! 要先用资源编译器 `rc.exe` 生成 `.res` 文件，再交给链接器一起链进 exe。
//! Cargo 本身不管这件事，所以得自己写。
//!
//! 做完之后：
//! - 资源管理器里看到的 exe 图标就是它
//! - **图标跟着 exe 走**，复制到别人电脑上也在（不用装任何东西）
//!
//! # 找不到 rc.exe 怎么办
//!
//! `rc.exe` 在 Windows SDK 里（装 Visual Studio 生成工具时会带上）。
//! 万一没有，这里只**警告、不报错** —— 让项目照样能编译，
//! 只是编出来的 exe 没有图标。这样别人克隆下来不至于因为缺个 SDK 就编不过。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // 图标换了要重新构建
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(windows)]
    embed_icon();

    #[cfg(not(windows))]
    println!("cargo:warning=非 Windows 平台，跳过图标资源");
}

#[cfg(windows)]
fn embed_icon() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("assets").join("icon.ico");
    if !icon.exists() {
        println!("cargo:warning=没找到 assets/icon.ico，跳过图标");
        return;
    }

    let Some(rc) = find_rc() else {
        println!(
            "cargo:warning=没找到 rc.exe（在 Windows SDK 里），本次构建的 exe 不带图标。\
             想带上图标请装 Visual Studio 生成工具。"
        );
        return;
    };

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_file = out_dir.join("icon.rc");
    let res_file = out_dir.join("icon.res");

    // 资源脚本。路径用绝对路径，避免 rc.exe 的工作目录不对。
    // 路径里的反斜杠要转义，否则会被当成转义字符。
    let script = format!(
        "IDI_ICON1 ICON \"{}\"\n",
        icon.display().to_string().replace('\\', "\\\\")
    );
    if std::fs::write(&rc_file, script).is_err() {
        println!("cargo:warning=写资源脚本失败，跳过图标");
        return;
    }

    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status();

    match status {
        Ok(s) if s.success() && res_file.exists() => {
            // 把 .res 直接交给链接器（MSVC 的 link.exe 认这个格式）
            println!("cargo:rustc-link-arg={}", res_file.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe 退出码 {s}，exe 将不带图标"),
        Err(e) => println!("cargo:warning=调用 rc.exe 失败（{e}），exe 将不带图标"),
    }
}

/// 找 `rc.exe`。
///
/// 优先找 Windows SDK 里**版本号最高**的那个（多个 SDK 并存时选新的）。
#[cfg(windows)]
fn find_rc() -> Option<PathBuf> {
    // 1) 环境变量里已经指定了 SDK 目录
    if let Ok(sdk) = std::env::var("WindowsSdkDir")
        && let Some(p) = newest_rc_in(Path::new(&sdk).join("bin"))
    {
        return Some(p);
    }

    // 2) 常见安装位置
    for base in [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ] {
        if let Some(p) = newest_rc_in(PathBuf::from(base)) {
            return Some(p);
        }
    }

    // 3) 最后碰碰运气：PATH 里有没有
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("rc.exe");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 在 `bin/<版本>/x64/rc.exe` 里找版本号最高的一个
#[cfg(windows)]
fn newest_rc_in(bin: PathBuf) -> Option<PathBuf> {
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&bin)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    // 目录名形如 10.0.26100.0，按字符串倒序就是版本倒序（位数一致时成立）
    versions.sort();
    for v in versions.into_iter().rev() {
        let rc = v.join("x64").join("rc.exe");
        if rc.exists() {
            return Some(rc);
        }
        let rc86 = v.join("x86").join("rc.exe");
        if rc86.exists() {
            return Some(rc86);
        }
    }
    None
}
