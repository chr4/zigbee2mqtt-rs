/// ZCL attribute data types and values (ZCL spec section 2.5.2)
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DataType {
    NoData    = 0x00,
    Data8     = 0x08,
    Data16    = 0x09,
    Data24    = 0x0A,
    Data32    = 0x0B,
    Boolean   = 0x10,
    Bitmap8   = 0x18,
    Bitmap16  = 0x19,
    Uint8     = 0x20,
    Uint16    = 0x21,
    Uint24    = 0x22,
    Uint32    = 0x23,
    Int8      = 0x28,
    Int16     = 0x29,
    Int24     = 0x2A,
    Int32     = 0x2B,
    Enum8     = 0x30,
    Enum16    = 0x31,
    SemiFloat = 0x38,
    Float     = 0x39,
    Double    = 0x3A,
    OctetStr  = 0x41,
    CharStr   = 0x42,
    LongOctetStr = 0x43,
    LongCharStr  = 0x44,
    Array        = 0x48,
    Invalid      = 0xFF,
}

impl DataType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0x00 => Self::NoData,
            0x08 => Self::Data8,
            0x09 => Self::Data16,
            0x0A => Self::Data24,
            0x0B => Self::Data32,
            0x10 => Self::Boolean,
            0x18 => Self::Bitmap8,
            0x19 => Self::Bitmap16,
            0x20 => Self::Uint8,
            0x21 => Self::Uint16,
            0x22 => Self::Uint24,
            0x23 => Self::Uint32,
            0x28 => Self::Int8,
            0x29 => Self::Int16,
            0x2A => Self::Int24,
            0x2B => Self::Int32,
            0x30 => Self::Enum8,
            0x31 => Self::Enum16,
            0x38 => Self::SemiFloat,
            0x39 => Self::Float,
            0x3A => Self::Double,
            0x41 => Self::OctetStr,
            0x42 => Self::CharStr,
            0x43 => Self::LongOctetStr,
            0x44 => Self::LongCharStr,
            0x48 => Self::Array,
            _    => Self::Invalid,
        }
    }

    /// Returns the fixed byte-length for types with known size, or None for variable.
    pub fn fixed_len(self) -> Option<usize> {
        match self {
            Self::NoData                => Some(0),
            Self::Boolean | Self::Bitmap8 | Self::Uint8 | Self::Int8 | Self::Enum8 | Self::Data8 => Some(1),
            Self::Bitmap16 | Self::Uint16 | Self::Int16 | Self::Enum16 | Self::Data16 | Self::SemiFloat => Some(2),
            Self::Uint24 | Self::Int24 | Self::Data24 => Some(3),
            Self::Uint32 | Self::Int32 | Self::Data32 | Self::Float => Some(4),
            Self::Double => Some(8),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AttributeValue {
    Bool(bool),
    U8(u8),
    U16(u16),
    U24(u32),
    U32(u32),
    I8(i8),
    I16(i16),
    I32(i32),
    Float(f32),
    Str(String),
    Bytes(Vec<u8>),
    Invalid,
}

impl AttributeValue {
    /// Parse a ZCL attribute value, advancing the slice cursor.
    /// Returns (value, bytes_consumed).
    pub fn parse(data_type: DataType, buf: &[u8]) -> Result<(Self, usize)> {
        match data_type {
            DataType::Boolean | DataType::Bitmap8 | DataType::Uint8
            | DataType::Int8   | DataType::Enum8  | DataType::Data8 => {
                if buf.is_empty() { return Err(Error::Zcl("truncated attribute value".into())); }
                let val = match data_type {
                    DataType::Boolean => Self::Bool(buf[0] != 0),
                    DataType::Int8    => Self::I8(buf[0] as i8),
                    _                 => Self::U8(buf[0]),
                };
                Ok((val, 1))
            }
            DataType::Bitmap16 | DataType::Uint16 | DataType::Enum16 | DataType::Data16 | DataType::SemiFloat => {
                if buf.len() < 2 { return Err(Error::Zcl("truncated u16".into())); }
                Ok((Self::U16(u16::from_le_bytes([buf[0], buf[1]])), 2))
            }
            DataType::Uint24 | DataType::Data24 => {
                if buf.len() < 3 { return Err(Error::Zcl("truncated u24".into())); }
                let v = u32::from_le_bytes([buf[0], buf[1], buf[2], 0]);
                Ok((Self::U24(v), 3))
            }
            DataType::Uint32 | DataType::Data32 => {
                if buf.len() < 4 { return Err(Error::Zcl("truncated u32".into())); }
                Ok((Self::U32(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])), 4))
            }
            DataType::Int16 => {
                if buf.len() < 2 { return Err(Error::Zcl("truncated i16".into())); }
                Ok((Self::I16(i16::from_le_bytes([buf[0], buf[1]])), 2))
            }
            DataType::Int32 => {
                if buf.len() < 4 { return Err(Error::Zcl("truncated i32".into())); }
                Ok((Self::I32(i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])), 4))
            }
            DataType::Float => {
                if buf.len() < 4 { return Err(Error::Zcl("truncated float".into())); }
                let bits = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                Ok((Self::Float(f32::from_bits(bits)), 4))
            }
            DataType::CharStr => {
                if buf.is_empty() { return Err(Error::Zcl("missing string length".into())); }
                let len = buf[0] as usize;
                if len == 0xFF { return Ok((Self::Invalid, 1)); } // invalid value
                if buf.len() < 1 + len { return Err(Error::Zcl("truncated string".into())); }
                let s = String::from_utf8_lossy(&buf[1..1 + len]).into_owned();
                Ok((Self::Str(s), 1 + len))
            }
            DataType::OctetStr => {
                if buf.is_empty() { return Err(Error::Zcl("missing octet-string length".into())); }
                let len = buf[0] as usize;
                if buf.len() < 1 + len { return Err(Error::Zcl("truncated octet string".into())); }
                Ok((Self::Bytes(buf[1..1 + len].to_vec()), 1 + len))
            }
            DataType::NoData => Ok((Self::Invalid, 0)),
            _ => {
                // Unknown type — skip based on fixed length if available
                if let Some(len) = data_type.fixed_len() {
                    Ok((Self::Bytes(buf[..len.min(buf.len())].to_vec()), len.min(buf.len())))
                } else {
                    Err(Error::Zcl(format!("unsupported attribute type {:?}", data_type)))
                }
            }
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::U8(v)  => Some(*v as f64),
            Self::U16(v) => Some(*v as f64),
            Self::U24(v) => Some(*v as f64),
            Self::U32(v) => Some(*v as f64),
            Self::I8(v)  => Some(*v as f64),
            Self::I16(v) => Some(*v as f64),
            Self::I32(v) => Some(*v as f64),
            Self::Float(v) => Some(*v as f64),
            Self::Bool(v)  => Some(if *v { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(v) => Some(*v),
            Self::U8(v)   => Some(*v != 0),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AttributeReport {
    pub attr_id: u16,
    pub value:   AttributeValue,
}

impl AttributeReport {
    /// Parse a sequence of attribute reports from a ZCL Report Attributes payload.
    pub fn parse_all(buf: &[u8]) -> Vec<Self> {
        let mut reports = Vec::new();
        let mut pos = 0;
        while pos + 3 <= buf.len() {
            let attr_id   = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
            let data_type = DataType::from_u8(buf[pos + 2]);
            pos += 3;
            match AttributeValue::parse(data_type, &buf[pos..]) {
                Ok((value, consumed)) => {
                    reports.push(AttributeReport { attr_id, value });
                    pos += consumed;
                }
                Err(e) => {
                    tracing::warn!("Error parsing attribute 0x{attr_id:04X}: {e}");
                    break;
                }
            }
        }
        reports
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bool_true() {
        let (val, consumed) = AttributeValue::parse(DataType::Boolean, &[0x01]).unwrap();
        assert_eq!(consumed, 1);
        assert_eq!(val.as_bool(), Some(true));
    }

    #[test]
    fn parse_bool_false() {
        let (val, _) = AttributeValue::parse(DataType::Boolean, &[0x00]).unwrap();
        assert_eq!(val.as_bool(), Some(false));
    }

    #[test]
    fn parse_u8() {
        let (val, consumed) = AttributeValue::parse(DataType::Uint8, &[0xFF]).unwrap();
        assert_eq!(consumed, 1);
        assert_eq!(val.as_f64(), Some(255.0));
    }

    #[test]
    fn parse_u16() {
        let (val, consumed) = AttributeValue::parse(DataType::Uint16, &[0x34, 0x12]).unwrap();
        assert_eq!(consumed, 2);
        assert_eq!(val.as_f64(), Some(0x1234 as f64));
    }

    #[test]
    fn parse_i16_negative() {
        let bytes = (-500i16).to_le_bytes();
        let (val, consumed) = AttributeValue::parse(DataType::Int16, &bytes).unwrap();
        assert_eq!(consumed, 2);
        assert_eq!(val.as_f64(), Some(-500.0));
    }

    #[test]
    fn parse_char_str() {
        let buf = [5, b'H', b'e', b'l', b'l', b'o'];
        let (val, consumed) = AttributeValue::parse(DataType::CharStr, &buf).unwrap();
        assert_eq!(consumed, 6);
        if let AttributeValue::Str(s) = val {
            assert_eq!(s, "Hello");
        } else {
            panic!("Expected Str");
        }
    }

    #[test]
    fn parse_char_str_empty() {
        let (val, consumed) = AttributeValue::parse(DataType::CharStr, &[0]).unwrap();
        assert_eq!(consumed, 1);
        if let AttributeValue::Str(s) = val {
            assert_eq!(s, "");
        } else {
            panic!("Expected Str");
        }
    }

    #[test]
    fn parse_char_str_invalid_length() {
        let (val, consumed) = AttributeValue::parse(DataType::CharStr, &[0xFF]).unwrap();
        assert_eq!(consumed, 1);
        assert!(matches!(val, AttributeValue::Invalid));
    }

    #[test]
    fn parse_u24() {
        let (val, consumed) = AttributeValue::parse(DataType::Uint24, &[0x01, 0x02, 0x03]).unwrap();
        assert_eq!(consumed, 3);
        assert_eq!(val.as_f64(), Some(0x030201 as f64));
    }

    #[test]
    fn parse_u32() {
        let (val, consumed) =
            AttributeValue::parse(DataType::Uint32, &[0x01, 0x00, 0x00, 0x00]).unwrap();
        assert_eq!(consumed, 4);
        assert_eq!(val.as_f64(), Some(1.0));
    }

    #[test]
    fn parse_truncated_errors() {
        assert!(AttributeValue::parse(DataType::Uint16, &[0x01]).is_err());
        assert!(AttributeValue::parse(DataType::Boolean, &[]).is_err());
    }

    #[test]
    fn parse_all_multiple_reports() {
        #[rustfmt::skip]
        let buf = [
            0x00, 0x00, // attr_id = 0x0000
            0x10,       // data_type = Boolean
            0x01,       // value = true
            0x01, 0x00, // attr_id = 0x0001
            0x20,       // data_type = Uint8
            0xFE,       // value = 254
        ];
        let reports = AttributeReport::parse_all(&buf);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].attr_id, 0x0000);
        assert_eq!(reports[0].value.as_bool(), Some(true));
        assert_eq!(reports[1].attr_id, 0x0001);
        assert_eq!(reports[1].value.as_f64(), Some(254.0));
    }

    #[test]
    fn data_type_fixed_len() {
        assert_eq!(DataType::Boolean.fixed_len(), Some(1));
        assert_eq!(DataType::Uint16.fixed_len(), Some(2));
        assert_eq!(DataType::Uint32.fixed_len(), Some(4));
        assert_eq!(DataType::Double.fixed_len(), Some(8));
        assert_eq!(DataType::CharStr.fixed_len(), None);
        assert_eq!(DataType::OctetStr.fixed_len(), None);
    }
}
