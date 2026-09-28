# 你的相机（ILCE-6300）TLS 真实能力 · 实测记录

> 数据来源：`D:\Sonny\tools\out\clienthello-*.bin`（真实 USB 录音，2026-09-27）
> 这是从**你的相机**发来的原始字节，不是推测。

---

## 1. 原始问候语（60 字节，逐字节）

```
16 03 01 00 37 01 00 00 33 03 01 7d 0b 8a 48 7f 64 fc ae ff 9b df 55 4f 4e f0 56
cd 58 90 97 18 05 9d 0b a4 9e 71 6f b0 c3 6c 94 00 00 06 00 2f 00 35 00 ff 01 00
00 04 00 23 00 00
```

逐段解读：

| 字节 | 含义 |
|---|---|
| `16` | 记录类型 = 握手（Handshake） |
| `03 01` | 记录版本 = **TLS 1.0** |
| `00 37` | 记录长度 = 55 字节 |
| `01` | 握手类型 = ClientHello |
| `00 00 33` | 握手体长度 = 51 字节 |
| `03 01` | 客户端版本 = **TLS 1.0** |
| `7d 0b 8a 48 …` | 32 字节随机数 |
| `00` | session id 长度 = 0 |
| `00 06` | 加密套件列表长度 = 6 字节 |
| `00 2f` | **TLS_RSA_WITH_AES_128_CBC_SHA**（静态 RSA 密钥交换） |
| `00 35` | **TLS_RSA_WITH_AES_256_CBC_SHA**（静态 RSA） |
| `00 ff` | TLS_EMPTY_RENEGOTIATION_INFO_SCSV（防重协商攻击标记） |
| `01 00` | 压缩方式：1 种，值 0（不压缩） |
| `00 04` | 扩展总长 = 4 字节 |
| `00 23 00 00` | **session_ticket** 扩展（长度为 0） |

---

## 2. 关键结论（直接影响 Rust 技术选型）

| 特性 | 实测结果 | 影响 |
|---|---|---|
| **TLS 版本** | **1.0**（`0x0301`） | 现代库默认最低 TLS 1.2 → **必须显式降版本** |
| **密钥交换** | **只有静态 RSA**（`0x002f`/`0x0035`） | **rustls 完全不支持**（rustls 只做 ECDHE/DHE，且只支持 TLS 1.2/1.3） |
| **ECDHE 支持** | ❌ 无 | 无法用 rustls 的"放宽配置"绕过 |
| **SNI 扩展** | ❌ 没有 | 服务端**不能要求 SNI**（否则握手失败） |
| **signature_algorithms** | ❌ 没有 | 说明是 TLS 1.0 时代行为；服务端签名算法要按 1.0 规则处理 |
| **session_ticket** | ✅ 有（长度 0） | 可以忽略，但要能正确跳过这个扩展 |
| **加密套件数量** | 只 3 个 | 服务端必须从这 3 个里挑，即 `AES128-CBC-SHA` 或 `AES256-CBC-SHA` |

### 判定结果

```
rustls          ❌ 不可用（不支持 TLS 1.0，不支持静态 RSA 密钥交换）
openssl crate   ✅ 可用（set_min_proto_version(TLS1) + set_cipher_list("ALL:@SECLEVEL=0")）
自写最小 TLS    ✅ 备选（固定 TLS_RSA_WITH_AES_128_CBC_SHA，约 300~500 行）
明文 http 旁路  ❓ 未验证（相机可能强制 https）
```

**这与设计文档 §4.4‴ 的预判完全一致**：当时我从原项目的 git 历史推断"相机只支持 TLS 1.0 + 静态 RSA"，现在被你的真机实测**逐字节证实**。

---

## 3. 完整录音的流量统计

```
相机 → 电脑：147 条 / 3791 字节 / 5 条 TLS 记录
电脑 → 相机：5 条 / 3046 字节 / 8 条 TLS 记录
```

注意：相机把 TLS 记录拆得很碎（第一个字节、再 4 字节头、再正文分开送），
所以"包数"远多于"记录数"。**Rust 侧必须按字节流累积处理，不能假设一次读到完整记录。**

---

## 4. 握手之后的流程（已实测跑通）

```
Sony Corporation ILCE-6300 is a camera in MTP mode   ← 识别相机
Switching to app install mode                         ← 切模式（相机会黑屏重启）
Sony Corporation ILCE-6300 is a camera in app install mode  ← 重新识别
TLS 握手完成 ✓（3 次）
Uploading 100% / Downloading 100% / Installing       ← 相机侧下载并安装
Task completed successfully                           ← 安装成功
```

