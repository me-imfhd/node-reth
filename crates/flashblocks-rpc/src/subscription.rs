use std::{sync::Arc, time::Duration};

use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use futures_util::{SinkExt as _, StreamExt};
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::time::{interval, Instant, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, trace, warn};
use url::Url;

use crate::metrics::Metrics;

/// Interval of liveness check of upstream, in milliseconds.
pub const PING_INTERVAL_MS: u64 = 15_000;

/// Max duration of backoff before reconnecting to upstream.
pub const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Max duration of timeout for sending a ping to upstream.
pub const PING_SEND_TIMEOUT_MS: u64 = 250;

/// Max duration of idle timeout before reconnecting to upstream.
pub const IDLE_TIMEOUT_MS: u64 = 60_000;

pub trait FlashblocksReceiver {
    fn on_flashblock_received(&self, flashblock: Flashblock);
}

#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq)]
pub struct Metadata {
    pub receipts: HashMap<B256, OpReceipt>,
    pub new_account_balances: HashMap<Address, U256>,
    pub block_number: u64,
}

#[derive(Clone, Debug, PartialEq, Default, Deserialize, Serialize)]
pub struct Flashblock {
    /// The payload id of the flashblock
    pub payload_id: PayloadId,
    /// The index of the flashblock in the block
    pub index: u64,
    /// The base execution payload configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<ExecutionPayloadBaseV1>,
    /// The delta/diff containing modified portions of the execution payload
    pub diff: ExecutionPayloadFlashblockDeltaV1,
    /// Additional metadata associated with the flashblock
    pub metadata: Metadata,
}

pub struct FlashblocksSubscriber<Receiver> {
    flashblocks_state: Arc<Receiver>,
    metrics: Metrics,
    ws_url: Url,
}

impl<Receiver> FlashblocksSubscriber<Receiver>
where
    Receiver: FlashblocksReceiver + Send + Sync + 'static,
{
    pub fn new(flashblocks_state: Arc<Receiver>, ws_url: Url) -> Self {
        Self {
            ws_url,
            flashblocks_state,
            metrics: Metrics::default(),
        }
    }

    pub fn start(&mut self) {
        info!(
            message = "Starting Flashblocks subscription",
            url = %self.ws_url,
        );

        let ws_url = self.ws_url.clone();

        let flashblocks_state = self.flashblocks_state.clone();
        let metrics = self.metrics.clone();

        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);

            loop {
                let ws_stream = match connect_async(ws_url.as_str()).await {
                    Ok((ws_stream, _)) => ws_stream,
                    Err(e) => {
                        error!(
                            message = "WebSocket connection error, retrying",
                            backoff_duration = ?backoff,
                            error = %e
                        );

                        backoff = sleep(&metrics, backoff).await;
                        continue;
                    }
                };

                info!(message = "WebSocket connection established");
                // Reset backoff to 1 second
                backoff = Duration::from_secs(1);

                let idle = tokio::time::sleep(Duration::from_millis(IDLE_TIMEOUT_MS));
                tokio::pin!(idle);

                let (mut write, mut read) = ws_stream.split();

                let (ping_err_tx, mut ping_err_rx) = oneshot::channel::<()>();
                let ping_handle = tokio::spawn({
                    async move {
                        let mut ping_interval = interval(Duration::from_millis(PING_INTERVAL_MS));
                        ping_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
                        loop {
                            ping_interval.tick().await;
                            let fut = write.send(Message::Ping(Default::default()));
                            match tokio::time::timeout(
                                Duration::from_millis(PING_SEND_TIMEOUT_MS),
                                fut,
                            )
                            .await
                            {
                                Ok(Ok(())) => {} // sent
                                Ok(Err(_)) | Err(_) => {
                                    let _ = ping_err_tx.send(()); // notify reader and exit
                                    break;
                                }
                            }
                        }
                    }
                });

                'conn: loop {
                    tokio::select! {
                        _ = &mut idle => {
                            warn!(
                                target: "flashblocks_rpc::subscription",
                                ?backoff,
                                "Idle timeout, reconnecting"
                            );
                            backoff = sleep(&metrics, backoff).await;
                            break 'conn;
                        }
                        Some(msg) = read.next() => {
                            metrics.upstream_messages.increment(1);
                            idle.as_mut().reset(Instant::now() + Duration::from_millis(IDLE_TIMEOUT_MS));

                            match msg {
                                Ok(Message::Binary(bytes)) => {
                                    let decode_time = Instant::now();
                                    // Do in a blocking task to avoid blocking the async task scheduler
                                    let decoded = match tokio::task::spawn_blocking(move || try_decode_message(&bytes)).await {
                                        Ok(decoded) => decoded,
                                        Err(e) => {
                                                error!(
                                                    message = "error joining blocking websocket decode task",
                                                    error = %e
                                                );
                                                continue;
                                            }
                                    };
                                    metrics.websocket_decode_duration.record(decode_time.elapsed());
                                    match decoded {
                                        Ok(payload) => {
                                            flashblocks_state.on_flashblock_received(payload);
                                        }
                                        Err(e) => {
                                            error!(
                                                message = "error decoding flashblock message",
                                                error = %e
                                            );
                                        }
                                    }
                                },
                                Ok(Message::Text(_)) => {
                                    error!("Received flashblock as plaintext, only compressed flashblocks supported. Set up websocket-proxy to use compressed flashblocks.");
                                }
                                Ok(Message::Close(_)) => {
                                    info!(message = "WebSocket connection closed by upstream");
                                    break;
                                }
                                Ok(Message::Pong(data)) => {
                                    trace!(target: "flashblocks_rpc::subscription",
                                        ?data,
                                        "Received pong from upstream"
                                    );
                                }
                                Err(e) => {
                                    metrics.upstream_errors.increment(1);
                                    error!(
                                        message = "error receiving message",
                                        error = %e
                                    );
                                    break;
                                }
                                _ => {}
                            }
                        },
                        _ = &mut ping_err_rx => {
                            warn!(
                                target: "flashblocks_rpc::subscription",
                                ?backoff,
                                "Ping task failed, reconnecting"
                            );
                            backoff = sleep(&metrics, backoff).await;
                            break 'conn;
                        }
                    }
                }
                ping_handle.abort();
            }
        });
    }
}

fn jitter(d: Duration) -> Duration {
    let ms = d.as_millis() as u64;
    let jitter = fastrand::u64(0..(ms / 2).max(1));
    Duration::from_millis(ms.saturating_add(jitter))
}

/// Sleeps for given backoff duration. Returns incremented backoff duration, capped at [`MAX_BACKOFF`].
async fn sleep(metrics: &Metrics, backoff: Duration) -> Duration {
    metrics.reconnect_attempts.increment(1);
    tokio::time::sleep(jitter(backoff)).await;
    std::cmp::min(backoff * 2, MAX_BACKOFF)
}

#[inline]
fn try_decode_message(bytes: &[u8]) -> eyre::Result<Flashblock> {
    // Try plain JSON first (no String allocation)
    if let Ok(v) = serde_json::from_slice(bytes) {
        return Ok(v);
    }

    // Fallback: stream-decompress brotli into serde_json without materializing a String
    let mut decomp = brotli::Decompressor::new(bytes, 4096);
    let v = sonic_rs::from_reader(&mut decomp)?;
    Ok(v)
}
