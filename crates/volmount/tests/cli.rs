//! volmount CLI 功能测试
//!
//! 启动 volmountd 子进程，执行 volmount CLI 命令验证输出。
//! CLI 通过 ~/.volmount/config.json 定位 daemon。

use std::process::{Child, Command, Output};
use std::time::Duration;

use tempfile::TempDir;

// ─── Helper ───

fn binary_in_same_dir(name: &str) -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let bin = path.join(name);
    if bin.exists() {
        return bin;
    }
    let alt = path.parent().unwrap_or(&path).join(name);
    assert!(
        alt.exists(),
        "{name} binary not found at {:?} or {:?}",
        bin,
        alt
    );
    alt
}

fn setup_daemon(home_dir: &std::path::Path) -> Child {
    let daemon_bin = binary_in_same_dir("volmountd");
    let child = Command::new(&daemon_bin)
        .arg("-c")
        .arg(home_dir.join(".volmount").join("config.json"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start volmountd");
    child
}

fn wait_for_port(port: u16) {
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

struct TestContext {
    _dir: TempDir,
    daemon: Child,
}

fn setup() -> TestContext {
    let dir = TempDir::new().expect("tempdir");
    let home_dir = dir.path();

    let volmount_cfg_dir = home_dir.join(".volmount");
    std::fs::create_dir_all(&volmount_cfg_dir).unwrap();

    let data_dir = home_dir.join("data");
    let port = 9878u16;
    let config = serde_json::json!({
        "home_dir": data_dir,
        "nbd_socket_path": "run/volmountd.sock",
        "http_port": port,
        "auto_exports": [],
    });
    std::fs::write(volmount_cfg_dir.join("config.json"), config.to_string()).unwrap();

    let daemon = setup_daemon(home_dir);
    wait_for_port(port);

    TestContext { _dir: dir, daemon }
}

fn teardown(ctx: &mut TestContext) {
    let _ = ctx.daemon.kill();
    let _ = ctx.daemon.wait();
}

fn run_cli(home_dir: &std::path::Path, args: &[&str]) -> Output {
    let bin = binary_in_same_dir("volmount");
    Command::new(&bin)
        .args(args)
        .env("HOME", home_dir)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("failed to run volmount")
}

// ─── Tests ───

#[test]
fn test_cli_status() {
    let mut ctx = setup();
    let home = ctx._dir.path();

    let output = run_cli(home, &["status"]);
    assert!(output.status.success(), "status failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.to_lowercase().contains("version"),
        "stdout: {stdout}"
    );

    teardown(&mut ctx);
}

#[test]
fn test_cli_volume_create_list_info() {
    let mut ctx = setup();
    let home = ctx._dir.path();

    let create = run_cli(home, &["vol", "create", "cli-vol", "--size", "10M"]);
    assert!(
        create.status.success(),
        "create failed: stdout={} stderr={}",
        String::from_utf8_lossy(&create.stdout),
        String::from_utf8_lossy(&create.stderr)
    );
    let create_out = String::from_utf8_lossy(&create.stdout);
    assert!(
        create_out.contains("cli-vol"),
        "create stdout: {create_out}"
    );

    let list = run_cli(home, &["vol", "list"]);
    assert!(list.status.success());
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(list_out.contains("cli-vol"), "list: {list_out}");

    let info = run_cli(home, &["vol", "info", "cli-vol"]);
    assert!(info.status.success());
    let info_out = String::from_utf8_lossy(&info.stdout);
    assert!(info_out.contains("cli-vol"), "info: {info_out}");

    teardown(&mut ctx);
}

#[test]
fn test_cli_volume_delete() {
    let mut ctx = setup();
    let home = ctx._dir.path();

    let create = run_cli(home, &["vol", "create", "del-vol", "--size", "5M"]);
    assert!(
        create.status.success(),
        "create: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    let delete = run_cli(home, &["vol", "delete", "del-vol"]);
    assert!(
        delete.status.success(),
        "delete: {}",
        String::from_utf8_lossy(&delete.stderr)
    );

    let list = run_cli(home, &["vol", "list"]);
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(
        !list_out.contains("del-vol"),
        "should not appear: {list_out}"
    );

    teardown(&mut ctx);
}

#[test]
fn test_cli_snapshot_lifecycle() {
    let mut ctx = setup();
    let home = ctx._dir.path();

    let create = run_cli(home, &["vol", "create", "snap-vol", "--size", "10M"]);
    assert!(create.status.success());

    let snap = run_cli(home, &["snap", "create", "snap-vol"]);
    assert!(
        snap.status.success(),
        "snap create: {}",
        String::from_utf8_lossy(&snap.stderr)
    );

    let list = run_cli(home, &["snap", "list", "snap-vol"]);
    assert!(list.status.success());
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(list_out.contains("snap-vol"), "snap list: {list_out}");

    let rollback = run_cli(home, &["snap", "rollback", "snap-vol/1"]);
    assert!(
        rollback.status.success(),
        "rollback: {}",
        String::from_utf8_lossy(&rollback.stderr)
    );

    let snaps_after = run_cli(home, &["snap", "list", "snap-vol"]);
    assert!(snaps_after.status.success());

    teardown(&mut ctx);
}

#[test]
fn test_cli_mount_umount() {
    let mut ctx = setup();
    let home = ctx._dir.path();

    let create = run_cli(home, &["vol", "create", "mount-vol", "--size", "10M"]);
    assert!(create.status.success());

    let mount = run_cli(home, &["vol", "mount", "mount-vol"]);
    assert!(
        mount.status.success(),
        "mount: {}",
        String::from_utf8_lossy(&mount.stderr)
    );
    let mount_out = String::from_utf8_lossy(&mount.stdout);
    assert!(
        mount_out.contains("Export") || mount_out.contains("export"),
        "mount: {mount_out}"
    );

    let umount = run_cli(home, &["vol", "umount", "mount-vol"]);
    assert!(
        umount.status.success(),
        "umount: {}",
        String::from_utf8_lossy(&umount.stderr)
    );

    teardown(&mut ctx);
}
