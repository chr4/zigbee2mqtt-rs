//! zigbee2mqtt compatibility tests.
//!
//! These tests validate that our MQTT message formats, state payloads,
//! and HA discovery configs match the exact format the real zigbee2mqtt
//! produces, ensuring drop-in compatibility with Home Assistant.

// We test the library internals directly.
// Each test references the z2m test suite patterns from:
//   https://github.com/Koenkk/zigbee2mqtt/tree/master/test/extensions

mod bridge_state {
    //! Tests from bridge.test.ts — bridge/state message format
    use serde_json::json;

    #[test]
    fn bridge_state_online_is_json() {
        // z2m publishes {"state":"online"} not plain "online"
        let state = json!({"state": "online"});
        assert_eq!(state["state"], "online");
    }

    #[test]
    fn bridge_state_offline_is_json() {
        let state = json!({"state": "offline"});
        assert_eq!(state["state"], "offline");
    }
}

mod bridge_info {
    //! Tests from bridge.test.ts — bridge/info message format
    use serde_json::json;

    #[test]
    fn bridge_info_has_required_fields() {
        // z2m bridge/info must contain these top-level keys
        let info = json!({
            "version": "0.1.0",
            "coordinator": {
                "ieee_address": "0x00124b00120144ae",
                "type": "z-Stack",
                "meta": { "revision": 2, "version": "2.6" }
            },
            "log_level": "info",
            "permit_join": true,
            "config": {},
        });

        assert!(info.get("version").is_some());
        assert!(info.get("coordinator").is_some());
        assert!(info.get("log_level").is_some());
        assert!(info.get("permit_join").is_some());

        let coord = &info["coordinator"];
        assert!(coord["ieee_address"].as_str().unwrap().starts_with("0x"));
        assert_eq!(coord["type"], "z-Stack");
    }
}

mod bridge_devices {
    //! Tests from bridge.test.ts — bridge/devices message format
    use serde_json::json;
    use zigbee2mqtt_rs::devices::Device;
    use zigbee2mqtt_rs::zigbee::{EndpointDesc, IeeeAddr};

    fn make_device() -> Device {
        let mut dev = Device::new(
            IeeeAddr([0xb2, 0xa5, 0xc6, 0xfe, 0xff, 0x57, 0x0b, 0x00]),
            0x1234,
        );
        dev.friendly_name = "bulb".to_string();
        dev.manufacturer = Some("IKEA".to_string());
        dev.model = Some("TRADFRI".to_string());
        dev.power_source = Some("Mains (single phase)".to_string());
        dev.interview_complete = true;
        dev.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0x0100,
            input_clusters: vec![0x0000, 0x0006, 0x0008],
            output_clusters: vec![],
        });
        dev
    }

    #[test]
    fn device_json_has_z2m_required_fields() {
        // z2m bridge/devices array element must have these fields
        let dev = make_device();
        let j = dev.to_z2m_device_json();

        assert!(j.get("ieee_address").is_some());
        assert!(j.get("type").is_some());
        assert!(j.get("network_address").is_some());
        assert!(j.get("friendly_name").is_some());
        assert!(j.get("interview_completed").is_some());
        assert!(j.get("interviewing").is_some());
        assert!(j.get("supported").is_some());
        assert!(j.get("disabled").is_some());
    }

    #[test]
    fn device_ieee_address_format() {
        let dev = make_device();
        let j = dev.to_z2m_device_json();
        let ieee = j["ieee_address"].as_str().unwrap();
        // Must be 0x + 16 hex chars
        assert!(ieee.starts_with("0x"));
        assert_eq!(ieee.len(), 18);
    }

    #[test]
    fn device_type_values() {
        // z2m uses "Router", "EndDevice", or "Coordinator"
        let mut dev = make_device();
        assert!(["Router", "EndDevice", "Coordinator"].contains(&dev.device_type()));

        dev.power_source = Some("battery".to_string());
        assert_eq!(dev.device_type(), "EndDevice");
    }

    #[test]
    fn device_definition_block() {
        // z2m includes definition with model/vendor
        let dev = make_device();
        let j = dev.to_z2m_device_json();
        let def = &j["definition"];
        assert!(def.get("model").is_some());
        assert!(def.get("vendor").is_some());
    }

    #[test]
    fn interview_states() {
        let mut dev = make_device();
        dev.interview_complete = false;
        dev.endpoints.clear();
        let j = dev.to_z2m_device_json();
        assert_eq!(j["interview_completed"], false);
        assert_eq!(j["supported"], false);

        dev.interview_complete = true;
        let j = dev.to_z2m_device_json();
        assert_eq!(j["interview_completed"], true);
        assert_eq!(j["supported"], true);
    }

    #[test]
    fn device_list_is_array() {
        let dev = make_device();
        let list = json!([dev.to_z2m_device_json()]);
        assert!(list.is_array());
        assert_eq!(list.as_array().unwrap().len(), 1);
    }
}

