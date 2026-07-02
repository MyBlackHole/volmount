//! volmountd 功能测试 — 完整生命周期 + 错误路径 + 边界情况
//!
//! 覆盖现有集成测试未覆盖的场景：
//!   - 各种错误路径（无效参数、重复创建、不存在的资源）
//!   - 多卷管理
//!   - mount/umount 幂等性
//!   - 快照边界情况

use std::io::Read;
use std::net::TcpListener;
use std::process::{Child, Command};
use std::time::Duration;

use tempfile::TempDir;

// ─── Helper ───

fn daemon_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let bin = path.join("volmountd");
    if bin.exists() {
        return bin;
    }
    let alt = path.parent().unwrap_or(&path).join("volmountd");
    if alt.exists() {
        return alt;
    }
    panic!("volmountd binary not found at {:?}", bin);
}

fn start_daemon() -> (Child, u16, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let home_dir = dir.path().join("home");
    let config_path = dir.path().join("config.json");
    let port = reserve_port();

    let config = serde_json::json!({
        "home_dir": home_dir,
        "nbd_socket_path": "run/volmountd.sock",
        "http_port": port,
        "auto_exports": [],
    });
    std::fs::write(&config_path, config.to_string()).unwrap();
    let bin = daemon_binary();
    let mut child = Command::new(&bin)
        .arg("-c")
        .arg(&config_path)
        .arg("--log-level")
        .arg("error")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start volmountd");

    // 检查 daemon 是否立即退出
    if let Ok(Some(status)) = child.try_wait() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        panic!("daemon exited immediately with status {status}: {stderr}");
    }

    wait_for_ready(port);
    (child, port, dir)
}

fn reserve_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

fn wait_for_ready(port: u16) {
    let url = format!("http://127.0.0.1:{port}/api/v1/daemon/status");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .build();
    for i in 0..30 {
        if agent.get(&url).call().is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
        if i == 29 {
            panic!("daemon did not start within 6s");
        }
    }
}

fn stop_daemon(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn read_response(resp: ureq::Response) -> (u16, String) {
    let status = resp.status();
    let mut body = String::new();
    let _ = resp.into_reader().read_to_string(&mut body);
    (status, body)
}

fn http_get(url: &str) -> Result<(u16, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .build();
    match agent.get(url).call() {
        Ok(resp) => Ok(read_response(resp)),
        Err(ureq::Error::Status(_, resp)) => Ok(read_response(resp)),
        Err(e) => Err(format!("{e}")),
    }
}

fn http_post(url: &str, body: &[u8]) -> Result<(u16, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .build();
    match agent
        .post(url)
        .set("Content-Type", "application/json")
        .send_bytes(body)
    {
        Ok(resp) => Ok(read_response(resp)),
        Err(ureq::Error::Status(_, resp)) => Ok(read_response(resp)),
        Err(e) => Err(format!("{e}")),
    }
}

fn http_delete(url: &str) -> Result<(u16, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .build();
    match agent.delete(url).call() {
        Ok(resp) => Ok(read_response(resp)),
        Err(ureq::Error::Status(_, resp)) => Ok(read_response(resp)),
        Err(e) => Err(format!("{e}")),
    }
}

fn create_volume(base: &str, name: &str, size: &str) {
    let body = serde_json::json!({"name": name, "size": size, "backend": "sparse"});
    let (status, resp) = http_post(
        &format!("{base}/blocks"),
        &serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    assert_eq!(status, 201, "create {name}: {resp}");
}

// ─── Tests ───

#[test]
fn test_create_volume_invalid_size() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    let body = serde_json::json!({"name": "bad-size", "size": "invalid", "backend": "sparse"});
    let (status, resp) = http_post(
        &format!("{base}/blocks"),
        &serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    assert_eq!(status, 400, "status: {resp}");
    assert!(resp.contains("invalid size"), "resp: {resp}");

    stop_daemon(&mut child);
}

#[test]
fn test_create_volume_unknown_backend() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    let body = serde_json::json!({"name": "bad-backend", "size": "10M", "backend": "ceph"});
    let (status, resp) = http_post(
        &format!("{base}/blocks"),
        &serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    assert_eq!(status, 400, "status: {resp}");
    assert!(
        resp.contains("unknown block device backend"),
        "resp: {resp}"
    );

    stop_daemon(&mut child);
}

#[test]
fn test_create_duplicate_volume() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    create_volume(&base, "dup", "10M");

    let body = serde_json::json!({"name": "dup", "size": "10M", "backend": "sparse"});
    let (status, resp) = http_post(
        &format!("{base}/blocks"),
        &serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    assert_eq!(status, 409, "status: {resp}");
    assert!(resp.contains("already exists"), "resp: {resp}");

    stop_daemon(&mut child);
}

#[test]
fn test_delete_nonexistent_volume() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    let (status, _) = http_delete(&format!("{base}/blocks/nonexistent")).unwrap();
    assert_eq!(status, 404);

    stop_daemon(&mut child);
}

#[test]
fn test_volume_info_nonexistent() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    let (status, resp) = http_get(&format!("{base}/blocks/nonexistent")).unwrap();
    assert_eq!(status, 404, "resp: {resp}");
    assert!(resp.contains("not found"), "resp: {resp}");

    stop_daemon(&mut child);
}

