use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct OnOffCluster;

// Cluster 0x0006 – On/Off
//   Attribute 0x0000 – OnOff (Boolean)
//
// Cluster-specific commands:
//   0x00 – Off
//   0x01 – On
//   0x02 – Toggle

const ON_OFF_ATTR: u16 = 0x0000;

impl ClusterHandler for OnOffCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            if r.attr_id == ON_OFF_ATTR {
                if let Some(on) = r.value.as_bool() {
                    out.push(("state".into(), json!(if on { "ON" } else { "OFF" })));
                }
            }
        }
        out
    }

    fn process_command(&self, command_id: u8, _payload: &[u8]) -> Vec<(String, Value)> {
        match command_id {
            0x00 => vec![("state".into(), json!("OFF"))],
            0x01 => vec![("state".into(), json!("ON"))],
            0x02 => vec![("state".into(), json!("TOGGLE"))],
            _    => vec![],
        }
    }
}

/// Build the ZCL payload to send an On/Off command.
/// `state`: "ON" | "OFF" | "TOGGLE"
pub fn set_state_payload(sequence: u8, state: &str) -> Option<Vec<u8>> {
    let cmd = match state.to_uppercase().as_str() {
        "ON"     | "TRUE"  => 0x01u8,
        "OFF"    | "FALSE" => 0x00u8,
        "TOGGLE"           => 0x02u8,
        _ => return None,
    };
    // Cluster-specific, client→server, no mfr, disable default response
    Some(vec![0x11, sequence, cmd])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::zcl::attribute::{AttributeReport, AttributeValue};

    #[test]
    fn on_report() {
        let reports = vec![AttributeReport {
            attr_id: 0x0000,
            value: AttributeValue::Bool(true),
        }];
        let result = OnOffCluster.process_reports(&reports);
        assert_eq!(result, vec![("state".into(), json!("ON"))]);
    }

    #[test]
    fn off_report() {
        let reports = vec![AttributeReport {
            attr_id: 0x0000,
            value: AttributeValue::Bool(false),
        }];
        let result = OnOffCluster.process_reports(&reports);
        assert_eq!(result, vec![("state".into(), json!("OFF"))]);
    }

    #[test]
    fn command_on_off_toggle() {
        assert_eq!(
            OnOffCluster.process_command(0x00, &[]),
            vec![("state".into(), json!("OFF"))]
        );
        assert_eq!(
            OnOffCluster.process_command(0x01, &[]),
            vec![("state".into(), json!("ON"))]
        );
        assert_eq!(
            OnOffCluster.process_command(0x02, &[]),
            vec![("state".into(), json!("TOGGLE"))]
        );
    }

    #[test]
    fn set_state_payload_on() {
        let p = set_state_payload(5, "ON").unwrap();
        assert_eq!(p, vec![0x11, 5, 0x01]);
    }

    #[test]
    fn set_state_payload_off() {
        let p = set_state_payload(0, "OFF").unwrap();
        assert_eq!(p, vec![0x11, 0, 0x00]);
    }

    #[test]
    fn set_state_payload_toggle() {
        assert!(set_state_payload(0, "TOGGLE").is_some());
    }

    #[test]
    fn set_state_payload_case_insensitive() {
        assert!(set_state_payload(0, "on").is_some());
        assert!(set_state_payload(0, "Off").is_some());
    }

    #[test]
    fn set_state_payload_invalid() {
        assert!(set_state_payload(0, "blah").is_none());
    }
}
