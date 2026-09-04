//! xkeeper — a lightweight process keeper (daemon) configured with TOML.
//!
//! `xkeeper run` starts every program declared in the config file and keeps
//! it alive; `xkeeper validate` checks a config file and exits.

mod config;
mod platform;
mod program;
mod supervisor;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use log::info;

use crate::config::Config;

#[derive(Parser, Debug)]
#[command(
    name = "xkeeper",
    version,
    about = "Lightweight process keeper/daemon with TOML config"
)]
struct Cli {
    /// Path to the TOML config file
    #[arg(short, long, default_value = "config.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the supervisor in the foreground (default)
    Run,
    /// Parse and validate the config file, then exit
    Validate,
}

fn main() {
    let cli = Cli::parse();
    std::process::exit(match run_cli(&cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("xkeeper: error: {e:#}");
            1
        }
    });
}

fn run_cli(cli: &Cli) -> Result<()> {
    let cfg = Config::load(&cli.config)?;
    match cli.cmd {
        Some(Cmd::Validate) => {
            println!(
                "OK: {} program(s) in {}",
                cfg.programs.len(),
                cli.config.display()
            );
            for p in &cfg.programs {
                println!("  - {} <{}>", p.name, p.command);
            }
            Ok(())
        }
        Some(Cmd::Run) | None => run_daemon(&cfg, &cli.config),
    }
}

fn run_daemon(cfg: &Config, config_path: &Path) -> Result<()> {
    // Honor RUST_LOG if set, otherwise use the level from the config file.
    env_logger::Builder::from_env(env_logger::Env::default()
        .default_filter_or(&cfg.daemon.log_level))
    .try_init()
    .ok();

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&shutdown);
        ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))
            .context("failed to install Ctrl+C / SIGTERM handler")?;
    }
    info!("config file: {}", config_path.display());

    let base_dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    supervisor::Supervisor::new(cfg, base_dir, shutdown)?.run();
    Ok(())
}
