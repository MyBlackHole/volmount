use std::process::Command;

use crate::config::VolmountdConfig;
use crate::http_client;

pub fn execute_start(config: &VolmountdConfig) {
    if is_running(config) {
        println!("volmountd is already running");
        return;
    }

    let daemon_bin = find_daemon_binary();
    let config_path = config_path(config);

    match Command::new(&daemon_bin)
        .arg("-c")
        .arg(&config_path)
        .spawn()
    {
        Ok(child) => {
            let pid = child.id();
            let pid_path = config.resolved_home_dir().join("volmountd.pid");
            if let Err(e) = std::fs::write(&pid_path, pid.to_string()) {
                eprintln!("warning: failed to write PID file: {e}");
            }
            println!("volmountd started (PID {pid})");
        }
        Err(e) => {
            eprintln!("error: failed to start volmountd: {e}");
            eprintln!(
                "Make sure volmountd is installed and in your PATH, or run it directly:\n  {} -c {}",
                daemon_bin, config_path
            );
        }
    }
}

pub fn execute_stop(config: &VolmountdConfig) {
    let pid_path = config.resolved_home_dir().join("volmountd.pid");
    let pid_str = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            eprintln!(
                "volmountd is not running (no PID file at {})",
                pid_path.display()
            );
            return;
        }
    };

    let pid: u32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("invalid PID file: {pid_str}");
            let _ = std::fs::remove_file(&pid_path);
            return;
        }
    };

    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        match status {
            Ok(s) if s.success() => {
                println!("sent SIGTERM to volmountd (PID {pid})");
                let _ = std::fs::remove_file(&pid_path);
            }
            Ok(_) => {
                eprintln!("failed to stop volmountd (PID {pid}) — maybe already exited?");
                let _ = std::fs::remove_file(&pid_path);
            }
            Err(e) => {
                eprintln!("error: kill failed: {e}");
            }
        }
    }

    #[cfg(not(unix))]
    {
        eprintln!("daemon stop only supported on Unix");
    }
}

pub fn execute_status(config: &VolmountdConfig) {
    let home = config.resolved_home_dir();
    let nbd_socket = config.resolved_nbd_socket();

    println!("volmountd status:");
    println!("  Home:     {}", home.display());
    println!("  Socket:   {}", nbd_socket.display());
    println!("  Config:   {}", config_path(config));
    println!("  HTTP:     http://127.0.0.1:{}", config.http_port);

    // 尝试 HTTP API 获取详细状态
    match http_client::get_status(config) {
        Ok(status) => {
            println!("  Version:  {}", status.version);
            println!("  Running:  yes (via HTTP)");
            println!("  Blocks:   {}", status.blocks.len());
            if !status.blocks.is_empty() {
                println!("  Open:     {:?}", status.blocks);
            }
        }
        Err(_) => {
            // 后备：检查 PID 文件
            let running = is_running(config);
            println!("  Running:  {}", if running { "yes" } else { "no" });

            if running {
                let pid_path = home.join("volmountd.pid");
                if let Ok(pid) = std::fs::read_to_string(&pid_path) {
                    println!("  PID:      {}", pid.trim());
                }
            }

            let block_dir = config.blocks_dir();
            if block_dir.exists() {
                let count = std::fs::read_dir(&block_dir)
                    .map(|e| {
                        e.filter_map(|e| e.ok().filter(|e| e.path().is_dir()))
                            .count()
                    })
                    .unwrap_or(0);
                println!("  Blocks:   {count} (local scan)");
            } else {
                println!("  Blocks:   0 (not initialized)");
            }
        }
    }
}

fn is_running(config: &VolmountdConfig) -> bool {
    let pid_path = config.resolved_home_dir().join("volmountd.pid");
    let pid_str = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return false,
    };

    let pid: u32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => {
            let _ = std::fs::remove_file(&pid_path);
            return false;
        }
    };

    #[cfg(unix)]
    {
        let status = Command::new("kill").arg("-0").arg(pid.to_string()).status();
        match status {
            Ok(s) if s.success() => true,
            _ => {
                let _ = std::fs::remove_file(&pid_path);
                false
            }
        }
    }

    #[cfg(not(unix))]
    {
        false
    }
}

fn find_daemon_binary() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("volmountd");
            if sibling.exists() {
                return sibling.to_string_lossy().to_string();
            }
        }
    }
    "volmountd".to_string()
}

fn config_path(config: &VolmountdConfig) -> String {
    let home = config.resolved_home_dir();
    let path = home.join("config.json");
    path.to_string_lossy().to_string()
}
