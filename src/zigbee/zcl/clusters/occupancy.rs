use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct OccupancyCluster;

// Cluster 0x0406 – Occupancy Sensing
//   0x0000 – Occupancy (Bitmap8, bit 0 = occupied)
//   0x0001 – OccupancySensorType (Enum8)

const OCCUPANCY:             u16 = 0x0000;
const OCCUPANCY_SENSOR_TYPE: u16 = 0x0001;

impl ClusterHandler for OccupancyCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            match r.attr_id {
                OCCUPANCY => {
                    if let Some(v) = r.value.as_f64() {
                        let occupied = (v as u8 & 0x01) != 0;
                        out.push(("occupancy".into(), json!(occupied)));
                    }
                }
                OCCUPANCY_SENSOR_TYPE => {
                    if let Some(v) = r.value.as_f64() {
                        let sensor = match v as u8 {
                            0 => "PIR",
                            1 => "ultrasonic",
                            2 => "PIR_and_ultrasonic",
                            _ => "unknown",
                        };
                        out.push(("occupancy_sensor_type".into(), json!(sensor)));
                    }
                }
                _ => {}
            }
        }
        out
    }
}
