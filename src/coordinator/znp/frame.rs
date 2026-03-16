/// ZNP (Zigbee Network Processor) frame format:
///   [0xFE] [len:u8] [cmd0:u8] [cmd1:u8] [data:len] [fcs:u8]
///
/// cmd0 = (frame_type << 5) | subsystem
/// FCS  = XOR of bytes from len through end of data
use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

pub const SOF: u8 = 0xFE;

// ── Frame types (bits [7:5] of CMD0) ─────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Poll = 0,
    SReq = 1,
    AReq = 2,
    SRsp = 3,
}

impl FrameType {
    pub fn from_cmd0(cmd0: u8) -> Self {
        match cmd0 >> 5 {
            0 => Self::Poll,
            1 => Self::SReq,
            2 => Self::AReq,
            3 => Self::SRsp,
            _ => unreachable!(),
        }
    }
}

// ── Subsystems (bits [4:0] of CMD0) ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Subsystem {
    Rpc = 0x00,
    Sys = 0x01,
    Mac = 0x02,
    Nwk = 0x03,
    Af = 0x04,
    Zdo = 0x05,
    Sapi = 0x06,
    Util = 0x07,
    Debug = 0x08,
    App = 0x09,
    AppCnf = 0x0F,
    Gp = 0x15,
    Unknown = 0x1F,
}

impl Subsystem {
    pub fn from_cmd0(cmd0: u8) -> Self {
        match cmd0 & 0x1F {
            0x00 => Self::Rpc,
            0x01 => Self::Sys,
            0x02 => Self::Mac,
            0x03 => Self::Nwk,
            0x04 => Self::Af,
            0x05 => Self::Zdo,
            0x06 => Self::Sapi,
            0x07 => Self::Util,
            0x08 => Self::Debug,
            0x09 => Self::App,
            0x0F => Self::AppCnf,
            0x15 => Self::Gp,
            _ => Self::Unknown,
        }
    }
}

// ── ZNP Frame ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZnpFrame {
    pub frame_type: FrameType,
    pub subsystem: Subsystem,
    pub cmd1: u8,
    pub data: Vec<u8>,
}

impl ZnpFrame {
    pub fn new(frame_type: FrameType, subsystem: Subsystem, cmd1: u8, data: Vec<u8>) -> Self {
        Self {
            frame_type,
            subsystem,
            cmd1,
            data,
        }
    }

    pub fn sreq(subsystem: Subsystem, cmd1: u8, data: Vec<u8>) -> Self {
        Self::new(FrameType::SReq, subsystem, cmd1, data)
    }

    pub fn areq(subsystem: Subsystem, cmd1: u8, data: Vec<u8>) -> Self {
        Self::new(FrameType::AReq, subsystem, cmd1, data)
    }

    pub fn cmd0(&self) -> u8 {
        ((self.frame_type as u8) << 5) | (self.subsystem as u8)
    }

    pub fn encode_to(&self, buf: &mut BytesMut) {
        let len = self.data.len() as u8;
        let cmd0 = self.cmd0();
        let cmd1 = self.cmd1;

        let mut fcs = len ^ cmd0 ^ cmd1;
        for &b in &self.data {
            fcs ^= b;
        }

        buf.reserve(5 + self.data.len());
        buf.put_u8(SOF);
        buf.put_u8(len);
        buf.put_u8(cmd0);
        buf.put_u8(cmd1);
        buf.put_slice(&self.data);
        buf.put_u8(fcs);
    }

    #[cfg(test)]
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(5 + self.data.len());
        self.encode_to(&mut buf);
        buf.to_vec()
    }
}

fn compute_fcs(len: u8, cmd0: u8, cmd1: u8, data: &[u8]) -> u8 {
    let mut fcs = len ^ cmd0 ^ cmd1;
    for &b in data {
        fcs ^= b;
    }
    fcs
}

// ── Codec ─────────────────────────────────────────────────────────────────────

pub struct ZnpCodec;

impl Decoder for ZnpCodec {
    type Item = ZnpFrame;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        src: &mut BytesMut,
    ) -> std::result::Result<Option<Self::Item>, Self::Error> {
        loop {
            let sof_pos = match src.iter().position(|&b| b == SOF) {
                Some(pos) => pos,
                None => {
                    src.clear();
                    return Ok(None);
                }
            };

            if sof_pos > 0 {
                src.advance(sof_pos);
            }

            if src.len() < 5 {
                return Ok(None);
            }

            let len = src[1] as usize;
            let total = 5 + len;

            if src.len() < total {
                return Ok(None);
            }

            let frame_bytes = src[..total].to_vec();
            src.advance(total);

            let len_byte = frame_bytes[1];
            let cmd0 = frame_bytes[2];
            let cmd1 = frame_bytes[3];
            let data = frame_bytes[4..4 + len].to_vec();
            let received_fcs = frame_bytes[4 + len];

            let expected_fcs = compute_fcs(len_byte, cmd0, cmd1, &data);
            if received_fcs != expected_fcs {
                tracing::warn!(
                    "FCS mismatch: expected 0x{expected_fcs:02X} got 0x{received_fcs:02X}, skipping"
                );
                continue;
            }

            let frame_type = FrameType::from_cmd0(cmd0);
            let subsystem = Subsystem::from_cmd0(cmd0);

            return Ok(Some(ZnpFrame {
                frame_type,
                subsystem,
                cmd1,
                data,
            }));
        }
    }
}

impl Encoder<ZnpFrame> for ZnpCodec {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        item: ZnpFrame,
        dst: &mut BytesMut,
    ) -> std::result::Result<(), Self::Error> {
        item.encode_to(dst);
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_empty_data() {
        let frame = ZnpFrame::sreq(Subsystem::Sys, 0x02, vec![]);
        let bytes = frame.to_bytes();
        assert_eq!(bytes[0], SOF);
        assert_eq!(bytes[1], 0);

        let mut buf = BytesMut::from(bytes.as_slice());
        let decoded = ZnpCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn round_trip_with_data() {
        let data = vec![0x01, 0x02, 0x03];
        let frame = ZnpFrame::areq(Subsystem::Zdo, 0xC1, data.clone());
        let bytes = frame.to_bytes();

        let mut buf = BytesMut::from(bytes.as_slice());
        let decoded = ZnpCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.data, data);
    }

    #[test]
    fn fcs_computed_correctly() {
        let frame = ZnpFrame::sreq(Subsystem::Sys, 0x02, vec![]);
        let bytes = frame.to_bytes();
        // FCS = 0x00 ^ 0x21 ^ 0x02 = 0x23
        assert_eq!(bytes[4], 0x23);
    }

    #[test]
    fn unknown_subsystem_not_discarded() {
        // cmd0=0x60 = SRsp | subsystem 0x00
        let mut buf = BytesMut::new();
        buf.put_u8(SOF);
        buf.put_u8(1); // len=1
        buf.put_u8(0x60); // cmd0: SRsp | Rpc(0)
        buf.put_u8(0x09); // cmd1
        buf.put_u8(0x0A); // data
        let fcs = 1 ^ 0x60 ^ 0x09 ^ 0x0A;
        buf.put_u8(fcs);

        let frame = ZnpCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.frame_type, FrameType::SRsp);
        assert_eq!(frame.subsystem, Subsystem::Rpc);
    }
}
