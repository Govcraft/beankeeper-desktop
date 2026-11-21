//! Extend upstream's command tree without copying its ledger commands.

use std::{net::SocketAddr, path::PathBuf};

use beankeeper_cli::cli::{Cli, PassphraseOptions};
use clap::{ArgMatches, Args, Command, CommandFactory, FromArgMatches, Parser};

/// Options for the combined UI and MCP listener.
#[derive(Args, Debug, Clone, Copy)]
pub struct ServeOptions {
    /// Listen address; port 0 requests an available port.
    #[arg(long, default_value = "127.0.0.1:8750")]
    pub bind: SocketAddr,
    /// Permit a non-loopback listener over plaintext HTTP, without relaxing Host/Origin checks.
    #[arg(long)]
    pub allow_remote: bool,
    /// Expose only observation tools; disable agent ledger and UI mutations.
    #[arg(long)]
    pub read_only: bool,
}

/// Address of an already-running Desktop server.
#[derive(Args, Debug, Clone, Copy)]
pub struct OpenOptions {
    /// Loopback address used by `bk serve`.
    #[arg(long, default_value = "127.0.0.1:8750", value_parser = loopback_address)]
    pub bind: SocketAddr,
}

pub(crate) fn loopback_address(value: &str) -> Result<SocketAddr, String> {
    let address: SocketAddr = value
        .parse()
        .map_err(|error| format!("invalid address: {error}"))?;
    if !crate::server::ui_loopback(address.ip()) || address.port() == 0 {
        return Err("bk open requires 127.0.0.1 or [::1] with a nonzero port".into());
    }
    Ok(address)
}

/// The upstream CLI extended with Desktop onboarding, hosting and bridging.
#[must_use]
pub fn command() -> Command {
    Cli::command()
        .version(env!("CARGO_PKG_VERSION"))
        .about("Beankeeper Desktop: local UI and agent-driven accounting")
        .subcommand(crate::service::ServiceOptions::augment_args(
            Command::new("service")
                .about("Manage per-user background hosting and lifecycle MCP tools"),
        ))
        .subcommand(ServeOptions::augment_args(Command::new("serve").about(
            "Serve the embedded UI and authenticated MCP on one listener",
        )))
        .subcommand(
            Command::new("ui")
                .about("Optional Desktop view links")
                .subcommand_required(true)
                .subcommand(crate::links::LinkOptions::augment_args(
                    Command::new("link").about("Return a view URL without opening a browser"),
                )),
        )
        .subcommand(OpenOptions::augment_args(
            Command::new("open").about("Open the running Desktop UI in your default browser"),
        ))
        .subcommand(crate::setup::SetupOptions::augment_args(
            Command::new("setup")
                .about("Create an encrypted ledger and configure your MCP clients")
                .subcommand(
                    Command::new("status")
                        .about("Read book and account setup status without changing the ledger")
                        .after_help("Example: bk setup status --json"),
                ),
        ))
        .subcommand(crate::product::LicenseOptions::augment_args(
            Command::new("license").about("Show, activate, or print compiled-in license trust"),
        ))
        .subcommand(
            Command::new("update-check")
                .about("Check the signed Desktop release feed, at most daily"),
        )
        .subcommand(
            Command::new("self-update")
                .about("Verify and install an eligible Desktop release (requires cosign)"),
        )
        .mut_subcommand("mcp", |command| {
            command.subcommand(crate::bridge::BridgeOptions::augment_args(
                Command::new("bridge").about("Forward stdio MCP to the running Desktop server"),
            ))
        })
}

/// Adapt the shared ledger options to the public upstream router factory.
///
/// Upstream owns parsing of these flags, passphrase resolution and token rules.
/// Its placeholder command is used only to construct the factory's `Cli` input.
///
/// # Errors
/// Returns a clap error if the upstream argument contract changes.
pub fn ledger_options(matches: &ArgMatches) -> Result<Cli, clap::Error> {
    let mut cli = Cli::try_parse_from(["bk", "mcp", "serve"])?;
    if let Some(path) = matches.get_one::<PathBuf>("db") {
        cli.db.clone_from(path);
    }
    cli.company = matches.get_one("company").cloned();
    cli.passphrase = PassphraseOptions::from_arg_matches(matches)?;
    cli.agent_token = matches.get_one("agent_token").cloned();
    Ok(cli)
}
