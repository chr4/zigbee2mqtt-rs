/// Persist the device registry across restarts, in zigbee2mqtt's own formats:
///
///   - `database.db` -- zigbee-herdsman NDJSON device records (the same file
///     `database::load_database` imports), so devices paired or interviewed by
///     this bridge survive a restart and the file stays readable by z2m.
///   - `state.json`  -- `{ "<ieee>": { ...last device state } }`, like z2m's.
///
/// The existing database.db is merged into rather than regenerated: records
/// and fields this bridge doesn't model (Coordinator, groups, `clusters`,
/// `binds`, `meta`, ...) are kept as-is. Records of devices that left the
/// network are removed, as z2m does; records we couldn't import are kept.
///
/// Writes are driven by a periodic snapshot (`spawn_saver`) instead of hooks
/// on every registry mutation; a file is only rewritten when its rendered
/// content changed, via temp file + rename so a crash mid-write can't leave
/// a truncated database behind. database.db only changes when devices join,
/// leave or get interviewed, so it is checked often; state.json changes with
/// every message (`last_seen`), so it is written far less often to spare the
/// SD card -- and always on exit.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};

use crate::database;
use crate::devices::{Device, DeviceRegistry};
use crate::zigbee::IeeeAddr;

/// How often database.db is checked for changes.
const SAVE_INTERVAL: Duration = Duration::from_secs(30);
/// How often state.json is rewritten (if changed). zigbee2mqtt is recalled
/// to save its state cache every 5 minutes as well (unverified).
const STATE_SAVE_INTERVAL: Duration = Duration::from_secs(300);

pub struct Store {
    db_path: PathBuf,
    state_path: PathBuf,
    /// Last content written (or read at startup) per file, to skip no-op writes.
    last_written: Mutex<HashMap<PathBuf, String>>,
    /// IEEE addresses (as hex) of devices that left the network: the only
    /// records dropped from database.db.
    removed: Mutex<HashSet<String>>,
}

impl Store {
    /// Choose where to persist, based on the config file location.
    ///
    /// An existing database.db next to the config (or in its `data/` dir) is
    /// written back in place. One only found in a system-wide z2m install
    /// location is imported but never modified -- that install may still be
    /// in use -- and a fresh database.db is created next to the config
    /// instead, which `find_database` will then pick up on the next start.
    pub fn new(config_path: &Path) -> Self {
        let dir = config_path.parent().unwrap_or(Path::new("."));
        let local = [dir.join("database.db"), dir.join("data/database.db")];
        let db_path = database::find_database(config_path)
            .filter(|p| local.contains(p))
            .unwrap_or_else(|| local[0].clone());
        let state_path = db_path.with_file_name("state.json");
        Self {
            db_path,
            state_path,
            last_written: Mutex::new(HashMap::new()),
            removed: Mutex::new(HashSet::new()),
        }
    }

