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
// Remote controls (e.g. IKEA TRADFRI) also send manufacturer-specific Scenes
// commands (manufacturer code 0x117C for IKEA) for buttons that don't map to
// a standard ZCL command. The exact command-id -> button mapping isn't
// verified against real hardware here, so those are surfaced as a generic,
// honestly-labeled `manufacturer_0xNNNN_cmd_0xNN` action rather than a
// guessed button name -- refine the mapping once confirmed against a real
// device (zcl::parse_message logs the raw manufacturer code/command id/
// payload bytes at info level for exactly this purpose).

const STORE_SCENE: u8 = 0x04;
const RECALL_SCENE: u8 = 0x05;

impl ClusterHandler for ScenesCluster {
    fn process_reports(&self, _reports: &[AttributeReport]) -> Vec<(String, Value)> {
        // Scenes' own attributes (CurrentScene/CurrentGroup/SceneValid/...)
        // aren't meaningful bridge state for the devices this project targets
        // (remote controls, which don't report them; scene-capable lights,
        // which aren't otherwise supported here).
        vec![]
    }

    fn process_command(&self, command_id: u8, payload: &[u8]) -> Vec<(String, Value)> {
        process_command_with_mfr_code(command_id, payload, None)
    }
}

/// Manufacturer-code-aware equivalent of `ClusterHandler::process_command`.
/// Scenes' manufacturer-specific commands need the manufacturer code, which
/// the generic `ClusterHandler` trait has no way to carry -- `zcl::parse_message`
/// calls this directly for cluster 0x0005 instead of going through
/// `clusters::handler_for`, the same way it special-cases IAS Zone for
/// `zone_type`.
pub fn process_command_with_mfr_code(
    command_id: u8,
    payload: &[u8],
    mfr_code: Option<u16>,
) -> Vec<(String, Value)> {
    if let Some(mfr) = mfr_code {
        return vec![(
            "action".into(),
            json!(format!("manufacturer_0x{mfr:04x}_cmd_0x{command_id:02x}")),
        )];
    }

    match command_id {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_scene() {
        let payload = [0x01, 0x00, 0x05]; // group=1, scene=5
        let result = process_command_with_mfr_code(RECALL_SCENE, &payload, None);
        assert!(result
            .iter()
            .any(|(k, v)| k == "action" && v == &json!("recall")));
        assert!(result
            .iter()
            .any(|(k, v)| k == "action_group" && v == &json!(1)));
        assert!(result
            .iter()
            .any(|(k, v)| k == "action_scene" && v == &json!(5)));
    }

    #[test]
    fn store_scene() {
        let payload = [0x02, 0x00, 0x03]; // group=2, scene=3
        let result = process_command_with_mfr_code(STORE_SCENE, &payload, None);
        assert!(result
            .iter()
            .any(|(k, v)| k == "action" && v == &json!("store")));
    }

    #[test]
    fn manufacturer_specific_command_is_generic_and_honest() {
        let result = process_command_with_mfr_code(0x07, &[0x01, 0x02], Some(0x117C));
        assert_eq!(
            result,
            vec![("action".into(), json!("manufacturer_0x117c_cmd_0x07"))]
        );
    }

    #[test]
    fn unknown_standard_command_is_ignored() {
        assert_eq!(process_command_with_mfr_code(0x99, &[], None), vec![]);
    }

    #[test]
    fn truncated_recall_scene_ignored() {
        assert_eq!(
            process_command_with_mfr_code(RECALL_SCENE, &[0x01], None),
            vec![]
        );
    }
}
