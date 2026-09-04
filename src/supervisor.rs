//! The supervision loop that drives all [`ManagedProgram`]s.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::info;

use crate::config::{resolve_path, Config};
use crate::program::ManagedProgram;

/// Shutdown flag is polled at this granularity so Ctrl+C feels instant.
const TICK_SLICE: Duration = Duration::from_millis(100);

pub struct Supervisor {
    programs: Vec<ManagedProgram>,
    interval: Duration,
    shutdown: Arc<AtomicBool>,
}

impl Supervisor {
    pub fn new(cfg: &Config, base_dir: &Path, shutdown: Arc<AtomicBool>) -> Result<Self> {
        let log_dir = resolve_path(&cfg.daemon.log_dir, base_dir);
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("failed to create log dir: {}", log_dir.display()))?;
        let programs = cfg
            .programs
            .iter()
            .map(|p| ManagedProgram::new(p.clone(), base_dir, &log_dir))
            .collect();
        Ok(Self {
            programs,
            interval: Duration::from_secs_f64(cfg.daemon.monitor_interval.max(0.05)),
            shutdown,
        })
    }

    /// Start every configured program once.
    pub fn spawn_all(&mut self) {
        for p in &mut self.programs {
            p.spawn();
        }
    }

    /// One monitoring pass over all programs.
    pub fn tick_once(&mut self, now: Instant) {
        for p in &mut self.programs {
            p.tick(now);
        }
    }

    /// Stop every running child (gracefully where the platform allows).
    pub fn stop_all(&mut self) {
        for p in &mut self.programs {
            if p.is_running() {
                p.stop();
            }
        }
    }

    /// Run until the shutdown flag is set, then stop all children.
    pub fn run(&mut self) {
        info!(
            "xkeeper: supervising {} program(s), monitor interval {:.1}s",
            self.programs.len(),
            self.interval.as_secs_f64()
        );
        self.spawn_all();
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            // Sleep in slices so a shutdown request is noticed quickly even
            // with a large monitor_interval.
            let mut waited = Duration::ZERO;
            while waited < self.interval {
                if self.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let slice = TICK_SLICE.min(self.interval - waited);
                std::thread::sleep(slice);
                waited += slice;
            }
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            self.tick_once(Instant::now());
        }
        info!("shutdown requested, stopping all programs...");
        self.stop_all();
        info!("all programs stopped, xkeeper exits");
    }
}
