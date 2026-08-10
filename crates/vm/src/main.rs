//! audiobridge-vm — runs on the Windows box that should borrow the Linux
//! headset.
//!
//! Windows has no user-mode way to publish a microphone, so this agent does not
//! try: it plays the audio arriving from Linux into the *render* half of a
//! virtual audio cable, and applications pick the cable's *capture* half from
//! their ordinary microphone list. See the README for which cable to install.
//!
//! The other direction is a plain WASAPI capture, either of a real input or of
//! a render endpoint in loopback mode.

mod audio;
mod config;
mod net;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::config::VmConfig;

#[derive(Parser, Debug)]
#[command(
    name = "audiobridge-vm",
    version,
    about = "Bridge a Linux headset into this machine's audio devices"
)]
struct Cli {
    /// Path to the TOML configuration file. By default, look in the current
    /// working directory first, then beside the executable.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,
    /// Print the audio endpoints this machine has, then exit.
    #[arg(long)]
    list_devices: bool,
    /// Log level (trace, debug, info, warn, error). RUST_LOG overrides it.
    #[arg(long, default_value = "info")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log);

    if cli.list_devices {
        return list_devices();
    }

    let config_path = resolve_config_path(cli.config)?;
    let cfg = Arc::new(VmConfig::load(&config_path)?);
    if cli.check {
        print_summary(&cfg, &config_path);
        return Ok(());
    }

    net::run(cfg).await
}

fn resolve_config_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }

    let cwd = std::env::current_dir().context("finding the current working directory")?;
    let cwd_config = cwd.join("vm.toml");
    if cwd_config.is_file() {
        return Ok(cwd_config);
    }

    let executable = std::env::current_exe().context("finding the executable path")?;
    let executable_dir = executable
        .parent()
        .context("the executable path has no parent directory")?;
    let executable_config = executable_dir.join("vm.toml");
    if executable_config.is_file() {
        return Ok(executable_config);
    }

    bail!(
        "vm.toml was not found in the current working directory ({}) or beside the executable ({}); use --config <path>",
        cwd.display(),
        executable_dir.display()
    )
}

fn print_summary(cfg: &VmConfig, path: &std::path::Path) {
    println!("configuration OK: {}", path.display());
    println!("  listen  : {}", cfg.listen);
    println!(
        "  auth    : {}",
        if cfg.token.is_empty() {
            "disabled"
        } else {
            "token"
        }
    );
    if cfg.mic.enabled {
        println!(
            "  mic     : Linux -> {}  [{} ms jitter buffer]",
            cfg.mic.device().unwrap_or("(default render device)"),
            cfg.mic.buffer_ms
        );
    } else {
        println!("  mic     : disabled");
    }
    if cfg.speaker.enabled {
        println!(
            "  speaker : {:?} of {} -> Linux",
            cfg.speaker.mode,
            cfg.speaker.device().unwrap_or("(default endpoint)")
        );
    } else {
        println!("  speaker : disabled");
    }
    println!("\nAudio formats are chosen by the host and arrive in its Hello.");
}

fn list_devices() -> Result<()> {
    let (render, capture) = audio::list_devices()?;

    println!("render endpoints (playback) — [mic].device, and [speaker].device in loopback mode");
    for d in &render {
        println!("  {} {}", if d.is_default { "*" } else { " " }, d.name);
    }
    println!("\ncapture endpoints (recording) — [speaker].device in capture mode");
    for d in &capture {
        println!("  {} {}", if d.is_default { "*" } else { " " }, d.name);
    }
    println!("\n* = current default.");
    println!("Feed the virtual cable's *render* endpoint (e.g. \"CABLE Input\"); applications");
    println!("then select its capture side (e.g. \"CABLE Output\") as their microphone.");
    Ok(())
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("audiobridge_vm={level},warn")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
