//! volmountd 集成测试
//!
//! 启动 volmountd 子进程，通过 HTTP API 测试完整生命周期。

use std::io::Read;
use std::process::{Child, Command};
use std::time::Duration;

use tempfile::TempDir;

/// 找到编译后的 volmountd 二进制路径
fn daemon_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop(); // 去掉测试二进制名称
    if path.ends_with("deps") {
        path.pop(); // 去掉 deps
    }
    let bin = path.join("volmountd");
    if !bin.exists() {
        // 可能在 target/debug/ 下
        let alt = path.parent().unwrap_or(&path).join("volmountd");
        if alt.exists() {
            return alt;
        }
    }
    bin
}

/// 创建临时配置
fn create_temp_config(dir: &TempDir) -> (std::path::PathBuf, String) {
    let config_path = dir.path().join("config.json");
    let home_dir = dir.path().join("home");
    let config = serde_json::json!({
        "home_dir": home_dir,
        "nbd_socket_path": "volmountd.sock",
        "auto_exports": [],
    });
    std::fs::write(&config_path, config.to_string()).unwrap();
    (
        config_path.clone(),
        config_path.to_string_lossy().to_string(),
    )
}

/// 启动 volmountd 并等待 HTTP 就绪，返回进程和端口
fn start_daemon(config_path: &std::path::Path) -> (Child, u16) {
    let port = 9876u16;
    let bin = daemon_binary();
    let mut child = Command::new(&bin)
        .arg("-c")
        .arg(config_path)
        .arg("--log-level")
        .arg("debug")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("failed to start volmountd");

    // 等待 HTTP 服务就绪
    let url = format!("http://127.0.0.1:{port}/api/v1/daemon/status");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .build();

    let max_retries = 20;
    for i in 0..max_retries {
        match agent.get(&url).call() {
            Ok(resp) if resp.status() == 200 => {
                return (child, port);
            }
            _ => {
                std::thread::sleep(Duration::from_millis(200));
                if i == max_retries - 1 {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("daemon did not start within {} ms", max_retries * 200);
                }
            }
        }
    }

    (child, port)
}

fn stop_daemon(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn http_get(url: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .build();
    let resp = agent.get(url).call().map_err(|e| format!("{e}"))?;
    let mut body = String::new();
    resp.into_reader()
        .read_to_string(&mut body)
        .map_err(|e| format!("{e}"))?;
    Ok(body)
}

fn http_post(url: &str, body: &[u8]) -> Result<(u16, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .build();
    let resp = agent
        .post(url)
        .set("Content-Type", "application/json")
        .send_bytes(body)
        .map_err(|e| format!("{e}"))?;
    let status = resp.status();
    let mut body_str = String::new();
    resp.into_reader()
        .read_to_string(&mut body_str)
        .map_err(|e| format!("{e}"))?;
    Ok((status, body_str))
}

fn http_delete(url: &str) -> Result<(u16, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .build();
    let resp = agent.delete(url).call().map_err(|e| format!("{e}"))?;
    let status = resp.status();
    let mut body_str = String::new();
    resp.into_reader()
        .read_to_string(&mut body_str)
        .map_err(|e| format!("{e}"))?;
    Ok((status, body_str))
}

#[test]
fn test_daemon_lifecycle() {
    let dir = TempDir::new().unwrap();
    let (config_path, _) = create_temp_config(&dir);

    let (mut child, port) = start_daemon(&config_path);
    let base = format!("http://127.0.0.1:{port}/api/v1");

    // 1. 状态检查
    let status = http_get(&format!("{base}/daemon/status")).unwrap();
    assert!(
        status.contains("version"),
        "status should contain version: {status}"
    );

    // 2. 创建 volume
    let create_body = serde_json::json!({
        "name": "test-vol",
        "size": "10M",
        "backend": "sparse",
    });
    let (status_code, body) = http_post(
        &format!("{base}/blocks"),
        &serde_json::to_vec(&create_body).unwrap(),
    )
    .unwrap();
    assert_eq!(status_code, 201, "create volume: {body}");

    // 3. 列出 blocks
    let list = http_get(&format!("{base}/blocks")).unwrap();
    assert!(
        list.contains("test-vol"),
        "list should contain test-vol: {list}"
    );

    // 4. 获取 volume 信息
    let info = http_get(&format!("{base}/blocks/test-vol")).unwrap();
    assert!(info.contains("test-vol"), "info: {info}");
    assert!(info.contains("10485760"), "capacity should be 10M: {info}");

    // 5. 创建快照
    let snap_body = serde_json::json!({ "description": "initial state" });
    let (status_code, body) = http_post(
        &format!("{base}/blocks/test-vol/snapshots"),
        &serde_json::to_vec(&snap_body).unwrap(),
    )
    .unwrap();
    assert_eq!(status_code, 201, "create snapshot: {body}");

    // 6. 列出快照
    let snaps = http_get(&format!("{base}/blocks/test-vol/snapshots")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&snaps).unwrap();
    assert_eq!(
        parsed["snapshots"].as_array().unwrap().len(),
        2,
        "snaps: {snaps}"
    );

    // 7. 挂载 volume（HTTP API）
    let (status_code, body) = http_post(&format!("{base}/blocks/test-vol/mount"), b"{}").unwrap();
    assert_eq!(status_code, 200, "mount: {body}");
    assert!(
        body.contains("export_name"),
        "mount response should have export_name: {body}"
    );
    assert!(
        body.contains("socket"),
        "mount response should have socket: {body}"
    );

    // 8. 卸载 volume（HTTP API）
    let (status_code, body) = http_post(&format!("{base}/blocks/test-vol/umount"), b"{}").unwrap();
    assert_eq!(status_code, 200, "umount: {body}");

    // 9. 创建快照集
    let snap1_body = serde_json::json!({ "description": "snapshot-alpha" });
    let (status_code, body) = http_post(
        &format!("{base}/blocks/test-vol/snapshots"),
        &serde_json::to_vec(&snap1_body).unwrap(),
    )
    .unwrap();
    assert_eq!(status_code, 201, "create snapshot 1: {body}");
    let snap1_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_u64()
        .unwrap();

    let snap2_body = serde_json::json!({ "description": "snapshot-beta" });
    let (status_code, body) = http_post(
        &format!("{base}/blocks/test-vol/snapshots"),
        &serde_json::to_vec(&snap2_body).unwrap(),
    )
    .unwrap();
    assert_eq!(status_code, 201, "create snapshot 2: {body}");
    let snap2_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_u64()
        .unwrap();

    // 10. 列出快照
    let snaps = http_get(&format!("{base}/blocks/test-vol/snapshots")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&snaps).unwrap();
    assert_eq!(
        parsed["snapshots"].as_array().unwrap().len(),
        4,
        "snaps: {snaps}"
    );

    // 11. 删除其中一个快照（第一个创建的 snapshot）
    let (status_code, body) =
        http_delete(&format!("{base}/blocks/test-vol/snapshots/{snap1_id}")).unwrap();
    assert_eq!(status_code, 200, "delete snapshot: {body}");

    // 12. 回滚到剩余快照（第二个创建的 snapshot）
    let (status_code, body) = http_post(
        &format!("{base}/blocks/test-vol/snapshots/{snap2_id}/rollback"),
        b"{}",
    )
    .unwrap();
    assert_eq!(status_code, 200, "rollback: {body}");

    // 13. 删除 volume
    let (status_code, _) = http_delete(&format!("{base}/blocks/test-vol")).unwrap();
    assert_eq!(status_code, 200, "delete volume");

    // 清理
    stop_daemon(&mut child);
}
