use std::sync::Arc;

use alloy_consensus::{Header, Sealed};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    Address, BlockNumber, B256, U256,
};
use alloy_provider::network::TransactionResponse;
use alloy_rpc_types::{state::StateOverride, BlockTransactions};
use alloy_rpc_types_eth::{Filter, Header as RPCHeader, Log};
use eyre::eyre;
use op_alloy_network::Optimism;
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use reth::revm::{db::Cache, state::EvmState};
use reth_rpc_eth_api::RpcBlock;

use crate::subscription::Flashblock;

pub struct PendingBlocksBuilder {
    flashblocks: Vec<Arc<Flashblock>>,
    headers: Vec<Sealed<Header>>,

    transactions: Vec<Arc<Transaction>>,
    transactions_by_index: HashMap<B256, usize>,

    transaction_state: HashMap<B256, EvmState>,
    transaction_receipts: HashMap<B256, Arc<OpTransactionReceipt>>,

    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,

    state_overrides: Option<StateOverride>,

    db_cache: Cache,
}

impl PendingBlocksBuilder {
    pub fn new() -> Self {
        Self {
            flashblocks: Vec::new(),
            headers: Vec::new(),
            transactions: Vec::new(),
            transactions_by_index: HashMap::new(),
            account_balances: HashMap::new(),
            transaction_count: HashMap::new(),
            transaction_receipts: HashMap::new(),
            transaction_state: HashMap::new(),
            state_overrides: None,
            db_cache: Cache::default(),
        }
    }

    /// Accept any iterator of Arc<Flashblock> to avoid forcing ownership moves/clones on callers.
    #[inline]
    pub fn with_flashblocks<I>(&mut self, iter: I) -> &mut Self
    where
        I: IntoIterator<Item = Arc<Flashblock>>,
    {
        self.flashblocks.extend(iter);
        self
    }

    #[inline]
    pub fn with_header(&mut self, header: Sealed<Header>) -> &mut Self {
        self.headers.push(header);
        self
    }

    /// Store Transaction as Arc and keep index mapping to avoid duplicates and clones.
    #[inline]
    pub(crate) fn with_transaction(&mut self, tx: Arc<Transaction>) -> &mut Self {
        let idx = self.transactions.len();
        let hash = tx.tx_hash();
        self.transactions_by_index.insert(hash, idx);
        self.transactions.push(tx);
        self
    }

    #[inline]
    pub(crate) fn with_db_cache(&mut self, cache: Cache) -> &mut Self {
        self.db_cache = cache;
        self
    }

    #[inline]
    pub(crate) fn with_transaction_state(&mut self, hash: B256, state: EvmState) -> &mut Self {
        self.transaction_state.insert(hash, state);
        self
    }

    #[inline]
    pub(crate) fn increment_nonce(&mut self, sender: Address) -> &Self {
        let zero = U256::from(0);
        let current_count = self.transaction_count.get(&sender).unwrap_or(&zero);

        _ = self
            .transaction_count
            .insert(sender, *current_count + U256::from(1));
        self
    }

    #[inline]
    pub(crate) fn with_receipt(
        &mut self,
        hash: B256,
        receipt: Arc<OpTransactionReceipt>,
    ) -> &mut Self {
        self.transaction_receipts.insert(hash, receipt);
        self
    }

    #[inline]
    pub(crate) fn with_account_balance(&mut self, address: Address, balance: U256) -> &Self {
        self.account_balances.insert(address, balance);
        self
    }

    #[inline]
    pub(crate) fn with_state_overrides(&mut self, state_overrides: StateOverride) -> &Self {
        self.state_overrides = Some(state_overrides);
        self
    }

    pub fn build(self) -> eyre::Result<PendingBlocks> {
        if self.headers.is_empty() {
            return Err(eyre!("missing headers"));
        }
        if self.flashblocks.is_empty() {
            return Err(eyre!("no flashblocks"));
        }

        Ok(PendingBlocks {
            flashblocks: self.flashblocks,
            headers: self.headers,
            transactions: self.transactions,
            transactions_by_index: self.transactions_by_index,
            transaction_state: self.transaction_state,
            transaction_receipts: self.transaction_receipts,
            account_balances: self.account_balances,
            transaction_count: self.transaction_count,
            state_overrides: self.state_overrides,
            db_cache: self.db_cache,
        })
    }
}

