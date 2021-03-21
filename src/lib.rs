#[macro_use]
extern crate anyhow;

#[macro_use]
extern crate log;

#[macro_use]
extern crate serde_derive;

extern crate configure_me;

// export specific versions of rust-bitcoin crates
pub use bitcoin;
use bitcoincore_rpc as rpc;

mod cache;
mod chain;
mod config;
mod db;
mod electrum;
mod index;
mod mempool;
mod merkle;
mod metrics;
mod p2p;
pub mod server;
mod signals;
mod status;
mod tracker;
mod types;

pub use {
    cache::Cache,
    config::Config,
    electrum::{Client, Rpc},
    status::Status,
    tracker::Tracker,
    types::ScriptHash,
};
