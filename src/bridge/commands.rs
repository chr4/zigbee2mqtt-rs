//! Translate MQTT `set`/`get` payloads into ZCL commands and attribute reads.
//!
//! Pure functions over a device snapshot, so the z2m payload semantics are
//! unit-testable without a coordinator.
use serde_json::{json, Map, Value};

use crate::devices::Device;
use crate::zigbee::zcl::clusters::{color, level, on_off};

pub const CLUSTER_ON_OFF: u16 = 0x0006;
pub const CLUSTER_LEVEL: u16 = 0x0008;
pub const CLUSTER_COLOR: u16 = 0x0300;

/// Highest valid CurrentLevel (255 is reserved).
const MAX_LEVEL: u8 = 254;
/// Highest valid ColorTemperatureMireds.
const MAX_COLOR_VALUE: u16 = 0xFEFF;
/// 0xFFFF means "use the device default" for transition times.
const MAX_TRANSITION: u16 = 0xFFFE;

/// One ZCL command to send for a `set` request.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedCommand {
    pub endpoint: u8,
    pub cluster: u16,
    pub command: Command,
    /// State to publish once the device acknowledged the command.
    pub optimistic: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    OnOff(OnOffCommand),
    /// Move to Level *with On/Off*: level 0 turns the light off, anything
    /// else turns it on.
    MoveToLevel {
        level: u8,
        transition: u16,
    },
    MoveToColorTemp {
        mireds: u16,
        transition: u16,
    },
    MoveToColorXy {
        x: f64,
        y: f64,
        transition: u16,
    },
    MoveToHueSat {
        hue: u8,
        saturation: u8,
        transition: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnOffCommand {
    Off,
    On,
    Toggle,
}

impl Command {
    /// The ZCL frame for this command with transaction sequence `seq`.
    pub fn payload(&self, seq: u8) -> Vec<u8> {
        match *self {
            Command::OnOff(c) => on_off::command_payload(
                seq,
                match c {
                    OnOffCommand::Off => 0x00,
                    OnOffCommand::On => 0x01,
                    OnOffCommand::Toggle => 0x02,
                },
            ),
            Command::MoveToLevel { level, transition } => {
                level::move_to_level_payload(seq, level, transition)
            }
            Command::MoveToColorTemp { mireds, transition } => {
                color::move_to_color_temp_payload(seq, mireds, transition)
            }
            Command::MoveToColorXy { x, y, transition } => {
                color::move_to_color_xy_payload(seq, x, y, transition)
            }
            Command::MoveToHueSat {
                hue,
                saturation,
                transition,
            } => color::move_to_hue_sat_payload(seq, hue, saturation, transition),
        }
    }

    /// Short description for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Command::OnOff(_) => "on/off",
            Command::MoveToLevel { .. } => "brightness",
            Command::MoveToColorTemp { .. } => "color temperature",
            Command::MoveToColorXy { .. } => "color (xy)",
            Command::MoveToHueSat { .. } => "color (hue/saturation)",
        }
    }
}

/// A JSON number, or a string holding one (`"brightness": "128"`).
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|f| f.is_finite())
}

/// `transition` in seconds -> ZCL tenths of a second.
fn transition_time(payload: &Map<String, Value>) -> u16 {
    payload
        .get("transition")
        .and_then(number)
        .map(|s| (s.max(0.0) * 10.0).round().min(f64::from(MAX_TRANSITION)) as u16)
        .unwrap_or(0)
}

fn on_off_command(v: &Value) -> Option<OnOffCommand> {
    match v.as_str()?.trim().to_ascii_uppercase().as_str() {
        "ON" => Some(OnOffCommand::On),
        "OFF" => Some(OnOffCommand::Off),
        "TOGGLE" => Some(OnOffCommand::Toggle),
        _ => None,
    }
}

