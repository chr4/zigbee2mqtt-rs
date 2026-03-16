use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Serial port error: {0}")]
    Serial(#[from] tokio_serial::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("MQTT error: {0}")]
    Mqtt(#[from] rumqttc::ClientError),

    #[error("MQTT connection error: {0}")]
    MqttConnection(#[from] rumqttc::ConnectionError),

    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("ZNP protocol error: {0}")]
    Znp(String),

    #[error("ZCL protocol error: {0}")]
    Zcl(String),

    #[error("Coordinator timeout waiting for response")]
    Timeout,

    #[error("Coordinator not initialized")]
    NotInitialized,

    #[error("Channel closed")]
    ChannelClosed,

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Frame too short: expected {expected}, got {got}")]
    FrameTooShort { expected: usize, got: usize },

    #[error("Invalid SOF byte: 0x{0:02X}")]
    InvalidSof(u8),

    #[error("FCS mismatch: expected 0x{expected:02X}, got 0x{got:02X}")]
    FcsMismatch { expected: u8, got: u8 },
}

pub type Result<T> = std::result::Result<T, Error>;
