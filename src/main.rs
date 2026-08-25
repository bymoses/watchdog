mod activity;
mod agent_adapters;
mod agent_presence;
mod agent_telemetry;
mod app;
mod browser_api;
mod charge_control;
mod cli;
mod collect;
mod config;
mod context;
mod context_api;
mod db;
mod install;
mod migrate;
mod server;
mod terminal_context;
mod util;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Start(args) => app::start(args).await,
        Commands::Install(args) => install::install(args),
        Commands::Migrate(args) => migrate::migrate(args),
    }
}
