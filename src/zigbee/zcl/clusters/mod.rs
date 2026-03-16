pub mod basic;
pub mod color;
pub mod humidity;
pub mod ias_zone;
pub mod illuminance;
pub mod level;
pub mod occupancy;
pub mod on_off;
pub mod power;
pub mod temperature;

use serde_json::Value;

use super::attribute::AttributeReport;

/// Trait implemented by every cluster handler.
pub trait ClusterHandler: Send + Sync {
    /// Process incoming attribute reports, returning key/value pairs
    /// to merge into the device state JSON.
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)>;

    /// Process a cluster-specific command (frame_type = 1).
    fn process_command(&self, _command_id: u8, _payload: &[u8]) -> Vec<(String, Value)> {
        vec![]
    }
}

/// Return a handler for the given cluster_id, or None if unsupported.
pub fn handler_for(cluster_id: u16) -> Option<Box<dyn ClusterHandler>> {
    match cluster_id {
        0x0000 => Some(Box::new(basic::BasicCluster)),
        0x0001 => Some(Box::new(power::PowerCluster)),
        0x0006 => Some(Box::new(on_off::OnOffCluster)),
        0x0008 => Some(Box::new(level::LevelCluster)),
        0x0300 => Some(Box::new(color::ColorCluster)),
        0x0400 => Some(Box::new(illuminance::IlluminanceCluster)),
        0x0402 => Some(Box::new(temperature::TemperatureCluster)),
        0x0405 => Some(Box::new(humidity::HumidityCluster)),
        0x0406 => Some(Box::new(occupancy::OccupancyCluster)),
        0x0500 => Some(Box::new(ias_zone::IasZoneCluster)),
        _ => None,
    }
}
