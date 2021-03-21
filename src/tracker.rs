use anyhow::{Context, Result};
use bitcoin::{BlockHash, Transaction, Txid};
use serde_json::Value;

use std::collections::HashMap;
use std::convert::TryInto;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::{
    chain::Chain,
    config::Config,
    db::DBStore,
    index::Index,
    mempool::{Histogram, Mempool},
    metrics::Metrics,
    p2p, rpc,
    status::Status,
    types::ScriptHash,
};

/// Electrum protocol subscriptions' tracker
pub struct Tracker {
    p2p_client: p2p::Client,
    rpc_client: rpc::Client,
    index: Index,
    mempool: Mempool,
    tx_cache: Arc<RwLock<HashMap<Txid, Transaction>>>,
    metrics: Metrics,
}

impl Tracker {
    pub fn new(config: &Config) -> Result<Self> {
        let p2p_client = p2p::Client::connect(config.network, config.daemon_p2p_addr)?;
        let rpc_url = format!("http://{}", config.daemon_rpc_addr);
        let rpc_auth = rpc::Auth::CookieFile(config.daemon_cookie_file.clone());
        let rpc_client =
            rpc::Client::new(rpc_url, rpc_auth).context("failed to connect to daemon RPC")?;

        let metrics = Metrics::new(config.monitoring_addr)?;
        let store = DBStore::open(Path::new(&config.db_path), config.low_memory)?;
        let chain = Chain::new(config.network);
        let index = Index::load(store, chain, &metrics).context("failed to open index")?;
        Ok(Self {
            rpc_client,
            p2p_client,
            index,
            mempool: Mempool::new(),
            tx_cache: Arc::new(RwLock::new(HashMap::new())),
            metrics,
        })
    }

    pub(crate) fn chain(&self) -> &Chain {
        self.index.chain()
    }

    pub(crate) fn rpc_client(&self) -> &rpc::Client {
        &self.rpc_client
    }

    pub(crate) fn fees_histogram(&self) -> &Histogram {
        &self.mempool.fees_histogram()
    }

    pub(crate) fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn get_history(&self, status: &Status) -> impl Iterator<Item = Value> {
        let confirmed = status
            .get_confirmed(&self.index.chain())
            .into_iter()
            .map(|entry| entry.value());
        let mempool = status
            .get_mempool(&self.mempool)
            .into_iter()
            .map(|entry| entry.value());
        confirmed.chain(mempool)
    }

    pub fn subscribe(&self, script_hash: ScriptHash) -> Result<Status> {
        let mut status = Status::new(script_hash);
        self.update_status(&mut status)?;
        Ok(status)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.index.sync(&self.p2p_client)?;
        self.mempool.sync(&self.rpc_client)?;
        // TODO: double check tip - and retry on diff
        Ok(())
    }

    pub fn update_status(&self, status: &mut Status) -> Result<bool> {
        let prev_statushash = status.statushash();
        let txs = status.sync(&self.index, &self.mempool, &self.p2p_client)?;
        self.tx_cache.write().unwrap().extend(txs);
        Ok(prev_statushash != status.statushash())
    }

    pub fn get_balance(&self, status: &Status) -> bitcoin::Amount {
        let unspent = status.get_unspent(&self.index.chain());
        let mut balance = bitcoin::Amount::ZERO;
        let tx_cache = self.tx_cache.read().unwrap();
        for outpoint in &unspent {
            let tx = tx_cache.get(&outpoint.txid).expect("missing tx");
            let vout: usize = outpoint.vout.try_into().unwrap();
            let value = tx.output[vout].value;
            balance += bitcoin::Amount::from_sat(value);
        }
        balance
    }

    pub fn get_blockhash_by_txid(&self, txid: Txid) -> Option<BlockHash> {
        // Note: there are two blocks with coinbase transactions having same txid (see BIP-30)
        self.index.filter_by_txid(txid).next()
    }

    pub fn get_cached_tx<F, T>(&self, txid: Txid, f: F) -> Option<T>
    where
        F: FnOnce(&Transaction) -> T,
    {
        self.tx_cache.read().unwrap().get(&txid).map(f)
    }
}