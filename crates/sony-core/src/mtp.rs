//! PTP/MTP 容器格式与设备信息解析。
//!
//! 相机通过 USB 走的是 PTP（Picture Transfer Protocol）容器；
//! 索尼在这之上又叠了自己的"代理消息"层（见 [`crate::proxy`]）。
//!
//! # 容器格式（全部小端）
//!
//! ```text
//! offset 0 : size        u32   整包长度（含本头）
//! offset 4 : type        u16   1=命令 2=数据 3=响应
//! offset 6 : code        u16   操作码 / 响应码
//! offset 8 : transaction u32   事务号，每发一条命令 +1
//! offset 12: 数据（可选，长度 = size - 12）
//! ```
//!
//! 对应原项目 `pmca/usb/driver/generic/__init__.py:8-13` 与 `:194-252`。

use anyhow::{bail, Context, Result};

/// PTP 容器头长度
pub const PTP_HEADER_LEN: usize = 12;

/// 容器类型
pub const PTP_TYPE_COMMAND: u16 = 1;
pub const PTP_TYPE_DATA: u16 = 2;
pub const PTP_TYPE_RESPONSE: u16 = 3;

/// 相机一次最多传 512 字节，超过要续读
pub const PTP_MAX_PACKET: usize = 512;

// ---- 通用操作码 ----
pub const PTP_OC_GET_DEVICE_INFO: u16 = 0x1001;
pub const PTP_OC_OPEN_SESSION: u16 = 0x1002;
pub const PTP_OC_CLOSE_SESSION: u16 = 0x1003;

// ---- 通用响应码 ----
pub const PTP_RC_OK: u16 = 0x2001;
pub const PTP_RC_SESSION_NOT_OPEN: u16 = 0x2003;
pub const PTP_RC_PARAMETER_NOT_SUPPORTED: u16 = 0x2006;
pub const PTP_RC_DEVICE_BUSY: u16 = 0x2019;
pub const PTP_RC_SESSION_ALREADY_OPENED: u16 = 0x201E;

/// 一个 PTP 容器
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtpPacket {
    pub kind: u16,
    pub code: u16,
    pub transaction: u32,
    pub data: Vec<u8>,
}

impl PtpPacket {
    pub fn new(kind: u16, code: u16, transaction: u32, data: Vec<u8>) -> Self {
        Self {
            kind,
            code,
            transaction,
            data,
        }
    }

    /// 序列化成 USB 上要发的字节（小端）
    pub fn encode(&self) -> Vec<u8> {
        let size = (PTP_HEADER_LEN + self.data.len()) as u32;
        let mut out = Vec::with_capacity(size as usize);
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&self.code.to_le_bytes());
        out.extend_from_slice(&self.transaction.to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// 从收到的字节解析。`data` 至少要有一个完整头。
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < PTP_HEADER_LEN {
            bail!("PTP 容器太短：{} 字节", data.len());
        }
        let size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if size < PTP_HEADER_LEN {
            bail!("PTP 容器声明的长度非法：{size}");
        }
        if data.len() < size {
            bail!("PTP 容器被截断：声明 {size} 字节，实际 {}", data.len());
        }
        let kind = u16::from_le_bytes([data[4], data[5]]);
        let code = u16::from_le_bytes([data[6], data[7]]);
        let transaction = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        Ok(Self {
            kind,
            code,
            transaction,
            data: data[PTP_HEADER_LEN..size].to_vec(),
        })
    }
}

/// MTP 设备信息（`GetDeviceInfo` 的响应）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub model: String,
    pub serial_number: String,
    /// 设备支持的操作码集合
    pub operations_supported: Vec<u16>,
    /// 厂商扩展字符串 —— **这是识别索尼相机模式的关键**
    pub vendor_extension: String,
}

impl DeviceInfo {
    pub fn supports(&self, op: u16) -> bool {
        self.operations_supported.contains(&op)
    }

    /// 是否支持全部给定的操作码
    pub fn supports_all(&self, ops: &[u16]) -> bool {
        ops.iter().all(|o| self.supports(*o))
    }
}

