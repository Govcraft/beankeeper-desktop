//! Extend upstream's command tree without copying its ledger commands.

use std::{net::SocketAddr, path::PathBuf};

use beankeeper_cli::cli::{Cli, PassphraseOptions};
use clap::{ArgMatches, Args, Command, CommandFactory, FromArgMatches, Parser};

use crate::instance::ListenerAddress;

/// Options for the combined UI and MCP listener.
#[derive(Args, Debug, Clone, Copy)]
pub struct ServeOptions {
    /// Listen address; port 0 requests an available port.
    #[arg(long, default_value_t = ListenerAddress::default().into())]
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
    /// Select a loopback listener instead of the running managed service.
    #[arg(long)]
    pub(crate) bind: Option<ListenerAddress>,
}

/// The upstream CLI extended with Desktop onboarding, hosting and bridging.
#[must_use]
pub fn command() -> Command {
    with_desktop_setup(Cli::command())
        .mut_subcommand("audit", |audit| {
            audit
                .subcommand(
                    Command::new("since")
                        .about("Read grouped changes since the owner was last informed"),
                )
                .subcommand(crate::changes::MarkSeen::augment_args(
                    Command::new("mark-seen").about("Record that a page of changes was delivered"),
                ))
        })
        .version(env!("CARGO_PKG_VERSION"))
        .about("Beankeeper Desktop: local UI and agent-driven accounting")
        .subcommand(crate::agents::AgentOptions::augment_args(
            Command::new("agents").about("Connect local agents to Beankeeper"),
        ))
        .subcommand(crate::service::ServiceOptions::augment_args(
            Command::new("service")
                .about("Manage per-user background hosting and lifecycle MCP tools"),
        ))
        .subcommand(crate::backups::BackupOptions::augment_args(
            Command::new("backups").about("List, create, and restore Desktop's local backups"),
        ))
        .subcommand(crate::demo::DemoOptions::augment_args(
            Command::new("demo").about("Manage fictional books on the running service"),
        ))
        .subcommand(crate::imports::command::ImportOptions::augment_args(
            Command::new("import").about("Import a statement through the running Desktop"),
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
        .subcommand(crate::product::LicenseOptions::augment_args(
            Command::new("license")
                .about("Show, activate, remove, return, or print compiled-in license trust"),
        ))
        .subcommand(
            Command::new("doctor").about("Report the locally installed Claude extension status"),
        )
        .subcommand(
            Command::new("update-check")
                .about("Check the signed Desktop release feed, at most daily"),
        )
        .subcommand(
            Command::new("self-update").about("Verify and install an eligible Desktop release"),
        )
        .mut_subcommand("mcp", |command| {
            command
                .subcommand(crate::bridge::BridgeOptions::augment_args(
                    Command::new("bridge").about("Forward stdio MCP to the running Desktop server"),
                ))
                .subcommand(
                    Command::new("extension")
                        .about("Serve the Claude extension's setup and ledger tools over stdio")
                        .hide(true),
                )
        })
}

fn with_desktop_setup(command: Command) -> Command {
    command.mut_subcommand("setup", |setup| {
        crate::setup::SetupOptions::augment_args(setup)
            .about("Create an encrypted ledger and configure your MCP clients")
            .subcommand_required(false)
            .arg_required_else_help(false)
            .mut_subcommand("status", |status| {
                status
                    .about("Read book and account setup status without changing the ledger")
                    .after_help("Example: bk setup status --json")
            })
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
    cli.actor = matches.get_one("actor").cloned();
    cli.passphrase = PassphraseOptions::from_arg_matches(matches)?;
    cli.agent_token = matches.get_one("agent_token").cloned();
    Ok(cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_setup_preserves_the_upstream_command_tree() -> Result<(), clap::Error> {
        let upstream = Command::new("bk").subcommand(
            Command::new("setup")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(Command::new("status").alias("state")),
        );
        let combined = with_desktop_setup(upstream);
        combined.clone().debug_assert();

        let bare = combined.clone().try_get_matches_from(["bk", "setup"])?;
        assert!(bare.subcommand_matches("setup").is_some());
        let status = combined
            .clone()
            .try_get_matches_from(["bk", "setup", "state"])?;
        assert_eq!(
            status
                .subcommand_matches("setup")
                .and_then(ArgMatches::subcommand_name),
            Some("status")
        );
        Ok(())
    }

    #[test]
    fn desktop_setup_still_accepts_provisioning_without_a_subcommand() -> Result<(), clap::Error> {
        for (args, force) in [
            (vec!["bk", "setup"], false),
            (vec!["bk", "setup", "--force", "--non-interactive"], true),
        ] {
            let matches = command().try_get_matches_from(args)?;
            let setup = matches
                .subcommand_matches("setup")
                .unwrap_or_else(|| unreachable!());
            assert!(setup.subcommand_name().is_none());
            let options = crate::setup::SetupOptions::from_arg_matches(setup)?;
            assert_eq!(options.force, force);
            assert_eq!(options.non_interactive, force);
        }
        Ok(())
    }
}
