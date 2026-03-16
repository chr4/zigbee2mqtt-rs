use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct IlluminanceCluster;

// Cluster 0x0400 – Illuminance Measurement
//   0x0000 – MeasuredValue (Uint16)
//            value = 10000 * log10(lux) + 1  (ZCL formula)
//            lux = 10^((value - 1) / 10000)

const MEASURED_VALUE: u16 = 0x0000;

impl ClusterHandler for IlluminanceCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            if r.attr_id == MEASURED_VALUE {
                if let Some(v) = r.value.as_f64() {
                    if v > 0 as f64 && v < 0xFFFF as f64 {
                        let lux = f64::powf(10.0, (v - 1.0) / 10_000.0);
                        out.push(("illuminance".into(), json!(lux.round() as u32)));
                        out.push(("illuminance_lux".into(), json!(lux.round() as u32)));
                    }
                }
            }
        }
        out
    }
}