/// 解析 MTP 的字符串：1 字节字符数 + UTF-16LE 内容（**含结尾 NUL，要去掉**）
fn parse_ptp_string(data: &[u8], offset: usize) -> Result<(usize, String)> {
    if offset >= data.len() {
        bail!("解析字符串时越界（offset {offset}）");
    }
    let chars = data[offset] as usize;
    let start = offset + 1;
    let end = start + chars * 2;
    if end > data.len() {
        bail!("字符串被截断：声明 {chars} 个字符");
    }
    let mut units = Vec::with_capacity(chars);
    for i in 0..chars {
        units.push(u16::from_le_bytes([data[start + i * 2], data[start + i * 2 + 1]]));
    }
    // 末尾的 NUL 不属于内容
    if units.last() == Some(&0) {
        units.pop();
    }
    Ok((end, String::from_utf16_lossy(&units)))
}

/// 解析 MTP 的数组：4 字节**元素个数** + 每元素 2 字节
fn parse_ptp_array(data: &[u8], offset: usize) -> Result<(usize, Vec<u16>)> {
    if offset + 4 > data.len() {
        bail!("解析数组时越界（offset {offset}）");
    }
    let count = u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]) as usize;
    let start = offset + 4;
    let end = start + count * 2;
    if end > data.len() {
        bail!("数组被截断：声明 {count} 个元素");
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(u16::from_le_bytes([data[start + i * 2], data[start + i * 2 + 1]]));
    }
    Ok((end, out))
}

/// 解析 `GetDeviceInfo` 的响应数据。
///
/// 结构（对应原项目 `pmca/usb/__init__.py:102-118`）：
/// ```text
/// offset 0 : StandardVersion u16
/// offset 2 : VendorExtensionID u32
/// offset 6 : VendorExtensionVersion u16
/// offset 8 : VendorExtensionDesc（字符串）
/// 跳过 2 字节（FunctionalMode u16）
/// 然后依次是 5 个数组：操作码 / 事件 / 设备属性 / 采集格式 / 图像格式
/// 最后是 4 个字符串：厂商 / 型号 / 版本 / 序列号
/// ```
pub fn parse_device_info(data: &[u8]) -> Result<DeviceInfo> {
    if data.len() < 10 {
        bail!("设备信息太短：{} 字节", data.len());
    }
    let mut p = 8usize;
    let (np, vendor_extension) = parse_ptp_string(data, p).context("解析厂商扩展字符串失败")?;
    p = np + 2; // 跳过 FunctionalMode

    let (np, operations_supported) = parse_ptp_array(data, p).context("解析操作码数组失败")?;
    let (np, _events) = parse_ptp_array(data, np).context("解析事件数组失败")?;
    let (np, _props) = parse_ptp_array(data, np).context("解析设备属性数组失败")?;
    let (np, _capture) = parse_ptp_array(data, np).context("解析采集格式数组失败")?;
    let (np, _image) = parse_ptp_array(data, np).context("解析图像格式数组失败")?;

    let (np, manufacturer) = parse_ptp_string(data, np).context("解析厂商名失败")?;
    let (np, model) = parse_ptp_string(data, np).context("解析型号失败")?;
    let (np, _version) = parse_ptp_string(data, np).context("解析版本失败")?;
    let (_np, serial_number) = parse_ptp_string(data, np).context("解析序列号失败")?;

    Ok(DeviceInfo {
        manufacturer,
        model,
        serial_number,
        operations_supported,
        vendor_extension,
    })
}

/// 索尼私有的"MTP 扩展命令"操作码。
///
/// 相机在**普通 MTP 模式**下也支持这三个，原项目就是靠它们来下达
/// 索尼私有命令（包括"切换到应用安装模式"）。
/// 定义见 `libInfraMtpServer.so`，对应原项目 `pmca/usb/sony.py:119-121`。
pub const PTP_OC_SONY_DI_EXT_CMD_WRITE: u16 = 0x9280;
pub const PTP_OC_SONY_DI_EXT_CMD_READ: u16 = 0x9281;
pub const PTP_OC_SONY_REQ_RECONNECT: u16 = 0x9282;

/// 索尼扩展命令的请求头长度（`dataSize` + `cmd` + `direction` + 8 字节保留）
pub const EXT_CMD_HEADER_LEN: usize = 16;

