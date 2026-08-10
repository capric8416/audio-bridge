//! audiobridge-host — runs on the Linux box that physically has the headset.
//!
//! It records from the real microphone and ships the PCM to the Windows agent,
//! which replays it into a virtual microphone that Windows applications can
//! select. In the other direction it receives whatever Windows is playing and
//! puts it on the real headphones.

mod config;
mod net;
mod pulse;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use audiobridge_proto::JitterBuffer;
use clap::Parser;

use crate::config::HostConfig;

/// Microphone packets buffered between the audio thread and the socket. Eight
/// packets is ~80 ms: enough to ride out a scheduler hiccup, short enough that
/// anything worse is better dropped than delivered late.
const MIC_QUEUE: usize = 8;

#[derive(Parser, Debug)]
#[command(
    name = "audiobridge-host",
    version,
    about = "Share this machine's headset with a Windows box over the network"
)]
struct Cli {
    /// Path to the TOML configuration file. By default, look in the current
    /// working directory first, then beside the executable.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,
    /// Print the available sources and sinks, then exit.
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
    let cfg = Arc::new(HostConfig::load(&config_path)?);
    if cli.check {
        print_summary(&cfg, &config_path);
        return Ok(());
    }

    let counters = Arc::new(net::Counters::default());
    let (mic_tx, mic_rx) = tokio::sync::mpsc::channel(MIC_QUEUE);

    let capture = cfg.mic.enabled.then(|| {
        pulse::spawn_capture(
            cfg.mic.device().map(str::to_owned),
            cfg.mic.format(),
            mic_tx,
        )
    });

    let speaker = cfg.speaker.enabled.then(|| {
        let format = cfg.speaker.format();
        Arc::new(Mutex::new(JitterBuffer::new(
            format.channels as usize,
            format.rate,
            format.frame_ms,
            cfg.speaker.buffer_ms,
        )))
    });

    let playback = speaker.as_ref().map(|jitter| {
        pulse::spawn_playback(
            cfg.speaker.device().map(str::to_owned),
            cfg.speaker.format(),
            jitter.clone(),
        )
    });

    let result = net::run(net::Bridge {
        cfg: cfg.clone(),
        mic_rx,
        speaker: speaker.clone(),
        counters,
    })
    .await;

    if let Some(t) = capture {
        t.stop();
    }
    if let Some(t) = playback {
        t.stop();
    }
    result
}

fn resolve_config_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }

    let cwd = std::env::current_dir().context("finding the current working directory")?;
    let cwd_config = cwd.join("host.toml");
    if cwd_config.is_file() {
        return Ok(cwd_config);
    }

    let executable = std::env::current_exe().context("finding the executable path")?;
    let executable_dir = executable
        .parent()
        .context("the executable path has no parent directory")?;
    let executable_config = executable_dir.join("host.toml");
    if executable_config.is_file() {
        return Ok(executable_config);
    }

    bail!(
        "host.toml was not found in the current working directory ({}) or beside the executable ({}); use --config <path>",
        cwd.display(),
        executable_dir.display()
    )
}

fn print_summary(cfg: &HostConfig, path: &std::path::Path) {
    println!("configuration OK: {}", path.display());
    println!("  server  : {}", cfg.server.address);
    println!(
        "  auth    : {}",
        if cfg.server.token.is_empty() {
            "disabled"
        } else {
            "token"
        }
    );
    if cfg.mic.enabled {
        println!(
            "  mic     : {} -> VM  [{}]",
            cfg.mic.device().unwrap_or("(default source)"),
            cfg.mic.format()
        );
    } else {
        println!("  mic     : disabled");
    }
    if cfg.speaker.enabled {
        println!(
            "  speaker : VM -> {}  [{}, {} ms jitter buffer]",
            cfg.speaker.device().unwrap_or("(default sink)"),
            cfg.speaker.format(),
            cfg.speaker.buffer_ms
        );
    } else {
        println!("  speaker : disabled");
    }
}

fn list_devices() -> Result<()> {
    let (sources, sinks) = pulse::list_devices()?;

    println!("sources (microphones) — put one in [mic].device");
    for d in &sources {
        let mark = if d.is_default { "*" } else { " " };
        println!("  {mark} {}\n      {}", d.name, d.description);
    }
    println!("\nsinks (speakers) — put one in [speaker].device");
    for d in &sinks {
        let mark = if d.is_default { "*" } else { " " };
        println!("  {mark} {}\n      {}", d.name, d.description);
    }
    println!("\n* = current default; leave the config field empty to follow the default.");
    Ok(())
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("audiobridge_host={level},warn")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
