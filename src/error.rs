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

    #[error("Channel closed")]
    ChannelClosed,

    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;
