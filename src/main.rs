use std::process::ExitCode;

use anyhow::{Context, Result};
use log::{error, info, warn};

use mongoose::cli::{Cli, Command};
use mongoose::forward::{self, ForwardConfig};
use mongoose::service::{DEFAULT_UNIT_DIR, HostSystem, ServiceManager, SetupOutcome};
use mongoose::spec::ForwardSpec;
use mongoose::{logging, sys};

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
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn start(specs: &[ForwardSpec]) -> Result<()> {
    match sys::raise_fd_limit() {
        Ok(limit) => log::debug!("open file limit: {limit}"),
        Err(err) => warn!("could not raise open file limit: {err}"),
    }

    let listeners = forward::bind_all(specs)?;
    for spec in specs {
        info!("forwarding {spec}");
    }

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
    let exe = std::env::current_exe().context("cannot determine the mongoose executable path")?;
    let manager = ServiceManager::new(HostSystem, DEFAULT_UNIT_DIR);
    match manager.setup(&exe, specs, skip_enable)? {
        SetupOutcome::Installed => info!("setup complete"),
        SetupOutcome::Replaced => info!("setup complete (replaced previous unit)"),
    }
    Ok(())
}

fn remove() -> Result<()> {
    ServiceManager::new(HostSystem, DEFAULT_UNIT_DIR).remove()?;
    info!("mongoose service removed");
    Ok(())
}
