//! 子卷相关命令 — 通过 HTTP 与 volmountd 通信

use crate::config::VolmountdConfig;
use crate::http_client;

pub fn execute_list(config: &VolmountdConfig, vol: &str) {
    match http_client::list_subvols(config, vol) {
        Ok(resp) => {
            if resp.subvols.is_empty() {
                println!("no subvolumes for block '{vol}'");
                return;
            }
            println!("Subvolumes for block '{vol}':");
            for sv in &resp.subvols {
                println!(
                    "  [{:>4}] size={:>12}  status={}",
                    sv.id, sv.size, sv.status,
                );
            }
        }
        Err(e) => {
            eprintln!("error: failed to list subvolumes: {e}");
        }
    }
}

pub fn execute_create(config: &VolmountdConfig, vol: &str, name: &str, size: &str) {
    match http_client::create_subvol(config, vol, name, size) {
        Ok(resp) => {
            println!(
                "created subvolume #{} '{}' in block '{}'",
                resp.id, name, vol
            );
        }
        Err(e) => {
            eprintln!("error: failed to create subvolume: {e}");
        }
    }
}

pub fn execute_delete(config: &VolmountdConfig, vol: &str, id: u64) {
    match http_client::delete_subvol(config, vol, id) {
        Ok(()) => {
            println!("deleted subvolume #{} from block '{}'", id, vol);
        }
        Err(e) => {
            eprintln!("error: failed to delete subvolume: {e}");
        }
    }
}
