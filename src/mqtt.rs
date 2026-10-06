//! Persistent MQTT client for the Spotflow platform.
//!
//! Maintains a single TLS connection to `mqtt.spotflow.io:8883`.
//! Publishes CBOR-encoded logs, metrics, and crash dumps on `ingest-cbor`
//! with QoS 0 (best effort, without broker acknowledgements).
//! rumqttc handles reconnection internally; we track connection state via
//! events so the orchestrator knows whether to drain the buffer or hold.

use crate::config::MqttConfig;
use crate::log_entry::{LabelValue, LogEntry};
use crate::status::StatusHandle;
use anyhow::{Context, Result};
use ciborium::value::Value as CborValue;
use rumqttc::{
    AsyncClient, Event, MqttOptions, Outgoing, Packet, QoS, TlsConfiguration, Transport,
};
use rustls::ClientConfig;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

const INGEST_TOPIC: &str = "ingest-cbor";

// ---------------------------------------------------------------------------
// CBOR encoding
// ---------------------------------------------------------------------------

/// Encode a single log entry as a CBOR map per the Spotflow spec:
///   1 = body, 4 = severity, 6 = deviceUptimeMs, 7 = deviceTimestampMs
/// One MQTT message is published per entry.
fn encode_entry(entry: &LogEntry) -> Result<Vec<u8>> {
    let mut map = vec![(
        CborValue::Integer(1u64.into()),
        CborValue::Text(entry.body.clone()),
    )];
    if let Some(severity) = entry.severity {
        map.push((
            CborValue::Integer(4u64.into()),
            CborValue::Integer((severity as u64).into()),
        ));
    }
    if !entry.labels.is_empty() {
        let label_map: Vec<(CborValue, CborValue)> = entry
            .labels
            .iter()
            .map(|(k, v)| {
                let key = CborValue::Text(k.clone());
                let val = match v {
                    LabelValue::Str(s) => CborValue::Text(s.clone()),
                    LabelValue::Int(i) => CborValue::Integer((*i).into()),
                };
                (key, val)
            })
            .collect();
        map.push((CborValue::Integer(5u64.into()), CborValue::Map(label_map)));
    }
    if let Some(uptime) = entry.uptime_ms {
        map.push((
            CborValue::Integer(6u64.into()),
            CborValue::Integer(uptime.into()),
        ));
    }
    if let Some(ts) = entry.timestamp_ms {
        map.push((
            CborValue::Integer(7u64.into()),
            CborValue::Integer(ts.into()),
        ));
    }
    let mut buf = Vec::new();
    ciborium::into_writer(&CborValue::Map(map), &mut buf).context("CBOR encode failed")?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Publisher handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MqttPublisher {
    client: AsyncClient,
    connected: Arc<AtomicBool>,
}

impl MqttPublisher {
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Queue a pre-encoded CBOR payload to the ingest topic with QoS 0.
    /// Success means accepted by the local MQTT queue, not delivered to the broker.
    pub async fn publish_payload(&self, payload: Vec<u8>) -> Result<()> {
        self.client
            .publish(INGEST_TOPIC, QoS::AtMostOnce, false, payload)
            .await
            .context("MQTT publish failed")
    }

    /// Queue a slice of entries — one MQTT message per entry (QoS 0).
    pub async fn publish_batch(&self, entries: &[LogEntry]) -> Result<()> {
        for entry in entries {
            self.publish_payload(encode_entry(entry)?).await?;
        }
        if !entries.is_empty() {
            debug!("queued {} log entries for MQTT", entries.len());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Connection setup + event loop
// ---------------------------------------------------------------------------

fn build_client_config() -> Arc<ClientConfig> {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().unwrap_or_default() {
        root_store.add(cert).ok(); // skip any malformed certs silently
    }
    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Arc::new(config)
}

/// Create the MQTT client + spawn the event loop task.
/// Returns a `MqttPublisher` handle the orchestrator uses to publish.
pub fn start(
    device_id: &str,
    ingest_key: &str,
    cfg: &MqttConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    status: StatusHandle,
) -> Result<MqttPublisher> {
    let mut options = MqttOptions::new(device_id, &cfg.broker, cfg.port);
    options.set_credentials(device_id, ingest_key);
    options.set_keep_alive(Duration::from_secs(cfg.keepalive_secs));
    options.set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(
        build_client_config(),
    )));

    let (client, mut eventloop) = AsyncClient::new(options, 64);

    let connected = Arc::new(AtomicBool::new(false));
    let connected_flag = connected.clone();
    let event_status = status;

    // Event loop task — must be polled continuously for rumqttc to function.
    // rumqttc handles reconnection automatically; we just watch for state changes.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = eventloop.poll() => {
                    match result {
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            info!("MQTT connected to Spotflow platform");
                            connected_flag.store(true, Ordering::Relaxed);
                            event_status.connected();
                        }
                        Ok(Event::Outgoing(Outgoing::Publish(_))) => event_status.publish_sent(),
                        Ok(_) => {}
                        Err(e) => {
                            if connected_flag.load(Ordering::Relaxed) {
                                warn!("MQTT connection lost: {e}");
                                connected_flag.store(false, Ordering::Relaxed);
                            } else {
                                debug!("MQTT reconnecting: {e}");
                            }
                            event_status.disconnected(&e);
                            // rumqttc will retry automatically; brief yield before next poll
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    debug!("MQTT event loop shutting down");
                    break;
                }
            }
        }
    });

    Ok(MqttPublisher { client, connected })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    async fn read_packet(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let header = stream.read_u8().await.unwrap();
        let mut remaining_len = 0usize;
        let mut multiplier = 1;
        loop {
            let byte = stream.read_u8().await.unwrap();
            remaining_len += usize::from(byte & 0x7f) * multiplier;
            if byte & 0x80 == 0 {
                break;
            }
            multiplier *= 128;
            assert!(multiplier <= 128 * 128 * 128);
        }
        let mut body = vec![0; remaining_len];
        stream.read_exact(&mut body).await.unwrap();
        (header, body)
    }

    #[tokio::test]
    async fn logs_and_payloads_use_qos_zero_without_publish_acknowledgements() {
        tokio::time::timeout(Duration::from_secs(5), async {
            // More than rumqttc's default 100 in-flight QoS 1 messages: publishing
            // must continue even though this broker never sends a PUBACK.
            const LOG_COUNT: usize = 128;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let mut broker = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert_eq!(read_packet(&mut stream).await.0, 0x10);
                stream.write_all(&[0x20, 0x02, 0x00, 0x00]).await.unwrap();
                let mut payloads = Vec::new();
                for _ in 0..=LOG_COUNT {
                    let (header, body) = read_packet(&mut stream).await;
                    // PUBLISH, QoS 0, no DUP, no retain, and no packet identifier.
                    assert_eq!(header, 0x30);
                    let topic_len = usize::from(u16::from_be_bytes([body[0], body[1]]));
                    assert_eq!(&body[2..2 + topic_len], INGEST_TOPIC.as_bytes());
                    payloads.push(body[2 + topic_len..].to_vec());
                }
                payloads
            });

            let (client, mut eventloop) =
                AsyncClient::new(MqttOptions::new("test", "127.0.0.1", port), 64);
            let publisher = MqttPublisher {
                client,
                connected: Arc::new(AtomicBool::new(false)),
            };
            let producer = tokio::spawn(async move {
                let entries: Vec<_> = (0..LOG_COUNT)
                    .map(|index| LogEntry {
                        body: format!("log {index}"),
                        severity: None,
                        timestamp_ms: None,
                        uptime_ms: None,
                        source: "test".into(),
                        labels: Default::default(),
                        queued_at_ms: 0,
                    })
                    .collect();
                publisher.publish_batch(&entries).await.unwrap();
                // Metrics and crash dumps share this pre-encoded publish path.
                publisher
                    .publish_payload(vec![0xa1, 0x00, 0x05])
                    .await
                    .unwrap();
            });

            let payloads = loop {
                tokio::select! {
                    result = &mut broker => break result.unwrap(),
                    result = eventloop.poll() => {
                        // The broker may close after receiving the final publish.
                        if result.is_err() {
                            break broker.await.unwrap();
                        }
                    }
                }
            };
            producer.await.unwrap();
            let first: CborValue = ciborium::from_reader(payloads[0].as_slice()).unwrap();
            assert_eq!(
                first,
                CborValue::Map(vec![(
                    CborValue::Integer(1.into()),
                    CborValue::Text("log 0".into())
                )])
            );
            assert_eq!(payloads.last().unwrap(), &[0xa1, 0x00, 0x05]);
        })
        .await
        .expect("QoS 0 publishing stalled without broker publish acknowledgements");
    }

    #[tokio::test]
    async fn publish_reports_a_closed_local_queue() {
        let (client, eventloop) = AsyncClient::new(MqttOptions::new("test", "127.0.0.1", 1883), 64);
        drop(eventloop);
        let publisher = MqttPublisher {
            client,
            connected: Arc::new(AtomicBool::new(false)),
        };
        assert!(publisher.publish_payload(vec![0xa0]).await.is_err());
    }
}
