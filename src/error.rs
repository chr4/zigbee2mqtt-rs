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
    MqttConnection(Box<rumqttc::ConnectionError>),

    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yml::Error),

    #[error("ZNP protocol error: {0}")]
    Znp(String),

    /// The ZNP firmware answered an SREQ with an RPC error (e.g. a command
    /// this Z-Stack version doesn't implement).
    #[error("ZNP rejected command sub=0x{subsystem:02X} cmd1=0x{cmd1:02X}: {reason}")]
    ZnpRejected {
        subsystem: u8,
        cmd1: u8,
        reason: &'static str,
    },

    /// The coordinator could not deliver a frame to a device: either the
    /// AF_DATA_REQUEST SRSP or the later AF_DATA_CONFIRM carried a non-zero
    /// Z-Stack status (no route, no ack, buffer full, ...).
    #[error("delivery to 0x{nwk:04X} failed: {} (0x{status:02X})", crate::coordinator::znp::commands::status_name(*status))]
    DeliveryFailed { nwk: u16, status: u8 },

    #[error("ZCL protocol error: {0}")]
    Zcl(String),

    #[error("Coordinator timeout waiting for response")]
    Timeout,

    #[error("Channel closed")]
    ChannelClosed,

    #[error("Configuration error: {0}")]
    Config(String),
}

impl From<rumqttc::ConnectionError> for Error {
    fn from(e: rumqttc::ConnectionError) -> Self {
        Error::MqttConnection(Box::new(e))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
