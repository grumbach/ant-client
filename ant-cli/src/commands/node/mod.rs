pub mod add;
pub mod daemon;
pub mod reset;

use clap::Subcommand;

use crate::commands::node::add::AddArgs;
use crate::commands::node::daemon::DaemonCommand;
use crate::commands::node::reset::ResetArgs;

#[derive(Subcommand)]
pub enum NodeCommand {
    /// Add one or more nodes to the registry
    Add(Box<AddArgs>),
    /// Manage the node daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Reset all node state (removes all data, logs, and clears the registry)
    Reset(ResetArgs),
}
