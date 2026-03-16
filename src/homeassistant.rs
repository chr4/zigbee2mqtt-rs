/// Home Assistant MQTT auto-discovery message generation.
/// Publishes discovery configs to `homeassistant/<component>/<node_id>/<object_id>/config`
/// so that HA automatically creates entities for each Zigbee device.
use serde_json::{json, Value};

use crate::devices::Device;
use crate::mqtt::MqttBridge;

/// Publish all HA discovery messages for a device.
pub async fn publish_discovery(mqtt: &MqttBridge, device: &Device, base_topic: &str) {
    let ieee_hex = device.ieee_addr.as_hex();
    // Node ID: strip 0x prefix for cleaner HA entity IDs
    let node_id = ieee_hex.trim_start_matches("0x");

    let ha_device = device_block(device, base_topic);
    let availability = availability_block(base_topic);
    let clusters = device.all_input_clusters();

    let has_on_off = clusters.contains(&0x0006);
    let has_level = clusters.contains(&0x0008);
    let has_color = clusters.contains(&0x0300);

    // ── Light or Switch ───────────────────────────────────────────────────
    if has_on_off {
        if has_level || has_color {
            // It's a light
            let mut config = json!({
                "name": null,
                "schema": "json",
                "state_topic": format!("{}/{}", base_topic, device.friendly_name),
                "command_topic": format!("{}/{}/set", base_topic, device.friendly_name),
                "unique_id": format!("{node_id}_light"),
                "object_id": format!("{}_light", device.friendly_name),
                "device": ha_device,
                "availability": availability,
                "brightness": has_level,
                "state_value_template": "{{ value_json.state }}",
                "brightness_scale": 254,
            });
            if has_color {
                config["color_mode"] = json!(true);
                let mut modes: Vec<&str> = Vec::new();
                // Check for color temperature support
                if device
                    .endpoints
                    .iter()
                    .any(|ep| ep.input_clusters.contains(&0x0300))
                {
                    modes.push("color_temp");
                    modes.push("xy");
                }
                if modes.is_empty() {
                    modes.push("xy");
                }
                config["supported_color_modes"] = json!(modes);
            }

            mqtt.publish_ha_discovery("light", node_id, "light", &config)
                .await
                .ok();
        } else {
            // It's a switch
            let config = json!({
                "name": null,
                "state_topic": format!("{}/{}", base_topic, device.friendly_name),
                "command_topic": format!("{}/{}/set", base_topic, device.friendly_name),
                "unique_id": format!("{node_id}_switch"),
                "object_id": format!("{}_switch", device.friendly_name),
                "device": ha_device,
                "availability": availability,
                "value_template": "{{ value_json.state }}",
                "state_on": "ON",
                "state_off": "OFF",
                "payload_on": "{\"state\": \"ON\"}",
                "payload_off": "{\"state\": \"OFF\"}",
            });
            mqtt.publish_ha_discovery("switch", node_id, "switch", &config)
                .await
                .ok();
        }
    }

    // ── Temperature sensor ────────────────────────────────────────────────
    if clusters.contains(&0x0402) {
        let config = sensor_config(
            device,
            base_topic,
            node_id,
            "temperature",
            "temperature",
            "°C",
            "temperature",
            &ha_device,
            &availability,
        );
        mqtt.publish_ha_discovery("sensor", node_id, "temperature", &config)
            .await
            .ok();
    }

    // ── Humidity sensor ───────────────────────────────────────────────────
    if clusters.contains(&0x0405) {
        let config = sensor_config(
            device,
            base_topic,
            node_id,
            "humidity",
            "humidity",
            "%",
            "humidity",
            &ha_device,
            &availability,
        );
        mqtt.publish_ha_discovery("sensor", node_id, "humidity", &config)
            .await
            .ok();
    }

    // ── Illuminance sensor ────────────────────────────────────────────────
    if clusters.contains(&0x0400) {
        let config = sensor_config(
            device,
            base_topic,
            node_id,
            "illuminance_lux",
            "illuminance",
            "lx",
            "illuminance",
            &ha_device,
            &availability,
        );
        mqtt.publish_ha_discovery("sensor", node_id, "illuminance", &config)
            .await
            .ok();
    }

    // ── Battery sensor ────────────────────────────────────────────────────
    if clusters.contains(&0x0001) {
        let config = sensor_config(
            device,
            base_topic,
            node_id,
            "battery",
            "battery",
            "%",
            "battery",
            &ha_device,
            &availability,
        );
        mqtt.publish_ha_discovery("sensor", node_id, "battery", &config)
            .await
            .ok();
    }

    // ── Occupancy binary sensor ───────────────────────────────────────────
    if clusters.contains(&0x0406) {
        let config = json!({
            "name": "Occupancy",
            "state_topic": format!("{}/{}", base_topic, device.friendly_name),
            "unique_id": format!("{node_id}_occupancy"),
            "object_id": format!("{}_occupancy", device.friendly_name),
            "device": ha_device,
            "availability": availability,
            "device_class": "occupancy",
            "value_template": "{{ value_json.occupancy }}",
            "payload_on": true,
            "payload_off": false,
        });
        mqtt.publish_ha_discovery("binary_sensor", node_id, "occupancy", &config)
            .await
            .ok();
    }

    // ── IAS Zone contact sensor ───────────────────────────────────────────
    if clusters.contains(&0x0500) {
        let config = json!({
            "name": "Contact",
            "state_topic": format!("{}/{}", base_topic, device.friendly_name),
            "unique_id": format!("{node_id}_contact"),
            "object_id": format!("{}_contact", device.friendly_name),
            "device": ha_device,
            "availability": availability,
            "device_class": "door",
            "value_template": "{{ value_json.contact }}",
            "payload_on": false,
            "payload_off": true,
        });
        mqtt.publish_ha_discovery("binary_sensor", node_id, "contact", &config)
            .await
            .ok();
    }

    // ── Link quality sensor ───────────────────────────────────────────────
    {
        let config = json!({
            "name": "Linkquality",
            "state_topic": format!("{}/{}", base_topic, device.friendly_name),
            "unique_id": format!("{node_id}_linkquality"),
            "object_id": format!("{}_linkquality", device.friendly_name),
            "device": ha_device,
            "availability": availability,
            "icon": "mdi:signal",
            "unit_of_measurement": "lqi",
            "state_class": "measurement",
            "value_template": "{{ value_json.linkquality }}",
            "entity_category": "diagnostic",
        });
        mqtt.publish_ha_discovery("sensor", node_id, "linkquality", &config)
            .await
            .ok();
    }
}

