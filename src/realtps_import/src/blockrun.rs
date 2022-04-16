#![allow(unused)]

use crate::helpers::*;
use anyhow::{Result, anyhow};
use realtps_common::chain::Chain;
use realtps_common::db::{Block, BlockRun, PreviousBlock, BlockRunSummary, Db};
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use tokio::sync::mpsc::{self, Sender, Receiver};
use tokio::task;
use std::future::Future;
use std::iter::Peekable;
use std::vec;
use futures::stream::{Stream, StreamExt};

const BLOCK_RUN_MIN_SECS: u64 = 60 * 60;

pub async fn block_run_stream(
    chain: Chain,
    db: Arc<dyn Db>,
    starting_block_number: u64,
) -> Result<ReceiverStream<Result<BlockRun>>> {
    let stream = make_block_run_stream(chain, db, starting_block_number);
    let stream = merge_short_runs(stream).await?;
    let stream = check_stream(stream).await?;

    Ok(stream)
}

fn make_block_run_stream(
    chain: Chain,
    db: Arc<dyn Db>,
    starting_block_number: u64,
) -> ReceiverStream<Result<BlockRun>> {
    let (tx, rx) = mpsc::channel(1);
    task::spawn(send_block_runs(chain, db, tx, starting_block_number));
    ReceiverStream::new(rx)
}

async fn send_block_runs(
    chain: Chain,
    db: Arc<dyn Db>,
    tx: Sender<Result<BlockRun>>,
    starting_block_number: u64,
) {
    let r = send_block_runs_fallible(chain, db, tx.clone(), starting_block_number).await;
    if let Err(e) = r {
        let _ = tx.send(Err(e)).await;
    }
}

async fn send_block_runs_fallible(
    chain: Chain,
    db: Arc<dyn Db + 'static>,
    tx: Sender<Result<BlockRun>>,
    starting_block_number: u64,
) -> Result<()> {
    let block_runs = load_block_run_summary(chain, &db).await?;
    let block_runs: Vec<BlockRun> = block_runs.map(|br| br.block_runs).unwrap_or_default();
    let mut block_runs = block_runs.into_iter().peekable();

    // Build new block runs from the highest block until
    // it joins with the existing block runs.
    {
        async fn send_block_run(
            block_buffer: &mut Vec<Block>,
            last_block_run_timestamp: &mut u64,
            tx: &Sender<Result<BlockRun>>,
        ) -> Result<()> {
            if !block_buffer.is_empty() {
                let newest_block = block_buffer.first().unwrap();
                let oldest_block = block_buffer.last().unwrap();
                assert!(newest_block.block_number >= oldest_block.block_number);

                let block_run = BlockRun {
                    newest_block_number: newest_block.block_number,
                    newest_block_hash: newest_block.hash.clone(),
                    newest_block_timestamp: newest_block.timestamp,
                    oldest_block_timestamp: oldest_block.timestamp,
                    prev_block: oldest_block.prev_block_number.map(|prev_block_number| {
                        PreviousBlock {
                            prev_block_number,
                            prev_block_hash: oldest_block.parent_hash.clone(),
                        }
                    }),
                    num_blocks: u64::try_from(block_buffer.len()).expect("overflow"),
                    num_txs: block_buffer.iter().fold(0, |accum, block| accum.saturating_add(block.num_txs))
                };

                *last_block_run_timestamp = oldest_block.timestamp;

                tx.send(Ok(block_run)).await?;
            }
            Ok(())
        }

        fn handle_reorg(block_runs: &mut impl Iterator) {
            block_runs.next();
        }

        let starting_block_timestamp = load_block(chain, &db, starting_block_number).await?
            .ok_or_else(|| anyhow!("missing first block"))?
            .timestamp;

        let mut block_number = starting_block_number;
        let mut last_block_run_timestamp = starting_block_timestamp;
        let mut block_buffer = vec![];

        loop {
            let block = load_block(chain, &db, block_number).await?;

            if block.is_none() {
                // Finished.
                send_block_run(&mut block_buffer, &mut last_block_run_timestamp, &tx).await?;
                return Ok(());
            }

            let block = block.unwrap();

            if let Some(first_block_run) = block_runs.peek() {
                if block.block_number == first_block_run.newest_block_number {
                    if block.hash == first_block_run.newest_block_hash {
                        // Joined with existing block runs.
                        send_block_run(&mut block_buffer, &mut last_block_run_timestamp, &tx).await?;
                        break;
                    } else {
                        handle_reorg(&mut block_runs);
                    }
                } else if block.block_number < first_block_run.newest_block_number {
                    handle_reorg(&mut block_runs);
                }
            } else {
                // Nothing needed in this case.
            }

            let timestamp = block.timestamp;
            let prev_block_number = block.prev_block_number;

            block_buffer.push(block);

            let should_send = timestamp <= last_block_run_timestamp.saturating_sub(BLOCK_RUN_MIN_SECS);

            if should_send {
                send_block_run(&mut block_buffer, &mut last_block_run_timestamp, &tx).await?;
            }

            if let Some(prev_block_number) = prev_block_number {
                block_number = prev_block_number;
            } else {
                // Finished.
                send_block_run(&mut block_buffer, &mut last_block_run_timestamp, &tx).await?;
                return Ok(());
            }
        }

        assert!(block_buffer.is_empty());
    }
    
    // Now just send all the remaning block runs
    for block_run in block_runs {
        tx.send(Ok(block_run)).await?;
    }

    Ok(())
}

