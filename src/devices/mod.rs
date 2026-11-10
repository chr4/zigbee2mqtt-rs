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
    /// IAS Zone (0x0500) ZoneType attribute, when the device exposes that
    /// cluster. Determines whether a zone status change means "contact",
    /// "occupancy", "smoke", etc. -- see zigbee::zcl::clusters::ias_zone.
    #[serde(default)]
    pub zone_type: Option<u16>,
    /// IKEA TRADFRI-style remote: the arrow direction ("left"/"right")
    /// remembered from the most recent unreleased ArrowHold genScenes
    /// command, if any -- consumed by the matching ArrowRelease. Internal
    /// bookkeeping only, never published as part of device `state`. See
    /// zigbee::zcl::clusters::scenes.
    #[serde(default)]
    pub arrow_hold_direction: Option<String>,
    /// Last known state values (merged from all cluster reports)
    #[serde(default)]
    pub state: serde_json::Map<String, serde_json::Value>,
    pub interview_complete: bool,
    /// Bindings and attribute reporting have been set up on the device --
    /// by this bridge, or by zigbee2mqtt before it (database.db
    /// `meta.configured`).
    #[serde(default)]
    pub configured: bool,
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
            zone_type: None,
            arrow_hold_direction: None,
            state: serde_json::Map::new(),
            interview_complete: false,
            configured: false,
            disabled: false,
        }
    }

    /// Merge new state values in. Nested objects (e.g. `color`) are merged
    /// key by key, like zigbee2mqtt's deep state merge: a report carrying
    /// only `currentY` must not wipe the known `color.x`.
    pub fn merge_state(&mut self, values: serde_json::Map<String, serde_json::Value>) {
        merge_json(&mut self.state, values);
    }

    /// Whether the device runs on mains power (and so is always reachable,
    /// unlike a sleepy battery device). Unknown counts as mains, matching
    /// `device_type()`.
    #[must_use]
    pub fn is_mains_powered(&self) -> bool {
        self.device_type() == "Router"
    }

    /// First endpoint implementing `cluster_id` as a server (input cluster).
    #[must_use]
    pub fn endpoint_with_input_cluster(&self, cluster_id: u16) -> Option<u8> {
        self.endpoints
            .iter()
            .find(|e| e.input_clusters.contains(&cluster_id))
            .map(|e| e.endpoint)
    }

    pub fn display_name(&self) -> &str {
        &self.friendly_name
    }

    /// Determine device type based on capabilities.
    #[must_use]
    pub fn device_type(&self) -> &'static str {
        // Simple heuristic: devices with only battery power or no routing are EndDevice
        if let Some(ref ps) = self.power_source {
            if ps.eq_ignore_ascii_case("battery") {
                return "EndDevice";
            }
        }
        "Router"
    }

    /// All unique output clusters across all endpoints.
    #[must_use]
    pub fn all_output_clusters(&self) -> Vec<u16> {
        let mut clusters: Vec<u16> = self
            .endpoints
            .iter()
            .flat_map(|ep| ep.output_clusters.iter().copied())
            .collect();
        clusters.sort_unstable();
        clusters.dedup();
        clusters
    }

    /// All unique input clusters across all endpoints.
    #[must_use]
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
    #[must_use]
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

fn merge_json(
    target: &mut serde_json::Map<String, serde_json::Value>,
    values: serde_json::Map<String, serde_json::Value>,
) {
    for (k, v) in values {
        match (target.get_mut(&k), v) {
            (Some(serde_json::Value::Object(existing)), serde_json::Value::Object(new)) => {
                merge_json(existing, new);
            }
            (_, v) => {
                target.insert(k, v);
            }
        }
    }
}

