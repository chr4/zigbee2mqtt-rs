/// Device registry – tracks all paired Zigbee devices.
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::zigbee::{EndpointDesc, IeeeAddr, NwkAddr};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub ieee_addr: IeeeAddr,
    pub nwk_addr: NwkAddr,
    pub friendly_name: String,
    pub endpoints: Vec<EndpointDesc>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub power_source: Option<String>,
    #[serde(default)]
    pub sw_build_id: Option<String>,
    /// Last known state values (merged from all cluster reports)
    #[serde(default)]
    pub state: serde_json::Map<String, serde_json::Value>,
    pub interview_complete: bool,
    #[serde(default)]
    pub disabled: bool,
}

impl Device {
    pub fn new(ieee_addr: IeeeAddr, nwk_addr: NwkAddr) -> Self {
        let friendly_name = ieee_addr.as_hex();
        Self {
            ieee_addr,
            nwk_addr,
            friendly_name,
            endpoints: Vec::new(),
            manufacturer: None,
            model: None,
            power_source: None,
            sw_build_id: None,
            state: serde_json::Map::new(),
            interview_complete: false,
            disabled: false,
        }
    }

    pub fn merge_state(&mut self, values: serde_json::Map<String, serde_json::Value>) {
        for (k, v) in values {
            self.state.insert(k, v);
        }
    }

    pub fn display_name(&self) -> &str {
        &self.friendly_name
    }

    /// Determine device type based on capabilities.
    pub fn device_type(&self) -> &'static str {
        // Simple heuristic: devices with only battery power or no routing are EndDevice
        if let Some(ref ps) = self.power_source {
            if ps == "battery" {
                return "EndDevice";
            }
        }
        "Router"
    }

    /// All unique input clusters across all endpoints.
    pub fn all_input_clusters(&self) -> Vec<u16> {
        let mut clusters: Vec<u16> = self
            .endpoints
            .iter()
            .flat_map(|ep| ep.input_clusters.iter().copied())
            .collect();
        clusters.sort_unstable();
        clusters.dedup();
        clusters
    }

    /// Generate z2m-compatible device info JSON for bridge/devices.
    pub fn to_z2m_device_json(&self) -> serde_json::Value {
        let definition = if self.manufacturer.is_some() || self.model.is_some() {
            json!({
                "model": self.model.as_deref().unwrap_or("Unknown"),
                "vendor": self.manufacturer.as_deref().unwrap_or("Unknown"),
                "description": "",
            })
        } else {
            serde_json::Value::Null
        };

        json!({
            "ieee_address": self.ieee_addr.as_hex(),
            "type": self.device_type(),
            "network_address": self.nwk_addr,
            "friendly_name": self.friendly_name,
            "definition": definition,
            "power_source": self.power_source.as_deref().unwrap_or("Unknown"),
            "model_id": self.model,
            "manufacturer": self.manufacturer,
            "interviewing": !self.interview_complete && !self.endpoints.is_empty(),
            "interview_completed": self.interview_complete,
            "software_build_id": self.sw_build_id,
            "supported": self.interview_complete,
            "disabled": self.disabled,
        })
    }
}

// ── Registry ──────────────────────────────────────────────────────────────────

