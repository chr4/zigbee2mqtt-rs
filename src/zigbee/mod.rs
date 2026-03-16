pub mod zcl;

use serde::{Deserialize, Serialize};

/// IEEE 802.15.4 extended address (8 bytes, little-endian on-wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IeeeAddr(pub [u8; 8]);

impl IeeeAddr {
    pub fn as_hex(&self) -> String {
        format!(
            "0x{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            self.0[7], self.0[6], self.0[5], self.0[4],
            self.0[3], self.0[2], self.0[1], self.0[0],
        )
    }

}

impl std::fmt::Display for IeeeAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_hex())
    }
}

/// Zigbee 16-bit network address.
pub type NwkAddr = u16;

/// A device endpoint descriptor discovered via ZDO Simple Descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointDesc {
    pub endpoint:        u8,
    pub profile_id:      u16,
    pub device_id:       u16,
    pub input_clusters:  Vec<u16>,
    pub output_clusters: Vec<u16>,
}
