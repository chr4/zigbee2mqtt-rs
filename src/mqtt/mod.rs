/// MQTT bridge – publishes device state and subscribes to set/get commands.
use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::MqttConfig;
use crate::error::{Error, Result};

// ── Inbound MQTT commands ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum MqttCommand {
    /// `<base>/bridge/request/permit_join`
    PermitJoin { duration: u8 },
    /// `<base>/<name>/set`
    SetDevice { friendly_name: String, payload: serde_json::Value },
    /// `<base>/<name>/get`
    GetDevice { friendly_name: String, payload: serde_json::Value },
}

// ── MqttBridge ────────────────────────────────────────────────────────────────

pub struct MqttBridge {
    client:     AsyncClient,
    base_topic: String,
    cmd_tx:     mpsc::Sender<MqttCommand>,
}

impl MqttBridge {
    /// Connect to the broker, subscribe to command topics, and spawn the event loop.
    pub fn connect(cfg: &MqttConfig) -> Result<(Self, mpsc::Receiver<MqttCommand>)> {
        let mut opts = MqttOptions::new(
            &cfg.client_id,
            &cfg.server,
            cfg.port,
        );
        opts.set_keep_alive(Duration::from_secs(cfg.keepalive as u64));
        opts.set_clean_session(true);

        if let (Some(user), Some(pass)) = (&cfg.username, &cfg.password) {
            opts.set_credentials(user, pass);
        }

        // Last-will message so consumers know the bridge went offline
        let will_topic = format!("{}/bridge/state", cfg.base_topic);
        opts.set_last_will(rumqttc::LastWill::new(
            &will_topic,
            b"offline".to_vec(),
            QoS::AtLeastOnce,
            true,
        ));

        let (client, event_loop) = AsyncClient::new(opts, 64);
        let (cmd_tx, cmd_rx) = mpsc::channel::<MqttCommand>(64);

        let base_topic  = cfg.base_topic.clone();
        let client_clone = client.clone();
        let cmd_tx_clone = cmd_tx.clone();

        // Spawn the receive loop
        tokio::spawn(async move {
            run_event_loop(event_loop, client_clone, &base_topic, cmd_tx_clone).await;
        });

        Ok((
            Self { client, base_topic: cfg.base_topic.clone(), cmd_tx },
            cmd_rx,
        ))
    }

    // ── Publish helpers ───────────────────────────────────────────────────────

    pub async fn publish_bridge_state(&self, online: bool) -> Result<()> {
        let topic = format!("{}/bridge/state", self.base_topic);
        let payload = if online { "online" } else { "offline" };
        self.publish_retained(&topic, payload.as_bytes()).await
    }

    pub async fn publish_device_state(&self, friendly_name: &str, state: &serde_json::Value) -> Result<()> {
        let topic = format!("{}/{}", self.base_topic, friendly_name);
        let payload = serde_json::to_vec(state).map_err(Error::Serde)?;
        self.publish_retained(&topic, &payload).await
    }

    pub async fn publish_bridge_devices(&self, devices: &serde_json::Value) -> Result<()> {
        let topic   = format!("{}/bridge/devices", self.base_topic);
        let payload = serde_json::to_vec(devices).map_err(Error::Serde)?;
        self.publish_retained(&topic, &payload).await
    }

    pub async fn publish_bridge_log(&self, level: &str, message: &str) -> Result<()> {
        let topic = format!("{}/bridge/log", self.base_topic);
        let payload = serde_json::json!({
            "level":   level,
            "message": message,
        });
        let bytes = serde_json::to_vec(&payload).map_err(Error::Serde)?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, bytes)
            .await
            .map_err(Error::Mqtt)
    }

    async fn publish_retained(&self, topic: &str, payload: &[u8]) -> Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, true, payload.to_vec())
            .await
            .map_err(Error::Mqtt)
    }
}

// ── Event loop task ───────────────────────────────────────────────────────────

async fn run_event_loop(
    mut event_loop: EventLoop,
    client:         AsyncClient,
    base_topic:     &str,
    cmd_tx:         mpsc::Sender<MqttCommand>,
) {
    // Wait for the first ConnAck before subscribing
    loop {
        match event_loop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => break,
            Ok(_) => {}
            Err(e) => {
                error!("MQTT connect error: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    // Subscribe to command topics
    let set_wildcard = format!("{}/+/set", base_topic);
    let get_wildcard = format!("{}/+/get", base_topic);
    let permit_topic = format!("{}/bridge/request/permit_join", base_topic);
    for topic in &[&set_wildcard, &get_wildcard, &permit_topic] {
        if let Err(e) = client.subscribe(*topic, QoS::AtLeastOnce).await {
            error!("MQTT subscribe error for {topic}: {e}");
        }
    }
    info!("MQTT connected and subscribed");

    loop {
        match event_loop.poll().await {
            Ok(Event::Incoming(Packet::Publish(pub_msg))) => {
                let topic = &pub_msg.topic;
                let payload = &pub_msg.payload;

                if topic.ends_with("/set") || topic.ends_with("/get") {
                    let (is_set, name) = if topic.ends_with("/set") {
                        let name = topic
                            .trim_start_matches(base_topic)
                            .trim_start_matches('/')
                            .trim_end_matches("/set")
                            .to_string();
                        (true, name)
                    } else {
                        let name = topic
                            .trim_start_matches(base_topic)
                            .trim_start_matches('/')
                            .trim_end_matches("/get")
                            .to_string();
                        (false, name)
                    };

                    let json_value = serde_json::from_slice::<serde_json::Value>(payload)
                        .unwrap_or_else(|_| {
                            // Treat raw strings like `"ON"` as `{"state":"ON"}`
                            if let Ok(s) = std::str::from_utf8(payload) {
                                serde_json::json!({ "state": s.trim() })
                            } else {
                                serde_json::Value::Null
                            }
                        });

                    let cmd = if is_set {
                        MqttCommand::SetDevice { friendly_name: name, payload: json_value }
                    } else {
                        MqttCommand::GetDevice { friendly_name: name, payload: json_value }
                    };
                    let _ = cmd_tx.send(cmd).await;
                } else if topic.contains("permit_join") {
                    let duration = std::str::from_utf8(payload)
                        .ok()
                        .and_then(|s| s.trim().parse::<u8>().ok())
                        .unwrap_or(254);
                    let _ = cmd_tx.send(MqttCommand::PermitJoin { duration }).await;
                }
            }
            Ok(Event::Incoming(Packet::Disconnect)) => {
                warn!("MQTT disconnected, reconnecting…");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Ok(_) => {}
            Err(e) => {
                error!("MQTT event loop error: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}
