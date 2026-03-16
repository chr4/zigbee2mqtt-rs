use std::path::PathBuf;

use clap::Parser;
use tracing::error;
use tracing_subscriber::{fmt, EnvFilter};

use zigbee2mqtt_rs::bridge::Bridge;
use zigbee2mqtt_rs::config::Config;

#[derive(Debug, Parser)]
#[command(name = "zigbee2mqtt-rs", about = "Zigbee to MQTT bridge")]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "configuration.yaml")]
    config: PathBuf,

    /// Log level (trace, debug, info, warn, error)
    #[arg(short, long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let cfg = match Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config {}: {e}", args.config.display());
            std::process::exit(1);
        }
    };

    let log_level = args
        .log_level
        .as_deref()
        .or(Some(cfg.advanced.log_level.as_str()))
        .unwrap_or("info");

    let filter = EnvFilter::try_new(format!("zigbee2mqtt_rs={log_level},rumqttc=warn"))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    tracing::info!(
        "zigbee2mqtt-rs v{} starting (serial={}, mqtt={}:{})",
        env!("CARGO_PKG_VERSION"),
        cfg.serial.port,
        cfg.mqtt.server,
        cfg.mqtt.port
    );

    let bridge = Bridge::new(cfg, args.config.clone());

    if let Err(e) = bridge.run().await {
        error!("Bridge error: {e}");
        std::process::exit(1);
    }
}
