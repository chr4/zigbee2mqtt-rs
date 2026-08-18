use super::super::attribute::AttributeReport;
use super::ClusterHandler;
use serde_json::{json, Value};

pub struct ScenesCluster;

// Cluster 0x0005 – Scenes
//
// Standard ZCL commands (client→server):
//   0x04 – Store Scene   (payload: GroupID u16, SceneID u8)
//   0x05 – Recall Scene  (payload: GroupID u16, SceneID u8, [TransitionTime u16])
//
// IKEA TRADFRI remotes (manufacturer code 0x117C) also send manufacturer-
// specific Scenes commands for their left/right arrow buttons, mirroring
// zigbee-herdsman-converters' `ikeaArrowClick()` exactly:
//   0x07 – ArrowSingle (click): payload = value(u16 LE) + value2(u16 LE);
//          direction = "left" if value==257 else "right"
//   0x08 – ArrowHold:           payload = value(u16 LE);
//          direction = "left" if value==3329 else "right"; remembered for
//          the matching Release
//   0x09 – ArrowRelease:        payload = value(u16 LE), a duration counter
//          (value/1000 = seconds); uses the direction remembered from the
//          prior Hold -- emits nothing if there wasn't one (e.g. right after
//          a restart), matching the real converter's guard.
//
// Any other manufacturer-specific command (different mfr code, or a command
// id not listed above) isn't verified against real hardware, so it's
// surfaced as a generic, honestly-labeled `manufacturer_0xNNNN_cmd_0xNN`
// action rather than a guessed button name (zcl::parse_message logs the raw
// manufacturer code/command id/payload bytes at info level for this case).

const STORE_SCENE: u8 = 0x04;
const RECALL_SCENE: u8 = 0x05;

const IKEA_MFR_CODE: u16 = 0x117C;
const IKEA_ARROW_SINGLE: u8 = 0x07;
const IKEA_ARROW_HOLD: u8 = 0x08;
const IKEA_ARROW_RELEASE: u8 = 0x09;

impl ClusterHandler for ScenesCluster {
    fn process_reports(&self, _reports: &[AttributeReport]) -> Vec<(String, Value)> {
        // Scenes' own attributes (CurrentScene/CurrentGroup/SceneValid/...)
        // aren't meaningful bridge state for the devices this project targets
        // (remote controls, which don't report them; scene-capable lights,
        // which aren't otherwise supported here).
        vec![]
    }

    fn process_command(&self, command_id: u8, payload: &[u8]) -> Vec<(String, Value)> {
        process_command_with_mfr_code(command_id, payload, None, None).0
    }
}

