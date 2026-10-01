//! 读取录音器产出的 pcap，取出**真机握手**的双向字节流。
//!
//! 录音器把双向字节写成一个简单的 pcap 文件（"包头"其实是 20 字节的 IPv4 头，
//! 其中源/目的地址被复用为方向标记）。这里把它还原成两条连续的字节流，
//! 用于验证新实现的密钥推导与记录层解密。
//!
//! 格式约定（由资料目录里的 `tools\pmca_recorder.py` 写入 —— 脚本不在本仓库里）：
//! - pcap 全局头 24 字节，每条记录头 16 字节
//! - "IP 头"的源地址：`0x7F000001` = 相机→电脑；`0x7F000002` = 电脑→相机

/// 录音数据的原始字节（真机：索尼 ILCE-6300，2026-09-27）
pub const HANDSHAKE_PCAP: &[u8] = include_bytes!("../assets/handshake-a6300.pcap");

const PCAP_MAGIC: u32 = 0xA1B2C3D4;
const IP_CAM: u32 = 0x7F00_0001;
const IP_PC: u32 = 0x7F00_0002;

/// 两个方向的字节流
#[derive(Debug, Clone, Default)]
pub struct Streams {
    /// 相机发来的字节（ClientHello、ClientKeyExchange、Finished…）
    pub cam_to_pc: Vec<u8>,
    /// 电脑发去的字节（ServerHello、Certificate、ServerHelloDone…）
    pub pc_to_cam: Vec<u8>,
}

/// 解析 pcap，恢复双向字节流
pub fn parse_pcap(data: &[u8]) -> anyhow::Result<Streams> {
    anyhow::ensure!(data.len() >= 24, "pcap 文件太小");
    let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    anyhow::ensure!(magic == PCAP_MAGIC, "pcap 魔数不对：0x{magic:08x}");

    let mut out = Streams::default();
    let mut off = 24usize;
    while off + 16 <= data.len() {
        let incl = u32::from_le_bytes([
            data[off + 8],
            data[off + 9],
            data[off + 10],
            data[off + 11],
        ]) as usize;
        off += 16;
        if off + incl > data.len() {
            break;
        }
        let pkt = &data[off..off + incl];
        off += incl;
        if pkt.len() < 20 {
            continue;
        }
        let ihl = ((pkt[0] & 0x0F) as usize) * 4;
        if pkt.len() < ihl {
            continue;
        }
        let src = u32::from_le_bytes([pkt[12], pkt[13], pkt[14], pkt[15]]);
        let payload = &pkt[ihl..];
        match src {
            IP_CAM => out.cam_to_pc.extend_from_slice(payload),
            IP_PC => out.pc_to_cam.extend_from_slice(payload),
            _ => {}
        }
    }
    anyhow::ensure!(
        !out.cam_to_pc.is_empty() && !out.pc_to_cam.is_empty(),
        "pcap 里没有找到双向数据"
    );
    Ok(out)
}

/// 一条 TLS 记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub content_type: u8,
    pub version: u16,
    /// 加密/明文正文
    pub body: Vec<u8>,
}

/// 按 TLS 记录层切分（允许最后一条不完整）
pub fn split_records(mut data: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    while data.len() >= 5 {
        let content_type = data[0];
        let version = u16::from_be_bytes([data[1], data[2]]);
        let len = u16::from_be_bytes([data[3], data[4]]) as usize;
        if data.len() < 5 + len {
            break;
        }
        out.push(Record {
            content_type,
            version,
            body: data[5..5 + len].to_vec(),
        });
        data = &data[5 + len..];
    }
    out
}

/// 从一条握手记录里逐条取出握手消息（4 字节头 + 正文）
pub fn handshake_messages(body: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 4 <= body.len() {
        let msg_type = body[p];
        let len =
            ((body[p + 1] as usize) << 16) | ((body[p + 2] as usize) << 8) | body[p + 3] as usize;
        if p + 4 + len > body.len() {
            break;
        }
        out.push((msg_type, body[p + 4..p + 4 + len].to_vec()));
        p += 4 + len;
    }
    out
}