    /// Drop `ieee`'s database.db record on the next save (it left).
    pub fn forget_device(&self, ieee: &IeeeAddr) {
        self.removed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ieee.as_hex());
    }

    /// Restore device state from state.json into already-registered devices.
    pub fn load_state(&self, devices: &DeviceRegistry) {
        let content = match std::fs::read_to_string(&self.state_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!("Cannot read {}: {e}", self.state_path.display());
                return;
            }
        };
        let saved: Map<String, Value> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                warn!("Ignoring unparseable {}: {e}", self.state_path.display());
                return;
            }
        };
        let mut restored = 0;
        for (key, value) in saved {
            let (Some(ieee), Value::Object(state)) = (IeeeAddr::from_hex(&key), value) else {
                continue;
            };
            if let Some(mut dev) = devices.get_mut_by_ieee(&ieee) {
                dev.merge_state(state);
                strip_transient(&mut dev.state);
                restored += 1;
            }
        }
        info!(
            "Restored state for {restored} devices from {}",
            self.state_path.display()
        );
    }

    /// Snapshot the registry and write both files if their content changed.
    pub fn save(&self, devices: &DeviceRegistry) {
        let snapshot = snapshot(devices);
        self.save_database(&snapshot);
        self.save_state(&snapshot);
    }

    fn save_database(&self, snapshot: &[Device]) {
        let existing = match std::fs::read_to_string(&self.db_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                // Don't risk overwriting a database we failed to read.
                warn!(
                    "Cannot read {}, not saving devices: {e}",
                    self.db_path.display()
                );
                return;
            }
        };
        let removed = self.removed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.write_if_changed(&self.db_path, render_database(&existing, snapshot, &removed));
    }

    fn save_state(&self, snapshot: &[Device]) {
        self.write_if_changed(&self.state_path, render_state(snapshot));
    }

    /// Periodically save in the background. File I/O (including fsync,
    /// which can take a while on an SD card) runs on the blocking pool so it
    /// never stalls the bridge's event loop.
    pub fn spawn_saver(
        self: &Arc<Self>,
        devices: Arc<DeviceRegistry>,
    ) -> tokio::task::JoinHandle<()> {
        let store = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SAVE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // first tick fires immediately; nothing new to save yet
            let mut last_state_save = tokio::time::Instant::now();
            loop {
                tick.tick().await;
                let with_state = last_state_save.elapsed() >= STATE_SAVE_INTERVAL;
                if with_state {
                    last_state_save = tokio::time::Instant::now();
                }
                let (store, devices) = (Arc::clone(&store), Arc::clone(&devices));
                let saved = tokio::task::spawn_blocking(move || {
                    let snapshot = snapshot(&devices);
                    store.save_database(&snapshot);
                    if with_state {
                        store.save_state(&snapshot);
                    }
                })
                .await;
                if let Err(e) = saved {
                    warn!("Device save task failed: {e}");
                }
            }
        })
    }

    fn write_if_changed(&self, path: &Path, content: String) {
        let mut last = self.last_written.lock().unwrap_or_else(|e| e.into_inner());
        if last.get(path) == Some(&content) {
            return;
        }
        // Also compare against disk, so the first save after startup doesn't
        // rewrite an unchanged file.
        if !last.contains_key(path) && std::fs::read_to_string(path).ok().as_ref() == Some(&content)
        {
            last.insert(path.to_path_buf(), content);
            return;
        }
        match write_atomic(path, &content) {
            Ok(()) => {
                debug!("Saved {}", path.display());
                last.insert(path.to_path_buf(), content);
            }
            Err(e) => warn!("Failed to write {}: {e}", path.display()),
        }
    }
}

fn snapshot(devices: &DeviceRegistry) -> Vec<Device> {
    let mut snapshot = devices.all_devices();
    snapshot.sort_by_key(|d| d.ieee_addr.as_hex());
    snapshot
}

fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Persist the rename itself, or a power cut can bring back the old file
    // (or none). Best effort: not every filesystem supports syncing a dir.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// `action`/`action_*` are momentary events, never persisted state.
fn strip_transient(state: &mut Map<String, Value>) {
    state.retain(|k, _| k != "action" && !k.starts_with("action_"));
}

/// Devices worth a database record: seen on the network or interviewed.
/// Pre-seeded configuration.yaml stubs that never joined are left out --
/// z2m wouldn't have a record for them either.
fn is_persistable(dev: &Device) -> bool {
    dev.nwk_addr != 0 || dev.interview_complete || !dev.endpoints.is_empty()
}

fn render_state(devices: &[Device]) -> String {
    let mut out = Map::new();
    for dev in devices.iter().filter(|d| is_persistable(d)) {
        let mut state = dev.state.clone();
        strip_transient(&mut state);
        if !state.is_empty() {
            out.insert(dev.ieee_addr.as_hex(), Value::Object(state));
        }
    }
    serde_json::to_string_pretty(&Value::Object(out)).unwrap_or_default()
}