/// Requested brightness (0-254) from `brightness` or `brightness_percent`.
fn requested_level(payload: &Map<String, Value>) -> Option<u8> {
    if let Some(b) = payload.get("brightness").and_then(number) {
        return Some(b.round().clamp(0.0, f64::from(MAX_LEVEL)) as u8);
    }
    payload
        .get("brightness_percent")
        .and_then(number)
        .map(|p| (p.clamp(0.0, 100.0) * f64::from(MAX_LEVEL) / 100.0).round() as u8)
}

/// Plan the ZCL commands for a z2m `set` payload. Only keys the device has a
/// matching input cluster for produce commands; unknown keys are ignored.
///
/// Mirrors zigbee2mqtt's light semantics: a brightness with state ON (or no
/// state) is a single Move-to-Level-with-On/Off (no separate On, which would
/// first flash the old level); OFF wins over a brightness; TOGGLE ignores
/// brightness.
pub fn plan_set(dev: &Device, payload: &Value) -> Vec<PlannedCommand> {
    let Some(payload) = payload.as_object() else {
        return Vec::new();
    };
    let transition = transition_time(payload);
    let mut out = Vec::new();

    let state_cmd = payload.get("state").and_then(on_off_command);
    let level = requested_level(payload);
    let level_ep = dev.endpoint_with_input_cluster(CLUSTER_LEVEL);
    let on_off_ep = dev.endpoint_with_input_cluster(CLUSTER_ON_OFF);

    let use_level = matches!(state_cmd, None | Some(OnOffCommand::On));
    match (level, level_ep) {
        (Some(level), Some(ep)) if use_level => {
            let mut optimistic = Map::new();
            optimistic.insert("brightness".into(), json!(level));
            optimistic.insert("state".into(), json!(if level == 0 { "OFF" } else { "ON" }));
            out.push(PlannedCommand {
                endpoint: ep,
                cluster: CLUSTER_LEVEL,
                command: Command::MoveToLevel { level, transition },
                optimistic,
            });
        }
        _ => {
            if let (Some(cmd), Some(ep)) = (state_cmd, on_off_ep) {
                let mut optimistic = Map::new();
                // TOGGLE's new state is worked out by the main loop when it
                // was delivered (from the then-current state): this snapshot
                // may predate earlier, still unmerged commands.
                let new_state = match cmd {
                    OnOffCommand::On => Some("ON"),
                    OnOffCommand::Off => Some("OFF"),
                    OnOffCommand::Toggle => None,
                };
                if let Some(s) = new_state {
                    optimistic.insert("state".into(), json!(s));
                }
                out.push(PlannedCommand {
                    endpoint: ep,
                    cluster: CLUSTER_ON_OFF,
                    command: Command::OnOff(cmd),
                    optimistic,
                });
            }
        }
    }

    let color_ep = dev.endpoint_with_input_cluster(CLUSTER_COLOR);
    if let (Some(mireds), Some(ep)) = (payload.get("color_temp").and_then(number), color_ep) {
        let mireds = mireds.round().clamp(1.0, f64::from(MAX_COLOR_VALUE)) as u16;
        let mut optimistic = Map::new();
        optimistic.insert("color_temp".into(), json!(mireds));
        optimistic.insert("color_mode".into(), json!("color_temp"));
        out.push(PlannedCommand {
            endpoint: ep,
            cluster: CLUSTER_COLOR,
            command: Command::MoveToColorTemp { mireds, transition },
            optimistic,
        });
    }

    if let (Some(c), Some(ep)) = (payload.get("color").and_then(Value::as_object), color_ep) {
        let get = |k: &str| c.get(k).and_then(number);
        if let (Some(x), Some(y)) = (get("x"), get("y")) {
            let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
            let mut optimistic = Map::new();
            optimistic.insert("color".into(), json!({"x": x, "y": y}));
            optimistic.insert("color_mode".into(), json!("xy"));
            out.push(PlannedCommand {
                endpoint: ep,
                cluster: CLUSTER_COLOR,
                command: Command::MoveToColorXy { x, y, transition },
                optimistic,
            });
        } else if let (Some(h), Some(s)) = (
            get("hue").or_else(|| get("h")),
            get("saturation").or_else(|| get("s")),
        ) {
            let h = h.rem_euclid(360.0);
            let s = s.clamp(0.0, 100.0);
            let mut optimistic = Map::new();
            optimistic.insert(
                "color".into(),
                json!({"hue": h, "saturation": s, "h": h, "s": s}),
            );
            optimistic.insert("color_mode".into(), json!("hs"));
            out.push(PlannedCommand {
                endpoint: ep,
                cluster: CLUSTER_COLOR,
                command: Command::MoveToHueSat {
                    hue: ((h / 360.0) * 254.0).round() as u8,
                    saturation: ((s / 100.0) * 254.0).round() as u8,
                    transition,
                },
                optimistic,
            });
        }
    }

    out
}

