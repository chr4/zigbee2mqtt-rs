/// MQTT bridge – publishes device state and subscribes to set/get commands.
/// Message formats are compatible with zigbee2mqtt for Home Assistant integration.
use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::MqttConfig;
use crate::error::{Error, Result};

// ── Inbound MQTT commands ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum MqttCommand {
    PermitJoin { duration: u8 },
    SetDevice {
        friendly_name: String,
        payload: serde_json::Value,
    },
    GetDevice {
        friendly_name: String,
    },
}

// ── MqttBridge ────────────────────────────────────────────────────────────────

pub struct MqttBridge {
    client: AsyncClient,
    base_topic: String,
}

impl MqttBridge {
    /// Connect to the broker, subscribe to command topics, and spawn the event loop.
    pub fn connect(cfg: &MqttConfig) -> Result<(Self, mpsc::Receiver<MqttCommand>)> {
        let mut opts = MqttOptions::new(&cfg.client_id, &cfg.server, cfg.port);
        opts.set_keep_alive(Duration::from_secs(cfg.keepalive as u64));
        opts.set_clean_session(true);

        if let (Some(user), Some(pass)) = (&cfg.username, &cfg.password) {
            opts.set_credentials(user, pass);
        }

        // Last-will: bridge goes offline (z2m JSON format)
        let will_topic = format!("{}/bridge/state", cfg.base_topic);
        let will_payload = serde_json::to_vec(&json!({"state": "offline"})).unwrap();
        opts.set_last_will(rumqttc::LastWill::new(
            &will_topic,
            will_payload,
            QoS::AtLeastOnce,
            true,
        ));

        let (client, event_loop) = AsyncClient::new(opts, 64);
        let (cmd_tx, cmd_rx) = mpsc::channel::<MqttCommand>(64);

        let base_topic = cfg.base_topic.clone();
        let client_clone = client.clone();
        let cmd_tx_clone = cmd_tx.clone();

        tokio::spawn(async move {
            run_event_loop(event_loop, client_clone, &base_topic, cmd_tx_clone).await;
        });

        Ok((
            Self {
                client,
                base_topic: cfg.base_topic.clone(),
            },
            cmd_rx,
        ))
    }

    // ── Publish helpers ───────────────────────────────────────────────────────

    /// Publish bridge/state as JSON (z2m format).
    pub async fn publish_bridge_state(&self, online: bool) -> Result<()> {
        let topic = format!("{}/bridge/state", self.base_topic);
        let state = if online { "online" } else { "offline" };
        let payload = serde_json::to_vec(&json!({"state": state}))?;
        self.publish_retained(&topic, &payload).await
    }

    /// Publish bridge/info (z2m format).
    pub async fn publish_bridge_info(&self, info: &serde_json::Value) -> Result<()> {
        let topic = format!("{}/bridge/info", self.base_topic);
        let payload = serde_json::to_vec(info)?;
        self.publish_retained(&topic, &payload).await
    }

    pub async fn publish_device_state(
        &self,
        friendly_name: &str,
        state: &serde_json::Value,
    ) -> Result<()> {
        let topic = format!("{}/{}", self.base_topic, friendly_name);
        let payload = serde_json::to_vec(state)?;
        self.publish_retained(&topic, &payload).await
    }

    pub async fn publish_bridge_devices(&self, devices: &serde_json::Value) -> Result<()> {
        let topic = format!("{}/bridge/devices", self.base_topic);
        let payload = serde_json::to_vec(devices)?;
        self.publish_retained(&topic, &payload).await
    }

    /// Publish to bridge/logging (z2m format).
    pub async fn publish_bridge_log(&self, level: &str, message: &str) -> Result<()> {
        let topic = format!("{}/bridge/logging", self.base_topic);
        let payload = json!({
            "level": level,
            "message": message,
        });
        let bytes = serde_json::to_vec(&payload)?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, bytes)
            .await
            .map_err(Error::Mqtt)
    }

    /// Publish a Home Assistant MQTT discovery config.
    pub async fn publish_ha_discovery(
        &self,
        component: &str,
        node_id: &str,
        object_id: &str,
        config: &serde_json::Value,
    ) -> Result<()> {
        let topic = format!("homeassistant/{component}/{node_id}/{object_id}/config");
        let payload = serde_json::to_vec(config)?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
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
    client: AsyncClient,
    base_topic: &str,
    cmd_tx: mpsc::Sender<MqttCommand>,
) {
    // Wait for the first ConnAck
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
                    let is_set = topic.ends_with("/set");
                    let suffix = if is_set { "/set" } else { "/get" };
                    let name = topic
                        .trim_start_matches(base_topic)
                        .trim_start_matches('/')
                        .trim_end_matches(suffix)
                        .to_string();

                    let json_value = serde_json::from_slice::<serde_json::Value>(payload)
                        .unwrap_or_else(|_| {
                            if let Ok(s) = std::str::from_utf8(payload) {
                                json!({ "state": s.trim() })
                            } else {
                                serde_json::Value::Null
                            }
                        });

                    let cmd = if is_set {
                        MqttCommand::SetDevice {
                            friendly_name: name,
                            payload: json_value,
                        }
                    } else {
                        MqttCommand::GetDevice {
                            friendly_name: name,
                        }
                    };
                    let _ = cmd_tx.send(cmd).await;
                } else if topic.contains("permit_join") {
                    let duration = parse_permit_join_payload(payload);
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

/// Parse permit_join payload. Supports z2m JSON format and plain number.
fn parse_permit_join_payload(payload: &[u8]) -> u8 {
    // Try JSON: {"value": true, "time": 254} or {"value": true}
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
        if let Some(time) = v.get("time").and_then(|t| t.as_u64()) {
            return time.min(254) as u8;
        }
        if let Some(val) = v.get("value") {
            if val.as_bool() == Some(true) {
                return 254;
            }
            if val.as_bool() == Some(false) {
                return 0;
            }
            if let Some(n) = val.as_u64() {
                return n.min(254) as u8;
            }
        }
    }
    // Plain number
    std::str::from_utf8(payload)
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(254)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_permit_join_json_with_time() {
        assert_eq!(
            parse_permit_join_payload(br#"{"value": true, "time": 120}"#),
            120
        );
    }

    #[test]
    fn parse_permit_join_json_true() {
        assert_eq!(parse_permit_join_payload(br#"{"value": true}"#), 254);
    }

    #[test]
    fn parse_permit_join_json_false() {
        assert_eq!(parse_permit_join_payload(br#"{"value": false}"#), 0);
    }

    #[test]
    fn parse_permit_join_plain_number() {
        assert_eq!(parse_permit_join_payload(b"120"), 120);
    }

    #[test]
    fn parse_permit_join_empty() {
        assert_eq!(parse_permit_join_payload(b""), 254);
    }
}
