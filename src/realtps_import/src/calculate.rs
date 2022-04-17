use crate::helpers::*;
use anyhow::{anyhow, Result, bail};
use realtps_common::{chain::Chain, db::Db};
use std::sync::Arc;
use crate::blockrun;
use futures::StreamExt;
use std::pin::Pin;
use tokio::task;
use realtps_common::db::BlockRunSummary;

pub struct ChainCalcs {
    pub chain: Chain,
    pub tps: f64,
}

pub async fn calculate_for_chain(chain: Chain, db: Arc<dyn Db>) -> Result<ChainCalcs> {
    let highest_block_number = load_highest_known_block_number(chain, &db).await?;
    let highest_block_number =
        highest_block_number.ok_or_else(|| anyhow!("no data for chain {}", chain))?;

    let load_block = |number| load_block(chain, &db, number);

    let highest_block = load_block(highest_block_number).await?
        .ok_or_else(|| anyhow!("missing first block"))?;
    let highest_block_hash = highest_block.hash;
    let highest_block_timestamp = highest_block.timestamp;

    let seconds_per_week = 60 * 60 * 24 * 7;
    let min_timestamp = highest_block_timestamp
        .checked_sub(seconds_per_week)
        .expect("underflow");

    let calc_from_blocks_task = task::spawn(
        calculate_from_blocks(
            chain,
            db.clone(),
            highest_block_number,
            highest_block_hash.clone(),
            highest_block_timestamp,
            min_timestamp
        )
    );

    let calc_from_block_runs_task = task::spawn(
        calculate_from_block_runs(
            chain,
            db.clone(),
            highest_block_number,
            highest_block_hash.clone(),
            highest_block_timestamp,
            min_timestamp
        )
    );

    let calcs_from_blocks = calc_from_blocks_task.await??;
    let calcs_from_block_runs = calc_from_block_runs_task.await??;

    if calcs_from_blocks.tps != calcs_from_block_runs.tps {
        bail!("calcs from blocks != calcs_from_block_runs for {}", chain);
    }

    Ok(calcs_from_block_runs)
}

async fn calculate_from_block_runs(
    chain: Chain,
    db: Arc<dyn Db>,
    highest_block_number: u64,
    highest_block_hash: String,
    highest_block_timestamp: u64,
    min_timestamp: u64,
) -> Result<ChainCalcs> {
    let block_run_stream = blockrun::block_run_stream(chain, db.clone(), highest_block_number).await?;
    let mut block_run_stream = block_run_stream.peekable();

    if let Some(first_block_run) = Pin::new(&mut block_run_stream).peek().await {
        if let Ok(first_block_run) = first_block_run {
            if first_block_run.newest_block_hash != highest_block_hash {
                log::warn!("incorrect first block for hash of {}. reorg", chain);
                bail!("incorrect first block hash for calculation of {}", chain);
            }
        }
    }

    let mut num_txs: u64 = 0;
    let mut init_timestamp = 0;
    let mut block_runs = vec![];    

    while let Some(block_run) = block_run_stream.next().await {
        let block_run = block_run?;
        block_runs.push(block_run.clone());
        if block_run.newest_block_timestamp <= min_timestamp {
            init_timestamp = block_run.newest_block_timestamp;
            break;
        } else if block_run.oldest_block_timestamp <= min_timestamp {
            let (more_txs, init_timestamp_) = num_txs_from_blocks(
                chain,
                db.clone(),
                block_run.newest_block_number,
                block_run.newest_block_hash,
                min_timestamp,
            ).await?;
            num_txs = num_txs.saturating_add(more_txs);
            init_timestamp = init_timestamp_;
            break;
        } else {
            num_txs = num_txs.saturating_add(block_run.num_txs);
        }
    }

    blockrun::save_block_runs(chain, &db, BlockRunSummary { block_runs }).await?;

    let tps = calculate_tps(init_timestamp, highest_block_timestamp, num_txs)?;

    Ok(ChainCalcs { chain, tps })
}

async fn calculate_from_blocks(
    chain: Chain,
    db: Arc<dyn Db>,
    highest_block_number: u64,
    highest_block_hash: String,
    highest_block_timestamp: u64,
    min_timestamp: u64,
) -> Result<ChainCalcs> {
    let (num_txs, init_timestamp) = num_txs_from_blocks(
        chain,
        db,
        highest_block_number,
        highest_block_hash,
        min_timestamp,
    ).await?;

    let tps = calculate_tps(init_timestamp, highest_block_timestamp, num_txs)?;

    Ok(ChainCalcs { chain, tps })
}

async fn num_txs_from_blocks(
    chain: Chain,
    db: Arc<dyn Db>,
    highest_block_number: u64,
    highest_block_hash: String,
    min_timestamp: u64,
) -> Result<(u64, u64)> {
    let load_block = |number| load_block(chain, &db, number);

    let mut current_block = load_block(highest_block_number)
        .await?
        .ok_or_else(|| anyhow!("missing first block"))?;

    if current_block.hash != highest_block_hash {
        log::warn!("incorrect first block for hash of {}. reorg", chain);
        bail!("incorrect first block hash for calculation of {}", chain);
    }

    let mut num_txs: u64 = 0;

    let init_timestamp = loop {
        let prev_block_number = current_block.prev_block_number;

        if prev_block_number.is_none() {
            break current_block.timestamp;
        }

        let prev_block_number = prev_block_number.unwrap();

        let prev_block = load_block(prev_block_number).await?;

        if prev_block.is_none() {
            break current_block.timestamp;
        }

        let prev_block = prev_block.unwrap();

        num_txs = num_txs
            .saturating_add(current_block.num_txs);

        if prev_block.timestamp <= min_timestamp {
            break prev_block.timestamp;
        }
        if prev_block.block_number == 0 {
            break prev_block.timestamp;
        }

        current_block = prev_block;
    };

    Ok((num_txs, init_timestamp))
}

fn calculate_tps(init_timestamp: u64, latest_timestamp: u64, num_txs: u64) -> Result<f64> {
    let total_seconds = latest_timestamp.saturating_sub(init_timestamp);
    let total_seconds_u32 =
        u32::try_from(total_seconds).map_err(|_| anyhow!("seconds overflows u32"))?;
    let num_txs_u32 = u32::try_from(num_txs).map_err(|_| anyhow!("num txs overflows u32"))?;
    let total_seconds_f64 = f64::from(total_seconds_u32);
    let num_txs_f64 = f64::from(num_txs_u32);
    let mut tps = num_txs_f64 / total_seconds_f64;

    // Special float values will not serialize sensibly
    if tps.is_nan() || tps.is_infinite() {
        tps = 0.0;
    }

    Ok(tps)
}