#[test]
fn test_volume_list_empty() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    let (status, resp) = http_get(&format!("{base}/blocks")).unwrap();
    assert_eq!(status, 200);
    let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        parsed["blocks"].as_array().unwrap().is_empty(),
        "expected empty list: {resp}"
    );

    stop_daemon(&mut child);
}

#[test]
fn test_mount_nonexistent_volume() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    let (status, resp) = http_post(&format!("{base}/blocks/nope/mount"), b"{}").unwrap();
    assert_eq!(status, 404, "resp: {resp}");
    assert!(resp.contains("not open"), "resp: {resp}");

    stop_daemon(&mut child);
}

#[test]
fn test_mount_umount_idempotent() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    create_volume(&base, "idempotent", "10M");

    let (s1, _) = http_post(&format!("{base}/blocks/idempotent/mount"), b"{}").unwrap();
    assert_eq!(s1, 200);

    let (s2, _) = http_post(&format!("{base}/blocks/idempotent/mount"), b"{}").unwrap();
    assert_eq!(s2, 200);

    let (s3, _) = http_post(&format!("{base}/blocks/idempotent/umount"), b"{}").unwrap();
    assert_eq!(s3, 200);

    let (s4, _) = http_post(&format!("{base}/blocks/idempotent/umount"), b"{}").unwrap();
    assert_eq!(s4, 200);

    stop_daemon(&mut child);
}

#[test]
fn test_multi_volume_lifecycle() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    create_volume(&base, "alpha", "1M");
    create_volume(&base, "beta", "2M");
    create_volume(&base, "gamma", "4M");

    let (_, list) = http_get(&format!("{base}/blocks")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&list).unwrap();
    let names: Vec<&str> = parsed["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alpha"), "names: {names:?}");
    assert!(names.contains(&"beta"), "names: {names:?}");
    assert!(names.contains(&"gamma"), "names: {names:?}");

    for name in &["alpha", "beta", "gamma"] {
        let (_, info) = http_get(&format!("{base}/blocks/{name}")).unwrap();
        assert!(info.contains(name), "info for {name}: {info}");
    }

    http_delete(&format!("{base}/blocks/beta")).unwrap();

    let (_, list2) = http_get(&format!("{base}/blocks")).unwrap();
    let parsed2: serde_json::Value = serde_json::from_str(&list2).unwrap();
    let names2: Vec<&str> = parsed2["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(
        !names2.contains(&"beta"),
        "beta should be deleted: {names2:?}"
    );

    let (s, _) = http_delete(&format!("{base}/blocks/beta")).unwrap();
    assert_eq!(s, 404);

    stop_daemon(&mut child);
}

#[test]
fn test_snapshot_edge_cases() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    let snap_body = serde_json::json!({"description": "should fail"});
    let (status, _) = http_post(
        &format!("{base}/blocks/nope/snapshots"),
        &serde_json::to_vec(&snap_body).unwrap(),
    )
    .unwrap();
    assert_eq!(status, 404);

    create_volume(&base, "snap-edge", "10M");

    let (status, _) = http_get(&format!("{base}/blocks/nope/snapshots")).unwrap();
    assert_eq!(status, 404);

    let body = serde_json::json!({"description": "first"});
    let (s, resp) = http_post(
        &format!("{base}/blocks/snap-edge/snapshots"),
        &serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    assert_eq!(s, 201);
    let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let snap_id = parsed["id"].as_u64().unwrap();

    let (s, _) = http_delete(&format!("{base}/blocks/snap-edge/snapshots/999")).unwrap();
    assert_eq!(s, 404);

    let (s, _) = http_post(
        &format!("{base}/blocks/snap-edge/snapshots/999/rollback"),
        b"{}",
    )
    .unwrap();
    assert_eq!(s, 404);

    let (s, _) = http_post(
        &format!("{base}/blocks/snap-edge/snapshots/{snap_id}/rollback"),
        b"{}",
    )
    .unwrap();
    assert_eq!(s, 200);

    let (s, _) = http_delete(&format!("{base}/blocks/snap-edge/snapshots/{snap_id}")).unwrap();
    assert_eq!(s, 200);

    stop_daemon(&mut child);
}

#[test]
fn test_mount_response_content() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    create_volume(&base, "mount-vol", "10M");

    let (_, resp) = http_post(&format!("{base}/blocks/mount-vol/mount"), b"{}").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        parsed["export_name"].as_str().is_some(),
        "missing export_name"
    );
    assert!(parsed["socket"].as_str().is_some(), "missing socket");
    assert!(parsed["size"].as_u64().is_some(), "missing size");

    stop_daemon(&mut child);
}

#[test]
fn test_delete_volume_then_recreate() {
    let (mut child, port, _dir) = start_daemon();
    let base = format!("http://127.0.0.1:{port}/api/v1");

    create_volume(&base, "recreate", "10M");
    http_delete(&format!("{base}/blocks/recreate")).unwrap();

    create_volume(&base, "recreate", "20M");

    let (_, info) = http_get(&format!("{base}/blocks/recreate")).unwrap();
    assert!(info.contains("20971520"), "capacity should be 20M: {info}");

    stop_daemon(&mut child);
}
