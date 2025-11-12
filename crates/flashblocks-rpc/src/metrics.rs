use metrics::{Counter, Gauge, Histogram};
use metrics_derive::Metrics;
/// Metrics for the `reth_flashblocks` component.
/// Conventions:
/// - Durations are recorded in seconds (histograms).
/// - Counters are monotonic event counts.
/// - Gauges reflect the current value/state.
#[derive(Metrics, Clone)]
#[metrics(scope = "reth_flashblocks")]
pub struct Metrics {
    #[metric(describe = "Count of times upstream receiver was closed/errored")]
    pub upstream_errors: Counter,

    #[metric(describe = "Count of messages received from the upstream source")]
    pub upstream_messages: Counter,

    #[metric(describe = "Time taken to decode a websocket message")]
    pub websocket_decode_duration: Histogram,

    #[metric(describe = "Time taken to process a message")]
    pub block_processing_duration: Histogram,

    #[metric(describe = "Time taken to clone flashblocks vector")]
    pub flashblock_clone_duration: Histogram,

    #[metric(describe = "Time taken to query header by number from database")]
    pub db_query_header_duration: Histogram,

    #[metric(describe = "Time taken to query state by block number from database")]
    pub db_query_state_duration: Histogram,

    #[metric(describe = "Time taken to set up EVM instance")]
    pub evm_setup_duration: Histogram,

    #[metric(describe = "Time taken to build data structures (transactions, receipts, etc.)")]
    pub data_structure_build_duration: Histogram,

    #[metric(describe = "Time taken to process a single transaction (receipt building, etc.)")]
    pub transaction_processing_duration: Histogram,

    #[metric(describe = "Time taken to execute a transaction in EVM")]
    pub evm_transact_duration: Histogram,

    #[metric(describe = "Time taken to process a all transaction (receipt building, etc.)")]
    pub total_txn_execution_duration: Histogram,

    #[metric(describe = "Time taken to process a all transaction (receipt building, etc.)")]
    pub total_txn_processing_duration: Histogram,

    #[metric(describe = "Time taken to update state overrides after transaction execution")]
    pub state_override_update_duration: Histogram,

    #[metric(describe = "Number of Flashblocks that arrive in an unexpected order")]
    pub unexpected_block_order: Counter,

    #[metric(describe = "Count of times flashblocks get_transaction_count is called")]
    pub get_transaction_count: Counter,

    #[metric(describe = "Count of times flashblocks get_transaction_receipt is called")]
    pub get_transaction_receipt: Counter,

    #[metric(describe = "Count of times flashblocks get_balance is called")]
    pub get_balance: Counter,

    #[metric(describe = "Count of times flashblocks get_block_by_number is called")]
    pub get_block_by_number: Counter,

    #[metric(describe = "Number of flashblocks in a block")]
    pub flashblocks_in_block: Histogram,

    #[metric(describe = "Count of times flashblocks are unable to be converted to blocks")]
    pub block_processing_error: Counter,

    #[metric(describe = "Count of times flashblocks call is called")]
    pub call: Counter,

    #[metric(describe = "Count of times flashblocks estimate_gas is called")]
    pub estimate_gas: Counter,

    #[metric(describe = "Count of times flashblocks simulate_v1 is called")]
    pub simulate_v1: Counter,

    #[metric(describe = "Count of times flashblocks get_logs is called")]
    pub get_logs: Counter,

    #[metric(
        describe = "Number of times pending snapshot was cleared because canonical caught up"
    )]
    pub pending_clear_catchup: Counter,

    #[metric(describe = "Number of times pending snapshot was cleared because of reorg")]
    pub pending_clear_reorg: Counter,

    #[metric(describe = "Pending snapshot flashblock index (current)")]
    pub pending_snapshot_fb_index: Gauge,

    #[metric(describe = "Pending snapshot block number (current)")]
    pub pending_snapshot_height: Gauge,

    #[metric(describe = "Total number of WebSocket reconnection attempts")]
    pub reconnect_attempts: Counter,
}
