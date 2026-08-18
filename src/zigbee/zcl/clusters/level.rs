use super::super::attribute::AttributeReport;
use super::ClusterHandler;
use serde_json::{json, Value};

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
                if payload.is_empty() {
                    return vec![];
                }
                let level = payload[0];
                vec![
                    ("brightness".into(), json!(level)),
                    (
                        "brightness_percent".into(),
                        json!((level as f64 / 254.0 * 100.0) as u8),
                    ),
                ]
            }
            // Move/Step/Stop, plain vs. With-On-Off variants: real remotes
            // (e.g. IKEA TRADFRI) encode which button (up/down) was pressed
            // by *which command variant* they send, not by the MoveMode/
            // StepMode payload byte -- mirrors zigbee-herdsman-converters'
            // `tradfriCommandsLevelCtrl()` exactly (a flat command-id ->
            // action lookup, payload content unused/irrelevant).
            0x06 => vec![("action".into(), json!("brightness_up_click"))],
            0x02 => vec![("action".into(), json!("brightness_down_click"))],
            0x05 => vec![("action".into(), json!("brightness_up_hold"))],
            0x07 => vec![("action".into(), json!("brightness_up_release"))],
            0x01 => vec![("action".into(), json!("brightness_down_hold"))],
            0x03 => vec![("action".into(), json!("brightness_down_release"))],
            _ => vec![],
        }
    }
}

/// Build ZCL Move-to-Level payload (brightness 0-254, transition time in 100ms units).
pub fn move_to_level_payload(sequence: u8, level: u8, transition_time: u16) -> Vec<u8> {
    vec![
        0x11,
        sequence,
        0x04, // cluster-specific, move-to-level with on/off
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
        assert!(result
            .iter()
            .any(|(k, v)| k == "brightness" && v == &json!(254)));
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
        assert!(result
            .iter()
            .any(|(k, v)| k == "brightness" && v == &json!(127)));
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

    #[test]
    fn brightness_up_click() {
        assert_eq!(
            LevelCluster.process_command(0x06, &[]),
            vec![("action".into(), json!("brightness_up_click"))]
        );
    }

    #[test]
    fn brightness_down_click() {
        assert_eq!(
            LevelCluster.process_command(0x02, &[]),
            vec![("action".into(), json!("brightness_down_click"))]
        );
    }

    #[test]
    fn brightness_up_hold() {
        assert_eq!(
            LevelCluster.process_command(0x05, &[]),
            vec![("action".into(), json!("brightness_up_hold"))]
        );
    }

    #[test]
    fn brightness_up_release() {
        assert_eq!(
            LevelCluster.process_command(0x07, &[]),
            vec![("action".into(), json!("brightness_up_release"))]
        );
    }

    #[test]
    fn brightness_down_hold() {
        assert_eq!(
            LevelCluster.process_command(0x01, &[]),
            vec![("action".into(), json!("brightness_down_hold"))]
        );
    }

    #[test]
    fn brightness_down_release() {
        assert_eq!(
            LevelCluster.process_command(0x03, &[]),
            vec![("action".into(), json!("brightness_down_release"))]
        );
    }

    #[test]
    fn payload_content_is_irrelevant_for_move_step_stop() {
        // Real remotes' payload bytes (MoveMode/StepMode/Rate/StepSize/
        // TransitionTime) carry no meaning here -- direction comes from the
        // command id alone. Arbitrary/nonempty payload shouldn't change it.
        assert_eq!(
            LevelCluster.process_command(0x06, &[0xFF, 0xFF, 0xFF]),
            vec![("action".into(), json!("brightness_up_click"))]
        );
    }
}
