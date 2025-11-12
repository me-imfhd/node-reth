use crate::metrics::Metrics;
use crate::pending_blocks::{PendingBlocks, PendingBlocksBuilder};
use crate::rpc::{FlashblocksAPI, PendingBlocksAPI};
use crate::subscription::{Flashblock, FlashblocksReceiver};
use alloy_consensus::transaction::{Recovered, SignerRecoverable, TransactionMeta};
use alloy_consensus::{Header, TxReceipt};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, BlockNumber, Bytes, FixedBytes, Sealable, B256, U256};
use alloy_rpc_types::{TransactionTrait, Withdrawal};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use alloy_rpc_types_eth::state::StateOverride;
use alloy_rpc_types_eth::{Filter, Log};
use arc_swap::{ArcSwapOption, Guard};
use eyre::eyre;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_network::{Optimism, TransactionResponse};
use op_alloy_rpc_types::Transaction;
use reth::chainspec::{ChainSpecProvider, EthChainSpec};
use reth::providers::{BlockReaderIdExt, StateProviderFactory};
use reth::revm::context::result::ResultAndState;
use reth::revm::database::StateProviderDatabase;
use reth::revm::db::CacheDB;
use reth::revm::{DatabaseCommit, State};
use reth_evm::{ConfigureEvm, Evm};
use reth_optimism_chainspec::OpHardforks;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_primitives::{DepositReceipt, OpBlock, OpPrimitives};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_primitives::RecoveredBlock;
use reth_rpc_convert::{transaction::ConvertReceiptInput, RpcTransaction};
use reth_rpc_eth_api::{RpcBlock, RpcReceipt};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast::{self, Sender};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

// Buffer 4s of flashblocks for flashblock_sender
const BUFFER_SIZE: usize = 20;

enum StateUpdate {
    Canonical(RecoveredBlock<OpBlock>),
    Flashblock(Flashblock),
}

#[derive(Debug, Clone)]
pub struct FlashblocksState<Client> {
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    queue: mpsc::UnboundedSender<StateUpdate>,
    flashblock_sender: Sender<Arc<PendingBlocks>>,
    state_processor: StateProcessor<Client>,
}

impl<Client> FlashblocksState<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + 'static,
{
    pub fn new(client: Client) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<StateUpdate>();
        let pending_blocks: Arc<ArcSwapOption<PendingBlocks>> = Arc::new(ArcSwapOption::new(None));
        let (flashblock_sender, _) = broadcast::channel(BUFFER_SIZE);
        let state_processor = StateProcessor::new(
            client,
            pending_blocks.clone(),
            Arc::new(Mutex::new(rx)),
            flashblock_sender.clone(),
        );

        Self {
            pending_blocks,
            queue: tx,
            flashblock_sender,
            state_processor,
        }
    }

    pub fn start(&self) {
        let sp = self.state_processor.clone();
        tokio::spawn(async move {
            sp.start().await;
        });
    }

    pub fn on_canonical_block_received(&self, block: &RecoveredBlock<OpBlock>) {
        match self.queue.send(StateUpdate::Canonical(block.clone())) {
            Ok(_) => {
                info!(
                    message = "added canonical block to processing queue",
                    block_number = block.number
                )
            }
            Err(e) => {
                error!(message = "could not add canonical block to processing queue", block_number = block.number, error = %e);
            }
        }
    }
}

impl<Client> FlashblocksReceiver for FlashblocksState<Client> {
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        let block_number = flashblock.metadata.block_number;
        let index = flashblock.index;
        match self.queue.send(StateUpdate::Flashblock(flashblock)) {
            Ok(_) => {
                info!(
                    message = "added flashblock to processing queue",
                    block_number = block_number,
                    flashblock_index = index
                );
            }
            Err(e) => {
                error!(message = "could not add flashblock to processing queue", block_number = block_number, flashblock_index = index, error = %e);
            }
        }
    }
}

impl<Client> FlashblocksAPI for FlashblocksState<Client> {
    fn get_pending_blocks(&self) -> Guard<Option<Arc<PendingBlocks>>> {
        self.pending_blocks.load()
    }

    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Arc<PendingBlocks>> {
        self.flashblock_sender.subscribe()
    }
}

