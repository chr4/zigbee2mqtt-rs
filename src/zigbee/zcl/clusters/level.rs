use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct LevelCluster;

// Cluster 0x0008 – Level Control
//   0x0000 – CurrentLevel (Uint8, range 0-254)
//   0x0001 – RemainingTime (Uint16, 1/10 s)

const CURRENT_LEVEL: u16 = 0x0000;

impl ClusterHandler for LevelCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            if r.attr_id == CURRENT_LEVEL {
                if let Some(v) = r.value.as_f64() {
                    // Expose as 0-100 percentage (ZCL range is 0-254)
                    let brightness_pct = (v / 254.0 * 100.0).round() as u8;
                    out.push(("brightness".into(), json!(v as u8)));
                    out.push(("brightness_percent".into(), json!(brightness_pct)));
                }
            }
        }
        out
    }

    fn process_command(&self, command_id: u8, payload: &[u8]) -> Vec<(String, Value)> {
        match command_id {
            // Move to Level (0x00) and Move to Level / On (0x04)
            0x00 | 0x04 => {
                if payload.is_empty() { return vec![]; }
                let level = payload[0];
                vec![
                    ("brightness".into(), json!(level)),
                    ("brightness_percent".into(), json!((level as f64 / 254.0 * 100.0) as u8)),
                ]
            }
            _ => vec![],
        }
    }
}

/// Build ZCL Move-to-Level payload (brightness 0-254, transition time in 100ms units).
pub fn move_to_level_payload(sequence: u8, level: u8, transition_time: u16) -> Vec<u8> {
    vec![
        0x11, sequence, 0x04, // cluster-specific, move-to-level with on/off
        level,
        (transition_time & 0xFF) as u8,
        (transition_time >> 8) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::zcl::attribute::{AttributeReport, AttributeValue};

    #[test]
    fn brightness_report() {
        let reports = vec![AttributeReport {
            attr_id: 0x0000,
            value: AttributeValue::U8(254),
        }];
        let result = LevelCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "brightness" && v == &json!(254)));
        assert!(result
            .iter()
            .any(|(k, v)| k == "brightness_percent" && v == &json!(100)));
    }

    #[test]
    fn brightness_half() {
        let reports = vec![AttributeReport {
            attr_id: 0x0000,
            value: AttributeValue::U8(127),
        }];
        let result = LevelCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "brightness" && v == &json!(127)));
        assert!(result
            .iter()
            .any(|(k, v)| k == "brightness_percent" && v == &json!(50)));
    }

    #[test]
    fn move_to_level_format() {
        let p = move_to_level_payload(3, 200, 15);
        assert_eq!(p[0], 0x11); // cluster-specific
        assert_eq!(p[1], 3); // sequence
        assert_eq!(p[2], 0x04); // Move to Level with On/Off
        assert_eq!(p[3], 200); // level
        assert_eq!(u16::from_le_bytes([p[4], p[5]]), 15); // transition time
    }
}