fn device_block(device: &Device, base_topic: &str) -> Value {
    let ieee_hex = device.ieee_addr.as_hex();
    json!({
        "identifiers": [format!("zigbee2mqtt_{ieee_hex}")],
        "name": device.friendly_name,
        "manufacturer": device.manufacturer.as_deref().unwrap_or("Unknown"),
        "model": device.model.as_deref().unwrap_or("Unknown"),
        "sw_version": device.sw_build_id,
        "via_device": format!("zigbee2mqtt_bridge_{}", base_topic),
    })
}

fn availability_block(base_topic: &str) -> Value {
    json!([
        {
            "topic": format!("{base_topic}/bridge/state"),
            "value_template": "{{ value_json.state }}"
        }
    ])
}

fn sensor_config(
    device: &Device,
    base_topic: &str,
    node_id: &str,
    value_key: &str,
    device_class: &str,
    unit: &str,
    object_id: &str,
    ha_device: &Value,
    availability: &Value,
) -> Value {
    json!({
        "name": capitalize(device_class),
        "state_topic": format!("{}/{}", base_topic, device.friendly_name),
        "unique_id": format!("{node_id}_{object_id}"),
        "object_id": format!("{}_{object_id}", device.friendly_name),
        "device": ha_device,
        "availability": availability,
        "device_class": device_class,
        "unit_of_measurement": unit,
        "state_class": "measurement",
        "value_template": format!("{{{{ value_json.{value_key} }}}}"),
    })
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::Device;
    use crate::zigbee::{EndpointDesc, IeeeAddr};

    fn make_light() -> Device {
        let mut dev = Device::new(
            IeeeAddr([0x00, 0x15, 0x8D, 0x00, 0x01, 0x02, 0x03, 0x04]),
            0x1234,
        );
        dev.friendly_name = "kitchen_light".to_string();
        dev.manufacturer = Some("IKEA".to_string());
        dev.model = Some("TRADFRI bulb".to_string());
        dev.interview_complete = true;
        dev.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0x0100,
            input_clusters: vec![0x0000, 0x0006, 0x0008, 0x0300],
            output_clusters: vec![],
        });
        dev
    }

    fn make_sensor() -> Device {
        let mut dev = Device::new(
            IeeeAddr([0x00, 0x15, 0x8D, 0x00, 0x05, 0x06, 0x07, 0x08]),
            0x5678,
        );
        dev.friendly_name = "temp_sensor".to_string();
        dev.manufacturer = Some("Xiaomi".to_string());
        dev.model = Some("WSDCGQ11LM".to_string());
        dev.power_source = Some("battery".to_string());
        dev.interview_complete = true;
        dev.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0x0302,
            input_clusters: vec![0x0000, 0x0001, 0x0402, 0x0405],
            output_clusters: vec![],
        });
        dev
    }

    #[test]
    fn device_block_format() {
        let dev = make_light();
        let block = device_block(&dev, "zigbee2mqtt");
        assert_eq!(block["name"], "kitchen_light");
        assert_eq!(block["manufacturer"], "IKEA");
        let ids = block["identifiers"].as_array().unwrap();
        assert!(ids[0].as_str().unwrap().starts_with("zigbee2mqtt_0x"));
    }

    #[test]
    fn availability_block_format() {
        let avail = availability_block("zigbee2mqtt");
        let arr = avail.as_array().unwrap();
        assert_eq!(arr[0]["topic"], "zigbee2mqtt/bridge/state");
    }

    #[test]
    fn sensor_config_format() {
        let dev = make_sensor();
        let ha_dev = device_block(&dev, "zigbee2mqtt");
        let avail = availability_block("zigbee2mqtt");
        let cfg = sensor_config(
            &dev,
            "zigbee2mqtt",
            "00158d0005060708",
            "temperature",
            "temperature",
            "°C",
            "temperature",
            &ha_dev,
            &avail,
        );
        assert_eq!(cfg["device_class"], "temperature");
        assert_eq!(cfg["unit_of_measurement"], "°C");
        assert!(cfg["unique_id"].as_str().unwrap().contains("temperature"));
        assert!(cfg["state_topic"]
            .as_str()
            .unwrap()
            .contains("temp_sensor"));
    }

    #[test]
    fn light_has_correct_clusters() {
        let clusters = make_light().all_input_clusters();
        assert!(clusters.contains(&0x0006));
        assert!(clusters.contains(&0x0008));
        assert!(clusters.contains(&0x0300));
    }

    #[test]
    fn sensor_has_battery_and_temp() {
        let clusters = make_sensor().all_input_clusters();
        assert!(clusters.contains(&0x0001));
        assert!(clusters.contains(&0x0402));
        assert!(clusters.contains(&0x0405));
    }
}