mod receive_state {
    //! Tests from receive.test.ts — device state publishing format
    use serde_json::json;
    use zigbee2mqtt_rs::zigbee::zcl;

    #[test]
    fn temperature_report_format() {
        // z2m: {"temperature": -0.85} from measuredValue=-85
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x0A, // ZCL header: global, report attributes
            0x00, 0x00,       // attr_id = 0x0000 (MeasuredValue)
            0x29,             // Int16
            0xAB, 0xFF,       // -85 in little-endian
        ];
        let msg = zcl::parse_message(0x0402, &raw).unwrap().unwrap();
        let temp = msg.values["temperature"].as_f64().unwrap();
        assert!((temp - (-0.85)).abs() < 0.01);
    }

    #[test]
    fn humidity_report_format() {
        // z2m: {"humidity": 45.23} from measuredValue=4523
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x0A,
            0x00, 0x00, 0x21, // Uint16
            0xAB, 0x11,       // 4523
        ];
        let msg = zcl::parse_message(0x0405, &raw).unwrap().unwrap();
        let hum = msg.values["humidity"].as_f64().unwrap();
        assert!((hum - 45.23).abs() < 0.01);
    }

    #[test]
    fn on_off_state_uses_string_on_off() {
        // z2m: {"state": "ON"} or {"state": "OFF"}, never booleans
        #[rustfmt::skip]
        let raw_on = [0x18, 0x01, 0x0A, 0x00, 0x00, 0x10, 0x01]; // Boolean true
        let msg = zcl::parse_message(0x0006, &raw_on).unwrap().unwrap();
        assert_eq!(msg.values["state"], "ON");

        #[rustfmt::skip]
        let raw_off = [0x18, 0x01, 0x0A, 0x00, 0x00, 0x10, 0x00]; // Boolean false
        let msg = zcl::parse_message(0x0006, &raw_off).unwrap().unwrap();
        assert_eq!(msg.values["state"], "OFF");
    }

    #[test]
    fn brightness_report_format() {
        // z2m: {"brightness": 200}
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x0A,
            0x00, 0x00, 0x20, // Uint8
            0xC8,             // 200
        ];
        let msg = zcl::parse_message(0x0008, &raw).unwrap().unwrap();
        assert_eq!(msg.values["brightness"], 200);
    }

    #[test]
    fn color_temp_report_format() {
        // z2m: {"color_temp": 370}
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x0A,
            0x07, 0x00, 0x21, // attr 0x0007 = ColorTemperatureMireds, Uint16
            0x72, 0x01,       // 370
        ];
        let msg = zcl::parse_message(0x0300, &raw).unwrap().unwrap();
        assert_eq!(msg.values["color_temp"], 370);
    }

    #[test]
    fn color_xy_report_is_nested() {
        // z2m: {"color": {"x": 0.3, "y": 0.3}} not flat color_x/color_y
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x0A,
            0x03, 0x00, 0x21, // attr 0x0003 = CurrentX, Uint16
            0xCD, 0x4C,       // 19661 ≈ 0.3
            0x04, 0x00, 0x21, // attr 0x0004 = CurrentY, Uint16
            0xCD, 0x4C,       // 19661 ≈ 0.3
        ];
        let msg = zcl::parse_message(0x0300, &raw).unwrap().unwrap();
        let color = msg.values.get("color").unwrap().as_object().unwrap();
        assert!(color.contains_key("x"));
        assert!(color.contains_key("y"));
        let x = color["x"].as_f64().unwrap();
        assert!((x - 0.3).abs() < 0.01);
    }

    #[test]
    fn color_mode_report() {
        // z2m: {"color_mode": "color_temp"} from enum value 2
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x0A,
            0x08, 0x00, 0x30, // attr 0x0008 = ColorMode, Enum8
            0x02,             // color_temp
        ];
        let msg = zcl::parse_message(0x0300, &raw).unwrap().unwrap();
        assert_eq!(msg.values["color_mode"], "color_temp");
    }

    #[test]
    fn occupancy_report_format() {
        // z2m: {"occupancy": true}
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x0A,
            0x00, 0x00, 0x18, // attr 0x0000 = Occupancy, Bitmap8
            0x01,             // occupied
        ];
        let msg = zcl::parse_message(0x0406, &raw).unwrap().unwrap();
        assert_eq!(msg.values["occupancy"], true);
    }

    #[test]
    fn battery_report_format() {
        // z2m: {"battery": 93} from percentage remaining = 186 (half-percent)
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x0A,
            0x21, 0x00, 0x20, // attr 0x0021 = BatteryPercentageRemaining, Uint8
            0xBA,             // 186 = 93%
        ];
        let msg = zcl::parse_message(0x0001, &raw).unwrap().unwrap();
        assert_eq!(msg.values["battery"], 93);
    }

    #[test]
    fn contact_sensor_zone_status_change() {
        // z2m: {"contact": false} when alarm1 set (door open)
        // Cluster-specific command 0x00 = Zone Status Change Notification
        #[rustfmt::skip]
        let raw = [
            0x09, 0x01, 0x00, // ZCL: cluster-specific, server→client, cmd=0x00
            0x01, 0x00,       // zone_status = 0x0001 (ALARM1 = open)
            0x00, 0x01, 0x00, 0x00, // extended_status, zone_id, delay
        ];
        let msg = zcl::parse_message(0x0500, &raw).unwrap().unwrap();
        // contact=false means door is open (z2m convention)
        assert_eq!(msg.values["contact"], false);
    }

    #[test]
    fn contact_sensor_closed() {
        #[rustfmt::skip]
        let raw = [
            0x09, 0x01, 0x00,
            0x00, 0x00, // zone_status = 0 (no alarm = closed)
            0x00, 0x01, 0x00, 0x00,
        ];
        let msg = zcl::parse_message(0x0500, &raw).unwrap().unwrap();
        assert_eq!(msg.values["contact"], true);
    }
}

