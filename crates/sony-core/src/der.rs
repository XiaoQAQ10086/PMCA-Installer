//! 极简 DER 解析器：只解析我们真正需要的东西（RSA 私钥的整数）。
//!
//! 之所以自己写：RSA 私钥是 PKCS#1 格式，`rsa` crate 的 `pkcs1` 解码需要额外 feature，
//! 而我们只需要 8 个大整数，用 60 行代码换掉一个依赖更划算，也更容易审计。

/// DER 里的一个 TLV（类型-长度-值）
#[derive(Debug, Clone, Copy)]
pub struct Tlv<'a> {
    pub tag: u8,
    pub value: &'a [u8],
}

impl<'a> Tlv<'a> {
    /// 读出一个 TLV，返回 (自身, 剩余字节)
    pub fn parse(data: &'a [u8]) -> Result<(Self, &'a [u8]), DerError> {
        if data.len() < 2 {
            return Err(DerError::Truncated);
        }
        let tag = data[0];
        let (len, header_len) = parse_length(&data[1..])?;
        let start = 1 + header_len;
        let end = start
            .checked_add(len)
            .ok_or(DerError::Truncated)?;
        if data.len() < end {
            return Err(DerError::Truncated);
        }
        Ok((
            Tlv {
                tag,
                value: &data[start..end],
            },
            &data[end..],
        ))
    }

    /// 把 value 当作一个大整数（DER INTEGER：大端、可能有前导 0）
    pub fn as_int_be(&self) -> &'a [u8] {
        let mut v = self.value;
        while v.len() > 1 && v[0] == 0 {
            v = &v[1..];
        }
        v
    }
}

fn parse_length(data: &[u8]) -> Result<(usize, usize), DerError> {
    if data.is_empty() {
        return Err(DerError::Truncated);
    }
    let first = data[0];
    if first & 0x80 == 0 {
        return Ok((first as usize, 1));
    }
    let n = (first & 0x7f) as usize;
    if n == 0 || n > 4 || data.len() < 1 + n {
        return Err(DerError::BadLength);
    }
    let mut len = 0usize;
    for &b in &data[1..1 + n] {
        len = (len << 8) | b as usize;
    }
    Ok((len, 1 + n))
}

/// 一个 SEQUENCE 里的所有 TLV
pub fn sequence_items(seq_value: &[u8]) -> Result<Vec<Tlv<'_>>, DerError> {
    let mut out = Vec::new();
    let mut rest = seq_value;
    while !rest.is_empty() {
        let (tlv, next) = Tlv::parse(rest)?;
        out.push(tlv);
        rest = next;
    }
    Ok(out)
}

/// 取出顶层 SEQUENCE 的内容
pub fn top_sequence(data: &[u8]) -> Result<&[u8], DerError> {
    let (tlv, _) = Tlv::parse(data)?;
    if tlv.tag != 0x30 {
        return Err(DerError::NotSequence);
    }
    Ok(tlv.value)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DerError {
    #[error("DER 数据被截断")]
    Truncated,
    #[error("DER 长度字段非法")]
    BadLength,
    #[error("期望 SEQUENCE")]
    NotSequence,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_length() {
        let (tlv, rest) = Tlv::parse(&[0x02, 0x01, 0x2a, 0xff]).unwrap();
        assert_eq!(tlv.tag, 0x02);
        assert_eq!(tlv.value, &[0x2a]);
        assert_eq!(rest, &[0xff]);
    }

    #[test]
    fn parses_long_length() {
        // 0x30 0x81 0x05 + 5 字节内容
        let data = [0x30, 0x81, 0x05, 1, 2, 3, 4, 5];
        let (tlv, rest) = Tlv::parse(&data).unwrap();
        assert_eq!(tlv.tag, 0x30);
        assert_eq!(tlv.value, &[1, 2, 3, 4, 5]);
        assert!(rest.is_empty());
    }

    #[test]
    fn trims_leading_zero_of_integer() {
        let (tlv, _) = Tlv::parse(&[0x02, 0x02, 0x00, 0x80]).unwrap();
        assert_eq!(tlv.as_int_be(), &[0x80]);
    }

    #[test]
    fn rejects_truncated() {
        assert_eq!(Tlv::parse(&[0x02]).unwrap_err(), DerError::Truncated);
        assert_eq!(
            Tlv::parse(&[0x02, 0x05, 0x01]).unwrap_err(),
            DerError::Truncated
        );
    }
}
