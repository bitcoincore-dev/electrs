use anyhow::{Context, Result};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) type Row = Box<[u8]>;

#[derive(Default)]
pub(crate) struct WriteBatch {
    pub(crate) tip_row: Row,
    pub(crate) header_rows: Vec<Row>,
    pub(crate) funding_rows: Vec<Row>,
    pub(crate) spending_rows: Vec<Row>,
    pub(crate) txid_rows: Vec<Row>,
}

impl WriteBatch {
    pub(crate) fn sort(&mut self) {
        self.header_rows.sort_unstable();
        self.funding_rows.sort_unstable();
        self.spending_rows.sort_unstable();
        self.txid_rows.sort_unstable();
    }
}

#[derive(Debug)]
struct Options {
    path: PathBuf,
    low_memory: bool,
}

/// RocksDB wrapper for index storage
pub struct DBStore {
    db: rocksdb::DB,
    path: PathBuf,
    bulk_import: AtomicBool,
    cfs: Vec<&'static str>,
}

const CONFIG_CF: &str = "config";
const HEADERS_CF: &str = "headers";
const TXID_CF: &str = "txid";
const FUNDING_CF: &str = "funding";
const SPENDING_CF: &str = "spending";

const CONFIG_KEY: &str = "C";
const TIP_KEY: &[u8] = b"T";

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    compacted: bool,
    format: u64,
}

const CURRENT_FORMAT: u64 = 2;

fn default_opts(low_memory: bool) -> rocksdb::Options {
    let mut opts = rocksdb::Options::default();
    opts.set_keep_log_file_num(10);
    opts.set_max_open_files(16);
    opts.set_compaction_style(rocksdb::DBCompactionStyle::Level);
    opts.set_compression_type(rocksdb::DBCompressionType::Zstd);
    opts.set_target_file_size_base(256 << 20);
    opts.set_write_buffer_size(256 << 20);
    opts.set_disable_auto_compactions(true); // for initial bulk load
    opts.set_advise_random_on_open(false); // bulk load uses sequential I/O
    opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(8));
    if !low_memory {
        opts.set_compaction_readahead_size(1 << 20);
    }
    opts
}

impl DBStore {
    /// Opens a new RocksDB at the specified location.
    pub fn open(path: &Path, low_memory: bool) -> Result<Self> {
        let cfs = vec![CONFIG_CF, HEADERS_CF, TXID_CF, FUNDING_CF, SPENDING_CF];
        let cf_descriptors: Vec<rocksdb::ColumnFamilyDescriptor> = cfs
            .iter()
            .map(|&name| rocksdb::ColumnFamilyDescriptor::new(name, default_opts(low_memory)))
            .collect();

        let mut db_opts = default_opts(low_memory);
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        let db = rocksdb::DB::open_cf_descriptors(&db_opts, path, cf_descriptors)
            .with_context(|| format!("failed to open DB: {:?}", path))?;
        let live_files = db.live_files()?;
        info!(
            "{:?}: {} SST files, {} GB, {} Grows",
            path,
            live_files.len(),
            live_files.iter().map(|f| f.size).sum::<usize>() as f64 / 1e9,
            live_files.iter().map(|f| f.num_entries).sum::<u64>() as f64 / 1e9
        );
        // The old version of electrs used the default column family.
        let is_old = db.iterator(rocksdb::IteratorMode::Start).next().is_some();
        let store = DBStore {
            db,
            path: path.to_path_buf(),
            cfs,
            bulk_import: AtomicBool::new(true),
        };

        let config = store.get_config();
        debug!("DB {:?}", config);
        if config.format < CURRENT_FORMAT || is_old {
            bail!("unsupported DB format {}, re-index required", config.format);
        }
        if config.compacted {
            store.start_compactions();
        }
        store.set_config(config);
        Ok(store)
    }

    fn config_cf(&self) -> &rocksdb::ColumnFamily {
        self.db.cf_handle(CONFIG_CF).expect("missing CONFIG_CF")
    }

    fn funding_cf(&self) -> &rocksdb::ColumnFamily {
        self.db.cf_handle(FUNDING_CF).expect("missing FUNDING_CF")
    }

    fn spending_cf(&self) -> &rocksdb::ColumnFamily {
        self.db.cf_handle(SPENDING_CF).expect("missing SPENDING_CF")
    }

    fn txid_cf(&self) -> &rocksdb::ColumnFamily {
        self.db.cf_handle(TXID_CF).expect("missing TXID_CF")
    }

    fn headers_cf(&self) -> &rocksdb::ColumnFamily {
        self.db.cf_handle(HEADERS_CF).expect("missing HEADERS_CF")
    }

    pub(crate) fn iter_funding(&self, prefix: Row) -> ScanIterator {
        self.iter_prefix_cf(self.funding_cf(), prefix)
    }

