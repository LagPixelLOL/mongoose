use std::process::ExitCode;

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use tokio::signal::unix::{Signal, SignalKind, signal};

use mongoose::cli::{Cli, Command};
use mongoose::forward::{self, ForwardConfig};
use mongoose::service::{ServiceManager, SetupOutcome};
use mongoose::spec::ForwardSpec;
use mongoose::{conflict, logging, sys};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse_args();
    logging::init(cli.verbose());

    let result = match cli.command {
        Command::Start { specs, .. } => start(&specs).await,
        Command::Setup {
            skip_enable, specs, ..
        } => setup(&specs, skip_enable),
        Command::Remove { .. } => remove(),
        Command::Show { .. } => show(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Logs the fully expanded rules, one line per rule.
fn log_rules(specs: &[ForwardSpec], ipv6_dual_stack: bool) {
    for (index, spec) in specs.iter().enumerate() {
        info!("rule #{}: {}", index + 1, spec.describe(ipv6_dual_stack));
    }
}

async fn start(specs: &[ForwardSpec]) -> Result<()> {
    // Listen for signals before binding anything: a SIGTERM that arrives
    // between handler installation and subscription would otherwise be lost.
    let mut shutdown = Shutdown::install()?;
    let ipv6_dual_stack = sys::ipv6_dual_stack_default();
    conflict::check(specs, ipv6_dual_stack)?;
    log_rules(specs, ipv6_dual_stack);

    match sys::raise_fd_limit() {
        Ok(limit) => debug!("open file limit: {limit}"),
        Err(err) => warn!("could not raise open file limit: {err}"),
    }
    let config = ForwardConfig::default();
    let bound = forward::bind_all(specs, &config)?;
    if bound.in_use.is_empty() {
        info!("forwarding on {} socket(s)", bound.listeners.len());
    } else {
        info!(
            "forwarding on {} socket(s), {} waiting for their port",
            bound.listeners.len(),
            bound.in_use.len()
        );
    }

    tokio::select! {
        result = forward::serve(bound, config) => Ok(result?),
        () = shutdown.wait() => {
            info!("shutting down");
            Ok(())
        }
    }
}

struct Shutdown {
    terminate: Signal,
    interrupt: Signal,
}

impl Shutdown {
    fn install() -> Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate()).context("installing SIGTERM handler")?,
            interrupt: signal(SignalKind::interrupt()).context("installing SIGINT handler")?,
        })
    }

    async fn wait(&mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
        }
    }
}

fn setup(specs: &[ForwardSpec], skip_enable: bool) -> Result<()> {
    let ipv6_dual_stack = sys::ipv6_dual_stack_default();
    conflict::check(specs, ipv6_dual_stack)?;
    // The unit runs the binary from where it is now; it is never copied or moved.
    let exe = std::env::current_exe().context("cannot determine the mongoose executable path")?;
    let manager = ServiceManager::host();
    manager.precheck_setup(&exe)?;
    log_rules(specs, ipv6_dual_stack);
    match manager.setup(&exe, specs, skip_enable)? {
        SetupOutcome::Installed => info!("setup complete"),
        SetupOutcome::Replaced => info!("setup complete (replaced previous unit)"),
    }
    Ok(())
}

fn remove() -> Result<()> {
    ServiceManager::host().remove()?;
    info!("mongoose service removed");
    Ok(())
}

fn show() -> Result<()> {
    println!("{}", ServiceManager::host().show()?);
    Ok(())
}
