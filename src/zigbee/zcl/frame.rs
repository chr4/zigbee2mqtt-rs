/// ZCL (Zigbee Cluster Library) frame header parsing.
///
/// ZCL frame structure:
///   Frame Control  [1 byte]
///   Manufacturer Code [0 or 2 bytes]
///   Sequence Number [1 byte]
///   Command ID      [1 byte]
///   Payload         [variable]
///
/// Frame Control bits:
///   [1:0] Frame type: 0=global, 1=cluster-specific
///   [2]   Manufacturer specific
///   [3]   Direction: 0=client→server, 1=server→client
///   [4]   Disable default response
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Global = 0,
    ClusterSpecific = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToServer = 0,
}

#[derive(Debug, Clone)]
pub struct ZclFrameHeader {
    pub frame_type: FrameType,
    pub command_id: u8,
}

impl ZclFrameHeader {
    pub fn parse(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.is_empty() {
            return Err(Error::Zcl("empty ZCL frame".into()));
        }
        let fc = buf[0];
        let frame_type = if (fc & 0x03) == 1 {
            FrameType::ClusterSpecific
        } else {
            FrameType::Global
        };
        let mfr_specific = (fc & 0x04) != 0;

        let mut pos = 1usize;
        if mfr_specific {
            if buf.len() < pos + 2 {
                return Err(Error::Zcl("truncated manufacturer code".into()));
            }
            pos += 2;
        }

        if buf.len() < pos + 2 {
            return Err(Error::Zcl(
                "ZCL frame too short for sequence+command".into(),
            ));
        }
        pos += 1; // sequence
        let command_id = buf[pos];
        pos += 1;

        Ok((
            ZclFrameHeader {
                frame_type,
                command_id,
            },
            pos,
        ))
    }

    fn encode_global(sequence: u8, command_id: u8, _direction: Direction) -> Vec<u8> {
        vec![0x10, sequence, command_id] // client→server, disable default rsp
    }
}

pub mod global {
    pub const READ_ATTRIBUTES: u8 = 0x00;
    pub const READ_ATTRIBUTES_RSP: u8 = 0x01;
    pub const REPORT_ATTRIBUTES: u8 = 0x0A;
}

/// Build a ZCL Read Attributes request payload.
pub fn read_attributes_payload(attr_ids: &[u16]) -> Vec<u8> {
    let mut payload =
        ZclFrameHeader::encode_global(0x01, global::READ_ATTRIBUTES, Direction::ClientToServer);
    for &id in attr_ids {
        payload.extend_from_slice(&id.to_le_bytes());
    }
    payload
}
