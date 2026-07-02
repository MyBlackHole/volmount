//! NBD 导出相关命令 — 通过 HTTP 与 volmountd 通信

use crate::config::VolmountdConfig;
use crate::http_client;

pub fn execute_list(config: &VolmountdConfig) {
    match http_client::list_nbd_exports(config) {
        Ok(resp) => {
            if resp.exports.is_empty() {
                println!("No NBD exports.");
                return;
            }
            println!("NBD Exports:");
            for export in &resp.exports {
                println!(
                    "  {}  size={}  status={}",
                    export.name, export.size, export.status,
                );
            }
        }
        Err(e) => {
            eprintln!("error: failed to list NBD exports: {e}");
        }
    }
}
