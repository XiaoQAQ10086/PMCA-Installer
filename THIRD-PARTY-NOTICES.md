# 第三方作品与致谢

本项目是**独立实现**，但它的可行性完全建立在 [ma1co/Sony-PMCA-RE](https://github.com/ma1co/Sony-PMCA-RE)
这个开源项目之上 —— 是它搞清楚了索尼相机安装应用的整套流程。没有它，这个项目不会存在。

---

## Sony-PMCA-RE

- 项目地址：https://github.com/ma1co/Sony-PMCA-RE
- 作者：ma1co
- 许可证：MIT

本项目**没有复制它的源代码**（Rust 与 Python 语言不同，实现方式也不同），
但是通过阅读它的源码理解了以下关键协议细节：

- 相机要用哪几个操作码来判断"是否支持安装应用"
- 索尼私有扩展命令的格式，以及"让相机切换到应用安装模式"用的那条命令
- XPD 清单（TCD / TKN / CIC）的结构与校验值算法
- SPK 容器的打包格式与 AES 密钥的取法
- 假商店服务器该回什么样的 JSON 才能让相机去下载安装包
- 相机在普通 MTP 模式、应用安装模式下的 USB 产品号

按 MIT 许可证的要求，下面保留原项目的版权声明：

```
The MIT License (MIT)

Copyright (c) 2015 ma1co

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

---

## 相机固件中的证书

`crates/sony-core/src/certs.rs` 里内置的那张证书，
来自索尼相机固件本身（相机只信任特定根证书签发的证书）。
它**不是**本项目的作品，仅作为与相机通信的必要材料随附。

---

## 开源依赖

主要依赖（均为 MIT / Apache-2.0 等宽松许可证）：

| 依赖 | 用途 |
|---|---|
| [iced](https://github.com/iced-rs/iced) | 图形界面框架 |
| [rfd](https://github.com/PolyMechanix/rfd) | 系统「打开文件」对话框 |
| [rsa](https://github.com/RustCrypto/RSA)、[aes](https://github.com/RustCrypto/block-ciphers)、[sha1](https://github.com/RustCrypto/hashes)、[sha2](https://github.com/RustCrypto/hashes)、[hmac](https://github.com/RustCrypto/MACs)、[md-5](https://github.com/RustCrypto/hashes) | 手写 TLS 1.0 所需的密码学原语 |
| [serde_json](https://github.com/serde-rs/json) | 解析相机回报的 JSON |
| [windows-sys](https://github.com/microsoft/windows-rs) | 调用 Windows 的 WPD / SetupAPI 接口 |

完整的依赖清单见 `Cargo.lock`。
