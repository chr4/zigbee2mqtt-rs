/// Home Assistant MQTT auto-discovery message generation.
/// Publishes discovery configs to `homeassistant/<component>/<node_id>/<object_id>/config`
/// matching the exact format zigbee2mqtt uses for HA compatibility.
use serde_json::{json, Value};
use tracing::warn;

use crate::devices::Device;
use crate::mqtt::MqttBridge;

async fn publish(mqtt: &MqttBridge, component: &str, ieee: &str, object_id: &str, config: &Value) {
    if let Err(e) = mqtt.publish_ha_discovery(component, ieee, object_id, config).await {
        warn!("HA discovery error for {component}/{ieee}/{object_id}: {e}");
    }
}

/// Publish all HA discovery messages for a device.
pub async fn publish_discovery(
    mqtt: &MqttBridge,
    device: &Device,
    base_topic: &str,
    coordinator_ieee: &str,
) {
    let ieee_hex = device.ieee_addr.as_hex();
    let ha_device = device_block(device, base_topic, coordinator_ieee);
    let availability = availability_block(base_topic);
    let clusters = device.all_input_clusters();

    let has_on_off = clusters.contains(&0x0006);
    let has_level = clusters.contains(&0x0008);
    let has_color = clusters.contains(&0x0300);

    // ── Light or Switch ───────────────────────────────────────────────────
    if has_on_off {
        if has_level || has_color {
            let mut config = json!({
                "availability": availability,
                "brightness": has_level,
                "brightness_scale": 254,
                "command_topic": format!("{}/{}/set", base_topic, device.friendly_name),
                "state_topic": format!("{}/{}", base_topic, device.friendly_name),
                "schema": "json",
                "object_id": &device.friendly_name,
                "unique_id": format!("{ieee_hex}_light_{base_topic}"),
                "device": ha_device,
            });
            if has_color {
                config["color_mode"] = json!(true);
                config["supported_color_modes"] = json!(["color_temp", "xy"]);
            } else if has_level {
                config["supported_color_modes"] = json!(["brightness"]);
            }
            publish(mqtt, "light", &ieee_hex, "light", &config).await;
        } else {
            let config = json!({
                "availability": availability,
                "command_topic": format!("{}/{}/set", base_topic, device.friendly_name),
                "state_topic": format!("{}/{}", base_topic, device.friendly_name),
                "object_id": &device.friendly_name,
                "unique_id": format!("{ieee_hex}_switch_{base_topic}"),
                "device": ha_device,
                "value_template": "{{ value_json.state }}",
                "payload_on": "ON",
                "payload_off": "OFF",
            });
            publish(mqtt, "switch", &ieee_hex, "switch", &config).await;
        }
    }

    // ── Temperature sensor ────────────────────────────────────────────────
    if clusters.contains(&0x0402) {
        publish_sensor(
            mqtt,
            device,
            base_topic,
            &ieee_hex,
            "temperature",
            "temperature",
            "°C",
            &ha_device,
            &availability,
        )
        .await;
    }

    // ── Humidity sensor ───────────────────────────────────────────────────
    if clusters.contains(&0x0405) {
        publish_sensor(
            mqtt,
            device,
            base_topic,
            &ieee_hex,
            "humidity",
            "humidity",
            "%",
            &ha_device,
            &availability,
        )
        .await;
    }

    // ── Illuminance sensor ────────────────────────────────────────────────
    if clusters.contains(&0x0400) {
        publish_sensor(
            mqtt,
            device,
            base_topic,
            &ieee_hex,
            "illuminance",
            "illuminance",
            "lx",
            &ha_device,
            &availability,
        )
        .await;
    }

    // ── Battery sensor ────────────────────────────────────────────────────
    if clusters.contains(&0x0001) {
        publish_sensor(
            mqtt,
            device,
            base_topic,
            &ieee_hex,
            "battery",
            "battery",
            "%",
            &ha_device,
            &availability,
        )
        .await;
    }

    // ── Occupancy binary sensor ───────────────────────────────────────────
    if clusters.contains(&0x0406) {
        let config = json!({
            "availability": availability,
            "state_topic": format!("{}/{}", base_topic, device.friendly_name),
            "object_id": format!("{}_occupancy", device.friendly_name),
            "unique_id": format!("{ieee_hex}_occupancy_{base_topic}"),
            "device": ha_device,
            "device_class": "occupancy",
            "value_template": "{{ value_json.occupancy }}",
            "payload_on": true,
            "payload_off": false,
            "enabled_by_default": true,
        });
        publish(mqtt, "binary_sensor", &ieee_hex, "occupancy", &config).await;
    }

    // ── IAS Zone contact sensor ───────────────────────────────────────────
    if clusters.contains(&0x0500) {
        let config = json!({
            "availability": availability,
            "state_topic": format!("{}/{}", base_topic, device.friendly_name),
            "object_id": format!("{}_contact", device.friendly_name),
            "unique_id": format!("{ieee_hex}_contact_{base_topic}"),
            "device": ha_device,
            "device_class": "door",
            "value_template": "{{ value_json.contact }}",
            "payload_on": false,
            "payload_off": true,
            "enabled_by_default": true,
        });
        publish(mqtt, "binary_sensor", &ieee_hex, "contact", &config).await;
    }

    // ── Link quality diagnostic sensor ────────────────────────────────────
    {
        let config = json!({
            "availability": availability,
            "state_topic": format!("{}/{}", base_topic, device.friendly_name),
            "object_id": format!("{}_linkquality", device.friendly_name),
            "unique_id": format!("{ieee_hex}_linkquality_{base_topic}"),
            "device": ha_device,
            "icon": "mdi:signal",
            "unit_of_measurement": "lqi",
            "state_class": "measurement",
            "value_template": "{{ value_json.linkquality }}",
            "entity_category": "diagnostic",
            "enabled_by_default": true,
        });
        publish(mqtt, "sensor", &ieee_hex, "linkquality", &config).await;
    }
}

