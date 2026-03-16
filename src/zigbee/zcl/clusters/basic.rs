use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct BasicCluster;

// Cluster 0x0000 – Basic
//   0x0004 – Manufacturer name (CharStr)
//   0x0005 – Model identifier  (CharStr)
//   0x0007 – Power source       (Enum8)
//   0x4000 – SW build ID        (CharStr)

const MANUFACTURER_NAME: u16 = 0x0004;
const MODEL_IDENTIFIER:  u16 = 0x0005;
const POWER_SOURCE:      u16 = 0x0007;
const SW_BUILD_ID:       u16 = 0x4000;

impl ClusterHandler for BasicCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            match r.attr_id {
                MANUFACTURER_NAME => {
                    if let crate::zigbee::zcl::attribute::AttributeValue::Str(s) = &r.value {
                        out.push(("manufacturer".into(), json!(s)));
                    }
                }
                MODEL_IDENTIFIER => {
                    if let crate::zigbee::zcl::attribute::AttributeValue::Str(s) = &r.value {
                        out.push(("model".into(), json!(s)));
                    }
                }
                POWER_SOURCE => {
                    if let Some(v) = r.value.as_f64() {
                        let source = match v as u8 {
                            0x01 => "mains",
                            0x03 => "battery",
                            0x04 => "dc_source",
                            _    => "unknown",
                        };
                        out.push(("power_source".into(), json!(source)));
                    }
                }
                SW_BUILD_ID => {
                    if let crate::zigbee::zcl::attribute::AttributeValue::Str(s) = &r.value {
                        out.push(("sw_build_id".into(), json!(s)));
                    }
                }
                _ => {}
            }
        }
        out
    }
}
