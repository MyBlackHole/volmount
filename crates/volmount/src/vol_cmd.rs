//! Block device 相关命令 — 通过 HTTP 与 volmountd 通信

use std::process::Command;

use crate::config::VolmountdConfig;
use crate::http_client;

pub fn execute_create(config: &VolmountdConfig, name: &str, size: &str) {
    match http_client::create_block(config, name, size, None) {
        Ok(resp) => {
            println!("created block device '{name}'");
            println!("  capacity: {}", format_size(resp.capacity));
            println!("  block size: {}", resp.block_size);
            println!("  block device backend: {}", resp.backend);
        }
        Err(e) => {
            eprintln!("error: failed to create block device '{name}': {e}");
            eprintln!("  Make sure volmountd is running (vol mountd start)");
        }
    }
}

pub fn execute_list(config: &VolmountdConfig) {
    match http_client::list_blocks(config) {
        Ok(resp) => {
            if resp.blocks.is_empty() {
                println!("no blocks found");
                return;
            }

            println!("{:<24} {:>12} {:>12}  STATUS", "NAME", "CAPACITY", "USED");
            println!("{}", "-".repeat(62));
            for vol in &resp.blocks {
                println!(
                    "{:<24} {:>12} {:>12}  {}",
                    vol.name,
                    format_size(vol.capacity),
                    format_size(vol.used),
                    vol.status,
                );
            }
        }
        Err(e) => {
            eprintln!("error: failed to list blocks: {e}");
            eprintln!("  Make sure volmountd is running (vol mountd start)");
        }
    }
}

pub fn execute_info(config: &VolmountdConfig, name: &str) {
    match http_client::get_block_info(config, name) {
        Ok(info) => {
            println!("Block: {}", info.name);
            println!("  ID:        {}", info.id);
            println!("  Capacity:  {}", format_size(info.capacity));
            println!("  Block:     {}", info.block_size);
            println!("  Block backend: {}", info.backend);
            println!("  Created:   {}", info.created_at);
            println!("  Stored:    {}", format_size(info.stored));
            println!("  Status:    {}", info.status);
        }
        Err(e) => {
            eprintln!("error: failed to get block info: {e}");
            eprintln!("  Make sure volmountd is running (vol mountd start)");
        }
    }
}

pub fn execute_delete(config: &VolmountdConfig, name: &str) {
    match http_client::delete_block(config, name) {
        Ok(resp) => {
            if resp.freed > 0 {
                println!("deleted block '{name}' (freed {})", format_size(resp.freed));
            } else {
                println!("deleted block '{name}'");
            }
        }
        Err(e) => {
            eprintln!("error: failed to delete block '{name}': {e}");
            eprintln!("  Make sure volmountd is running (vol mountd start)");
        }
    }
}

pub fn execute_mount(config: &VolmountdConfig, name: &str, device: &str) {
    match http_client::mount_block(config, name) {
        Ok(resp) => {
            println!("Registered NBD export for block '{name}'");
            println!("  Export:   {}", resp.export_name);
            println!("  Socket:   {}", resp.socket);
            println!("  Size:     {}", format_size(resp.size));

            // 尝试加载 nbd 内核模块
            let mod_result = Command::new("sudo").args(["modprobe", "nbd"]).output();
            if let Err(e) = mod_result {
                println!("\n  warning: could not load nbd kernel module: {e}");
                println!("  Try: sudo modprobe nbd");
            }

            // 尝试连接 nbd-client
            let result = Command::new("sudo")
                .args([
                    "nbd-client",
                    "-unix",
                    &resp.socket,
                    device,
                    &resp.export_name,
                ])
                .output();

            match result {
                Ok(output) if output.status.success() => {
                    let msg = String::from_utf8_lossy(&output.stdout);
                    println!("  Device:   {device}");
                    if !msg.trim().is_empty() {
                        println!("  {}", msg.trim());
                    }
                    println!("\nBlock '{name}' mounted at {device}");
                    println!("  sudo mount {device} /mnt/{name}   # after mkfs if first time");
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    eprintln!("\n  error: nbd-client failed: {stderr}");
                    eprintln!("  Try manually:");
                    eprintln!(
                        "    sudo nbd-client -unix {} {} {}",
                        resp.socket, device, resp.export_name
                    );
                }
                Err(e) => {
                    eprintln!("\n  warning: nbd-client not available: {e}");
                    eprintln!("  Install nbd-client and try manually:");
                    eprintln!(
                        "    sudo nbd-client -unix {} {} {}",
                        resp.socket, device, resp.export_name
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("error: failed to mount block '{name}': {e}");
            eprintln!("  Make sure volmountd is running (vol mountd start)");
        }
    }
}

pub fn execute_umount(config: &VolmountdConfig, name: &str, device: &str) {
    // 先断开内核 NBD 设备
    let disconnect = Command::new("sudo")
        .args(["nbd-client", "-d", device])
        .output();

    match disconnect {
        Ok(output) if output.status.success() => {
            println!("Disconnected {device}");
        }
        Ok(output) => {
            let msg = String::from_utf8_lossy(&output.stderr);
            eprintln!("warning: nbd-client -d {device}: {msg}");
        }
        Err(e) => {
            println!("  (nbd-client not available, skipping device disconnect: {e})");
        }
    }

    // 从 daemon 取消注册 NBD export
    match http_client::umount_block(config, name) {
        Ok(()) => {
            println!("Unmounted block '{name}'");
        }
        Err(e) => {
            eprintln!("error: failed to unmount block '{name}': {e}");
        }
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{bytes} B")
    } else {
        format!("{:.1}{}", size, UNITS[unit_idx])
    }
}