/// Attribute reads (endpoint, cluster, attribute ids) that refresh the
/// requested state keys -- all light state when `keys` is `None` (e.g. after
/// a power cycle, zigbee2mqtt reads state/brightness/color/color_temp).
pub fn plan_reads(dev: &Device, keys: Option<&[String]>) -> Vec<(u8, u16, Vec<u16>)> {
    let wants = |k: &str| keys.is_none_or(|keys| keys.iter().any(|x| x == k));
    let mut reads: Vec<(u8, u16, Vec<u16>)> = Vec::new();
    let mut add = |cluster: u16, attrs: &[u16]| {
        let Some(ep) = dev.endpoint_with_input_cluster(cluster) else {
            return;
        };
        match reads.iter_mut().find(|(e, c, _)| *e == ep && *c == cluster) {
            Some((_, _, existing)) => {
                for a in attrs {
                    if !existing.contains(a) {
                        existing.push(*a);
                    }
                }
            }
            None => reads.push((ep, cluster, attrs.to_vec())),
        }
    };
    if wants("state") {
        add(CLUSTER_ON_OFF, &[0x0000]);
    }
    if wants("brightness") {
        add(CLUSTER_LEVEL, &[0x0000]);
    }
    if wants("color_temp") {
        add(CLUSTER_COLOR, &[0x0007, 0x0008]);
    }
    if wants("color") {
        add(CLUSTER_COLOR, &[0x0003, 0x0004, 0x0000, 0x0001, 0x0008]);
    }
    if wants("color_mode") {
        add(CLUSTER_COLOR, &[0x0008]);
    }
    reads
}

