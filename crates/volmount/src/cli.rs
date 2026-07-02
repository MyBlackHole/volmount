//! CLI 命令树定义

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "volmount",
    version,
    about = "Copy-on-write block device manager"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Pool operations
    Pool {
        #[command(subcommand)]
        action: PoolAction,
    },
    /// Block device operations
    Block {
        #[command(subcommand)]
        action: BlockAction,
    },
    /// Snapshot operations
    Snap {
        #[command(subcommand)]
        action: SnapAction,
    },
    /// Start the volmount daemon
    #[command(name = "volmountd")]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Health check
    Status,
    /// Subvolume operations
    Subvol {
        #[command(subcommand)]
        action: SubvolAction,
    },
    /// NBD device export operations
    Nbd {
        #[command(subcommand)]
        action: NbdAction,
    },
    /// Debug: inspect storage files (offline)
    Inspect {
        /// Path to the backend file or block directory
        path: String,
        /// Optional sub-command for specific data type
        #[command(subcommand)]
        target: Option<InspectTarget>,
    },
}

impl Cli {
    /// Parse CLI args and return the selected action
    pub fn parse_args() -> Self {
        <Self as Parser>::parse()
    }
}

#[derive(Subcommand)]
pub enum PoolAction {
    Create {
        name: String,
        /// Block device backend type (sparse)
        #[arg(long)]
        backend: String,
    },
    List,
    Info {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum BlockAction {
    Create {
        name: String,
        #[arg(long)]
        size: String,
    },
    List {
        pool: Option<String>,
    },
    Resize {
        name: String,
        #[arg(long)]
        size: String,
    },
    Delete {
        name: String,
    },
    Info {
        name: String,
    },
    Mount {
        name: String,
        /// NBD block device path（默认 /dev/nbd0）
        #[arg(long, default_value = "/dev/nbd0")]
        device: String,
    },
    Umount {
        name: String,
        /// NBD block device path（默认 /dev/nbd0）
        #[arg(long, default_value = "/dev/nbd0")]
        device: String,
    },
    Usage {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum SnapAction {
    Create { name: String },
    List { vol: String },
    Rollback { name: String },
    Delete { name: String },
    Diff { snap1: String, snap2: String },
}

#[derive(Subcommand)]
pub enum DaemonAction {
    Start,
    Stop,
}

/// inspect 子命令（调试工具）
/// Subvolume operations
#[derive(Subcommand)]
pub enum SubvolAction {
    Create {
        /// Block device name
        vol: String,
        /// Subvolume name
        name: String,
        /// Size (e.g. "1G", "500M")
        #[arg(long, default_value = "1G")]
        size: String,
    },
    List {
        /// Block device name
        vol: String,
    },
    Delete {
        /// Block device name
        vol: String,
        /// Subvolume ID
        id: u64,
    },
}

/// NBD export operations
#[derive(Subcommand)]
pub enum NbdAction {
    /// List NBD exports
    List,
}

/// inspect 子命令（调试工具）
#[derive(Subcommand)]
pub enum InspectTarget {
    /// Dump COW mapping table
    Btree,
    /// Dump WAL entry sequence
    Wal,
    /// Dump block metadata
    Meta,
    /// Dump snapshot tree
    Snapshot,
    /// Dump raw block data at physical address
    Block {
        /// Physical block address
        paddr: u64,
    },
}
