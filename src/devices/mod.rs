/// Device registry – tracks all paired Zigbee devices.
use std::collections::HashMap;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::zigbee::{EndpointDesc, IeeeAddr, NwkAddr};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub ieee_addr:     IeeeAddr,
    pub nwk_addr:      NwkAddr,
    pub friendly_name: String,
    pub endpoints:     Vec<EndpointDesc>,
    #[serde(default)]
    pub manufacturer:  Option<String>,
    #[serde(default)]
    pub model:         Option<String>,
    #[serde(default)]
    pub power_source:  Option<String>,
    /// Last known state values (merged from all cluster reports)
    #[serde(default)]
    pub state:         serde_json::Map<String, serde_json::Value>,
    pub interview_complete: bool,
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
            state: serde_json::Map::new(),
            interview_complete: false,
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

    pub fn to_info_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ieee_address":        self.ieee_addr.as_hex(),
            "network_address":     self.nwk_addr,
            "friendly_name":       self.friendly_name,
            "manufacturer":        self.manufacturer,
            "model":               self.model,
            "power_source":        self.power_source,
            "interview_complete":  self.interview_complete,
            "endpoints":           self.endpoints,
        })
    }
}

// ── Registry ──────────────────────────────────────────────────────────────────

pub struct DeviceRegistry {
    /// Primary index: IEEE address → Device
    by_ieee:  DashMap<IeeeAddr, Device>,
    /// Secondary index: NWK address → IEEE address
    by_nwk:   DashMap<NwkAddr, IeeeAddr>,
    /// Friendly name → IEEE address
    by_name:  DashMap<String, IeeeAddr>,
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
            by_nwk:  DashMap::new(),
            by_name: DashMap::new(),
        }
    }

    pub fn add(&self, device: Device) {
        let ieee = device.ieee_addr;
        let nwk  = device.nwk_addr;
        let name = device.friendly_name.clone();
        self.by_nwk.insert(nwk, ieee);
        self.by_name.insert(name, ieee);
        self.by_ieee.insert(ieee, device);
    }

    pub fn get_by_ieee(&self, addr: &IeeeAddr) -> Option<dashmap::mapref::one::Ref<IeeeAddr, Device>> {
        self.by_ieee.get(addr)
    }

    pub fn get_by_nwk(&self, addr: NwkAddr) -> Option<dashmap::mapref::one::Ref<IeeeAddr, Device>> {
        let ieee = self.by_nwk.get(&addr)?;
        self.by_ieee.get(ieee.value())
    }

    pub fn get_mut_by_ieee(&self, addr: &IeeeAddr) -> Option<dashmap::mapref::one::RefMut<IeeeAddr, Device>> {
        self.by_ieee.get_mut(addr)
    }

    pub fn get_mut_by_nwk(&self, addr: NwkAddr) -> Option<dashmap::mapref::one::RefMut<IeeeAddr, Device>> {
        let ieee = self.by_nwk.get(&addr)?.value().clone();
        self.by_ieee.get_mut(&ieee)
    }

    pub fn remove_by_ieee(&self, addr: &IeeeAddr) {
        if let Some((_, dev)) = self.by_ieee.remove(addr) {
            self.by_nwk.remove(&dev.nwk_addr);
            self.by_name.remove(&dev.friendly_name);
        }
    }

    pub fn remove_by_nwk(&self, addr: NwkAddr) {
        if let Some((_, ieee)) = self.by_nwk.remove(&addr) {
            self.remove_by_ieee(&ieee);
        }
    }

    pub fn update_nwk_addr(&self, ieee: &IeeeAddr, new_nwk: NwkAddr) {
        if let Some(mut dev) = self.by_ieee.get_mut(ieee) {
            self.by_nwk.remove(&dev.nwk_addr);
            dev.nwk_addr = new_nwk;
            self.by_nwk.insert(new_nwk, *ieee);
        }
    }

    pub fn set_friendly_name(&self, ieee: &IeeeAddr, name: String) -> bool {
        if let Some(mut dev) = self.by_ieee.get_mut(ieee) {
            self.by_name.remove(&dev.friendly_name);
            dev.friendly_name = name.clone();
            self.by_name.insert(name, *ieee);
            true
        } else {
            false
        }
    }

    pub fn all_devices(&self) -> Vec<Device> {
        self.by_ieee.iter().map(|r| r.value().clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.by_ieee.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ieee.is_empty()
    }

    /// Load persisted devices from a JSON value (from config file).
    pub fn load_from_json(&self, value: &serde_json::Value) {
        if let Some(arr) = value.as_array() {
            for entry in arr {
                if let Ok(dev) = serde_json::from_value::<Device>(entry.clone()) {
                    self.add(dev);
                }
            }
        }
    }

    /// Serialize all devices to JSON for persistence.
    pub fn to_json(&self) -> serde_json::Value {
        let devices: Vec<_> = self.by_ieee.iter()
            .map(|r| serde_json::to_value(r.value()).unwrap_or(serde_json::Value::Null))
            .collect();
        serde_json::Value::Array(devices)
    }
}
