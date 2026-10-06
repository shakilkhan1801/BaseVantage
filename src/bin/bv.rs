use std::path::PathBuf;
use std::process::ExitCode;

use alloy::primitives::{Address, U256};
use clap::{Parser, Subcommand};

use basevantage::config::Config;
use basevantage::error::{EngineError, Result};
use basevantage::harness::Engine;

#[derive(Parser)]
#[command(
    name = "bv",
    about = "BaseVantage dev-harness — exercise the engine without Telegram"
)]
struct Cli {
    /// Path to the engine config (validated fail-fast at boot).
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Quote the best route for a sell.
    Quote {
        #[arg(long)]
        sell: Address,
        /// Human amount, e.g. 1.5
        #[arg(long = "with")]
        with: f64,
        #[arg(long, default_value_t = 18)]
        decimals: u8,
    },
    /// List every candidate route with its outcome.
    RouteList {
        #[arg(long)]
        sell: Address,
        #[arg(long = "with")]
        with: f64,
        #[arg(long, default_value_t = 18)]
        decimals: u8,
    },
    /// Token dossier: metadata, assessment probes, pools, stats.
    DossierData { token: Address },
    /// Simulate a route execution with settlement (observe mode only).
    Simulate {
        #[arg(long)]
        sell: Address,
        #[arg(long = "with")]
        with: f64,
        #[arg(long, default_value_t = 18)]
        decimals: u8,
        /// Route index from route-list (1-based).
        #[arg(long, default_value_t = 1)]
        route: usize,
        /// Target price, settlement raw units per whole token × 1e18.
        #[arg(long)]
        target: Option<U256>,
    },
}

fn to_raw(with: f64, decimals: u8) -> Result<U256> {
    if !with.is_finite() || with <= 0.0 {
        return Err(EngineError::Config(
            "--with must be a positive number".to_string(),
        ));
    }
    let scale = 10f64.powi(i32::from(decimals));
    let raw = with * scale;
    if raw > u128::MAX as f64 {
        return Err(EngineError::Config("--with too large".to_string()));
    }
    Ok(U256::from(raw as u128))
}

async fn run(cli: Cli) -> Result<String> {
    let config = Config::load(&cli.config)?;
    let engine = Engine::boot(config).await?;
    match cli.command {
        Cmd::Quote {
            sell,
            with,
            decimals,
        } => engine.cmd_quote(sell, to_raw(with, decimals)?).await,
        Cmd::RouteList {
            sell,
            with,
            decimals,
        } => engine.cmd_route_list(sell, to_raw(with, decimals)?).await,
        Cmd::DossierData { token } => engine.cmd_dossier(token).await,
        Cmd::Simulate {
            sell,
            with,
            decimals,
            route,
            target,
        } => {
            engine
                .cmd_simulate(sell, to_raw(with, decimals)?, route, target)
                .await
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