/// 索尼扩展命令的发送缓冲大小（原项目固定补齐到这个长度）
pub const EXT_CMD_WRITE_BUFFER: usize = 0x2000;

/// 一条索尼扩展命令：`(命令组, 命令号)`。
///
/// 原项目把它写成元组并在 `_sendCommand` 里拆成
/// `PTP_OC_SonyDiExtCmd_write` 的**参数**（用命令组）与
/// 请求头里的 **cmd 字段**（用命令号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SonyExtCommand {
    /// 命令组，作为 PTP 命令的参数传下去
    pub group: u16,
    /// 命令号，写进扩展命令头
    pub id: u16,
}

impl SonyExtCommand {
    pub const fn new(group: u16, id: u16) -> Self {
        Self { group, id }
    }

    /// 请求头里的 `cmd` 字段（原项目把这两个字节单独拎出来叫 cmd）
    pub fn body_cmd(self) -> u16 {
        self.id
    }
}

/// 构造一条索尼扩展命令的**完整发送缓冲**。
///
/// 对应原项目 `pmca/usb/sony.py:787-795` 的 `_sendCommand`：
/// ```text
/// ExtCmdHeader（小端，16 字节）
///   offset 0 : i32  dataSize   正文长度
///   offset 4 : i16  cmd        命令号
///   offset 6 : i16  direction  固定 0
///   offset 8 : 8 字节保留（全 0）
/// 之后是正文，最后**整体补齐到 0x2000 字节**（尾部填 0）
/// ```
///
/// ⚠️ 必须补齐到固定长度：相机是按固定块读的，长度不对它会拒绝。
pub fn build_sony_ext_payload(cmd: SonyExtCommand, data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; EXT_CMD_WRITE_BUFFER];
    // dataSize
    out[0..4].copy_from_slice(&(data.len() as i32).to_le_bytes());
    // cmd
    out[4..6].copy_from_slice(&cmd.body_cmd().to_le_bytes());
    // direction = 0（已由 0 填充保证）
    // 正文紧跟在头之后
    let end = EXT_CMD_HEADER_LEN + data.len();
    out[EXT_CMD_HEADER_LEN..end].copy_from_slice(data);
    out
}

/// 从索尼扩展命令的响应里取出正文。
///
/// 对应原项目 `_sendCommand` 里读回后的处理：先看头的 `dataSize`，
/// 再按它截取正文。
pub fn parse_sony_ext_response(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < EXT_CMD_HEADER_LEN {
        bail!("索尼扩展命令响应太短：{} 字节", data.len());
    }
    let size = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if size < 0 {
        bail!("索尼扩展命令响应的长度非法：{size}");
    }
    let size = size as usize;
    let end = EXT_CMD_HEADER_LEN + size;
    if data.len() < end {
        bail!("索尼扩展命令响应被截断：声明 {size} 字节正文");
    }
    Ok(data[EXT_CMD_HEADER_LEN..end].to_vec())
}