async fn merge_short_runs(mut stream: ReceiverStream<Result<BlockRun>>) -> Result<ReceiverStream<Result<BlockRun>>> {
    let (tx, rx) = mpsc::channel(1);
    task::spawn(async move {
        let mut newer_block_run: Option<BlockRun> = None;

        while let Some(older_block_run) = stream.next().await {
            newer_block_run = match (newer_block_run, older_block_run) {
                (None, Ok(older_block_run)) => {
                    Some(older_block_run)
                }
                (None, Err(e)) => {
                    tx.send(Err(e)).await?;
                    None
                }
                (Some(newer_block_run), Ok(older_block_run)) => {
                    let two_block_time = newer_block_run.newest_block_timestamp
                        .saturating_sub(older_block_run.oldest_block_timestamp);
                    let two_short_runs = two_block_time < BLOCK_RUN_MIN_SECS;
                    if two_short_runs {
                        Some(BlockRun {
                            newest_block_number: newer_block_run.newest_block_number,
                            newest_block_hash: newer_block_run.newest_block_hash,
                            newest_block_timestamp: newer_block_run.newest_block_timestamp,
                            oldest_block_timestamp: older_block_run.oldest_block_timestamp,
                            prev_block: older_block_run.prev_block,
                            num_blocks: newer_block_run.num_blocks.saturating_add(older_block_run.num_blocks),
                            num_txs: newer_block_run.num_txs.saturating_add(older_block_run.num_txs),
                        })
                    } else {
                        tx.send(Ok(newer_block_run)).await?;
                        Some(older_block_run)
                    }
                }
                (Some(newer_block_run), Err(e)) => {
                    tx.send(Ok(newer_block_run)).await?;
                    tx.send(Err(e)).await?;
                    None
                }
            }
        }

        if let Some(final_block_run) = newer_block_run {
            tx.send(Ok(final_block_run)).await?;
        }

        Ok::<_, anyhow::Error>(())
    });

    Ok(ReceiverStream::new(rx))
}

async fn check_stream(mut stream: ReceiverStream<Result<BlockRun>>) -> Result<ReceiverStream<Result<BlockRun>>> {
    let (tx, rx) = mpsc::channel(1);
    task::spawn(async move {
        todo!();
        while let Some(block_run) = stream.next().await {
            tx.send(block_run).await?;
        }

        Ok::<_, anyhow::Error>(())
    });

    Ok(ReceiverStream::new(rx))
}
