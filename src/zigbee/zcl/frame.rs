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
    Global          = 0,
    ClusterSpecific = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToServer = 0,
    ServerToClient = 1,
}

#[derive(Debug, Clone)]
pub struct ZclFrameHeader {
    pub frame_type:    FrameType,
    pub manufacturer:  Option<u16>,
    pub direction:     Direction,
    pub disable_default_resp: bool,
    pub sequence:      u8,
    pub command_id:    u8,
}

impl ZclFrameHeader {
    pub fn parse(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.is_empty() {
            return Err(Error::Zcl("empty ZCL frame".into()));
        }
        let fc = buf[0];
        let frame_type = if (fc & 0x03) == 1 { FrameType::ClusterSpecific } else { FrameType::Global };
        let mfr_specific = (fc & 0x04) != 0;
        let direction    = if (fc & 0x08) != 0 { Direction::ServerToClient } else { Direction::ClientToServer };
        let disable_dr   = (fc & 0x10) != 0;

        let mut pos = 1usize;
        let manufacturer = if mfr_specific {
            if buf.len() < pos + 2 { return Err(Error::Zcl("truncated manufacturer code".into())); }
            let mfr = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
            pos += 2;
            Some(mfr)
        } else {
            None
        };

        if buf.len() < pos + 2 {
            return Err(Error::Zcl("ZCL frame too short for sequence+command".into()));
        }
        let sequence   = buf[pos];     pos += 1;
        let command_id = buf[pos];     pos += 1;

        Ok((
            ZclFrameHeader {
                frame_type,
                manufacturer,
                direction,
                disable_default_resp: disable_dr,
                sequence,
                command_id,
            },
            pos,
        ))
    }

    /// Build a ZCL frame header byte-sequence for sending.
    pub fn encode_global(sequence: u8, command_id: u8, direction: Direction) -> Vec<u8> {
        let fc = match direction {
            Direction::ServerToClient => 0x08 | 0x10, // server→client, disable default rsp
            Direction::ClientToServer => 0x10,         // disable default rsp
        };
        vec![fc, sequence, command_id]
    }
}

// ── Global ZCL commands (frame_type = Global) ─────────────────────────────────

pub mod global {
    pub const READ_ATTRIBUTES:        u8 = 0x00;
    pub const READ_ATTRIBUTES_RSP:    u8 = 0x01;
    pub const WRITE_ATTRIBUTES:       u8 = 0x04;
    pub const WRITE_ATTRIBUTES_RSP:   u8 = 0x05;
    pub const CONFIGURE_REPORTING:    u8 = 0x06;
    pub const CONFIGURE_REPORTING_RSP: u8 = 0x07;
    pub const REPORT_ATTRIBUTES:      u8 = 0x0A;
    pub const DEFAULT_RESPONSE:       u8 = 0x0B;
    pub const DISCOVER_ATTRIBUTES:    u8 = 0x0C;
    pub const DISCOVER_ATTRIBUTES_RSP: u8 = 0x0D;
}

/// Build a ZCL Read Attributes request payload (cluster-agnostic).
pub fn read_attributes_payload(attr_ids: &[u16]) -> Vec<u8> {
    let mut payload = ZclFrameHeader::encode_global(0x01, global::READ_ATTRIBUTES, Direction::ClientToServer);
    for &id in attr_ids {
        payload.extend_from_slice(&id.to_le_bytes());
    }
    payload
}

/// Build a ZCL Configure Reporting request (for a single attribute).
pub fn configure_reporting_payload(
    sequence: u8,
    attr_id: u16,
    data_type: u8,
    min_interval: u16,
    max_interval: u16,
    reportable_change: Option<Vec<u8>>,
) -> Vec<u8> {
    let mut payload = ZclFrameHeader::encode_global(sequence, global::CONFIGURE_REPORTING, Direction::ClientToServer);
    payload.push(0x00); // direction: reported
    payload.extend_from_slice(&attr_id.to_le_bytes());
    payload.push(data_type);
    payload.extend_from_slice(&min_interval.to_le_bytes());
    payload.extend_from_slice(&max_interval.to_le_bytes());
    if let Some(change) = reportable_change {
        payload.extend(change);
    }
    payload
}
