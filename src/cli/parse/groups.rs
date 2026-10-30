//! Clap parser for the `:sync` group.
//!
//! `:tool` and `:db` derive `clap::Parser` directly on their subcommand enums:
//! they have no group-level options, so a wrapper struct would only restate the
//! subcommand. `:sync` needs a struct because `--reset` belongs to the group
//! rather than to any one account subcommand.

use clap::Parser;

use crate::cli::SyncSubcommand;

#[derive(Parser)]
#[command(
    name = "im :sync",
    bin_name = "im :sync",
    about = "Exchange events with other devices, or manage this device's account",
    disable_help_subcommand = true,
    args_conflicts_with_subcommands = true
)]
pub(super) struct SyncCli {
    #[command(subcommand)]
    pub(super) sub: Option<SyncSubcommand>,
    /// Discard the local copy and rebuild it from the server
    #[arg(long)]
    pub(super) reset: bool,
}