/// Merge the registry into existing database.db content and render it back.
fn render_database(existing: &str, devices: &[Device], removed: &HashSet<String>) -> String {
    let by_ieee: HashMap<String, &Device> = devices
        .iter()
        .filter(|d| is_persistable(d))
        .map(|d| (d.ieee_addr.as_hex(), d))
        .collect();
    let mut written: HashSet<String> = HashSet::new();
    let mut max_id = 0u64;
    let mut lines: Vec<String> = Vec::new();

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(mut record)) = serde_json::from_str::<Value>(trimmed) else {
            // Not ours to judge -- keep lines we can't parse verbatim.
            lines.push(trimmed.to_string());
            continue;
        };
        max_id = max_id.max(record.get("id").and_then(Value::as_u64).unwrap_or(0));

        let is_device = record.get("type").and_then(Value::as_str) != Some("Coordinator");
        let ieee = record
            .get("ieeeAddr")
            .and_then(Value::as_str)
            .and_then(IeeeAddr::from_hex)
            .map(|a| a.as_hex());
        if let (true, Some(ieee)) = (is_device, ieee) {
            match by_ieee.get(&ieee) {
                // Device left the network.
                None if removed.contains(&ieee) => continue,
                // Not in the registry for another reason (e.g. a record we
                // couldn't import): not ours to delete.
                None => {}
                // Duplicate record for one device: keep only the first.
                Some(_) if written.contains(&ieee) => continue,
                Some(dev) => {
                    update_record(&mut record, dev);
                    written.insert(ieee);
                }
            }
        }
        lines.push(Value::Object(record).to_string());
    }

    for dev in devices {
        let ieee = dev.ieee_addr.as_hex();
        if !by_ieee.contains_key(&ieee) || written.contains(&ieee) {
            continue;
        }
        max_id += 1;
        let mut record = Map::new();
        record.insert("id".into(), json!(max_id));
        record.insert("type".into(), json!(dev.device_type()));
        record.insert("ieeeAddr".into(), json!(ieee));
        record.insert("meta".into(), json!({}));
        update_record(&mut record, dev);
        lines.push(Value::Object(record).to_string());
    }

    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Overwrite the fields this bridge models, keeping everything else.
