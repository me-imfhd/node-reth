use std::{sync::Arc, time::Duration};

use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use futures_util::{SinkExt as _, StreamExt};
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, trace, warn};
use url::Url;

use crate::metrics::Metrics;

/// Interval of liveness check of upstream, in milliseconds.
pub const PING_INTERVAL_MS: u64 = 500;

/// Max duration of backoff before reconnecting to upstream.
pub const MAX_BACKOFF: Duration = Duration::from_secs(10);

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

// Simplify actor messages to just handle shutdown
#[derive(Debug)]
enum ActorMessage {
    BestPayload { payload: Flashblock },
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

        let (sender, mut mailbox) = mpsc::channel(100);

        let metrics = self.metrics.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);

            loop {
                let ws_stream = match connect_async(ws_url.as_str()).await {
                    Ok((ws_stream, _)) => {
                        ws_stream
                    },
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

                let mut ping_interval = interval(Duration::from_millis(PING_INTERVAL_MS));
                let mut awaiting_pong_resp = false;

                let (mut write, mut read) = ws_stream.split();

                'conn: loop {
                    tokio::select! {
                        Some(msg) = read.next() => {
                            metrics.upstream_messages.increment(1);

                            match msg {
                                Ok(Message::Binary(bytes)) => match try_decode_message(&bytes) {
                                    Ok(payload) => {
                                        let _ = sender.send(ActorMessage::BestPayload { payload }).await.map_err(|e| {
                                            error!(message = "Failed to publish message to channel", error = %e);
                                        });
                                    }
                                    Err(e) => {
                                        error!(
                                            message = "error decoding flashblock message",
                                            error = %e
                                        );
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
                                    awaiting_pong_resp = false
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
                        _ = ping_interval.tick() => {
                            if awaiting_pong_resp {
                                  warn!(
                                    target: "flashblocks_rpc::subscription",
                                    ?backoff,
                                    timeout_ms = PING_INTERVAL_MS,
                                    "No pong response from upstream, reconnecting",
                                );

                                backoff = sleep(&metrics, backoff).await;
                                break 'conn;
                            }

                            trace!(target: "flashblocks_rpc::subscription",
                                "Sending ping to upstream"
                            );

                            if let Err(error) = write.send(Message::Ping(Default::default())).await {
                                warn!(
                                    target: "flashblocks_rpc::subscription",
                                    ?backoff,
                                    %error,
                                    "WebSocket connection lost, reconnecting",
                                );

                                backoff = sleep(&metrics, backoff).await;
                                break 'conn;
                            }
                            awaiting_pong_resp = true
                        }
                    }
                };

            }
        });

        let flashblocks_state = self.flashblocks_state.clone();
        tokio::spawn(async move {
            while let Some(message) = mailbox.recv().await {
                match message {
                    ActorMessage::BestPayload { payload } => {
                        flashblocks_state.on_flashblock_received(payload);
                    }
                }
            }
        });
    }
}

/// Sleeps for given backoff duration. Returns incremented backoff duration, capped at [`MAX_BACKOFF`].
async fn sleep(metrics: &Metrics, backoff: Duration) -> Duration {
    metrics.reconnect_attempts.increment(1);
    tokio::time::sleep(backoff).await;
    std::cmp::min(backoff * 2, MAX_BACKOFF)
}

fn try_decode_message(bytes: &[u8]) -> eyre::Result<Flashblock> {
    // Try plain JSON first (no String allocation)
    match serde_json::from_slice(bytes) {
        Ok(v) => return Ok(v),
        Err(_) => {}
    }

    // Fallback: stream-decompress brotli into serde_json without materializing a String
    let mut decomp = brotli::Decompressor::new(bytes, 4096);
    let v = serde_json::from_reader(&mut decomp)?;
    Ok(v)
}