#[derive(Debug)]
pub struct PendingBlocks {
    flashblocks: Vec<Arc<Flashblock>>,
    headers: Vec<Sealed<Header>>,
    transactions: Vec<Arc<Transaction>>,
    transactions_by_index: HashMap<B256, usize>,

    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, Arc<OpTransactionReceipt>>,
    transaction_state: HashMap<B256, EvmState>,
    state_overrides: Option<StateOverride>,

    db_cache: Cache,
}

impl PendingBlocks {
    pub fn latest_block_number(&self) -> BlockNumber {
        self.headers.last().unwrap().number
    }

    pub fn canonical_block_number(&self) -> BlockNumberOrTag {
        BlockNumberOrTag::Number(self.headers.first().unwrap().number - 1)
    }

    pub fn latest_flashblock_index(&self) -> u64 {
        self.flashblocks.last().unwrap().index
    }

    pub fn latest_header(&self) -> Sealed<Header> {
        self.headers.last().unwrap().clone()
    }

    pub fn flashblocks(&self) -> Vec<Arc<Flashblock>> {
        self.flashblocks.clone()
    }

    pub fn get_transaction_state(&self, hash: B256) -> Option<EvmState> {
        self.transaction_state.get(&hash).cloned()
    }

    pub fn get_db_cache(&self) -> Cache {
        self.db_cache.clone()
    }

    pub fn transactions_for_block<'a>(
        &'a self,
        block_number: BlockNumber,
    ) -> impl Iterator<Item = Arc<Transaction>> + 'a {
        self.transactions.iter().filter_map(move |tx| {
            tx.block_number.and_then(|bn| {
                if bn == block_number {
                    Some(Arc::clone(tx))
                } else {
                    None
                }
            })
        })
    }

    // Used only in rpc api only so far for clone is fine
    pub fn get_latest_block(&self, full: bool) -> RpcBlock<Optimism> {
        let header = self.latest_header();
        let block_number = header.number;
        let block_transactions = self.transactions_for_block(block_number);

        let transactions = if full {
            // Clone Arc<Transaction> into Transaction
            BlockTransactions::Full(block_transactions.map(|tx| (*tx).clone()).collect())
        } else {
            let tx_hashes: Vec<B256> = block_transactions.map(|tx| tx.tx_hash()).collect();
            BlockTransactions::Hashes(tx_hashes)
        };

        RpcBlock::<Optimism> {
            header: RPCHeader::from_consensus(header.clone(), None, None),
            transactions,
            uncles: Vec::new(),
            withdrawals: None,
        }
    }

    pub fn get_receipt(&self, tx_hash: B256) -> Option<Arc<OpTransactionReceipt>> {
        self.transaction_receipts.get(&tx_hash).cloned()
    }

    // return Arc<Transaction> so caller can cheaply share it
    pub fn get_transaction_by_hash(&self, tx_hash: B256) -> Option<Arc<Transaction>> {
        self.transactions_by_index
            .get(&tx_hash)
            .and_then(|&idx| self.transactions.get(idx).cloned())
    }

    pub fn get_transaction_count(&self, address: Address) -> U256 {
        self.transaction_count
            .get(&address)
            .cloned()
            .unwrap_or(U256::from(0))
    }

    pub fn get_balance(&self, address: Address) -> Option<U256> {
        self.account_balances.get(&address).cloned()
    }

    pub fn get_state_overrides(&self) -> Option<StateOverride> {
        self.state_overrides.clone()
    }

    pub fn get_pending_logs(&self, filter: &Filter) -> Vec<Log> {
        let mut logs = Vec::new();

        // Iterate through all transaction receipts in pending state
        for receipt in self.transaction_receipts.values() {
            for log in receipt.inner.logs() {
                if filter.matches(&log.inner) {
                    logs.push(log.clone());
                }
            }
        }

        logs
    }
}