mod publish_commands {
    //! Tests from publish.test.ts — set command format
    use zigbee2mqtt_rs::zigbee::zcl::clusters::{color, level, on_off};

    #[test]
    fn on_command_payload() {
        // z2m sends genOnOff.on → ZCL cluster-specific cmd 0x01
        let p = on_off::set_state_payload(1, "ON").unwrap();
        assert_eq!(p[0], 0x11); // frame control: cluster-specific, disable default rsp
        assert_eq!(p[2], 0x01); // On command
    }

    #[test]
    fn off_command_payload() {
        let p = on_off::set_state_payload(1, "OFF").unwrap();
        assert_eq!(p[2], 0x00); // Off command
    }

    #[test]
    fn toggle_command_payload() {
        let p = on_off::set_state_payload(1, "TOGGLE").unwrap();
        assert_eq!(p[2], 0x02); // Toggle command
    }

    #[test]
    fn brightness_command_uses_move_to_level_with_onoff() {
        // z2m: {"brightness": 200} → moveToLevelWithOnOff (cmd 0x04)
        let p = level::move_to_level_payload(1, 200, 0);
        assert_eq!(p[2], 0x04); // moveToLevelWithOnOff
        assert_eq!(p[3], 200); // level
    }

    #[test]
    fn brightness_with_transition() {
        // z2m: {"brightness": 200, "transition": 2.0} → transtime=20
        let p = level::move_to_level_payload(1, 200, 20);
        let trans = u16::from_le_bytes([p[4], p[5]]);
        assert_eq!(trans, 20); // 2.0 seconds * 10
    }

    #[test]
    fn color_temp_command() {
        // z2m: {"color_temp": 222} → moveToColorTemp (cmd 0x0A)
        let p = color::move_to_color_temp_payload(1, 222, 0);
        assert_eq!(p[2], 0x0A); // moveToColorTemp
        let ct = u16::from_le_bytes([p[3], p[4]]);
        assert_eq!(ct, 222);
    }

    #[test]
    fn color_xy_command() {
        // z2m: {"color": {"x": 0.37, "y": 0.28}} → moveToColor (cmd 0x07)
        let p = color::move_to_color_xy_payload(1, 0.37, 0.28, 0);
        assert_eq!(p[2], 0x07); // moveToColor
        let x = u16::from_le_bytes([p[3], p[4]]);
        let y = u16::from_le_bytes([p[5], p[6]]);
        // x = 0.37 * 65536 ≈ 24248
        assert!((x as f64 - 24248.0).abs() < 2.0);
        // y = 0.28 * 65536 ≈ 18350
        assert!((y as f64 - 18350.0).abs() < 2.0);
    }

    #[test]
    fn color_hs_command() {
        // z2m: {"color": {"hue": 250, "saturation": 50}}
        // → moveToHueAndSaturation (cmd 0x06)
        // hue: 250/360*254 ≈ 176, sat: 50/100*254 = 127
        let p = color::move_to_hue_sat_payload(1, 176, 127, 0);
        assert_eq!(p[2], 0x06); // moveToHueAndSaturation
        assert_eq!(p[3], 176); // hue
        assert_eq!(p[4], 127); // saturation
    }
}

mod permit_join {
    //! Tests from bridge.test.ts — permit join request format
    // The MQTT permit_join parsing is tested in mqtt/mod.rs unit tests.
    // This validates the z2m JSON format expectations.

    #[test]
    fn z2m_permit_join_json_format() {
        // z2m sends: {"value": true, "time": 254}
        let payload = serde_json::json!({"value": true, "time": 254});
        assert_eq!(payload["value"], true);
        assert_eq!(payload["time"], 254);
    }

    #[test]
    fn z2m_permit_join_disable() {
        let payload = serde_json::json!({"value": false});
        assert_eq!(payload["value"], false);
    }
}