---

## 5. 这些文件在哪里

| 文件 | 用途 |
|---|---|
| `out\clienthello-*.bin` | **相机的问候语原始字节** → Rust 单元测试的黄金样本 |
| `out\capture-*.pcap` | 完整双向录音 → 可逐字节比对 Rust 实现 |
| `out\summary-*.txt` | 人话版结论 |
| `out\run.log` | 中文过程日志 |

---

## 7. 实施记录：为什么最终手写 TLS（2026-09-27 实测）

在设计文档里我推荐的方案是 `openssl` crate。实际动手时发现两件事，最终**改为手写最小 TLS 1.0**：

| 尝试 | 结果 |
|---|---|
| `openssl` + `vendored` feature | ❌ **失败**：vendored 要从源码编译 OpenSSL，需要 Perl，本机没装 |
| 借用 Git for Windows 自带的 Perl | ❌ **失败**：那是 cygwin 精简版，缺 `Locale::Maketext::Simple`、`IPC::Cmd` 等模块 |
| 下载便携版 Strawberry Perl（290 MB） | ⚠️ 下载中断（GitHub 大文件不稳），且给项目引入一个只用于构建的重型依赖 |
| 找 OpenSSL 官方预编译 Windows 二进制 | ❌ GitHub release 里不提供 |

**决策：手写。** 理由：

1. 我们需要的配置窄到只有一种 —— **TLS 1.0 + 静态 RSA + AES-CBC-SHA**，
   通用库里那些分支（ECDHE、会话票据、重协商、TLS 1.3）我们一个都用不上。
2. `rsa` / `aes` / `cbc` / `sha1` / `md-5` / `hmac` 都是**纯 Rust**，
   不需要 C 工具链、不需要 Perl —— **构建链反而更简单**，最终仍是单文件 exe。
3. 代码量可控（约 400~600 行），并且每一步都能用真机录制的字节做断言。

**已完成的里程碑**：

- [x] **M1**：解析 ClientHello；产出 ServerHello + Certificate + ServerHelloDone
      —— 用真机 60 字节问候语验证通过，协商出 `0x002f`，服务端首段 2653 字节结构正确
- [ ] M2：解析 ClientKeyExchange，用 RSA 私钥解出预主密钥
- [ ] M3：TLS 1.0 PRF（MD5/SHA1 各半）导出主密钥与密钥块
- [ ] M4：验证客户端 ChangeCipherSpec + Finished
- [ ] M5：发送服务端 ChangeCipherSpec + Finished；加解密应用数据

### 踩过的两个坑（记录以免重犯）

1. **握手长度是 3 字节，不是 2 字节**。握手头 = 1 字节类型 + **3 字节长度**。
   我写成读 `data[1..3]`，漏掉最高字节；真机问候语长度是 `0x000033`（最高字节为 0），
   于是被判成"长度为 0"，解析全线失败。
2. **不要用 PowerShell 的 `Get-Content -Raw` 改 UTF-8 源码**。
   Windows PowerShell 5.1 会按 ANSI 读取，把中文注释变成乱码并毁掉文件结构。
   改代码一律用编辑器工具。

## 8. 对 Rust 实现的具体要求（据此定稿）

1. **TLS 服务端**：手写最小实现（理由见上一节）——只支持 **TLS 1.0 + 静态 RSA + AES-CBC-SHA**，
   **不要**尝试启用 ECDHE、会话票据、重协商、TLS 1.2+。
2. **证书**：使用捆绑的 `certs/localtest.me.pem`（叶证书 + EssentialSSL 中间证书），
   私钥与叶证书模数一致（RSA-2048）；**证书链必须两张一起发**，否则相机链不到受信根。
3. **密码套件必须包含**：`0x002f`（AES128-SHA）与 `0x0035`（AES256-SHA），
   协商时优先 `0x002f`。`0x00ff` 是防重协商标记，不是真套件。
4. **SNI**：相机不发 SNI，服务端**绝不能要求** SNI。
5. **字节流处理**：按累积缓冲解析 TLS 记录，不假设一次读全
   （真机把记录拆得很碎：先 1 字节、再 4 字节、再正文）。
6. **黄金测试**：把 `D:\Sonny\ref\golden\clienthello-a6300.bin` 的字节内嵌为测试向量，
   断言新实现能解析它并协商出正确套件 —— 已完成（M1）。
