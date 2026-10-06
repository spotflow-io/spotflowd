//! CBOR encoding and MQTT publishing for metric messages.
//!
//! CBOR key assignments (Spotflow MQTT spec):
//!   0  = messageType (always 5 for metrics)
//!   5  = labels (map of tstr → tstr)
//!   6  = deviceUptimeMs
//!   13 = sequenceNumber
//!   21 = metricName
//!   22 = aggregationInterval  (0=none, 1=1m, 3=1h, 4=1d)
//!   24 = sum  (raw value for AGG_NONE, aggregated sum otherwise)
//!   26 = count (aggregated only)
//!   27 = min   (aggregated only)
//!   28 = max   (aggregated only)

use super::{ReadyMetric, MAX_PENDING};
use crate::status::{QueueStatus, StatusHandle};
use anyhow::{Context, Result};
use ciborium::value::Value as CborValue;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::{watch, Notify};
use tokio::time::{sleep, Duration};
use tracing::warn;

const MESSAGE_TYPE_METRIC: u64 = 5;
const RETRY_INTERVAL: Duration = Duration::from_millis(200);

struct QueuedMetric {
    payload: Vec<u8>,
    queued_at_ms: u64,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<QueuedMetric>,
    // Keep the current reading available for retry without holding the queue
    // lock across a potentially blocked MQTT publish.
    in_flight: Option<QueuedMetric>,
    published: bool,
}

#[derive(Clone)]
pub(super) struct PendingMetrics {
    inner: Arc<Mutex<Queue>>,
    ready: Arc<Notify>,
    status: StatusHandle,
}