fn update_record(record: &mut Map<String, Value>, dev: &Device) {
    // 0 means "address currently unknown" (stale one dropped); keep the last
    // real one rather than point zigbee2mqtt at the coordinator's address.
    if dev.nwk_addr != 0 || !record.contains_key("nwkAddr") {
        record.insert("nwkAddr".into(), json!(dev.nwk_addr));
    }
    let optional = [
        ("manufName", &dev.manufacturer),
        ("powerSource", &dev.power_source),
        ("modelId", &dev.model),
        ("swBuildId", &dev.sw_build_id),
    ];
    for (key, value) in optional {
        if let Some(v) = value {
            record.insert(key.into(), json!(v));
        }
    }
    record.insert("interviewCompleted".into(), json!(dev.interview_complete));
    let meta = record.entry("meta").or_insert_with(|| json!({}));
    if let Value::Object(meta) = meta {
        if dev.configured {
            // zigbee2mqtt's own `configured` marker already says so.
            if !meta.contains_key("configured") {
                meta.insert(database::META_CONFIGURED.into(), json!(true));
            }
        } else {
            // Needs configuring again (it re-joined): like zigbee2mqtt, drop
            // the marker so neither of us skips it after a restart.
            meta.remove("configured");
            meta.remove(database::META_CONFIGURED);
        }
    }

    if !dev.endpoints.is_empty() {
        let mut eps: Vec<_> = dev.endpoints.iter().collect();
        eps.sort_by_key(|e| e.endpoint);
        record.insert(
            "epList".into(),
            json!(eps.iter().map(|e| e.endpoint).collect::<Vec<_>>()),
        );

        let mut old = match record.remove("endpoints") {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let mut endpoints = Map::new();
        for ep in eps {
            let key = ep.endpoint.to_string();
            let mut obj = match old.remove(&key) {
                Some(Value::Object(m)) => m,
                _ => {
                    // Fresh endpoint: the empty collections herdsman expects.
                    let mut m = Map::new();
                    m.insert("clusters".into(), json!({}));
                    m.insert("binds".into(), json!([]));
                    m.insert("configuredReportings".into(), json!([]));
                    m.insert("meta".into(), json!({}));
                    m
                }
            };
            obj.insert("profId".into(), json!(ep.profile_id));
            obj.insert("epId".into(), json!(ep.endpoint));
            obj.insert("devId".into(), json!(ep.device_id));
            obj.insert("inClusterList".into(), json!(ep.input_clusters));
            obj.insert("outClusterList".into(), json!(ep.output_clusters));
            if let Some(zone_type) = dev.zone_type.filter(|_| ep.input_clusters.contains(&0x0500)) {
                set_cached_attribute(
                    &mut obj,
                    database::IAS_ZONE_CLUSTER,
                    database::ZONE_TYPE_ATTR,
                    json!(zone_type),
                );
            }
            endpoints.insert(key, Value::Object(obj));
        }
        record.insert("endpoints".into(), Value::Object(endpoints));
    } else if !record.contains_key("endpoints") {
        record.insert("epList".into(), json!([]));
        record.insert("endpoints".into(), json!({}));
    }
}

/// Set `clusters.<cluster>.attributes.<attr>` in a herdsman endpoint record,
/// keeping everything else there.
fn set_cached_attribute(endpoint: &mut Map<String, Value>, cluster: &str, attr: &str, value: Value) {
    fn object_at<'a>(parent: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
        let slot = parent.entry(key).or_insert_with(|| json!({}));
        if !slot.is_object() {
            *slot = json!({});
        }
        slot.as_object_mut().expect("just made an object")
    }
    let clusters = object_at(endpoint, "clusters");
    let cluster = object_at(clusters, cluster);
    let attributes = object_at(cluster, "attributes");
    attributes.insert(attr.into(), value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::EndpointDesc;

    const DB: &str = r#"{"id":1,"type":"Coordinator","ieeeAddr":"0x00124b00120144ae","nwkAddr":0,"epList":[1],"endpoints":{},"interviewCompleted":true,"meta":{}}
{"id":2,"type":"Router","ieeeAddr":"0xec1bbdfffeaa66db","nwkAddr":44718,"manufName":"IKEA of Sweden","modelId":"TRADFRI","epList":[1],"endpoints":{"1":{"profId":260,"epId":1,"devId":512,"inClusterList":[0,6],"outClusterList":[],"clusters":{"genBasic":{"attributes":{"x":1}}},"binds":[],"configuredReportings":[],"meta":{}}},"interviewCompleted":true,"meta":{"keep":true},"lastSeen":1710612345000}
{"id":7,"type":"EndDevice","ieeeAddr":"0xcc86ecfffe9fd1b1","nwkAddr":17895,"endpoints":{},"interviewCompleted":true,"meta":{}}
{"id":9,"groupID":5,"members":[]}"#;

    /// 0xcc86... left the network.
    fn gone() -> HashSet<String> {
        HashSet::from(["0xcc86ecfffe9fd1b1".to_string()])
    }

    #[test]
    fn records_not_in_registry_are_kept_unless_removed() {
        let out = render_database(DB, &[bulb()], &HashSet::new());
        assert_eq!(records(&out).len(), 4);
    }

    fn ieee(s: &str) -> IeeeAddr {
        IeeeAddr::from_hex(s).unwrap()
    }

    fn records(s: &str) -> Vec<Map<String, Value>> {
        s.lines()
            .map(|l| {
                serde_json::from_str::<Value>(l)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .clone()
            })
            .collect()
    }

    fn bulb() -> Device {
        let mut d = Device::new(ieee("0xec1bbdfffeaa66db"), 0x1111);
        d.manufacturer = Some("IKEA of Sweden".into());
        d.interview_complete = true;
        d.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 260,
            device_id: 512,
            input_clusters: vec![0, 6, 8],
            output_clusters: vec![],
        });
        d
    }

    #[test]
    fn merge_keeps_unknown_fields_and_records() {
        let out = render_database(DB, &[bulb()], &gone());
        let recs = records(&out);
        // Coordinator and group kept; 0xcc86... (not in registry) removed.
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0]["type"], "Coordinator");
        assert_eq!(recs[2]["groupID"], 5);

        let b = &recs[1];
        assert_eq!(b["nwkAddr"], 0x1111);
        assert_eq!(b["meta"]["keep"], true);
        assert_eq!(b["lastSeen"], 1710612345000u64);
        assert_eq!(b["modelId"], "TRADFRI"); // not overwritten by None
        assert_eq!(b["endpoints"]["1"]["inClusterList"], json!([0, 6, 8]));
        assert_eq!(
            b["endpoints"]["1"]["clusters"]["genBasic"]["attributes"]["x"],
            1
        );
    }

    #[test]
    fn new_device_gets_next_id_and_full_record() {
        let mut d = Device::new(ieee("0x0011223344556677"), 0x2222);
        d.power_source = Some("Battery".into());
        d.endpoints.push(EndpointDesc {
            endpoint: 2,
            profile_id: 260,
            device_id: 1,
            input_clusters: vec![0, 1],
            output_clusters: vec![6],
        });
        let out = render_database(DB, &[bulb(), d], &gone());
        let recs = records(&out);
        let new = recs.last().unwrap();
        assert_eq!(new["id"], 10);
        assert_eq!(new["type"], "EndDevice");
        assert_eq!(new["epList"], json!([2]));
        assert_eq!(new["endpoints"]["2"]["binds"], json!([]));
        assert_eq!(new["interviewCompleted"], false);
    }

    #[test]
    fn zone_type_and_configured_round_trip() {
        let mut d = Device::new(ieee("0x0011223344556677"), 0x2222);
        d.interview_complete = true;
        d.configured = true;
        d.zone_type = Some(0x000D);
        d.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 260,
            device_id: 0x0402,
            input_clusters: vec![0, 1, 0x0500],
            output_clusters: vec![],
        });
        let out = render_database("", &[d], &HashSet::new());
        let recs = records(&out);
        assert_eq!(
            recs[0]["endpoints"]["1"]["clusters"]["ssIasZone"]["attributes"]["zoneType"],
            13
        );
        assert_eq!(recs[0]["meta"][database::META_CONFIGURED], true);

        let dir = std::env::temp_dir().join(format!("z2m_persist_zone_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("database.db");
        write_atomic(&path, &out).unwrap();
        let (devs, _) = database::load_database(&path, &HashMap::new());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(devs[0].zone_type, Some(0x000D));
        assert!(devs[0].configured);
    }

    #[test]
    fn zone_type_merges_into_existing_cluster_cache() {
        let existing = r#"{"id":3,"type":"EndDevice","ieeeAddr":"0x0011223344556677","nwkAddr":1,"endpoints":{"1":{"epId":1,"inClusterList":[1280],"clusters":{"ssIasZone":{"attributes":{"iasCieAddr":"0x00124b00120144ae"}}}}},"meta":{"configured":42}}"#;
        let mut d = Device::new(ieee("0x0011223344556677"), 1);
        d.zone_type = Some(0x0015);
        d.configured = true;
        d.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 260,
            device_id: 0x0402,
            input_clusters: vec![0x0500],
            output_clusters: vec![],
        });
        let recs = records(&render_database(existing, &[d], &HashSet::new()));
        let attrs = &recs[0]["endpoints"]["1"]["clusters"]["ssIasZone"]["attributes"];
        assert_eq!(attrs["zoneType"], 0x15);
        assert_eq!(attrs["iasCieAddr"], "0x00124b00120144ae");
        // z2m's own marker is kept and ours isn't added next to it.
        assert_eq!(recs[0]["meta"]["configured"], 42);
        assert!(recs[0]["meta"].get(database::META_CONFIGURED).is_none());
    }

    #[test]
    fn unknown_nwk_keeps_last_address_and_unconfigured_drops_marker() {
        let mut d = bulb();
        d.nwk_addr = 0;
        d.configured = false;
        let existing = r#"{"id":2,"type":"Router","ieeeAddr":"0xec1bbdfffeaa66db","nwkAddr":44718,"endpoints":{},"interviewCompleted":true,"meta":{"configured":7,"keep":1}}"#;
        let recs = records(&render_database(existing, &[d], &HashSet::new()));
        assert_eq!(recs[0]["nwkAddr"], 44718);
        assert!(recs[0]["meta"].get("configured").is_none());
        assert_eq!(recs[0]["meta"]["keep"], 1);
    }

    #[test]
    fn unseen_config_stub_not_written() {
        let stub = Device::new(ieee("0x0011223344556677"), 0);
        let out = render_database("", &[stub], &HashSet::new());
        assert!(out.is_empty());
    }

    #[test]
    fn round_trips_through_importer() {
        let dir = std::env::temp_dir().join(format!("z2m_persist_rt_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("database.db");
        write_atomic(&path, &render_database("", &[bulb()], &HashSet::new())).unwrap();
        let (devs, _) = database::load_database(&path, &HashMap::new());
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].nwk_addr, 0x1111);
        assert!(devs[0].interview_complete);
        assert_eq!(devs[0].endpoints[0].input_clusters, vec![0, 6, 8]);
    }

    #[test]
    fn state_excludes_actions_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("z2m_persist_state_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::new(&dir.join("configuration.yaml"));

        let reg = DeviceRegistry::new();
        let mut b = bulb();
        b.state.insert("state".into(), json!("ON"));
        b.state.insert("action".into(), json!("toggle"));
        b.state.insert("action_group".into(), json!(1));
        reg.add(b);
        store.save(&reg);

        let rendered = std::fs::read_to_string(dir.join("state.json")).unwrap();
        assert!(!rendered.contains("action"));

        let reg2 = DeviceRegistry::new();
        reg2.add(bulb());
        store.load_state(&reg2);
        let dev = reg2.get_by_ieee(&ieee("0xec1bbdfffeaa66db")).unwrap();
        assert_eq!(dev.state["state"], "ON");
        drop(dev);

        assert!(dir.join("database.db").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
