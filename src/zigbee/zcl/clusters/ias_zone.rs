use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct IasZoneCluster;

// Cluster 0x0500 – IAS Zone (door/window sensors, motion sensors, smoke detectors)
//   Attribute 0x0000 – ZoneState  (Enum8)
//   Attribute 0x0001 – ZoneType   (Enum16)
//   Attribute 0x0002 – ZoneStatus (Bitmap16)
//
// Cluster-specific commands (server → client):
//   0x00 – Zone Status Change Notification

const ZONE_STATE:  u16 = 0x0000;
const ZONE_TYPE:   u16 = 0x0001;
const ZONE_STATUS: u16 = 0x0002;

// ZoneStatus bit masks
const ALARM1:    u16 = 0x0001;
const ALARM2:    u16 = 0x0002;
const TAMPER:    u16 = 0x0004;
const BATTERY:   u16 = 0x0008;
const SUPV_REP:  u16 = 0x0010;
const RESTORE:   u16 = 0x0020;
const TROUBLE:   u16 = 0x0040;
const AC_MAINS:  u16 = 0x0080;
const TEST:      u16 = 0x0100;
const BATTDEF:   u16 = 0x0200;

impl ClusterHandler for IasZoneCluster {
    fn cluster_id(&self) -> u16 { 0x0500 }

    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            if r.attr_id == ZONE_STATUS {
                if let Some(v) = r.value.as_f64() {
                    out.extend(decode_zone_status(v as u16));
                }
            }
        }
        out
    }

    fn process_command(&self, command_id: u8, payload: &[u8]) -> Vec<(String, Value)> {
        // 0x00 = Zone Status Change Notification
        // payload: zone_status (u16) | extended_status (u8) | zone_id (u8) | delay (u16)
        if command_id == 0x00 && payload.len() >= 2 {
            let zone_status = u16::from_le_bytes([payload[0], payload[1]]);
            return decode_zone_status(zone_status);
        }
        vec![]
    }
}

fn decode_zone_status(status: u16) -> Vec<(String, Value)> {
    vec![
        ("contact".into(),     json!((status & ALARM1) == 0)),  // contact closed = no alarm
        ("tamper".into(),      json!((status & TAMPER) != 0)),
        ("battery_low".into(), json!((status & BATTERY) != 0)),
        ("trouble".into(),     json!((status & TROUBLE) != 0)),
    ]
}
