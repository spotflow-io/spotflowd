mod buffer;
mod config;
mod log_entry;
mod metrics;
mod mqtt;
mod orchestrator;
mod sources;
mod status;

use anyhow::{Context, Result};
use config::{Config, DEFAULT_CONFIG_PATH};
use status::StatusHandle;
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

enum Command {
    Run { config: PathBuf },
    ConfigCheck { config: PathBuf },
    Status { socket: PathBuf, json: bool },
}

#[tokio::main]
async fn main() -> Result<()> {
    match parse_command()? {
        Command::Run { config } => run(config).await,
        Command::ConfigCheck { config } => config_check(config),
        Command::Status { socket, json } => show_status(socket, json).await,
    }
}

async fn run(config_path: PathBuf) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false)
        .init();

    let cfg = load_and_check(&config_path)?;
    info!(
        "spotflowd starting - device_id={} broker={}:{}",
        cfg.device.id, cfg.mqtt.broker, cfg.mqtt.port
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let status = StatusHandle::new(config_path.clone(), &cfg);
    let _status_task = status::start(
        cfg.status.socket_path.clone(),
        status.clone(),
        shutdown_rx.clone(),
    )
    .context("failed to start local status interface")?;

    let publisher = mqtt::start(
        &cfg.device.id,
        &cfg.device.ingest_key,
        &cfg.mqtt,
        shutdown_rx.clone(),
        status.clone(),
    )
    .context("failed to start MQTT client")?;

    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        info!("shutdown signal received");
        let _ = shutdown_tx.send(true);
    });

    orchestrator::run(cfg, publisher, shutdown_rx, status).await?;
    info!("spotflowd stopped");
    Ok(())
}

fn config_check(config_path: PathBuf) -> Result<()> {
    let _cfg = load_and_check(&config_path)?;
    println!(
        "configuration valid: {} (journald feature: {})",
        config_path.display(),
        if cfg!(feature = "journald") {
            "enabled"
        } else {
            "disabled"
        }
    );
    Ok(())
}

async fn show_status(socket: PathBuf, json: bool) -> Result<()> {
    let snapshot = status::fetch(&socket).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        status::print_human(&snapshot);
    }
    Ok(())
}

fn load_and_check(config_path: &std::path::Path) -> Result<Config> {
    let cfg = Config::load(config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))?;
    cfg.check_accessibility()
        .context("configuration resources are not accessible")?;
    Ok(cfg)
}

fn parse_command() -> Result<Command> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        print_usage();
        anyhow::bail!("a command is required");
    };

    match command.as_str() {
        "run" => Ok(Command::Run {
            config: parse_path_option(args, "--config", DEFAULT_CONFIG_PATH)?,
        }),
        "config-check" => Ok(Command::ConfigCheck {
            config: parse_path_option(args, "--config", DEFAULT_CONFIG_PATH)?,
        }),
        "status" => {
            let mut socket = PathBuf::from("/run/spotflow/status.sock");
            let mut json = false;
            let mut args = args;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--socket" => {
                        socket = PathBuf::from(
                            args.next()
                                .ok_or_else(|| anyhow::anyhow!("--socket requires a path"))?,
                        );
                    }
                    "--json" => json = true,
                    _ => anyhow::bail!("unknown status option: {arg}"),
                }
            }
            Ok(Command::Status { socket, json })
        }
        "--version" | "-V" => {
            println!("spotflowd {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        "--help" | "-h" | "help" => {
            print_usage();
            std::process::exit(0);
        }
        _ => anyhow::bail!("unknown command: {command}"),
    }
}

fn parse_path_option(
    mut args: impl Iterator<Item = String>,
    option: &str,
    default: &str,
) -> Result<PathBuf> {
    let mut path = PathBuf::from(default);
    while let Some(arg) = args.next() {
        if arg != option {
            anyhow::bail!("unknown option: {arg}");
        }
        path = PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{option} requires a path"))?,
        );
    }
    Ok(path)
}

fn print_usage() {
    eprintln!(
        "Usage:\n  spotflowd run [--config PATH]\n  spotflowd config-check [--config PATH]\n  spotflowd status [--socket PATH] [--json]\n  spotflowd --version"
    );
}

async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
}