impl PendingBlocksAPI for Guard<Option<Arc<PendingBlocks>>> {
    fn get_canonical_block_number(&self) -> BlockNumberOrTag {
        self.as_ref()
            .map(|pb| pb.canonical_block_number())
            .unwrap_or(BlockNumberOrTag::Latest)
    }

    fn get_transaction_count(&self, address: Address) -> U256 {
        self.as_ref()
            .map(|pb| pb.get_transaction_count(address))
            .unwrap_or_else(|| U256::from(0))
    }

    fn get_block(&self, full: bool) -> Option<RpcBlock<Optimism>> {
        self.as_ref().map(|pb| pb.get_latest_block(full))
    }

    fn get_transaction_receipt(
        &self,
        tx_hash: alloy_primitives::TxHash,
    ) -> Option<RpcReceipt<Optimism>> {
        self.as_ref()
            .and_then(|pb| pb.get_receipt(tx_hash))
            .map(|receipt| (*receipt).clone())
    }

    fn get_transaction_by_hash(
        &self,
        tx_hash: alloy_primitives::TxHash,
    ) -> Option<RpcTransaction<Optimism>> {
        self.as_ref()
            .and_then(|pb| pb.get_transaction_by_hash(tx_hash))
            .map(|t| (*t).clone())
    }

    fn get_balance(&self, address: Address) -> Option<U256> {
        self.as_ref().and_then(|pb| pb.get_balance(address))
    }

    fn get_state_overrides(&self) -> Option<StateOverride> {
        self.as_ref()
            .map(|pb| pb.get_state_overrides())
            .unwrap_or_default()
    }

