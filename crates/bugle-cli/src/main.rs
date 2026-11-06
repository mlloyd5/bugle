//! `bugle` — the command line front end.
//!
//! "I want pictures of Spider-Man!" — J. Jonah Jameson, every day.

mod cli;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let args = cli::Cli::parse();
    let filter = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()))
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    // `block_on` runs the future on this (main) thread, so AppKit overlays
    // invoked from command handlers are legal.
    rt.block_on(cli::run(args))
}