pub struct DeviceRegistry {
    by_ieee: DashMap<IeeeAddr, Device>,
    by_nwk: DashMap<NwkAddr, IeeeAddr>,
    by_name: DashMap<String, IeeeAddr>,
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            by_ieee: DashMap::new(),
            by_nwk: DashMap::new(),
            by_name: DashMap::new(),
        }
    }

    pub fn add(&self, device: Device) {
        let ieee = device.ieee_addr;
        let nwk = device.nwk_addr;
        let name = device.friendly_name.clone();
        self.by_nwk.insert(nwk, ieee);
        self.by_name.insert(name, ieee);
        self.by_ieee.insert(ieee, device);
    }

    pub fn get_by_ieee(
        &self,
        addr: &IeeeAddr,
    ) -> Option<dashmap::mapref::one::Ref<'_, IeeeAddr, Device>> {
        self.by_ieee.get(addr)
    }

    pub fn get_by_nwk(
        &self,
        addr: NwkAddr,
    ) -> Option<dashmap::mapref::one::Ref<'_, IeeeAddr, Device>> {
        let ieee = self.by_nwk.get(&addr)?;
        self.by_ieee.get(ieee.value())
    }

    pub fn get_mut_by_ieee(
        &self,
        addr: &IeeeAddr,
    ) -> Option<dashmap::mapref::one::RefMut<'_, IeeeAddr, Device>> {
        self.by_ieee.get_mut(addr)
    }

    pub fn get_mut_by_nwk(
        &self,
        addr: NwkAddr,
    ) -> Option<dashmap::mapref::one::RefMut<'_, IeeeAddr, Device>> {
        let ieee = self.by_nwk.get(&addr)?.value().clone();
        self.by_ieee.get_mut(&ieee)
    }

    pub fn find_by_name(&self, name: &str) -> Option<Device> {
        let ieee = self.by_name.get(name)?;
        self.by_ieee.get(ieee.value()).map(|r| r.value().clone())
    }

    pub fn remove_by_ieee(&self, addr: &IeeeAddr) {
        if let Some((_, dev)) = self.by_ieee.remove(addr) {
            self.by_nwk.remove(&dev.nwk_addr);
            self.by_name.remove(&dev.friendly_name);
        }
    }

    pub fn update_nwk_addr(&self, ieee: &IeeeAddr, new_nwk: NwkAddr) {
        if let Some(mut dev) = self.by_ieee.get_mut(ieee) {
            self.by_nwk.remove(&dev.nwk_addr);
            dev.nwk_addr = new_nwk;
            self.by_nwk.insert(new_nwk, *ieee);
        }
    }

    pub fn all_devices(&self) -> Vec<Device> {
        self.by_ieee.iter().map(|r| r.value().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ieee() -> IeeeAddr {
        IeeeAddr([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08])
    }

    #[test]
    fn add_and_get_device() {
        let reg = DeviceRegistry::new();
        let dev = Device::new(test_ieee(), 0x1234);
        reg.add(dev.clone());

        assert!(reg.get_by_ieee(&test_ieee()).is_some());
        assert!(reg.get_by_nwk(0x1234).is_some());
    }

    #[test]
    fn find_by_name() {
        let reg = DeviceRegistry::new();
        let mut dev = Device::new(test_ieee(), 0x1234);
        dev.friendly_name = "kitchen_light".to_string();
        reg.add(dev);

        assert!(reg.find_by_name("kitchen_light").is_some());
        assert!(reg.find_by_name("nonexistent").is_none());
    }

    #[test]
    fn update_nwk_addr() {
        let reg = DeviceRegistry::new();
        reg.add(Device::new(test_ieee(), 0x1234));
        reg.update_nwk_addr(&test_ieee(), 0x5678);

        assert!(reg.get_by_nwk(0x1234).is_none());
        assert!(reg.get_by_nwk(0x5678).is_some());
    }

    #[test]
    fn remove_clears_all_indexes() {
        let reg = DeviceRegistry::new();
        let dev = Device::new(test_ieee(), 0x1234);
        reg.add(dev);

        reg.remove_by_ieee(&test_ieee());
        assert!(reg.get_by_ieee(&test_ieee()).is_none());
        assert!(reg.get_by_nwk(0x1234).is_none());
    }

    #[test]
    fn merge_state() {
        let mut dev = Device::new(test_ieee(), 0x1234);
        let mut values = serde_json::Map::new();
        values.insert("state".into(), json!("ON"));
        values.insert("brightness".into(), json!(254));
        dev.merge_state(values);

        assert_eq!(dev.state["state"], "ON");
        assert_eq!(dev.state["brightness"], 254);
    }

    #[test]
    fn device_z2m_json_format() {
        let mut dev = Device::new(test_ieee(), 0x1234);
        dev.manufacturer = Some("IKEA".to_string());
        dev.model = Some("TRADFRI".to_string());
        dev.interview_complete = true;

        let j = dev.to_z2m_device_json();
        assert_eq!(j["type"], "Router");
        assert_eq!(j["interview_completed"], true);
        assert_eq!(j["definition"]["vendor"], "IKEA");
        assert_eq!(j["friendly_name"], dev.ieee_addr.as_hex());
    }

    #[test]
    fn all_input_clusters_deduped() {
        let mut dev = Device::new(test_ieee(), 0x1234);
        dev.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0,
            input_clusters: vec![0x0000, 0x0006, 0x0008],
            output_clusters: vec![],
        });

        let clusters = dev.all_input_clusters();
        assert!(clusters.contains(&0x0006));
        assert!(clusters.contains(&0x0008));
        assert!(!clusters.contains(&0x0300));
    }
}
