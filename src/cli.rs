//! Command line interface definition.

use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};

use crate::spec::ForwardSpec;

const SPEC_HELP: &str = "Forwarding rule: [tcp:|udp:]LOCAL_IP:LOCAL_PORT:TARGET_IP:TARGET_PORT\n\
    Without a protocol prefix both TCP and UDP are forwarded. LOCAL_IP may be '*' \
    to bind both 0.0.0.0 and [::]. IPv6 addresses must be wrapped in []. \
    Ports may be inclusive ranges such as 10000-10099.";

/// `-v/--verbose`, accepted both before and after the subcommand but at most
/// once in total. It is declared per level instead of `global = true` because
/// clap silently merges repeated global flags across levels.
#[derive(Debug, Clone, Copy, Default, Args)]
pub struct Verbose {
    /// Enable verbose (debug) logging
    #[arg(short, long, action = ArgAction::SetTrue)]
    verbose: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "mongoose",
    version,
    about = "Mongoose: a simple TCP/UDP port forwarder"
)]
pub struct Cli {
    #[command(flatten)]
    verbose: Verbose,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start forwarding in the foreground
    Start {
        #[command(flatten)]
        verbose: Verbose,

        #[arg(required = true, value_name = "SPEC", help = SPEC_HELP)]
        specs: Vec<ForwardSpec>,
    },
    /// Install, enable and start a systemd service running `mongoose start`
    Setup {
        #[command(flatten)]
        verbose: Verbose,

        /// Only write the unit file: do not reload systemd, enable, start or
        /// restart the unit (a running service keeps its previous rules)
        #[arg(short, long)]
        skip_enable: bool,

        /// Replace an existing unit even if it was not written by `setup`, as
        /// long as it is a regular, removable file on persistent storage
        #[arg(short, long)]
        force: bool,

        #[arg(required = true, value_name = "SPEC", help = SPEC_HELP)]
        specs: Vec<ForwardSpec>,
    },
    /// Stop, disable and delete the systemd service created by `setup`
    Remove {
        #[command(flatten)]
        verbose: Verbose,

        /// Remove the unit even if it was not written by `setup`, as long as
        /// it is a regular, removable file on persistent storage
        #[arg(short, long)]
        force: bool,
    },
    /// Show the rules and state of the systemd service created by `setup`
    Show {
        #[command(flatten)]
        verbose: Verbose,
    },
}

impl Command {
    fn verbose(&self) -> Verbose {
        match self {
            Command::Start { verbose, .. }
            | Command::Setup { verbose, .. }
            | Command::Remove { verbose, .. }
            | Command::Show { verbose } => *verbose,
        }
    }
}

impl Cli {
    /// Parses and validates arguments, exiting with a usage error on failure.
    pub fn parse_args() -> Self {
        Self::try_parse_args(std::env::args_os()).unwrap_or_else(|err| err.exit())
    }

    /// Parses and validates arguments.
    pub fn try_parse_args<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let cli = Self::try_parse_from(args)?;
        if cli.verbose.verbose && cli.command.verbose().verbose {
            return Err(Self::command().error(
                ErrorKind::ArgumentConflict,
                "the argument '--verbose' cannot be used multiple times",
            ));
        }
        Ok(cli)
    }

    pub fn verbose(&self) -> bool {
        self.verbose.verbose || self.command.verbose().verbose
    }
}
