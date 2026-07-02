//! volmount CLI — volmount 命令行工具

mod cli;
mod config;
mod daemon_cmd;
mod http_client;
mod inspect_cmd;
mod nbd_cmd;
mod snap_cmd;
mod subvol_cmd;
mod vol_cmd;

use cli::{BlockAction, Command, InspectTarget};
use config::VolmountdConfig;

fn main() {
    let cli = cli::Cli::parse_args();

    // 加载配置（~/.volmount/config.json）
    let config_path = VolmountdConfig::default_config_path();
    let config = if config_path.exists() {
        VolmountdConfig::load(&config_path).unwrap_or_else(|e| {
            eprintln!("warning: failed to load config: {e}, using defaults");
            VolmountdConfig::default()
        })
    } else {
        VolmountdConfig::default()
    };

    match cli.command {
        Command::Pool { action } => match action {
            cli::PoolAction::Create { name, backend } => {
                eprintln!(
                    "pool create not yet implemented (pool={name}, block_device_backend={backend})"
                );
            }
            cli::PoolAction::List => {
                eprintln!("pool list not yet implemented");
            }
            cli::PoolAction::Info { name } => {
                eprintln!("pool info not yet implemented (pool={name})");
            }
        },
        Command::Block { action } => match action {
            BlockAction::Create { name, size } => {
                vol_cmd::execute_create(&config, &name, &size);
            }
            BlockAction::List { .. } => {
                vol_cmd::execute_list(&config);
            }
            BlockAction::Info { name } => {
                vol_cmd::execute_info(&config, &name);
            }
            BlockAction::Delete { name } => {
                vol_cmd::execute_delete(&config, &name);
            }
            BlockAction::Mount { name, device } => {
                vol_cmd::execute_mount(&config, &name, &device);
            }
            BlockAction::Umount { name, device } => {
                vol_cmd::execute_umount(&config, &name, &device);
            }
            BlockAction::Resize { name, size } => {
                eprintln!("resize not yet implemented (vol={name}, size={size})");
            }
            BlockAction::Usage { name } => {
                vol_cmd::execute_info(&config, &name);
            }
        },
        Command::Snap { action } => match action {
            cli::SnapAction::Create { name } => {
                snap_cmd::execute_create(&config, &name);
            }
            cli::SnapAction::List { vol } => {
                snap_cmd::execute_list(&config, &vol);
            }
            cli::SnapAction::Rollback { name } => {
                snap_cmd::execute_rollback(&config, &name);
            }
            cli::SnapAction::Delete { name } => {
                snap_cmd::execute_delete(&config, &name);
            }
            cli::SnapAction::Diff { snap1, snap2 } => {
                eprintln!("snap diff not yet implemented ({snap1} vs {snap2})");
            }
        },
        Command::Daemon { action } => match action {
            cli::DaemonAction::Start => {
                daemon_cmd::execute_start(&config);
            }
            cli::DaemonAction::Stop => {
                daemon_cmd::execute_stop(&config);
            }
        },
        Command::Nbd { action } => match action {
            cli::NbdAction::List => {
                nbd_cmd::execute_list(&config);
            }
        },
        Command::Subvol { action } => match action {
            cli::SubvolAction::List { vol } => {
                subvol_cmd::execute_list(&config, &vol);
            }
            cli::SubvolAction::Create { vol, name, size } => {
                subvol_cmd::execute_create(&config, &vol, &name, &size);
            }
            cli::SubvolAction::Delete { vol, id } => {
                subvol_cmd::execute_delete(&config, &vol, id);
            }
        },
        Command::Status => {
            daemon_cmd::execute_status(&config);
        }
        Command::Inspect { path, target } => match target {
            None => inspect_cmd::auto_inspect(&config, &path),
            Some(InspectTarget::Meta) => inspect_cmd::inspect_meta(&config, &path),
            Some(InspectTarget::Btree) => inspect_cmd::inspect_btree(&config, &path),
            Some(InspectTarget::Wal) => inspect_cmd::inspect_wal(&path),
            Some(InspectTarget::Snapshot) => inspect_cmd::inspect_snapshot(&config, &path),
            Some(InspectTarget::Block { paddr }) => {
                inspect_cmd::inspect_block(&config, &path, paddr)
            }
        },
    }
}