    fn get_pending_logs(&self, filter: &Filter) -> Vec<Log> {
        self.as_ref()
            .map(|pb| pb.get_pending_logs(filter))
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
struct StateProcessor<Client> {
    rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    metrics: Metrics,
    client: Client,
    sender: Sender<Arc<PendingBlocks>>,
}

impl<Client> StateProcessor<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + 'static,
{
    fn new(
        client: Client,
        pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
        rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
        sender: Sender<Arc<PendingBlocks>>,
    ) -> Self {
        Self {
            metrics: Metrics::default(),
            pending_blocks,
            client,
            rx,
            sender,
        }
    }

    async fn start(&self) {
        while let Some(update) = self.rx.lock().await.recv().await {
            match update {
                StateUpdate::Canonical(block) => {
                    debug!(
                        message = "processing canonical block",
                        block_number = block.number
                    );
                    match self.process_canonical_block(&block) {
                        Ok(new_pending_blocks) => {
                            self.pending_blocks.swap(new_pending_blocks);
                        }
                        Err(e) => {
                            error!(message = "could not process canonical block", error = %e);
                        }
                    }
                }
                StateUpdate::Flashblock(flashblock) => {
                    let start_time = Instant::now();
                    debug!(
                        message = "processing flashblock",
                        block_number = flashblock.metadata.block_number,
                        flashblock_index = flashblock.index
                    );
                    match self.process_flashblock(flashblock) {
                        Ok(new_pending_blocks) => {
                            if new_pending_blocks.is_some() {
                                _ = self.sender.send(new_pending_blocks.clone().unwrap())
                            }

                            self.pending_blocks.swap(new_pending_blocks);
                            self.metrics
                                .block_processing_duration
                                .record(start_time.elapsed());
                        }
                        Err(e) => {
                            error!(message = "could not process Flashblock", error = %e);
                            self.metrics.block_processing_error.increment(1);
                        }
                    }
                }
            }
        }
    }

    fn process_canonical_block(
        &self,
        block: &RecoveredBlock<OpBlock>,
    ) -> eyre::Result<Option<Arc<PendingBlocks>>> {
        let prev_pending_blocks = self.pending_blocks.load_full();
        match &prev_pending_blocks {
            Some(pending_blocks) => {
                let mut flashblocks = pending_blocks.flashblocks();
                let num_flashblocks_for_canon = flashblocks
                    .iter()
                    .filter(|fb| fb.metadata.block_number == block.number)
                    .count();
                self.metrics
                    .flashblocks_in_block
                    .record(num_flashblocks_for_canon as f64);

                if pending_blocks.latest_block_number() <= block.number {
                    self.metrics.pending_clear_catchup.increment(1);
                    self.metrics
                        .pending_snapshot_height
                        .set(pending_blocks.latest_block_number() as f64);
                    self.metrics
                        .pending_snapshot_fb_index
                        .set(pending_blocks.latest_flashblock_index() as f64);

                    Ok(None)
                } else {
                    // If we had a reorg, we need to reset all flashblocks state
                    let tracked_txns = pending_blocks.transactions_for_block(block.number);
                    let tracked_txn_hashes: HashSet<_> =
                        tracked_txns.map(|tx| tx.tx_hash()).collect();
                    let block_txn_hashes: HashSet<_> =
                        block.body().transactions().map(|tx| tx.tx_hash()).collect();

                    flashblocks
                        .retain(|flashblock| flashblock.metadata.block_number > block.number);

                    if tracked_txn_hashes.len() != block_txn_hashes.len()
                        || tracked_txn_hashes != block_txn_hashes
                    {
                        debug!(
                            message = "reorg detected, recomputing pending flashblocks going ahead of reorg",
                            latest_pending_block = pending_blocks.latest_block_number(),
                            canonical_block = block.number,
                            tracked_txn_hashes_len = tracked_txn_hashes.len(),
                            block_txn_hashes_len = block_txn_hashes.len(),
                            tracked_txn_hashes = ?tracked_txn_hashes,
                            block_txn_hashes = ?block_txn_hashes,
                        );
                        self.metrics.pending_clear_reorg.increment(1);

                        // If there is a reorg, we re-process all future flashblocks without reusing the existing pending state
                        return self.build_pending_state(None, flashblocks);
                    }

                    // If no reorg, we can continue building on top of the existing pending state
                    self.build_pending_state(prev_pending_blocks, flashblocks)
                }
            }
            None => {
                debug!(message = "no pending state to update with canonical block, skipping");
                Ok(None)
            }
        }
    }

    fn process_flashblock(
        &self,
        flashblock: Flashblock,
    ) -> eyre::Result<Option<Arc<PendingBlocks>>> {
        let prev_pending_blocks = self.pending_blocks.load_full();
        match &prev_pending_blocks {
            Some(pending_blocks) => {
                if self.is_next_flashblock(pending_blocks, &flashblock) {
                    // We have received the next flashblock for the current block
                    // or the first flashblock for the next block
                    let clone_start = Instant::now();
                    let mut flashblocks = pending_blocks.flashblocks();
                    self.metrics
                        .flashblock_clone_duration
                        .record(clone_start.elapsed());
                    flashblocks.push(Arc::new(flashblock));
                    self.build_pending_state(prev_pending_blocks, flashblocks)
                } else if pending_blocks.latest_block_number() != flashblock.metadata.block_number {
                    // We have received a non-zero flashblock for a new block
                    self.metrics.unexpected_block_order.increment(1);
                    error!(
                        message = "Received non-zero index Flashblock for new block, zeroing Flashblocks until we receive a base Flashblock",
                        curr_block = %pending_blocks.latest_block_number(),
                        new_block = %flashblock.metadata.block_number,
                    );
                    Ok(None)
                } else if pending_blocks.latest_flashblock_index() == flashblock.index {
                    // We have received a duplicate flashblock for the current block
                    self.metrics.unexpected_block_order.increment(1);
                    warn!(
                        message = "Received duplicate Flashblock for current block, ignoring",
                        curr_block = %pending_blocks.latest_block_number(),
                        flashblock_index = %flashblock.index,
                    );
                    Ok(prev_pending_blocks)
                } else {
                    // We have received a non-sequential flashblock for the current block
                    self.metrics.unexpected_block_order.increment(1);

                    error!(
                        message = "Received non-sequential Flashblock for current block, zeroing Flashblocks until we receive a base Flashblock",
                        curr_block = %pending_blocks.latest_block_number(),
                        new_block = %flashblock.metadata.block_number,
                    );

                    Ok(None)
                }
            }
            None => {
                if flashblock.index == 0 {
                    self.build_pending_state(None, vec![Arc::new(flashblock)])
                } else {
                    info!(message = "waiting for first Flashblock");
                    Ok(None)
                }
            }
        }
    }

    fn build_pending_state(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblocks: Vec<Arc<Flashblock>>,
    ) -> eyre::Result<Option<Arc<PendingBlocks>>> {
        // BTreeMap guarantees ascending order of keys while iterating
        let mut flashblocks_per_block = BTreeMap::<BlockNumber, Vec<Arc<Flashblock>>>::new();
        for flashblock in flashblocks {
            flashblocks_per_block
                .entry(flashblock.metadata.block_number)
                .or_default()
                .push(flashblock);
        }

        let earliest_block_number = flashblocks_per_block.keys().min().unwrap();
        let canonical_block = earliest_block_number - 1;

        let db_header_start = Instant::now();
        let mut last_block_header = self.client.header_by_number(canonical_block)?.ok_or(eyre!(
            "Failed to extract header for canonical block number {}. This is okay if your node is not fully synced to tip yet.",
            canonical_block
        ))?;
        self.metrics
            .db_query_header_duration
            .record(db_header_start.elapsed());

        let evm_config = OpEvmConfig::optimism(self.client.chain_spec());

        let db_state_start = Instant::now();
        let state_provider = self
            .client
            .state_by_block_number_or_tag(BlockNumberOrTag::Number(canonical_block))?;
        self.metrics
            .db_query_state_duration
            .record(db_state_start.elapsed());
        let evm_setup_start = Instant::now();
        let state_provider_db = StateProviderDatabase::new(state_provider);
        let state = State::builder()
            .with_database(state_provider_db)
            .with_bundle_update()
            .build();
        let mut pending_blocks_builder = PendingBlocksBuilder::new();

        let db = match &prev_pending_blocks {
            Some(pending_blocks) => CacheDB {
                cache: pending_blocks.get_db_cache(),
                db: state,
            },
            None => CacheDB::new(state),
        };
        let mut state_overrides = match &prev_pending_blocks {
            Some(pending_blocks) => pending_blocks.get_state_overrides().unwrap_or_default(),
            None => StateOverride::default(),
        };

        let evm_env = evm_config.next_evm_env(
            &last_block_header,
            &OpNextBlockEnvAttributes {
                extra_data: Bytes::new(),
                gas_limit: 0,
                parent_beacon_block_root: None,
                prev_randao: FixedBytes([0u8; 32]),
                suggested_fee_recipient: Address(FixedBytes([0u8; 20])),
                timestamp: 0,
            },
        )?;
        let mut evm = evm_config.evm_with_env(db, evm_env);
        self.metrics
            .evm_setup_duration
            .record(evm_setup_start.elapsed());

        for (_block_number, block_flashblocks) in flashblocks_per_block {
            let base = block_flashblocks
                .first()
                .ok_or(eyre!("cannot build a pending block from no flashblocks"))?
                .base
                .clone()
                .ok_or(eyre!("first flashblock does not contain a base"))?;

            let latest_flashblock = block_flashblocks
                .last()
                .cloned()
                .ok_or(eyre!("cannot build a pending block from no flashblocks"))?;

            let data_build_start = Instant::now();
            let transactions: Vec<Bytes> = block_flashblocks
                .iter()
                .flat_map(|flashblock| flashblock.diff.transactions.clone())
                .collect();

            let withdrawals: Vec<Withdrawal> = block_flashblocks
                .iter()
                .flat_map(|flashblock| flashblock.diff.withdrawals.clone())
                .collect();

            let receipt_by_hash = block_flashblocks
                .iter()
                .map(|flashblock| &flashblock.metadata.receipts)
                .fold(HashMap::default(), |mut acc, receipts| {
                    acc.extend(receipts);
                    acc
                });

            let updated_balances = block_flashblocks
                .iter()
                .map(|flashblock| flashblock.metadata.new_account_balances.clone())
                .fold(HashMap::default(), |mut acc, balances| {
                    acc.extend(balances);
                    acc
                });
            self.metrics
                .data_structure_build_duration
                .record(data_build_start.elapsed());

            let execution_payload: ExecutionPayloadV3 = ExecutionPayloadV3 {
                blob_gas_used: 0,
                excess_blob_gas: 0,
                payload_inner: ExecutionPayloadV2 {
                    withdrawals,
                    payload_inner: ExecutionPayloadV1 {
                        parent_hash: base.parent_hash,
                        fee_recipient: base.fee_recipient,
                        state_root: latest_flashblock.diff.state_root,
                        receipts_root: latest_flashblock.diff.receipts_root,
                        logs_bloom: latest_flashblock.diff.logs_bloom,
                        prev_randao: base.prev_randao,
                        block_number: base.block_number,
                        gas_limit: base.gas_limit,
                        gas_used: latest_flashblock.diff.gas_used,
                        timestamp: base.timestamp,
                        extra_data: base.extra_data.clone(),
                        base_fee_per_gas: base.base_fee_per_gas,
                        block_hash: latest_flashblock.diff.block_hash,
                        transactions,
                    },
                },
            };

            let block: OpBlock = execution_payload.try_into_block()?;
            let mut l1_block_info = reth_optimism_evm::extract_l1_info(&block.body)?;
            let header = block.header.clone().seal_slow();
            let header_hash = header.hash_ref().clone();
            pending_blocks_builder.with_header(header);

            let block_env_attributes = OpNextBlockEnvAttributes {
                timestamp: base.timestamp,
                suggested_fee_recipient: base.fee_recipient,
                prev_randao: base.prev_randao,
                gas_limit: base.gas_limit,
                parent_beacon_block_root: Some(base.parent_beacon_block_root),
                extra_data: base.extra_data,
            };

            let new_config = evm_config.next_evm_env(&last_block_header, &block_env_attributes)?;
            evm.modify_block(|block| *block = new_config.block_env);
            evm.modify_cfg(|cfg| *cfg = new_config.cfg_env);

            let mut gas_used = 0;
            let mut next_log_index = 0;

            let Header {
                number,
                base_fee_per_gas,
                excess_blob_gas,
                timestamp,
                ..
            } = block.header;

            let total_tx_count = block.body.transactions.len();
            let mut executed_tx_count = 0;

            for (idx, transaction) in block.body.transactions.into_iter().enumerate() {
                let tx_process_start = Instant::now();
                let tx_hash = transaction.tx_hash();
                let sender = match transaction.recover_signer() {
                    Ok(signer) => signer,
                    Err(err) => return Err(err.into()),
                };
                pending_blocks_builder.increment_nonce(sender);

                let receipt = receipt_by_hash
                    .get(&tx_hash)
                    .cloned()
                    .ok_or(eyre!("missing receipt for {:?}", tx_hash))?;

                // Receipt Generation
                let meta = TransactionMeta {
                    tx_hash: tx_hash,
                    index: idx as u64,
                    block_hash: header_hash,
                    block_number: number,
                    base_fee: base_fee_per_gas,
                    excess_blob_gas: excess_blob_gas,
                    timestamp: timestamp,
                };

                let input: ConvertReceiptInput<'_, OpPrimitives> = ConvertReceiptInput {
                    receipt: receipt.clone(),
                    tx: Recovered::new_unchecked(&transaction, sender),
                    gas_used: receipt.cumulative_gas_used() - gas_used,
                    next_log_index,
                    meta,
                };

                let op_receipt = OpReceiptBuilder::new(
                    self.client.chain_spec().as_ref(),
                    input,
                    &mut l1_block_info,
                )?
                .build();

                // Build Transaction
                let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
                    let deposit_receipt = receipt
                        .as_deposit_receipt()
                        .ok_or(eyre!("deposit transaction, non deposit receipt"))?;

                    (
                        deposit_receipt.deposit_receipt_version,
                        deposit_receipt.deposit_nonce,
                    )
                } else {
                    (None, None)
                };

                let effective_gas_price = if transaction.is_deposit() {
                    0
                } else {
                    base_fee_per_gas
                        .map(|base_fee| {
                            transaction
                                .effective_tip_per_gas(base_fee)
                                .unwrap_or_default()
                                + base_fee as u128
                        })
                        .unwrap_or_else(|| transaction.max_fee_per_gas())
                };

                let recovered_transaction = Recovered::new_unchecked(transaction, sender);
                let envelope = recovered_transaction.clone().convert::<OpTxEnvelope>();

                let rpc_txn = Transaction {
                    inner: alloy_rpc_types_eth::Transaction {
                        inner: envelope,
                        block_hash: Some(header_hash),
                        block_number: Some(base.block_number),
                        transaction_index: Some(idx as u64),
                        effective_gas_price: Some(effective_gas_price),
                    },
                    deposit_nonce,
                    deposit_receipt_version,
                };

                pending_blocks_builder
                    .with_transaction_and_receipt(Arc::new(rpc_txn), Arc::new(op_receipt));
                gas_used = receipt.cumulative_gas_used();
                next_log_index += receipt.logs().len();

                let should_execute_transaction = match &prev_pending_blocks {
                    Some(pending_blocks) => match pending_blocks.get_transaction_by_hash(tx_hash) {
                        Some(_) => false,
                        None => true,
                    },
                    None => true,
                };

                if should_execute_transaction {
                    executed_tx_count += 1;
                    let evm_transact_start = Instant::now();
                    match evm.transact(recovered_transaction) {
                        Ok(ResultAndState { state, .. }) => {
                            self.metrics
                                .evm_transact_duration
                                .record(evm_transact_start.elapsed());

                            let state_override_start = Instant::now();
                            for (addr, acc) in &state {
                                let existing_override =
                                    state_overrides.entry(*addr).or_insert(Default::default());
                                existing_override.balance = Some(acc.info.balance);
                                existing_override.nonce = Some(acc.info.nonce);
                                existing_override.code =
                                    acc.info.code.as_ref().map(|code| code.bytes());

                                let existing = existing_override
                                    .state_diff
                                    .get_or_insert(Default::default());
                                let changed_slots = acc.storage.iter().map(|(&key, slot)| {
                                    (B256::from(key), B256::from(slot.present_value))
                                });

                                existing.extend(changed_slots);
                            }
                            self.metrics
                                .state_override_update_duration
                                .record(state_override_start.elapsed());

                            evm.db_mut().commit(state);
                        }
                        Err(e) => {
                            return Err(eyre!(
                                "failed to execute transaction: {:?} tx_hash: {:?} sender: {:?}",
                                e,
                                tx_hash,
                                sender
                            ));
                        }
                    }
                }

                self.metrics
                    .transaction_processing_duration
                    .record(tx_process_start.elapsed());
            }

            pending_blocks_builder.with_flashblocks(block_flashblocks.into_iter());

            for (address, balance) in updated_balances {
                pending_blocks_builder.with_account_balance(address, balance);
            }

            self.metrics
                .total_transactions_processed
                .record(total_tx_count as f64);
            self.metrics
                .total_transactions_executed
                .record(executed_tx_count as f64);

            last_block_header = block.header.clone();
        }

        pending_blocks_builder.with_db_cache(evm.into_db().cache);
        pending_blocks_builder.with_state_overrides(state_overrides);
        Ok(Some(Arc::new(pending_blocks_builder.build()?)))
    }

    fn is_next_flashblock(
        &self,
        pending_blocks: &Arc<PendingBlocks>,
        flashblock: &Flashblock,
    ) -> bool {
        let is_next_of_block = flashblock.metadata.block_number
            == pending_blocks.latest_block_number()
            && flashblock.index == pending_blocks.latest_flashblock_index() + 1;
        let is_first_of_next_block = flashblock.metadata.block_number
            == pending_blocks.latest_block_number() + 1
            && flashblock.index == 0;

        is_next_of_block || is_first_of_next_block
    }
}