    pub(crate) fn iter_spending(&self, prefix: Row) -> ScanIterator {
        self.iter_prefix_cf(self.spending_cf(), prefix)
    }

    pub(crate) fn iter_txid(&self, prefix: Row) -> ScanIterator {
        self.iter_prefix_cf(self.txid_cf(), prefix)
    }

    fn iter_prefix_cf<'a>(&'a self, cf: &rocksdb::ColumnFamily, prefix: Row) -> ScanIterator<'a> {
        let mode = rocksdb::IteratorMode::From(&prefix, rocksdb::Direction::Forward);
        let iter = self.db.iterator_cf(cf, mode);
        ScanIterator {
            prefix,
            iter,
            done: false,
        }
    }

    pub(crate) fn read_headers(&self) -> Vec<Row> {
        let mut opts = rocksdb::ReadOptions::default();
        opts.fill_cache(false);
        self.db
            .iterator_cf_opt(self.headers_cf(), opts, rocksdb::IteratorMode::Start)
            .map(|(key, _)| key)
            .filter(|key| &key[..] != TIP_KEY) // headers' rows are longer than TIP_KEY
            .collect()
    }

    pub(crate) fn get_tip(&self) -> Option<Vec<u8>> {
        self.db
            .get_cf(self.headers_cf(), TIP_KEY)
            .expect("get_tip failed")
    }

    pub(crate) fn write(&self, batch: WriteBatch) -> usize {
        let mut db_batch = rocksdb::WriteBatch::default();
        let mut total_rows_count = 0;
        for key in batch.funding_rows {
            db_batch.put_cf(self.funding_cf(), key, b"");
            total_rows_count += 1;
        }
        for key in batch.spending_rows {
            db_batch.put_cf(self.spending_cf(), key, b"");
            total_rows_count += 1;
        }
        for key in batch.txid_rows {
            db_batch.put_cf(self.txid_cf(), key, b"");
            total_rows_count += 1;
        }
        for key in batch.header_rows {
            db_batch.put_cf(self.headers_cf(), key, b"");
            total_rows_count += 1;
        }
        db_batch.put_cf(self.headers_cf(), TIP_KEY, batch.tip_row);

        let mut opts = rocksdb::WriteOptions::new();
        let bulk_import = self.bulk_import.load(Ordering::Relaxed);
        opts.set_sync(!bulk_import);
        opts.disable_wal(bulk_import);
        self.db.write_opt(db_batch, &opts).unwrap();
        total_rows_count
    }

    pub(crate) fn flush(&self) {
        let mut config = self.get_config();
        for name in &self.cfs {
            let cf = self.db.cf_handle(name).expect("missing CF");
            self.db.flush_cf(cf).expect("CF flush failed");
        }
        if !config.compacted {
            for name in &self.cfs {
                info!("starting {} compaction", name);
                let cf = self.db.cf_handle(name).expect("missing CF");
                self.db.compact_range_cf(cf, None::<&[u8]>, None::<&[u8]>);
            }
            config.compacted = true;
            self.set_config(config);
            info!("finished full compaction");
            self.start_compactions();
        }
    }

    fn start_compactions(&self) {
        self.bulk_import.store(false, Ordering::Relaxed);
        for name in &self.cfs {
            let cf = self.db.cf_handle(name).expect("missing CF");
            self.db
                .set_options_cf(cf, &[("disable_auto_compactions", "false")])
                .expect("failed to start auto-compactions");
        }
        debug!("auto-compactions enabled");
    }

    fn set_config(&self, config: Config) {
        let mut opts = rocksdb::WriteOptions::default();
        opts.set_sync(true);
        opts.disable_wal(false);
        let value = serde_json::to_vec(&config).expect("failed to serialize config");
        self.db
            .put_cf_opt(self.config_cf(), CONFIG_KEY, value, &opts)
            .expect("DB::put failed");
    }

    fn get_config(&self) -> Config {
        self.db
            .get_cf(self.config_cf(), CONFIG_KEY)
            .expect("DB::get failed")
            .map(|value| serde_json::from_slice(&value).expect("failed to deserialize Config"))
            .unwrap_or_else(|| Config {
                compacted: false,
                format: CURRENT_FORMAT,
            })
    }
}

pub(crate) struct ScanIterator<'a> {
    prefix: Row,
    iter: rocksdb::DBIterator<'a>,
    done: bool,
}

impl<'a> Iterator for ScanIterator<'a> {
    type Item = Row;

    fn next(&mut self) -> Option<Row> {
        if self.done {
            return None;
        }
        let (key, _) = self.iter.next()?;
        if !key.starts_with(&self.prefix) {
            self.done = true;
            return None;
        }
        Some(key)
    }
}

impl Drop for DBStore {
    fn drop(&mut self) {
        info!("closing DB at {:?}", self.path);
    }
}