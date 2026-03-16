use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DeviceConfig {
    pub friendly_name: Option<String>,
    pub retain: Option<bool>,
    pub qos: Option<u8>,
    pub disabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdvancedConfig {
    pub pan_id: u16,
    pub ext_pan_id: [u8; 8],
    pub channel: u8,
    pub network_key: [u8; 16],
    pub log_level: String,
    pub report_state_interval: u64,
    pub cache_state: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            serial: SerialConfig::default(),
            mqtt: MqttConfig::default(),
            permit_join: false,
            homeassistant: false,
            devices: HashMap::new(),
            advanced: AdvancedConfig::default(),
        }
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
            report_state_interval: 0,
            cache_state: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        let config: Config = serde_yaml::from_str(&content)?;
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
        Ok(())
    }
}