impl PendingMetrics {
    pub fn new(status: StatusHandle) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Queue::default())),
            ready: Arc::new(Notify::new()),
            status,
        }
    }

    pub fn enqueue(&self, metrics: Vec<ReadyMetric>, queued_at_ms: u64) -> Result<()> {
        // Encode before changing the queue, so an encoding failure cannot leave
        // its contents and reported status out of sync.
        let metrics = metrics
            .iter()
            .map(|metric| {
                Ok(QueuedMetric {
                    payload: encode_metric(metric)?,
                    queued_at_ms,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut queue = self.inner.lock().expect("metrics queue lock poisoned");
        let mut dropped = 0;
        for metric in metrics {
            if queue.pending.len() + usize::from(queue.in_flight.is_some()) >= MAX_PENDING {
                queue.pending.pop_front();
                dropped += 1;
            }
            queue.pending.push_back(metric);
        }
        if dropped > 0 {
            warn!("metrics pending buffer full, dropping {dropped} oldest readings");
            self.status.record_drop("metrics_pending_full", dropped);
        }
        self.update_status(&queue);
        drop(queue);
        self.ready.notify_one();
        Ok(())
    }

    fn next_payload(&self) -> Option<Vec<u8>> {
        let mut queue = self.inner.lock().expect("metrics queue lock poisoned");
        if queue.in_flight.is_none() {
            queue.in_flight = queue.pending.pop_front();
        }
        queue
            .in_flight
            .as_ref()
            .map(|metric| metric.payload.clone())
    }

    fn complete(&self) {
        let mut queue = self.inner.lock().expect("metrics queue lock poisoned");
        queue.in_flight = None;
        queue.published = true;
        self.update_status(&queue);
    }

    pub fn take_published(&self) -> bool {
        let mut queue = self.inner.lock().expect("metrics queue lock poisoned");
        std::mem::take(&mut queue.published)
    }

    fn update_status(&self, queue: &Queue) {
        let metrics = || queue.pending.iter().chain(queue.in_flight.iter());
        self.status.set_metrics_queue(QueueStatus {
            records: metrics().count() as u64,
            bytes: metrics().map(|metric| metric.payload.len() as u64).sum(),
            oldest_record_ms: metrics().map(|metric| metric.queued_at_ms).min(),
        });
    }
}

/// Delivery runs concurrently with collection. Only this loop awaits MQTT,
/// and shutdown can cancel a publish even when the local MQTT queue is full.
pub(super) async fn run<C, P, F>(
    pending: PendingMetrics,
    connected: C,
    mut publish: P,
    mut shutdown: watch::Receiver<bool>,
) where
    C: Fn() -> bool,
    P: FnMut(Vec<u8>) -> F,
    F: Future<Output = Result<()>>,
{
    loop {
        if *shutdown.borrow() {
            break;
        }
        if connected() {
            if let Some(payload) = pending.next_payload() {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    result = publish(payload) => match result {
                        Ok(()) => {
                            pending.complete();
                            continue;
                        }
                        Err(error) => warn!("metrics publish error: {error}"),
                    }
                }
            }
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = pending.ready.notified() => {},
            _ = sleep(RETRY_INTERVAL) => {},
        }
    }
}

fn encode_metric(m: &ReadyMetric) -> Result<Vec<u8>> {
    let mut map = vec![
        (
            CborValue::Integer(0u64.into()),
            CborValue::Integer(MESSAGE_TYPE_METRIC.into()),
        ),
        (
            CborValue::Integer(21u64.into()),
            CborValue::Text(m.name.clone()),
        ),
        (
            CborValue::Integer(22u64.into()),
            CborValue::Integer((m.agg_cbor as u64).into()),
        ),
    ];

    if !m.labels.is_empty() {
        let label_map: Vec<(CborValue, CborValue)> = m
            .labels
            .iter()
            .map(|(k, v)| (CborValue::Text(k.clone()), CborValue::Text(v.clone())))
            .collect();
        map.push((CborValue::Integer(5u64.into()), CborValue::Map(label_map)));
    }

    map.push((
        CborValue::Integer(6u64.into()),
        CborValue::Integer(m.uptime_ms.into()),
    ));
    map.push((
        CborValue::Integer(13u64.into()),
        CborValue::Integer(m.seq.into()),
    ));
    map.push((CborValue::Integer(24u64.into()), cbor_number(m.sum)));

    // count/min/max only for aggregated intervals (omitted for AGG_NONE per spec).
    if let Some(count) = m.count {
        map.push((
            CborValue::Integer(26u64.into()),
            CborValue::Integer(count.into()),
        ));
        map.push((
            CborValue::Integer(27u64.into()),
            cbor_number(m.min.unwrap_or(m.sum)),
        ));
        map.push((
            CborValue::Integer(28u64.into()),
            cbor_number(m.max.unwrap_or(m.sum)),
        ));
    }

    let mut buf = Vec::new();
    ciborium::into_writer(&CborValue::Map(map), &mut buf).context("CBOR encode metric failed")?;
    Ok(buf)
}

/// Encode a float as an integer when it has no fractional part (smaller payload).
fn cbor_number(v: f64) -> CborValue {
    if v.is_finite() && v.fract() == 0.0 && v >= i64::MIN as f64 && v <= i64::MAX as f64 {
        CborValue::Integer((v as i64).into())
    } else {
        CborValue::Float(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use ciborium::value::Value;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::time::timeout;

    fn queue() -> (PendingMetrics, StatusHandle) {
        let config: Config =
            toml::from_str("[device]\nid = 'test'\ningest_key = 'test'\n").unwrap();
        let status = StatusHandle::new("/tmp/config.toml".into(), &config);
        (PendingMetrics::new(status.clone()), status)
    }

    fn metric(seq: u64) -> ReadyMetric {
        ReadyMetric {
            name: "test".into(),
            agg_cbor: super::super::AGG_NONE,
            labels: vec![],
            sum: 1.0,
            count: None,
            min: None,
            max: None,
            seq,
            uptime_ms: seq,
        }
    }

    fn sequence(payload: &[u8]) -> u64 {
        let Value::Map(fields) = ciborium::from_reader(payload).unwrap() else {
            panic!("expected a metric map");
        };
        fields
            .into_iter()
            .find_map(|(key, value)| match (key, value) {
                (Value::Integer(key), Value::Integer(value)) if key == 13.into() => {
                    Some(u64::try_from(value).unwrap())
                }
                _ => None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn backpressure_keeps_queue_bounded_and_resumes_with_latest_waiting_readings() {
        let (pending, status) = queue();
        pending.enqueue(vec![metric(0)], 10).unwrap();
        let blocked = Arc::new(Notify::new());
        let unblock = Arc::new(Notify::new());
        let (sent_tx, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let publish_blocked = blocked.clone();
        let publish_unblock = unblock.clone();
        let task = tokio::spawn(run(
            pending.clone(),
            || true,
            move |payload| {
                let blocked = publish_blocked.clone();
                let unblock = publish_unblock.clone();
                let sent = sent_tx.clone();
                async move {
                    if sequence(&payload) == 0 {
                        blocked.notify_one();
                        unblock.notified().await;
                    }
                    sent.send(sequence(&payload)).unwrap();
                    Ok(())
                }
            },
            shutdown_rx,
        ));
        timeout(Duration::from_secs(1), blocked.notified())
            .await
            .unwrap();
        pending
            .enqueue((1..=MAX_PENDING as u64 + 5).map(metric).collect(), 20)
            .unwrap();
        let snapshot = status.snapshot();
        assert_eq!(snapshot.queue.records, MAX_PENDING as u64);
        assert_eq!(snapshot.drops_by_reason["metrics_pending_full"], 6);
        assert_eq!(snapshot.queue.oldest_record_ms, Some(10));

        unblock.notify_one();
        timeout(Duration::from_secs(2), async {
            assert_eq!(sent_rx.recv().await, Some(0));
            for expected in 7..=MAX_PENDING as u64 + 5 {
                assert_eq!(sent_rx.recv().await, Some(expected));
            }
        })
        .await
        .unwrap();
        assert_eq!(status.snapshot().queue.records, 0);
        assert!(pending.take_published());
        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn failed_publish_is_retried_after_reconnect_without_replaying_the_prefix() {
        let (pending, status) = queue();
        pending.enqueue(vec![metric(1), metric(2)], 10).unwrap();
        let connected = Arc::new(AtomicBool::new(true));
        let connection = connected.clone();
        let publish_connection = connected.clone();
        let (attempt_tx, mut attempt_rx) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut fail_once = true;
        let task = tokio::spawn(run(
            pending,
            move || connection.load(Ordering::Relaxed),
            move |payload| {
                let seq = sequence(&payload);
                let fail = seq == 2 && fail_once;
                if fail {
                    fail_once = false;
                    publish_connection.store(false, Ordering::Relaxed);
                }
                attempt_tx.send(seq).unwrap();
                async move {
                    if fail {
                        anyhow::bail!("connection lost");
                    }
                    Ok(())
                }
            },
            shutdown_rx,
        ));
        timeout(Duration::from_secs(1), async {
            assert_eq!(attempt_rx.recv().await, Some(1));
            assert_eq!(attempt_rx.recv().await, Some(2));
        })
        .await
        .unwrap();
        assert_eq!(status.snapshot().queue.records, 1);
        connected.store(true, Ordering::Relaxed);
        assert_eq!(
            timeout(Duration::from_secs(1), attempt_rx.recv())
                .await
                .unwrap(),
            Some(2)
        );
        assert_eq!(status.snapshot().queue.records, 0);
        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }
}