/// 索尼的**"切换到应用安装模式"**命令。
///
/// 原项目 `pmca/usb/sony.py:175` + `:336-338`：
/// `SONY_CMD_ScalarExtCmdPlugIn_NotifyScalarDlmode = (5, 2)`，
/// 调用时不带正文、也不读响应。
///
/// ⚠️ 相机收到后会**重启 USB 连接**并变成另一个设备，所以执行后必须等待重新枚举。
pub const SONY_CMD_NOTIFY_SCALAR_DLMODE: SonyExtCommand = SonyExtCommand::new(5, 2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_roundtrip() {
        let pkt = PtpPacket::new(PTP_TYPE_COMMAND, 0x1002, 7, vec![1, 0, 0, 0]);
        let bytes = pkt.encode();
        assert_eq!(bytes.len(), PTP_HEADER_LEN + 4);
        // 小端长度 = 16
        assert_eq!(&bytes[0..4], &[16, 0, 0, 0]);
        assert_eq!(&bytes[4..6], &[1, 0]);
        assert_eq!(&bytes[6..8], &[0x02, 0x10]);
        assert_eq!(&bytes[8..12], &[7, 0, 0, 0]);
        let back = PtpPacket::decode(&bytes).unwrap();
        assert_eq!(back, pkt);
    }

    #[test]
    fn packet_decode_rejects_truncated_and_bad_size() {
        // 太短
        assert!(PtpPacket::decode(&[0u8; 5]).is_err());
        // 声明的长度小于头长
        let mut bad = vec![0u8; PTP_HEADER_LEN];
        bad[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(PtpPacket::decode(&bad).is_err());
        // 声明比实际长
        let mut short = vec![0u8; PTP_HEADER_LEN];
        short[0..4].copy_from_slice(&100u32.to_le_bytes());
        assert!(PtpPacket::decode(&short).is_err());
    }

    /// 构造一个假的 GetDeviceInfo 响应来验证解析
    fn build_device_info(
        vendor_ext: &str,
        ops: &[u16],
        manufacturer: &str,
        model: &str,
        version: &str,
        serial: &str,
    ) -> Vec<u8> {
        fn put_str(out: &mut Vec<u8>, s: &str) {
            // PTP 字符串：字符数（含结尾 NUL）+ UTF-16LE + NUL
            let units: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
            out.push(units.len() as u8);
            for u in units {
                out.extend_from_slice(&u.to_le_bytes());
            }
        }
        fn put_arr(out: &mut Vec<u8>, a: &[u16]) {
            out.extend_from_slice(&(a.len() as u32).to_le_bytes());
            for v in a {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        let mut d = Vec::new();
        d.extend_from_slice(&100u16.to_le_bytes()); // StandardVersion
        d.extend_from_slice(&6u32.to_le_bytes()); // VendorExtensionID
        d.extend_from_slice(&100u16.to_le_bytes()); // VendorExtensionVersion
        put_str(&mut d, vendor_ext);
        d.extend_from_slice(&0u16.to_le_bytes()); // FunctionalMode
        put_arr(&mut d, ops);
        put_arr(&mut d, &[]); // events
        put_arr(&mut d, &[]); // props
        put_arr(&mut d, &[]); // capture
        put_arr(&mut d, &[]); // image
        put_str(&mut d, manufacturer);
        put_str(&mut d, model);
        put_str(&mut d, version);
        put_str(&mut d, serial);
        d
    }

    /// 普通 MTP 相机（真机 A6300 在 MTP 模式下的形态）
    #[test]
    fn parses_plain_mtp_camera() {
        let ops = [0x1001, 0x1002, 0x1003, 0x9280, 0x9281, 0x9282];
        let raw = build_device_info("", &ops, "Sony Corporation", "ILCE-6300", "3.20", "1234567");
        let info = parse_device_info(&raw).unwrap();

        assert_eq!(info.manufacturer, "Sony Corporation");
        assert_eq!(info.model, "ILCE-6300");
        assert_eq!(info.serial_number, "1234567");
        assert_eq!(info.vendor_extension, "", "普通 MTP 模式的厂商扩展是空的");
        assert!(info.supports_all(&[0x9280, 0x9281, 0x9282]), "应支持 ExtCmd 三件套");
        assert!(!info.supports(0x9488), "普通模式不该有代理消息操作码");
    }

    /// 应用安装模式的相机：厂商扩展含 sen_prxy_msg，且支持代理操作码
    #[test]
    fn parses_app_install_mode_camera() {
        let ops = [0x1001, 0x9488, 0x9489, 0x948c, 0x948d, 0x940a];
        let raw = build_device_info(
            "sony.net/SEN_PRXY_MSG:",
            &ops,
            "Sony Corporation",
            "ILCE-6300",
            "3.20",
            "1234567",
        );
        let info = parse_device_info(&raw).unwrap();
        assert_eq!(info.vendor_extension, "sony.net/SEN_PRXY_MSG:");
        assert!(info.supports_all(&[0x9488, 0x9489, 0x948c, 0x948d]));
        // 按设计文档 §4.1 的判定规则，这就是"安装模式"
        assert!(info.vendor_extension.contains("sony.net/SEN_PRXY_MSG:"));
    }

    /// 非 ASCII 的字符串也要能解析（原实现按 UTF-16 解，我们也一样）
    #[test]
    fn parses_non_ascii_strings() {
        let raw = build_device_info("", &[0x1001], "索尼公司", "相机型号", "1.0", "SN");
        let info = parse_device_info(&raw).unwrap();
        assert_eq!(info.manufacturer, "索尼公司");
        assert_eq!(info.model, "相机型号");
    }

    /// 截断的输入要报错而不是 panic
    #[test]
    fn rejects_truncated_device_info() {
        let full = build_device_info("", &[0x1001], "Sony", "A", "1", "2");
        for cut in [0, 5, 9, 12, full.len() - 2] {
            let r = parse_device_info(&full[..cut]);
            assert!(r.is_err(), "截断到 {cut} 字节时应当报错");
        }
    }

    /// 字符串解析必须正确去掉结尾 NUL
    #[test]
    fn string_parser_strips_nul() {
        let mut d = Vec::new();
        let units: Vec<u16> = "ABC".encode_utf16().chain(std::iter::once(0)).collect();
        d.push(units.len() as u8);
        for u in units {
            d.extend_from_slice(&u.to_le_bytes());
        }
        let (next, s) = parse_ptp_string(&d, 0).unwrap();
        assert_eq!(s, "ABC", "末尾 NUL 不应出现在结果里");
        assert_eq!(next, d.len());
    }

    /// 索尼扩展命令的请求缓冲：头部字段与补齐长度都要对
    #[test]
    fn sony_ext_payload_layout() {
        let cmd = SONY_CMD_NOTIFY_SCALAR_DLMODE;
        let p = build_sony_ext_payload(cmd, &[]);

        assert_eq!(p.len(), EXT_CMD_WRITE_BUFFER, "必须补齐到固定长度");
        assert_eq!(
            i32::from_le_bytes([p[0], p[1], p[2], p[3]]),
            0,
            "没有正文时 dataSize 应为 0"
        );
        assert_eq!(i16::from_le_bytes([p[4], p[5]]), 2, "cmd 应为 2（命令号）");
        assert_eq!(
            i16::from_le_bytes([p[6], p[7]]),
            0,
            "direction 固定为 0"
        );
        assert!(p[8..16].iter().all(|b| *b == 0), "8 字节保留应全为 0");
        assert!(
            p[EXT_CMD_HEADER_LEN..].iter().all(|b| *b == 0),
            "补齐部分应全为 0"
        );
    }

    /// 带正文时的编码
    #[test]
    fn sony_ext_payload_with_data() {
        let cmd = SonyExtCommand::new(1, 1);
        let data = [0xAAu8, 0xBB, 0xCC];
        let p = build_sony_ext_payload(cmd, &data);
        assert_eq!(p.len(), EXT_CMD_WRITE_BUFFER);
        assert_eq!(i32::from_le_bytes([p[0], p[1], p[2], p[3]]), 3);
        assert_eq!(i16::from_le_bytes([p[4], p[5]]), 1);
        assert_eq!(&p[EXT_CMD_HEADER_LEN..EXT_CMD_HEADER_LEN + 3], &data);
    }

    /// 响应解析：按头里的 dataSize 截取正文
    #[test]
    fn sony_ext_response_parsing() {
        let mut r = vec![0u8; EXT_CMD_HEADER_LEN + 5];
        r[0..4].copy_from_slice(&5i32.to_le_bytes());
        r[EXT_CMD_HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4, 5]);
        assert_eq!(parse_sony_ext_response(&r).unwrap(), vec![1, 2, 3, 4, 5]);

        // 太短要报错
        assert!(parse_sony_ext_response(&[0u8; 4]).is_err());
        // 声明比实际长要报错
        let mut bad = vec![0u8; EXT_CMD_HEADER_LEN + 2];
        bad[0..4].copy_from_slice(&100i32.to_le_bytes());
        assert!(parse_sony_ext_response(&bad).is_err());
    }

    /// 这三个操作码必须与原项目一致（写错就切不了模式）
    #[test]
    fn sony_ops_match_reference() {
        assert_eq!(PTP_OC_SONY_DI_EXT_CMD_WRITE, 0x9280);
        assert_eq!(PTP_OC_SONY_DI_EXT_CMD_READ, 0x9281);
        assert_eq!(PTP_OC_SONY_REQ_RECONNECT, 0x9282);
        assert_eq!(SONY_CMD_NOTIFY_SCALAR_DLMODE.group, 5);
        assert_eq!(SONY_CMD_NOTIFY_SCALAR_DLMODE.id, 2);
    }
}