/// Manufacturer-code-aware equivalent of `ClusterHandler::process_command`.
/// Scenes' manufacturer-specific commands need the manufacturer code, which
/// the generic `ClusterHandler` trait has no way to carry -- `zcl::parse_message`
/// calls this directly for cluster 0x0005 instead of going through
/// `clusters::handler_for`, the same way it special-cases IAS Zone for
/// `zone_type`.
///
/// `held_direction` is the arrow direction currently remembered for this
/// device (set by a prior ArrowHold, consumed by the matching ArrowRelease).
/// Returns `(values, direction_update)`, where `direction_update` is `None`
/// if the remembered direction shouldn't change, `Some(None)` to clear it,
/// or `Some(Some(dir))` to set/replace it -- the caller is responsible for
/// persisting this onto the device record (see `Device::arrow_hold_direction`).
pub fn process_command_with_mfr_code(
    command_id: u8,
    payload: &[u8],
    mfr_code: Option<u16>,
    held_direction: Option<&str>,
) -> (Vec<(String, Value)>, Option<Option<String>>) {
    if mfr_code == Some(IKEA_MFR_CODE) {
        match command_id {
            IKEA_ARROW_SINGLE if payload.len() >= 2 => {
                let value = u16::from_le_bytes([payload[0], payload[1]]);
                let direction = if value == 257 { "left" } else { "right" };
                return (
                    vec![("action".into(), json!(format!("arrow_{direction}_click")))],
                    None,
                );
            }
            IKEA_ARROW_HOLD if payload.len() >= 2 => {
                let value = u16::from_le_bytes([payload[0], payload[1]]);
                let direction = if value == 3329 { "left" } else { "right" };
                return (
                    vec![("action".into(), json!(format!("arrow_{direction}_hold")))],
                    Some(Some(direction.to_string())),
                );
            }
            IKEA_ARROW_RELEASE if payload.len() >= 2 => {
                let value = u16::from_le_bytes([payload[0], payload[1]]);
                return match held_direction {
                    Some(direction) => (
                        vec![
                            ("action".into(), json!(format!("arrow_{direction}_release"))),
                            ("action_duration".into(), json!(value as f64 / 1000.0)),
                        ],
                        Some(None),
                    ),
                    None => (vec![], None),
                };
            }
            _ => {}
        }
    }
    if let Some(mfr) = mfr_code {
        return (
            vec![(
                "action".into(),
                json!(format!("manufacturer_0x{mfr:04x}_cmd_0x{command_id:02x}")),
            )],
            None,
        );
    }

    let pairs = match command_id {
        STORE_SCENE if payload.len() >= 3 => {
            let group_id = u16::from_le_bytes([payload[0], payload[1]]);
            let scene_id = payload[2];
            vec![
                ("action".into(), json!("store")),
                ("action_group".into(), json!(group_id)),
                ("action_scene".into(), json!(scene_id)),
            ]
        }
        RECALL_SCENE if payload.len() >= 3 => {
            let group_id = u16::from_le_bytes([payload[0], payload[1]]);
            let scene_id = payload[2];
            vec![
                ("action".into(), json!("recall")),
                ("action_group".into(), json!(group_id)),
                ("action_scene".into(), json!(scene_id)),
            ]
        }
        _ => vec![],
    };
    (pairs, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_scene() {
        let payload = [0x01, 0x00, 0x05]; // group=1, scene=5
        let (result, update) = process_command_with_mfr_code(RECALL_SCENE, &payload, None, None);
        assert!(result
            .iter()
            .any(|(k, v)| k == "action" && v == &json!("recall")));
        assert!(result
            .iter()
            .any(|(k, v)| k == "action_group" && v == &json!(1)));
        assert!(result
            .iter()
            .any(|(k, v)| k == "action_scene" && v == &json!(5)));
        assert_eq!(update, None);
    }

    #[test]
    fn store_scene() {
        let payload = [0x02, 0x00, 0x03]; // group=2, scene=3
        let (result, _) = process_command_with_mfr_code(STORE_SCENE, &payload, None, None);
        assert!(result
            .iter()
            .any(|(k, v)| k == "action" && v == &json!("store")));
    }

    #[test]
    fn unmapped_manufacturer_specific_command_is_generic_and_honest() {
        // A manufacturer/command combo we haven't verified against real
        // hardware (different mfr code entirely here).
        let (result, update) =
            process_command_with_mfr_code(0x07, &[0x01, 0x02], Some(0x9999), None);
        assert_eq!(
            result,
            vec![("action".into(), json!("manufacturer_0x9999_cmd_0x07"))]
        );
        assert_eq!(update, None);
    }

    #[test]
    fn unknown_standard_command_is_ignored() {
        assert_eq!(
            process_command_with_mfr_code(0x99, &[], None, None),
            (vec![], None)
        );
    }

    #[test]
    fn truncated_recall_scene_ignored() {
        assert_eq!(
            process_command_with_mfr_code(RECALL_SCENE, &[0x01], None, None),
            (vec![], None)
        );
    }

    #[test]
    fn ikea_arrow_click_left() {
        // value = 257 (LE: 0x01, 0x01)
        let (result, update) = process_command_with_mfr_code(
            IKEA_ARROW_SINGLE,
            &[0x01, 0x01, 0x00, 0x00],
            Some(IKEA_MFR_CODE),
            None,
        );
        assert_eq!(result, vec![("action".into(), json!("arrow_left_click"))]);
        assert_eq!(update, None);
    }

    #[test]
    fn ikea_arrow_click_right() {
        // Any value other than 257 means "right" -- use 256 (LE: 0x00, 0x01)
        let (result, _) = process_command_with_mfr_code(
            IKEA_ARROW_SINGLE,
            &[0x00, 0x01, 0x00, 0x00],
            Some(IKEA_MFR_CODE),
            None,
        );
        assert_eq!(result, vec![("action".into(), json!("arrow_right_click"))]);
    }

    #[test]
    fn ikea_arrow_hold_left_remembers_direction() {
        // value = 3329 (LE: 0x01, 0x0D)
        let (result, update) = process_command_with_mfr_code(
            IKEA_ARROW_HOLD,
            &[0x01, 0x0D],
            Some(IKEA_MFR_CODE),
            None,
        );
        assert_eq!(result, vec![("action".into(), json!("arrow_left_hold"))]);
        assert_eq!(update, Some(Some("left".to_string())));
    }

    #[test]
    fn ikea_arrow_hold_right() {
        let (result, update) = process_command_with_mfr_code(
            IKEA_ARROW_HOLD,
            &[0x00, 0x00],
            Some(IKEA_MFR_CODE),
            None,
        );
        assert_eq!(result, vec![("action".into(), json!("arrow_right_hold"))]);
        assert_eq!(update, Some(Some("right".to_string())));
    }

    #[test]
    fn ikea_arrow_release_with_prior_hold() {
        // value = 1500 -> 1.5s (LE: 0xDC, 0x05)
        let (result, update) = process_command_with_mfr_code(
            IKEA_ARROW_RELEASE,
            &[0xDC, 0x05],
            Some(IKEA_MFR_CODE),
            Some("left"),
        );
        assert!(result
            .iter()
            .any(|(k, v)| k == "action" && v == &json!("arrow_left_release")));
        assert!(result
            .iter()
            .any(|(k, v)| k == "action_duration" && v == &json!(1.5)));
        assert_eq!(update, Some(None));
    }

    #[test]
    fn ikea_arrow_release_without_prior_hold_emits_nothing() {
        let (result, update) = process_command_with_mfr_code(
            IKEA_ARROW_RELEASE,
            &[0x00, 0x00],
            Some(IKEA_MFR_CODE),
            None,
        );
        assert_eq!(result, vec![]);
        assert_eq!(update, None);
    }
}
