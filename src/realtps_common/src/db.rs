use crate::chain::Chain;
use anyhow::{bail, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter};

#[derive(Serialize, Deserialize, Debug)]
pub struct Block {
    pub chain: Chain,
    pub block_number: u64,
    /// The previous block number, not always block_number - 1, as in Solana,
    /// where the "block" number is really a "slot" number, and slots may be
    /// empty.
    pub prev_block_number: Option<u64>,
    pub timestamp: u64, // seconds since unix epoch
    pub num_txs: u64,
    pub hash: String,
    // FIXME this could be None, like prev_block_number,
    // use PreviousBlock struct
    pub parent_hash: String,
}

/// Caches summed data for a contiguous "run" of blocks.
#[derive(Serialize, Deserialize, Debug)]
pub struct BlockRun {
    pub newest_block_number: u64,
    pub newest_block_hash: String,
    pub newest_block_timestamp: u64,
    pub oldest_block_timestamp: u64,
    pub prev_block: Option<PreviousBlock>,
    pub num_blocks: u64,
    pub num_txs: u64,
}

#[derive(Serialize, Deserialize, Debug)]
#[derive(Clone)]
pub struct PreviousBlock {
    pub prev_block_number: u64,
    pub prev_block_hash: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BlockRunSummary {
    pub block_runs: Vec<BlockRun>,
}

pub trait Db: Send + Sync + 'static {
    fn store_block(&self, block: Block) -> Result<()>;
    fn load_block(&self, chain: Chain, block_number: u64) -> Result<Option<Block>>;

    fn store_highest_block_number(&self, chain: Chain, block_number: u64) -> Result<()>;
    fn load_highest_block_number(&self, chain: Chain) -> Result<Option<u64>>;

    fn store_tps(&self, chain: Chain, tps: f64) -> Result<()>;
    fn load_tps(&self, chain: Chain) -> Result<Option<f64>>;

    fn store_block_run_summary(&self, chain: Chain, summary: BlockRunSummary) -> Result<()>;
    fn load_block_run_summary(&self, chain: Chain) -> Result<Option<BlockRunSummary>>;
}

pub struct JsonDb;

pub static JSON_DB_DIR: &str = "db";
pub static HIGHEST_BLOCK_NUMBER: &str = "highest_block_number";
pub static TRANSACTIONS_PER_SECOND: &str = "tps";
pub static BLOCK_RUN_SUMMARY: &str = "block_runs";

impl Db for JsonDb {
    fn store_block(&self, block: Block) -> Result<()> {
        write_json_db(
            &format!("{}", block.chain),
            &format!("{}", block.block_number),
            &block,
        )
    }

    fn load_block(&self, chain: Chain, block_number: u64) -> Result<Option<Block>> {
        read_json_db(&format!("{}", chain), &format!("{}", block_number))
    }

    fn store_highest_block_number(&self, chain: Chain, block_number: u64) -> Result<()> {
        write_json_db(
            &format!("{}", chain),
            &format!("{}", HIGHEST_BLOCK_NUMBER),
            &block_number,
        )
    }

    fn load_highest_block_number(&self, chain: Chain) -> Result<Option<u64>> {
        read_json_db(&format!("{}", chain), &format!("{}", HIGHEST_BLOCK_NUMBER))
    }

    fn store_tps(&self, chain: Chain, tps: f64) -> Result<()> {
        write_json_db(
            &format!("{}", chain),
            &format!("{}", TRANSACTIONS_PER_SECOND),
            &tps,
        )
    }

    fn load_tps(&self, chain: Chain) -> Result<Option<f64>> {
        read_json_db(
            &format!("{}", chain),
            &format!("{}", TRANSACTIONS_PER_SECOND),
        )
    }

    fn store_block_run_summary(&self, chain: Chain, summary: BlockRunSummary) -> Result<()> {
        write_json_db(
            &format!("{}", chain),
            &format!("{}", BLOCK_RUN_SUMMARY),
            &summary,
        )
    }

    fn load_block_run_summary(&self, chain: Chain) -> Result<Option<BlockRunSummary>> {
        read_json_db(
            &format!("{}", chain),
            &format!("{}", BLOCK_RUN_SUMMARY),
        )
    }
}

fn write_json_db<T>(dir: &str, path: &str, data: &T) -> Result<()>
where
    T: Serialize,
{
    let file_dir = format!("{}/{}", JSON_DB_DIR, &dir);
    fs::create_dir_all(&file_dir)?;

    let file_path = format!("{}/{}/{}", JSON_DB_DIR, &dir, &path);
    let temp_file_path = format!("{}.{}.temp", &file_path, rand::random::<u32>());

    let file = File::create(&temp_file_path)?;
    let mut writer = BufWriter::new(file);

    match serde_json::to_writer(&mut writer, &data) {
        Err(e) => {
            fs::remove_file(temp_file_path)?;
            bail!(e)
        }
        Ok(()) => {
            fs::rename(temp_file_path, file_path)?;
            Ok(())
        }
    }
}

fn read_json_db<T>(dir: &str, path: &str) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    let path = format!("{}/{}/{}", JSON_DB_DIR, &dir, &path);

    let file = File::open(path);
    match file {
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound => Ok(None),
            _ => bail!(e),
        },
        Ok(file) => {
            let reader = BufReader::new(file);
            let data = serde_json::from_reader(reader)?;
            Ok(Some(data))
        }
    }
}
