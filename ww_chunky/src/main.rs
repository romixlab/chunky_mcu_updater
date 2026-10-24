//! `ww-chunky`: updates of chunky devices, a plugin of `ww` (`ww dfu flash` runs `ww-chunky flash`, `ww chunky ..`
//! runs it too). The device selection options, their environment variables and `ww.toml` are `ww`'s, from
//! `wire_weaver_cli::plugin`; the shell completion is the same dynamic one, `ww` asks this program for the words after
//! its name.
#![forbid(unsafe_code)]

mod commands;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser};
use wire_weaver_cli::plugin::{self, DeviceArgs, Selection};
use wire_weaver_client::{ClientMeta, DynClient};

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CHUNKY_GIT_SHA"),
    ", built ",
    env!("CHUNKY_BUILD_TIME"),
    ")"
);

/// Updates of chunky devices (delta, trial, confirm) over WireWeaver, a plugin of ww.
///
/// Run as `ww dfu flash FILE` or `ww-chunky flash FILE`: the same command, with the same device selection (flags,
/// WW_* environment variables, ww.toml).
#[derive(Parser)]
#[command(name = "ww-chunky", version = VERSION)]
#[command(color = clap::ColorChoice::Auto)]
struct Cli {
    #[command(flatten)]
    device: DeviceArgs,

    #[command(subcommand)]
    command: commands::Command,
}

fn main() -> Result<()> {
    // `ww` asks for the words after its name (`ww dfu flash <TAB>`), the shells ask for a plain `ww-chunky <TAB>`
    plugin::complete_if_requested(Cli::command);
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    let selection = Selection::resolve(&matches, false)?;
    // who this program is to the device and a hub: one client instance for all its connections, so a claim it takes in
    // the hub holds for its own calls
    let meta = ClientMeta::new();
    let config = selection.client_config().traits_only().client_meta(meta.clone());
    let device = DynClient::from_config(config)
        .connect()
        .await
        .with_context(|| format!("connecting to device ({selection}), run 'ww list' to see connected devices"))?;
    commands::run_command(cli.command, device, &selection, &meta).await
}