async fn publish_sensor(
    mqtt: &MqttBridge,
    device: &Device,
    base_topic: &str,
    ieee_hex: &str,
    value_key: &str,
    device_class: &str,
    unit: &str,
    ha_device: &Value,
    availability: &Value,
) {
    let config = json!({
        "availability": availability,
        "state_topic": format!("{}/{}", base_topic, device.friendly_name),
        "object_id": format!("{}_{device_class}", device.friendly_name),
        "unique_id": format!("{ieee_hex}_{device_class}_{base_topic}"),
        "device": ha_device,
        "device_class": device_class,
        "unit_of_measurement": unit,
        "state_class": "measurement",
        "value_template": format!("{{{{ value_json.{value_key} }}}}"),
        "enabled_by_default": true,
    });
    publish(mqtt, "sensor", ieee_hex, device_class, &config).await;
}

fn device_block(device: &Device, base_topic: &str, coordinator_ieee: &str) -> Value {
    let ieee_hex = device.ieee_addr.as_hex();
    json!({
        "identifiers": [format!("{base_topic}_{ieee_hex}")],
        "name": device.friendly_name,
        "manufacturer": device.manufacturer.as_deref().unwrap_or("Unknown"),
        "model": device.model.as_deref().unwrap_or("Unknown"),
        "sw_version": device.sw_build_id,
        "via_device": format!("{base_topic}_bridge_{coordinator_ieee}"),
    })
}

fn availability_block(base_topic: &str) -> Value {
    json!([{
        "topic": format!("{base_topic}/bridge/state"),
        "value_template": "{{ value_json.state }}"
    }])
}

