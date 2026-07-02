//! volmountd: volmount 守护进程

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::signal;
use tokio::sync::{oneshot, Mutex, RwLock};
use tracing_subscriber::EnvFilter;

mod config;
mod server;
mod volume;

use config::{ConfigError, VolmountdConfig};
use server::AppState;
use volume::{init_dirs, init_volume, stop_volume};

#[derive(Parser, Debug)]
#[command(name = "volmountd", about = "volmount storage daemon")]
struct Cli {
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,
    #[arg(long)]
    home_dir: Option<PathBuf>,
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cli.log_level))
        .init();

    let config = match load_config(&cli).await {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!("failed to load config: {e}");
            std::process::exit(1);
        }
    };

    // 优雅关闭通道
    let (nbd_shutdown_tx, nbd_shutdown_rx) = oneshot::channel::<()>();
    let (http_shutdown_tx, http_shutdown_rx) = oneshot::channel::<()>();
    let nbd_shutdown_tx = Arc::new(Mutex::new(Some(nbd_shutdown_tx)));
    let http_shutdown_tx = Arc::new(Mutex::new(Some(http_shutdown_tx)));

    // 初始化目录结构（等价 bcachefs 创建挂载点）
    if let Err(e) = init_dirs(&config).await {
        tracing::error!("failed to init dirs: {e}");
        std::process::exit(1);
    }
    tracing::info!("directories initialized");

    // Block device 列表（等价 bcachefs 持有多个 bch_fs 实例）
    let blocks: Arc<RwLock<HashMap<String, Arc<volume::Volume>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // 自动加载导出块设备（等价 bcachefs auto-mount）
    for name in &config.auto_exports {
        match init_volume(&config, name).await {
            Ok(vol) => {
                tracing::info!("auto-loaded block device '{}'", name);
                blocks.write().await.insert(name.clone(), vol);
            }
            Err(e) => {
                tracing::warn!("failed to auto-load block device '{}': {e}", name);
            }
        }
    }

    // NBD socket 目录
    let nbd_socket = config.resolved_nbd_socket();
    if let Some(parent) = nbd_socket.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::error!("failed to create NBD socket directory {:?}: {e}", parent);
            std::process::exit(1);
        }
    }

    // NBD server
    let nbd_server = Arc::new(volmount_nbd::NbdServer::new(
        nbd_socket.to_string_lossy().to_string(),
    ));

    // 注册 NBD export
    {
        let vols = blocks.read().await;
        for (name, vol) in vols.iter() {
            let nbd_export = volmount_nbd::NbdExport {
                name: vol.meta.vol_name.clone(),
                size: vol.meta.capacity,
                backend: vol.backend.clone(),
                flags: 0,
            };
            nbd_server.register_export(nbd_export).await;
            tracing::info!("exported NBD block device '{}'", name);
        }
    }

    // HTTP API server
    let app_state = AppState {
        config: config.clone(),
        blocks: blocks.clone(),
        nbd_server: nbd_server.clone(),
    };
    let http_port = config.http_port;
    let http_handle = tokio::spawn(async move {
        if let Err(e) = server::run_server(app_state, http_port, http_shutdown_rx).await {
            tracing::error!("HTTP server error: {e}");
        }
    });

    // NBD server（后台任务）
    let nbd_server_clone = nbd_server.clone();
    let server_handle = tokio::spawn(async move {
        tokio::select! {
            result = nbd_server_clone.run() => {
                if let Err(e) = result {
                    tracing::error!("NBD server error: {e}");
                }
            }
            _ = async { nbd_shutdown_rx.await.ok(); } => {
                tracing::info!("NBD server shutting down");
            }
        }
    });

    tracing::info!(
        "volmountd ready — http=127.0.0.1:{}, nbd={}, home={}",
        http_port,
        config.resolved_nbd_socket().display(),
        config.resolved_home_dir().display(),
    );

    wait_for_shutdown().await;
    tracing::info!("shutdown signal received, cleaning up...");

    // 触发所有 server 关闭
    {
        let mut tx = nbd_shutdown_tx.lock().await;
        if let Some(tx) = tx.take() {
            let _ = tx.send(());
        }
    }
    {
        let mut tx = http_shutdown_tx.lock().await;
        if let Some(tx) = tx.take() {
            let _ = tx.send(());
        }
    }

    // stop/drain + clean_shutdown（等价 bcachefs bch2_fs_stop）
    {
        let vols = blocks.read().await;
        for (name, vol) in vols.iter() {
            if let Err(e) = stop_volume(vol).await {
                tracing::warn!("error stopping block device '{}': {e}", name);
            } else {
                tracing::info!("stopped block device '{}'", name);
            }
        }
    }

    let _ = tokio::fs::remove_file(&nbd_socket).await;
    let _ = http_handle.await;
    let _ = server_handle.await;

    tracing::info!("volmountd shut down cleanly");
}

async fn load_config(cli: &Cli) -> Result<VolmountdConfig, ConfigError> {
    let config_path = if let Some(ref path) = cli.config {
        path.clone()
    } else {
        let home = dirs_next::home_dir().unwrap_or_default();
        home.join(".volmount").join("config.json")
    };

    let mut config = if config_path.exists() {
        tracing::info!("loading config from {}", config_path.display());
        VolmountdConfig::load(&config_path)?
    } else {
        tracing::info!("no config found, using defaults");
        VolmountdConfig::default()
    };

    if let Some(ref home_dir) = cli.home_dir {
        config.home_dir = home_dir.clone();
    }

    Ok(config)
}

async fn wait_for_shutdown() {
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
        .expect("failed to register SIGINT handler");
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = sigint.recv() => {
            tracing::info!("received SIGINT");
        }
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
        }
    }
}
