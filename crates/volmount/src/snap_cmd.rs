//! Snapshot 相关命令 — 通过 HTTP 与 volmountd 通信

use crate::config::VolmountdConfig;
use crate::http_client;

pub fn execute_create(config: &VolmountdConfig, name: &str) {
    // name 格式: "volname" — 创建快照，description 用 name 字段
    match http_client::create_snapshot(config, name, name) {
        Ok(resp) => {
            println!("created snapshot #{} for block '{}'", resp.id, name);
        }
        Err(e) => {
            eprintln!("error: failed to create snapshot: {e}");
            eprintln!("  Make sure volmountd is running (vol mountd start)");
        }
    }
}

pub fn execute_list(config: &VolmountdConfig, vol: &str) {
    match http_client::list_snapshots(config, vol) {
        Ok(resp) => {
            if resp.snapshots.is_empty() {
                println!("no snapshots for block '{vol}'");
                return;
            }

            println!("Snapshots for block '{vol}':");
            for snap in &resp.snapshots {
                println!(
                    "  [{:>4}] {:<24}  entries={:>6}",
                    snap.id, snap.description, snap.root_set_count,
                );
            }
        }
        Err(e) => {
            eprintln!("error: failed to list snapshots: {e}");
        }
    }
}

pub fn execute_rollback(config: &VolmountdConfig, name: &str) {
    let (vol_name, snap_id_str) = if name.contains('/') {
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        (parts[0].to_string(), parts[1].to_string())
    } else {
        eprintln!("error: use 'vol/<snap-id>' format, e.g. 'myvol/3'");
        return;
    };

    let snap_id: u64 = match snap_id_str.parse() {
        Ok(id) => id,
        Err(_) => {
            eprintln!("error: invalid snapshot id '{snap_id_str}'");
            return;
        }
    };

    match http_client::rollback_snapshot(config, &vol_name, snap_id) {
        Ok(()) => {
            println!("rolled back block '{}' to snapshot {}", vol_name, snap_id);
        }
        Err(e) => {
            eprintln!("error: rollback failed: {e}");
        }
    }
}

pub fn execute_delete(config: &VolmountdConfig, name: &str) {
    let (vol_name, snap_id_str) = if name.contains('/') {
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        (parts[0].to_string(), parts[1].to_string())
    } else {
        eprintln!("error: use 'vol/<snap-id>' format, e.g. 'myvol/3'");
        return;
    };

    let snap_id: u64 = match snap_id_str.parse() {
        Ok(id) => id,
        Err(_) => {
            eprintln!("error: invalid snapshot id '{snap_id_str}'");
            return;
        }
    };

    match http_client::delete_snapshot(config, &vol_name, snap_id) {
        Ok(()) => {
            println!("deleted snapshot {} from block '{}'", snap_id, vol_name);
        }
        Err(e) => {
            eprintln!("error: failed to delete snapshot: {e}");
        }
    }
}