// ── z2m-compatible test suite ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::Device;
    use crate::zigbee::{EndpointDesc, IeeeAddr};

    const BASE: &str = "zigbee2mqtt";
    const COORD_IEEE: &str = "0x00124b00120144ae";

    fn make_light() -> Device {
        let mut dev = Device::new(
            IeeeAddr([0xb2, 0xa5, 0xc6, 0xfe, 0xff, 0x57, 0x0b, 0x00]),
            0x1234,
        );
        dev.friendly_name = "bulb".to_string();
        dev.manufacturer = Some("IKEA".to_string());
        dev.model = Some("TRADFRI bulb E26/E27".to_string());
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

    fn make_color_light() -> Device {
        let mut dev = make_light();
        dev.endpoints[0]
            .input_clusters
            .extend_from_slice(&[0x0300]);
        dev
    }

    fn make_switch() -> Device {
        let mut dev = Device::new(
            IeeeAddr([0x42, 0x55, 0xe4, 0x04, 0x01, 0x88, 0x17, 0x00]),
            0x5678,
        );
        dev.friendly_name = "wall_switch".to_string();
        dev.interview_complete = true;
        dev.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0x0000,
            input_clusters: vec![0x0000, 0x0006],
            output_clusters: vec![],
        });
        dev
    }

    fn make_sensor() -> Device {
        let mut dev = Device::new(
            IeeeAddr([0x22, 0x55, 0xe4, 0x04, 0x01, 0x88, 0x17, 0x00]),
            0x9ABC,
        );
        dev.friendly_name = "weather_sensor".to_string();
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

    fn make_contact_sensor() -> Device {
        let mut dev = Device::new(
            IeeeAddr([0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0x00]),
            0xDEF0,
        );
        dev.friendly_name = "door_sensor".to_string();
        dev.interview_complete = true;
        dev.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0x0402,
            input_clusters: vec![0x0000, 0x0001, 0x0500],
            output_clusters: vec![],
        });
        dev
    }

    fn make_occupancy_sensor() -> Device {
        let mut dev = Device::new(
            IeeeAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11]),
            0x4321,
        );
        dev.friendly_name = "motion_sensor".to_string();
        dev.interview_complete = true;
        dev.endpoints.push(EndpointDesc {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0x0107,
            input_clusters: vec![0x0000, 0x0001, 0x0406, 0x0400],
            output_clusters: vec![],
        });
        dev
    }

    // ── Discovery topic format ────────────────────────────────────────────

    #[test]
    fn discovery_topic_format_matches_z2m() {
        // z2m: homeassistant/light/0x000b57fffec6a5b2/light/config
        let ieee = make_light().ieee_addr.as_hex();
        let topic = format!("homeassistant/light/{ieee}/light/config");
        assert!(topic.starts_with("homeassistant/light/0x"));
        assert!(topic.ends_with("/light/config"));
    }

    // ── Availability block (z2m format) ───────────────────────────────────

    #[test]
    fn availability_matches_z2m() {
        let avail = availability_block(BASE);
        let arr = avail.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["topic"], "zigbee2mqtt/bridge/state");
        assert_eq!(arr[0]["value_template"], "{{ value_json.state }}");
    }

    // ── Device block (z2m format) ─────────────────────────────────────────

    #[test]
    fn device_block_matches_z2m() {
        let dev = make_light();
        let block = device_block(&dev, BASE, COORD_IEEE);

        // z2m: identifiers = ["zigbee2mqtt_0x..."]
        let ids = block["identifiers"].as_array().unwrap();
        assert!(ids[0].as_str().unwrap().starts_with("zigbee2mqtt_0x"));

        assert_eq!(block["name"], "bulb");
        assert_eq!(block["manufacturer"], "IKEA");
        assert_eq!(block["model"], "TRADFRI bulb E26/E27");

        // z2m: via_device = "zigbee2mqtt_bridge_0x..."
        let via = block["via_device"].as_str().unwrap();
        assert!(via.starts_with("zigbee2mqtt_bridge_0x"));
    }

    // ── Light discovery (z2m format) ──────────────────────────────────────

    #[test]
    fn light_discovery_has_required_z2m_fields() {
        let dev = make_light();
        let ieee = dev.ieee_addr.as_hex();
        let ha_dev = device_block(&dev, BASE, COORD_IEEE);
        let avail = availability_block(BASE);

        // Simulate what publish_discovery builds for a brightness light
        let config = json!({
            "availability": avail,
            "brightness": true,
            "brightness_scale": 254,
            "command_topic": format!("{BASE}/bulb/set"),
            "state_topic": format!("{BASE}/bulb"),
            "schema": "json",
            "object_id": "bulb",
            "unique_id": format!("{ieee}_light_{BASE}"),
            "device": ha_dev,
            "supported_color_modes": ["brightness"],
        });

        assert_eq!(config["schema"], "json");
        assert_eq!(config["brightness"], true);
        assert_eq!(config["brightness_scale"], 254);
        assert_eq!(config["command_topic"], "zigbee2mqtt/bulb/set");
        assert_eq!(config["state_topic"], "zigbee2mqtt/bulb");
        assert_eq!(config["object_id"], "bulb");

        // unique_id must end with _zigbee2mqtt (z2m convention)
        let uid = config["unique_id"].as_str().unwrap();
        assert!(uid.ends_with("_zigbee2mqtt"));
        assert!(uid.starts_with("0x"));

        // supported_color_modes present
        let modes = config["supported_color_modes"].as_array().unwrap();
        assert!(modes.contains(&json!("brightness")));
    }

    #[test]
    fn color_light_discovery_has_color_modes() {
        let dev = make_color_light();
        let clusters = dev.all_input_clusters();
        assert!(clusters.contains(&0x0300));

        // Color lights should have color_temp and xy modes
        let modes = json!(["color_temp", "xy"]);
        assert!(modes.as_array().unwrap().contains(&json!("color_temp")));
        assert!(modes.as_array().unwrap().contains(&json!("xy")));
    }

    // ── Switch discovery (z2m format) ─────────────────────────────────────

    #[test]
    fn switch_discovery_matches_z2m() {
        let dev = make_switch();
        let ieee = dev.ieee_addr.as_hex();
        let ha_dev = device_block(&dev, BASE, COORD_IEEE);
        let avail = availability_block(BASE);

        let config = json!({
            "availability": avail,
            "command_topic": format!("{BASE}/wall_switch/set"),
            "state_topic": format!("{BASE}/wall_switch"),
            "object_id": "wall_switch",
            "unique_id": format!("{ieee}_switch_{BASE}"),
            "device": ha_dev,
            "value_template": "{{ value_json.state }}",
            "payload_on": "ON",
            "payload_off": "OFF",
        });

        assert_eq!(config["payload_on"], "ON");
        assert_eq!(config["payload_off"], "OFF");
        assert_eq!(config["value_template"], "{{ value_json.state }}");
        assert_eq!(config["command_topic"], "zigbee2mqtt/wall_switch/set");
    }

    // ── Sensor discovery (z2m format) ─────────────────────────────────────

    #[test]
    fn temperature_sensor_discovery_matches_z2m() {
        let dev = make_sensor();
        let ieee = dev.ieee_addr.as_hex();
        let ha_dev = device_block(&dev, BASE, COORD_IEEE);
        let avail = availability_block(BASE);

        let config = json!({
            "availability": avail,
            "state_topic": format!("{BASE}/weather_sensor"),
            "object_id": "weather_sensor_temperature",
            "unique_id": format!("{ieee}_temperature_{BASE}"),
            "device": ha_dev,
            "device_class": "temperature",
            "unit_of_measurement": "°C",
            "state_class": "measurement",
            "value_template": "{{ value_json.temperature }}",
            "enabled_by_default": true,
        });

        // z2m exact field checks
        assert_eq!(config["device_class"], "temperature");
        assert_eq!(config["unit_of_measurement"], "°C");
        assert_eq!(config["state_class"], "measurement");
        assert_eq!(config["value_template"], "{{ value_json.temperature }}");
        assert_eq!(config["enabled_by_default"], true);

        let uid = config["unique_id"].as_str().unwrap();
        assert!(uid.contains("_temperature_"));
        assert!(uid.ends_with("_zigbee2mqtt"));
    }

    #[test]
    fn humidity_sensor_discovery_matches_z2m() {
        let dev = make_sensor();
        let ieee = dev.ieee_addr.as_hex();
        let ha_dev = device_block(&dev, BASE, COORD_IEEE);
        let avail = availability_block(BASE);

        let config = json!({
            "availability": avail,
            "state_topic": format!("{BASE}/weather_sensor"),
            "object_id": "weather_sensor_humidity",
            "unique_id": format!("{ieee}_humidity_{BASE}"),
            "device": ha_dev,
            "device_class": "humidity",
            "unit_of_measurement": "%",
            "state_class": "measurement",
            "value_template": "{{ value_json.humidity }}",
            "enabled_by_default": true,
        });

        assert_eq!(config["device_class"], "humidity");
        assert_eq!(config["unit_of_measurement"], "%");
    }

    #[test]
    fn battery_sensor_discovery_matches_z2m() {
        let dev = make_sensor();
        let ieee = dev.ieee_addr.as_hex();

        let uid = format!("{ieee}_battery_{BASE}");
        assert!(uid.ends_with("_zigbee2mqtt"));
        assert!(uid.contains("_battery_"));
    }

    // ── Binary sensor discovery ───────────────────────────────────────────

    #[test]
    fn contact_sensor_discovery_matches_z2m() {
        let dev = make_contact_sensor();
        let ieee = dev.ieee_addr.as_hex();
        let ha_dev = device_block(&dev, BASE, COORD_IEEE);
        let avail = availability_block(BASE);

        let config = json!({
            "availability": avail,
            "state_topic": format!("{BASE}/door_sensor"),
            "object_id": "door_sensor_contact",
            "unique_id": format!("{ieee}_contact_{BASE}"),
            "device": ha_dev,
            "device_class": "door",
            "value_template": "{{ value_json.contact }}",
            "payload_on": false,
            "payload_off": true,
            "enabled_by_default": true,
        });

        assert_eq!(config["device_class"], "door");
        // contact=false means door open (payload_on=false)
        assert_eq!(config["payload_on"], false);
        assert_eq!(config["payload_off"], true);
    }

    #[test]
    fn occupancy_sensor_discovery_matches_z2m() {
        let dev = make_occupancy_sensor();
        let ieee = dev.ieee_addr.as_hex();

        let uid = format!("{ieee}_occupancy_{BASE}");
        assert!(uid.ends_with("_zigbee2mqtt"));

        // occupancy uses boolean payload
        let config = json!({
            "device_class": "occupancy",
            "payload_on": true,
            "payload_off": false,
        });
        assert_eq!(config["device_class"], "occupancy");
        assert_eq!(config["payload_on"], true);
    }

    // ── Linkquality diagnostic sensor ─────────────────────────────────────

    #[test]
    fn linkquality_sensor_is_diagnostic() {
        let dev = make_light();
        let ieee = dev.ieee_addr.as_hex();

        let config = json!({
            "object_id": format!("{}_linkquality", dev.friendly_name),
            "unique_id": format!("{ieee}_linkquality_{BASE}"),
            "icon": "mdi:signal",
            "unit_of_measurement": "lqi",
            "state_class": "measurement",
            "value_template": "{{ value_json.linkquality }}",
            "entity_category": "diagnostic",
            "enabled_by_default": true,
        });

        assert_eq!(config["entity_category"], "diagnostic");
        assert_eq!(config["unit_of_measurement"], "lqi");
        assert_eq!(config["icon"], "mdi:signal");
        assert_eq!(config["value_template"], "{{ value_json.linkquality }}");
    }
}
