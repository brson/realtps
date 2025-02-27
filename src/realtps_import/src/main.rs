use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use client::{Client, EthersClient, NearClient, SolanaClient, StellarClient, TendermintClient};
use delay::retry_if_err;
use futures::future::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};
use jobs::{Job, JobRunner};
use log::{error, info};
use realtps_common::{
    chain::{Chain, ChainType},
    db::JsonDb,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use substrate::SubstrateClient;
use tokio::task;

mod calculate;
mod client;
mod delay;
mod helpers;
mod import;
mod jobs;
mod substrate;

#[derive(Parser, Debug)]
struct Opts {
    #[clap(subcommand)]
    cmd: Option<Command>,

    #[clap(long, arg_enum, global = true)]
    chain: Option<Chain>,
}

#[derive(Subcommand, Debug)]
enum Command {
    Run,
    Import,
    Calculate,
}

#[derive(Deserialize, Serialize)]
struct RpcConfig {
    chains: HashMap<Chain, String>,
}

static RPC_CONFIG_PATH: &str = "rpc_config.toml";

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let opts = Opts::parse();
    let rpc_config = load_rpc_config(RPC_CONFIG_PATH)?;

    Ok(run(opts, rpc_config).await?)
}

async fn run(opts: Opts, rpc_config: RpcConfig) -> Result<()> {
    let cmd = opts.cmd.unwrap_or(Command::Run);

    let chains = get_chains(opts.chain);
    let init_jobs = init_jobs(&chains, cmd);

    let job_runner = make_job_runner(&chains, &rpc_config).await?;
    let mut jobs: FuturesUnordered<_> = init_jobs
        .into_iter()
        .map(|job| job_runner.do_job(job))
        .collect();

    loop {
        let new_jobs = jobs.next().await;
        if let Some(new_jobs) = new_jobs {
            for new_job in new_jobs {
                jobs.push(job_runner.do_job(new_job));
            }
        } else {
            error!("no more jobs?!");
            break;
        }
    }

    Ok(())
}

fn get_chains(maybe_chain: Option<Chain>) -> Vec<Chain> {
    if let Some(chain) = maybe_chain {
        vec![chain]
    } else {
        Chain::all_chains()
    }
}

fn load_rpc_config<P: AsRef<Path>>(path: P) -> Result<RpcConfig> {
    let rpc_config_file = fs::read_to_string(path).context("unable to load RPC configuration")?;
    let rpc_config = toml::from_str::<RpcConfig>(&rpc_config_file)
        .context("unable to parse RPC configuration")?;

    Ok(rpc_config)
}

fn init_jobs(chains: &[Chain], cmd: Command) -> Vec<Job> {
    match cmd {
        Command::Run => {
            let import_jobs = init_jobs(chains, Command::Import);
            let calculate_jobs = init_jobs(chains, Command::Calculate);
            import_jobs
                .into_iter()
                .chain(calculate_jobs.into_iter())
                .collect()
        }
        Command::Import => chains.iter().cloned().map(Job::Import).collect(),
        Command::Calculate => vec![Job::Calculate(chains.to_vec())],
    }
}

async fn make_job_runner(chains: &[Chain], rpc_config: &RpcConfig) -> Result<JobRunner> {
    let clients = make_all_clients(chains, rpc_config).await?;

    Ok(JobRunner {
        db: Arc::new(JsonDb),
        clients,
    })
}

async fn make_all_clients(
    chains: &[Chain],
    rpc_config: &RpcConfig,
) -> Result<HashMap<Chain, Box<dyn Client>>> {
    let mut client_futures = FuturesUnordered::new();

    for chain in chains {
        let rpc_url = get_rpc_url(chain, rpc_config).to_string();
        let client_future = task::spawn(make_client(*chain, rpc_url));
        let client_future = client_future.map(move |client| (*chain, client));
        client_futures.push(client_future);
    }

    let mut clients = HashMap::new();

    while let Some((chain, client)) = client_futures.next().await {
        let client = client??;
        clients.insert(chain, client);
    }

    Ok(clients)
}

async fn make_client(chain: Chain, rpc_url: String) -> Result<Box<dyn Client>> {
    info!("creating client for {} at {}", chain, rpc_url);

    let client: Box<dyn Client> = match chain.chain_type() {
        ChainType::Ethers => Box::new(EthersClient::new(chain, &rpc_url)?),
        ChainType::Near => Box::new(NearClient::new(&rpc_url)?),
        ChainType::Solana => Box::new(SolanaClient::new(&rpc_url)?),
        ChainType::Stellar => Box::new(StellarClient::new(&rpc_url)?),
        ChainType::Tendermint => Box::new(TendermintClient::new(chain, &rpc_url)?),
        ChainType::Substrate => Box::new(SubstrateClient::new(chain, &rpc_url).await?),
    };

    let version = retry_if_err(|| client.client_version())
        .await
        .context(format!("error getting client version for {}", chain))?;
    info!("node version for {}: {}", chain, version);

    Ok(client)
}

fn get_rpc_url<'a>(chain: &Chain, rpc_config: &'a RpcConfig) -> &'a str {
    if let Some(url) = rpc_config.chains.get(chain) {
        url
    } else {
        todo!()
    }
}
