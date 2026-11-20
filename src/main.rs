use std::process::ExitCode;

use anyhow::{Context, Result};
use log::{debug, error, info, warn};

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
fn log_rules(specs: &[ForwardSpec]) {
    for (index, spec) in specs.iter().enumerate() {
        info!("rule #{}: {}", index + 1, spec.describe());
    }
}

async fn start(specs: &[ForwardSpec]) -> Result<()> {
    conflict::check(specs)?;
    log_rules(specs);

    match sys::raise_fd_limit() {
        Ok(limit) => debug!("open file limit: {limit}"),
        Err(err) => warn!("could not raise open file limit: {err}"),
    }
    let listeners = forward::bind_all(specs)?;
    info!("forwarding on {} socket(s)", listeners.len());

    tokio::select! {
        () = forward::serve(listeners, ForwardConfig::default()) => Ok(()),
        signal = shutdown_signal() => {
            signal?;
            info!("shutting down");
            Ok(())
        }
    }
}

async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("waiting for Ctrl-C"),
        _ = terminate.recv() => Ok(()),
    }
}

fn setup(specs: &[ForwardSpec], skip_enable: bool) -> Result<()> {
    conflict::check(specs)?;
    // The unit runs the binary from where it is now; it is never copied or moved.
    let exe = std::env::current_exe().context("cannot determine the mongoose executable path")?;
    let manager = ServiceManager::host();
    manager.precheck_setup(&exe)?;
    log_rules(specs);
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