// ── Registry ──────────────────────────────────────────────────────────────────
//
// Three maps, kept consistent by the methods below. None of them ever holds a
// guard on one map while locking another, so no lock-order deadlock is
// possible between callers on different tasks/threads (e.g. the persistence
// saver on the blocking pool). Callers must likewise never hold a returned
// `Ref`/`RefMut` across an `.await` or while calling back into the registry.

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

    /// Insert or replace a device. NWK 0x0000 is the coordinator's own
    /// address, used here as "not yet seen on the network" (config stubs),
    /// so it's never indexed -- otherwise stubs would clobber each other.
    pub fn add(&self, device: Device) {
        let ieee = device.ieee_addr;
        let nwk = device.nwk_addr;
        let name = device.friendly_name.clone();
        if let Some(old) = self.by_ieee.insert(ieee, device) {
            self.unindex(&old);
        }
        self.index_nwk(nwk, ieee);
        self.by_name.insert(name, ieee);
    }

    /// Point `nwk` at `ieee`. NWK addresses are unique within the network,
    /// so if another device still claims `nwk`, its address is stale (it
    /// rejoined elsewhere without us noticing): forget it rather than keep
    /// sending that device's commands to whoever has the address now. It is
    /// re-learned from its next frame or announcement.
    fn index_nwk(&self, nwk: NwkAddr, ieee: IeeeAddr) {
        if nwk == 0 {
            return;
        }
        let displaced = self.by_nwk.insert(nwk, ieee);
        if let Some(other) = displaced.filter(|other| *other != ieee) {
            if let Some(mut dev) = self.by_ieee.get_mut(&other) {
                if dev.nwk_addr == nwk {
                    tracing::info!(
                        "NWK 0x{nwk:04X} now belongs to {ieee}; {} must have moved, forgetting its stale address",
                        dev.friendly_name
                    );
                    dev.nwk_addr = 0;
                }
            }
        }
    }

    /// Drop `dev`'s index entries, but only those still pointing at it -- a
    /// NWK address may since have been reassigned to another device.
    fn unindex(&self, dev: &Device) {
        self.by_nwk.remove_if(&dev.nwk_addr, |_, v| *v == dev.ieee_addr);
        self.by_name.remove_if(&dev.friendly_name, |_, v| *v == dev.ieee_addr);
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
        let ieee = self.ieee_for_nwk(addr)?;
        self.by_ieee.get(&ieee)
    }

    /// IEEE address currently known for `addr`.
    #[must_use]
    pub fn ieee_for_nwk(&self, addr: NwkAddr) -> Option<IeeeAddr> {
        self.by_nwk.get(&addr).map(|r| *r.value())
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
        let ieee = self.ieee_for_nwk(addr)?;
        self.by_ieee.get_mut(&ieee)
    }

    #[must_use]
    pub fn find_by_name(&self, name: &str) -> Option<Device> {
        let ieee = *self.by_name.get(name)?.value();
        self.by_ieee.get(&ieee).map(|r| r.value().clone())
    }

    /// Snapshot of one device.
    #[must_use]
    pub fn snapshot(&self, ieee: &IeeeAddr) -> Option<Device> {
        self.by_ieee.get(ieee).map(|r| r.value().clone())
    }

    pub fn remove_by_ieee(&self, addr: &IeeeAddr) {
        if let Some((_, dev)) = self.by_ieee.remove(addr) {
            self.unindex(&dev);
        }
    }

    /// Record `ieee`'s new NWK address. Returns the previous one, or `None`
    /// if the device is unknown.
    pub fn update_nwk_addr(&self, ieee: &IeeeAddr, new_nwk: NwkAddr) -> Option<NwkAddr> {
        let old_nwk = {
            let mut dev = self.by_ieee.get_mut(ieee)?;
            std::mem::replace(&mut dev.nwk_addr, new_nwk)
        };
        if old_nwk != new_nwk {
            self.by_nwk.remove_if(&old_nwk, |_, v| v == ieee);
        }
        self.index_nwk(new_nwk, *ieee);
        Some(old_nwk)
    }

    /// Rename a device, keeping the `by_name` index in sync. Renaming a device
    /// by mutating `Device::friendly_name` directly would leave a stale/missing
    /// `by_name` entry, breaking MQTT set/get lookups for the new name.
    pub fn rename(&self, ieee: &IeeeAddr, new_name: String) {
        let old_name = {
            let Some(mut dev) = self.by_ieee.get_mut(ieee) else {
                return;
            };
            std::mem::replace(&mut dev.friendly_name, new_name.clone())
        };
        self.by_name.remove_if(&old_name, |_, v| v == ieee);
        self.by_name.insert(new_name, *ieee);
    }

    #[must_use]
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
    fn rename_updates_by_name_index() {
        let reg = DeviceRegistry::new();
        reg.add(Device::new(test_ieee(), 0x1234));
        // Default friendly_name is the IEEE hex string.
        let old_name = test_ieee().as_hex();

        reg.rename(&test_ieee(), "kitchen_light".to_string());

        assert!(reg.find_by_name(&old_name).is_none());
        let dev = reg
            .find_by_name("kitchen_light")
            .expect("renamed device findable");
        assert_eq!(dev.friendly_name, "kitchen_light");
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
    fn config_stubs_with_nwk_zero_not_indexed() {
        let reg = DeviceRegistry::new();
        reg.add(Device::new(test_ieee(), 0));
        reg.add(Device::new(IeeeAddr([9; 8]), 0));
        assert!(reg.get_by_nwk(0).is_none());
        reg.update_nwk_addr(&test_ieee(), 0x1234);
        assert_eq!(reg.get_by_nwk(0x1234).unwrap().ieee_addr, test_ieee());
    }

    #[test]
    fn reassigned_nwk_survives_old_owner_update_and_removal() {
        let reg = DeviceRegistry::new();
        let other = IeeeAddr([9; 8]);
        reg.add(Device::new(test_ieee(), 0x1234));
        // NWK 0x1234 gets reassigned to another device...
        reg.add(Device::new(other, 0x1234));
        // ...then the original device rejoins elsewhere, and later leaves.
        reg.update_nwk_addr(&test_ieee(), 0x5678);
        reg.remove_by_ieee(&test_ieee());
        assert_eq!(reg.get_by_nwk(0x1234).unwrap().ieee_addr, other);
    }

    #[test]
    fn re_add_replaces_stale_indexes() {
        let reg = DeviceRegistry::new();
        let mut dev = Device::new(test_ieee(), 0x1234);
        dev.friendly_name = "old".into();
        reg.add(dev);
        let mut dev = Device::new(test_ieee(), 0x5678);
        dev.friendly_name = "new".into();
        reg.add(dev);
        assert!(reg.get_by_nwk(0x1234).is_none());
        assert!(reg.find_by_name("old").is_none());
        assert!(reg.find_by_name("new").is_some());
    }

    #[test]
    fn nwk_reassignment_forgets_stale_owner() {
        let reg = DeviceRegistry::new();
        let other = IeeeAddr([9; 8]);
        reg.add(Device::new(test_ieee(), 0x1234));
        reg.add(Device::new(other, 0x5678));
        // `other` shows up at 0x1234: test_ieee() must have moved away.
        reg.update_nwk_addr(&other, 0x1234);
        assert_eq!(reg.get_by_nwk(0x1234).unwrap().ieee_addr, other);
        assert_eq!(reg.get_by_ieee(&test_ieee()).unwrap().nwk_addr, 0);
        assert!(reg.get_by_nwk(0x5678).is_none());
    }

    #[test]
    fn update_nwk_addr_returns_previous() {
        let reg = DeviceRegistry::new();
        reg.add(Device::new(test_ieee(), 0x1234));
        assert_eq!(reg.update_nwk_addr(&test_ieee(), 0x1234), Some(0x1234));
        assert!(reg.get_by_nwk(0x1234).is_some());
        assert_eq!(reg.update_nwk_addr(&IeeeAddr([7; 8]), 0x1), None);
    }

    #[test]
    fn merge_state_merges_nested_objects() {
        let mut dev = Device::new(test_ieee(), 0x1234);
        let mut a = serde_json::Map::new();
        a.insert("color".into(), json!({"x": 0.3, "y": 0.4}));
        dev.merge_state(a);
        let mut b = serde_json::Map::new();
        b.insert("color".into(), json!({"y": 0.5}));
        b.insert("state".into(), json!("ON"));
        dev.merge_state(b);
        assert_eq!(dev.state["color"], json!({"x": 0.3, "y": 0.5}));
        // Scalars replace objects and vice versa.
        let mut c = serde_json::Map::new();
        c.insert("color".into(), json!(null));
        dev.merge_state(c);
        assert_eq!(dev.state["color"], json!(null));
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