/// 以 4 字节头 + 正文的形式重新打包一条握手消息（用于计算握手哈希）
pub fn pack_handshake(msg_type: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.push(msg_type);
    let len = body.len() as u32;
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机 pcap 必须能解析出双向数据
    #[test]
    fn parses_golden_pcap() {
        let s = parse_pcap(HANDSHAKE_PCAP).expect("应能解析真机录音");
        assert!(s.cam_to_pc.len() > 3000, "相机方向应有数千字节");
        assert!(s.pc_to_cam.len() > 2000, "电脑方向应有数千字节");
    }

    /// 相机方向的第一条记录必须是 ClientHello，且字节与另一份黄金样本一致
    #[test]
    fn first_camera_record_is_client_hello() {
        let s = parse_pcap(HANDSHAKE_PCAP).unwrap();
        let recs = split_records(&s.cam_to_pc);
        assert!(!recs.is_empty());
        let r0 = &recs[0];
        assert_eq!(r0.content_type, 22, "应是握手记录");
        assert_eq!(r0.version, 0x0301, "应是 TLS 1.0");
        let msgs = handshake_messages(&r0.body);
        assert_eq!(msgs[0].0, 1, "第一条握手消息应是 ClientHello");
        // handshake_messages 返回的是"去掉 4 字节头"的正文，这里重新加上头再比对
        let rebuilt = pack_handshake(msgs[0].0, &msgs[0].1);
        assert_eq!(
            rebuilt,
            crate::golden::client_hello()[5..].to_vec(),
            "pcap 里的 ClientHello 必须与 clienthello-a6300.bin 完全一致"
        );
    }

    /// 相机方向的记录序列必须与我们观察到的真机行为一致
    #[test]
    fn camera_record_sequence_matches_observation() {
        let s = parse_pcap(HANDSHAKE_PCAP).unwrap();
        let recs = split_records(&s.cam_to_pc);
        assert!(recs.len() >= 4, "至少应有 ClientHello/KEX/CCS/Finished");
        assert_eq!(recs[0].content_type, 22, "#0 ClientHello");
        let msgs = handshake_messages(&recs[1].body);
        assert_eq!(recs[1].content_type, 22, "#1 ClientKeyExchange");
        assert_eq!(msgs[0].0, 16, "#1 应是 ClientKeyExchange");
        assert_eq!(recs[2].content_type, 20, "#2 ChangeCipherSpec");
        assert_eq!(recs[2].body, vec![1u8], "CCS 正文应为 0x01");
        assert_eq!(recs[3].content_type, 22, "#3 Finished（已加密）");
    }

    /// 电脑方向的记录序列：ServerHello / Certificate / ServerHelloDone / CCS / Finished
    #[test]
    fn pc_record_sequence_matches_observation() {
        let s = parse_pcap(HANDSHAKE_PCAP).unwrap();
        let recs = split_records(&s.pc_to_cam);
        assert_eq!(recs[0].content_type, 22);
        let m0 = handshake_messages(&recs[0].body);
        assert_eq!(m0[0].0, 2, "#0 ServerHello");
        let m1 = handshake_messages(&recs[1].body);
        assert_eq!(m1[0].0, 11, "#1 Certificate");
        let m2 = handshake_messages(&recs[2].body);
        assert_eq!(m2[0].0, 14, "#2 ServerHelloDone");
        assert_eq!(recs[3].content_type, 20, "#3 ChangeCipherSpec");
        assert_eq!(recs[4].content_type, 22, "#4 Finished（已加密）");
    }

    /// 服务端发来的证书链必须与捆绑的证书链一致（说明录音就是原项目那套证书）
    #[test]
    fn recorded_certificate_matches_bundled_chain() {
        let s = parse_pcap(HANDSHAKE_PCAP).unwrap();
        let recs = split_records(&s.pc_to_cam);
        let msgs = handshake_messages(&recs[1].body);
        assert_eq!(msgs[0].0, 11);
        let body = &msgs[0].1;
        // body = 3 字节总长 + 每张证书(3 字节长 + DER)
        let total = ((body[0] as usize) << 16) | ((body[1] as usize) << 8) | body[2] as usize;
        assert_eq!(total + 3, body.len(), "证书列表长度字段应自洽");

        let mut p = 3usize;
        let mut recorded: Vec<Vec<u8>> = Vec::new();
        while p + 3 <= body.len() {
            let l = ((body[p] as usize) << 16) | ((body[p + 1] as usize) << 8) | body[p + 2] as usize;
            p += 3;
            if p + l > body.len() {
                break;
            }
            recorded.push(body[p..p + l].to_vec());
            p += l;
        }
        let bundled = crate::certs::certificate_chain().unwrap();
        assert_eq!(recorded.len(), bundled.len(), "证书张数应一致");
        for (i, (a, b)) in recorded.iter().zip(bundled.iter()).enumerate() {
            assert_eq!(a, b, "第 {i} 张证书必须与捆绑的一致");
        }
    }
}