/// State keys named by a z2m `get` payload (`{"state": ""}`); `None` (read
/// everything) for an empty or non-object payload.
pub fn get_keys(payload: &Value) -> Option<Vec<String>> {
    let keys: Vec<String> = payload.as_object()?.keys().cloned().collect();
    (!keys.is_empty()).then_some(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::{EndpointDesc, IeeeAddr};

    fn light(clusters: &[u16]) -> Device {
        let mut d = Device::new(IeeeAddr([1; 8]), 0x1234);
        d.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0x0100,
            input_clusters: clusters.to_vec(),
            output_clusters: vec![],
        });
        d
    }

    fn color_light() -> Device {
        light(&[0x0000, 0x0006, 0x0008, 0x0300])
    }

    fn commands(plan: &[PlannedCommand]) -> Vec<Command> {
        plan.iter().map(|p| p.command).collect()
    }

    #[test]
    fn on_with_brightness_is_single_move_to_level_with_on_off() {
        let plan = plan_set(&color_light(), &json!({"state": "ON", "brightness": 128}));
        assert_eq!(
            commands(&plan),
            vec![Command::MoveToLevel {
                level: 128,
                transition: 0
            }]
        );
        assert_eq!(plan[0].optimistic["state"], "ON");
        assert_eq!(plan[0].optimistic["brightness"], 128);
    }

    #[test]
    fn off_wins_over_brightness() {
        let plan = plan_set(&color_light(), &json!({"state": "OFF", "brightness": 128}));
        assert_eq!(commands(&plan), vec![Command::OnOff(OnOffCommand::Off)]);
        assert_eq!(plan[0].optimistic["state"], "OFF");
    }

    #[test]
    fn brightness_zero_turns_off() {
        let plan = plan_set(&color_light(), &json!({"brightness": 0}));
        assert_eq!(plan[0].optimistic["state"], "OFF");
    }

    #[test]
    fn brightness_accepts_floats_strings_and_percent() {
        let plan = plan_set(&color_light(), &json!({"brightness": 127.6}));
        assert_eq!(
            commands(&plan),
            vec![Command::MoveToLevel {
                level: 128,
                transition: 0
            }]
        );
        let plan = plan_set(&color_light(), &json!({"brightness": "300"}));
        assert_eq!(
            commands(&plan),
            vec![Command::MoveToLevel {
                level: 254,
                transition: 0
            }]
        );
        let plan = plan_set(&color_light(), &json!({"brightness_percent": 50}));
        assert_eq!(
            commands(&plan),
            vec![Command::MoveToLevel {
                level: 127,
                transition: 0
            }]
        );
    }

    #[test]
    fn plain_on_off_light_ignores_brightness_but_switches() {
        let plan = plan_set(&light(&[0x0006]), &json!({"state": "on", "brightness": 10}));
        assert_eq!(commands(&plan), vec![Command::OnOff(OnOffCommand::On)]);
    }

    #[test]
    fn toggle_ignores_brightness_and_leaves_state_to_main_loop() {
        let mut dev = color_light();
        dev.state.insert("state".into(), json!("ON"));
        let plan = plan_set(&dev, &json!({"state": "toggle", "brightness": 5}));
        assert_eq!(commands(&plan), vec![Command::OnOff(OnOffCommand::Toggle)]);
        assert!(plan[0].optimistic.is_empty());
    }

    #[test]
    fn color_temp_and_xy_with_transition() {
        let plan = plan_set(
            &color_light(),
            &json!({"color_temp": 370, "color": {"x": 0.3, "y": 0.4}, "transition": 1.5}),
        );
        assert_eq!(
            commands(&plan),
            vec![
                Command::MoveToColorTemp {
                    mireds: 370,
                    transition: 15
                },
                Command::MoveToColorXy {
                    x: 0.3,
                    y: 0.4,
                    transition: 15
                },
            ]
        );
        assert_eq!(plan[1].optimistic["color"], json!({"x": 0.3, "y": 0.4}));
        assert_eq!(plan[1].optimistic["color_mode"], "xy");
    }

    #[test]
    fn hue_saturation_short_and_long_keys() {
        for c in [
            json!({"hue": 180, "saturation": 100}),
            json!({"h": 180, "s": 100}),
        ] {
            let plan = plan_set(&color_light(), &json!({ "color": c }));
            assert_eq!(
                commands(&plan),
                vec![Command::MoveToHueSat {
                    hue: 127,
                    saturation: 254,
                    transition: 0
                }]
            );
        }
    }

    #[test]
    fn nothing_for_unsupported_keys_or_payloads() {
        assert!(plan_set(&light(&[0x0402]), &json!({"state": "ON"})).is_empty());
        assert!(plan_set(&color_light(), &json!("ON")).is_empty());
        assert!(plan_set(&color_light(), &json!({"state": "blink"})).is_empty());
    }

    #[test]
    fn reads_cover_light_state_merged_per_cluster() {
        let reads = plan_reads(&color_light(), None);
        assert_eq!(
            reads,
            vec![
                (1, 0x0006, vec![0x0000]),
                (1, 0x0008, vec![0x0000]),
                (
                    1,
                    0x0300,
                    vec![0x0007, 0x0008, 0x0003, 0x0004, 0x0000, 0x0001]
                ),
            ]
        );
        let keys = vec!["state".to_string()];
        assert_eq!(
            plan_reads(&color_light(), Some(&keys)),
            vec![(1, 0x0006, vec![0x0000])]
        );
        assert!(plan_reads(&light(&[0x0402]), None).is_empty());
    }

    #[test]
    fn get_keys_from_payload() {
        assert_eq!(
            get_keys(&json!({"state": ""})),
            Some(vec!["state".to_string()])
        );
        assert_eq!(get_keys(&json!({})), None);
        assert_eq!(get_keys(&Value::Null), None);
    }
}
