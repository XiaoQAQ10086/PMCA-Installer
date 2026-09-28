//! 构建脚本：编译 WPD 的 C 薄封装。
//!
//! 为什么需要它：相机在 Windows 上被 MTP 驱动独占，只能通过 WPD 的 COM 接口通信。
//! Rust 侧手写 COM 绑定时参照的接口定义不完整，取到的 vtable 指针有偏差；
//! 改用 C 之后编译器会直接把这类错误指出来。`cc` 只在**构建时**用到，不进最终产物。

fn main() {
    println!("cargo:rerun-if-changed=src/wpd_shim.c");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let mut build = cc::Build::new();
        build
            .file("src/wpd_shim.c")
            // C 源文件里有中文注释与中文错误信息，且文件是 UTF-8。
            // MSVC 默认按系统区域编码（本机是 GBK）解析，会把字符串字面量读坏，
            // 报「字符串字面量中的换行符」。必须显式指定 UTF-8。
            .flag_if_supported("/utf-8")
            // 关掉 strcpy 之类的"不安全函数"弃用警告：用得很少，且都做了长度检查。
            .define("_CRT_SECURE_NO_WARNINGS", None)
            .warnings(true)
            .compile("wpd_shim");

        // 链接 WPD 所需的系统库：
        // - ole32        COM 基础（CoCreateInstance / CoTaskMemFree）
        // - oleaut32     PROPVARIANT 相关
        // - propsys      PropVariantClear
        // - PortableDeviceGUIDs  WPD 的 CLSID / IID / PROPERTYKEY 常量本体
        //   （这些常量在头文件里只是 `EXTERN_C const GUID` 声明，定义在 .lib 里）
        for lib in [
            "ole32",
            "oleaut32",
            "propsys",
            "PortableDeviceGUIDs",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }

    #[cfg(not(windows))]
    {
        // 非 Windows 平台不编译这个 C 文件，保证 workspace 仍能构建
    }
}
