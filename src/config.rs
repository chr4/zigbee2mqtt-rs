use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub serial: SerialConfig,
    pub mqtt: MqttConfig,
    pub permit_join: bool,
    pub homeassistant: bool,
    pub devices: HashMap<String, DeviceConfig>,
    pub advanced: AdvancedConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SerialConfig {
    pub port: String,
    pub baudrate: u32,
    pub adapter: AdapterType,
    pub rtscts: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AdapterType {
    Znp,
    Ezsp,
    Auto,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MqttConfig {
    pub server: String,
    pub port: u16,
    pub base_topic: String,
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub keepalive: u16,
    pub reject_unauthorized: bool,
}

impl std::fmt::Debug for MqttConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MqttConfig")
            .field("server", &self.server)
            .field("port", &self.port)
            .field("base_topic", &self.base_topic)
            .field("client_id", &self.client_id)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("keepalive", &self.keepalive)
            .field("reject_unauthorized", &self.reject_unauthorized)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DeviceConfig {
    pub friendly_name: Option<String>,
    pub retain: Option<bool>,
    pub qos: Option<u8>,
    pub disabled: Option<bool>,
}

/// How device state is published to MQTT (z2m: `advanced.output`).
///
/// - `Json`: single merged JSON message to `<base_topic>/<friendly_name>`
///   (this project's only behavior prior to this option -- still the
///   default, and required for the Home Assistant discovery configs in
///   `homeassistant.rs`, which reference the JSON state_topic/value_template).
/// - `Attribute`: each state key published as its own raw-value subtopic,
///   e.g. `<base_topic>/<friendly_name>/action` with payload `toggle`
///   (unquoted). No merged JSON message is published in this mode.
/// - `AttributeAndJson`: both of the above.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    #[default]
    Json,
    Attribute,
    AttributeAndJson,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdvancedConfig {
    pub pan_id: u16,
    pub ext_pan_id: [u8; 8],
    pub channel: u8,
    pub network_key: [u8; 16],
    pub log_level: String,
    pub cache_state: bool,
    pub output: OutputMode,
}

impl std::fmt::Debug for AdvancedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvancedConfig")
            .field("pan_id", &self.pan_id)
            .field("ext_pan_id", &self.ext_pan_id)
            .field("channel", &self.channel)
            .field("network_key", &"<redacted>")
            .field("log_level", &self.log_level)
            .field("cache_state", &self.cache_state)
            .field("output", &self.output)
            .finish()
    }
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            port: "/dev/ttyACM0".to_string(),
            baudrate: 115_200,
            adapter: AdapterType::Auto,
            rtscts: false,
        }
    }
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            server: "localhost".to_string(),
            port: 1883,
            base_topic: "zigbee2mqtt".to_string(),
            client_id: "zigbee2mqtt-rs".to_string(),
            username: None,
            password: None,
            keepalive: 60,
            reject_unauthorized: true,
        }
    }
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            pan_id: 0x1a62,
            ext_pan_id: [0xDD, 0xDD, 0xDD, 0xDD, 0xDD, 0xDD, 0xDD, 0xDD],
            channel: 11,
            network_key: [1, 3, 5, 7, 9, 11, 13, 15, 0, 2, 4, 6, 8, 10, 12, 13],
            log_level: "info".to_string(),
            cache_state: true,
            output: OutputMode::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        let config: Config = serde_yml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.advanced.channel < 11 || self.advanced.channel > 26 {
            return Err(Error::Config(format!(
                "Zigbee channel must be 11-26, got {}",
                self.advanced.channel
            )));
        }
        if self.serial.port.trim().is_empty() {
            return Err(Error::Config("serial.port must not be empty".to_string()));
        }
        if self.mqtt.server.trim().is_empty() {
            return Err(Error::Config("mqtt.server must not be empty".to_string()));
        }
        if self.mqtt.base_topic.trim().is_empty() {
            return Err(Error::Config("mqtt.base_topic must not be empty".to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mqtt_config_debug_redacts_password() {
        let cfg = MqttConfig {
            password: Some("hunter2".to_string()),
            ..MqttConfig::default()
        };
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn advanced_config_debug_redacts_network_key() {
        let cfg = AdvancedConfig::default();
        let debug = format!("{cfg:?}");
        let raw_key_debug = format!("{:?}", cfg.network_key);
        assert!(!debug.contains(&raw_key_debug));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn validate_accepts_defaults() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_channel_out_of_range() {
        let mut cfg = Config::default();
        cfg.advanced.channel = 30;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_serial_port() {
        let mut cfg = Config::default();
        cfg.serial.port = "  ".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_mqtt_server() {
        let mut cfg = Config::default();
        cfg.mqtt.server = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_base_topic() {
        let mut cfg = Config::default();
        cfg.mqtt.base_topic = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn output_mode_defaults_to_json() {
        assert_eq!(AdvancedConfig::default().output, OutputMode::Json);
        // Omitting `output` entirely from YAML must default to Json too.
        let cfg: AdvancedConfig = serde_yml::from_str("channel: 15").unwrap();
        assert_eq!(cfg.output, OutputMode::Json);
    }

    #[test]
    fn output_mode_parses_all_three_z2m_values() {
        assert_eq!(
            serde_yml::from_str::<OutputMode>("json").unwrap(),
            OutputMode::Json
        );
        assert_eq!(
            serde_yml::from_str::<OutputMode>("attribute").unwrap(),
            OutputMode::Attribute
        );
        assert_eq!(
            serde_yml::from_str::<OutputMode>("attribute_and_json").unwrap(),
            OutputMode::AttributeAndJson
        );
    }

    #[test]
    fn output_mode_rejects_unknown_value() {
        assert!(serde_yml::from_str::<OutputMode>("bogus").is_err());
    }

    #[test]
    fn shipped_example_config_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.yaml");
        let content = std::fs::read_to_string(&path).expect("config.example.yaml must exist");
        let cfg: Config = serde_yml::from_str(&content).expect("must parse as valid Config");
        // `advanced: {}` in the example must resolve to actual defaults, not
        // some parsed-but-empty/null state.
        assert_eq!(cfg.advanced.channel, 11);
        assert_eq!(cfg.advanced.output, OutputMode::Json);
        cfg.validate().expect("example config must pass validation");
    }
}
