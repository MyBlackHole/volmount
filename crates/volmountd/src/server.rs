use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, RwLock};
use tracing::info;

use volmount_core::types::{BackendType, StorageError};

use crate::config::VolmountdConfig;
use crate::volume::{self, Volume};

// ─── 路由状态 ───

#[derive(Clone)]
pub struct AppState {
    pub config: VolmountdConfig,
    pub blocks: Arc<RwLock<HashMap<String, Arc<Volume>>>>,
    pub nbd_server: Arc<volmount_nbd::NbdServer>,
}

// ─── 请求/响应类型 ───

#[derive(Serialize)]
struct ApiError {
    error: String,
}

#[derive(Serialize)]
struct StatusResponse {
    version: &'static str,
    nbd_socket: String,
    blocks: Vec<String>,
}

#[derive(Deserialize)]
struct CreateVolumeRequest {
    name: String,
    #[serde(default = "default_size")]
    size: String,
    #[serde(default = "default_backend")]
    backend: String,
}

fn default_size() -> String {
    "1G".to_string()
}
fn default_backend() -> String {
    "sparse".to_string()
}

#[derive(Serialize)]
struct CreateVolumeResponse {
    name: String,
    capacity: u64,
    block_size: u32,
    backend: String,
}

#[derive(Serialize)]
struct VolumeInfo {
    name: String,
    id: u64,
    capacity: u64,
    block_size: u32,
    backend: String,
    created_at: String,
    stored: u64,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<VolumeRecoveryInfo>,
}

#[derive(Serialize)]
struct VolumeRecoveryInfo {
    pass_done: u32,
    passes_complete: u32,
    passes_failing: u32,
}

#[derive(Serialize)]
struct VolumesListResponse {
    blocks: Vec<VolumeSummary>,
}

#[derive(Serialize)]
struct VolumeSummary {
    name: String,
    capacity: u64,
    used: u64,
    status: String,
}

#[derive(Serialize)]
struct DeleteResponse {
    freed: u64,
}

#[derive(Serialize)]
struct MountResponse {
    export_name: String,
    socket: String,
    size: u64,
}

#[derive(Serialize)]
struct UmountResponse {
    status: String,
}

#[derive(Serialize)]
struct SnapshotsResponse {
    snapshots: Vec<SnapshotSummary>,
}

#[derive(Serialize)]
struct SnapshotSummary {
    id: u64,
    description: String,
    root_set_count: u64,
    timestamp: String,
}

#[derive(Deserialize)]
struct CreateSnapshotRequest {
    description: String,
}

#[derive(Deserialize)]
struct CloneBlockRequest {
    name: String,
}

#[derive(Serialize)]
struct CreateSnapshotResponse {
    id: u64,
}

#[derive(Serialize)]
struct RollbackResponse {
    status: String,
    entries: usize,
}

// ─── D6: NBD 导出请求/响应类型 ───

#[derive(Serialize)]
struct NbdExportsResponse {
    exports: Vec<NbdExportInfo>,
}

#[derive(Serialize)]
struct NbdExportInfo {
    name: String,
    size: u64,
    status: String,
}

// ─── D2: 子卷请求/响应类型 ───

#[derive(Serialize)]
struct SubvolsResponse {
    subvols: Vec<SubvolSummary>,
}

#[derive(Serialize)]
struct SubvolSummary {
    id: u64,
    snapshot_id: u32,
    size: u64,
    status: String,
}

#[derive(Deserialize)]
struct CreateSubvolRequest {
    name: String,
    #[serde(default = "default_subvol_size")]
    size: String,
}

fn default_subvol_size() -> String {
    "1G".to_string()
}

#[derive(Serialize)]
struct CreateSubvolResponse {
    id: u64,
}

// ─── Server ───

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/daemon/status", get(handle_status))
        .route("/api/v1/blocks", get(handle_list_volumes))
        .route("/api/v1/blocks", post(handle_create_volume))
        .route("/api/v1/blocks/:name", get(handle_volume_info))
        .route("/api/v1/blocks/:name", delete(handle_delete_volume))
        .route("/api/v1/blocks/:name/mount", post(handle_mount))
        .route("/api/v1/blocks/:name/umount", post(handle_umount))
        .route("/api/v1/blocks/:name/snapshots", get(handle_list_snapshots))
        .route(
            "/api/v1/blocks/:name/snapshots",
            post(handle_create_snapshot),
        )
        .route(
            "/api/v1/blocks/:name/snapshots/:id",
            delete(handle_delete_snapshot),
        )
        .route(
            "/api/v1/blocks/:name/snapshots/:id/rollback",
            post(handle_rollback),
        )
        .route(
            "/api/v1/blocks/:name/snapshots/:id/clone",
            post(handle_clone_from_snapshot),
        )
        .route(
            "/api/v1/blocks/:name/recovery",
            get(handle_recovery_progress),
        )
        .route("/api/v1/blocks/:name/subvols", get(handle_list_subvols))
        .route("/api/v1/blocks/:name/subvols", post(handle_create_subvol))
        .route(
            "/api/v1/blocks/:name/subvols/:id",
            delete(handle_delete_subvol),
        )
        .route("/api/v1/nbd/exports", get(handle_nbd_list_exports))
        .with_state(state)
}

pub async fn run_server(
    state: AppState,
    port: u16,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<(), std::io::Error> {
    let addr = (Ipv4Addr::LOCALHOST, port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("HTTP API server listening on http://127.0.0.1:{port}");

    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async {
            shutdown_rx.await.ok();
        })
        .await
}

// ─── Handlers ───

async fn handle_status(State(state): State<AppState>) -> impl IntoResponse {
    let open_names: Vec<String> = state.blocks.read().await.keys().cloned().collect();
    Json(serde_json::json!(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        nbd_socket: "unix".to_string(),
        blocks: open_names,
    }))
}

async fn handle_list_volumes(State(state): State<AppState>) -> impl IntoResponse {
    let open_vols = state.blocks.read().await;
    let all_blocks = volume::list_all_volumes(&state.config)
        .await
        .unwrap_or_default();

    let mut blocks_list: Vec<VolumeSummary> = Vec::new();
    for name in &all_blocks {
        let status = if open_vols.contains_key(name) {
            "open"
        } else {
            "closed"
        };
        let capacity = if let Some(vol) = open_vols.get(name) {
            vol.meta.capacity
        } else {
            volume::read_volume_superblock(&state.config, name)
                .await
                .map(|sb| sb.vol_meta.capacity)
                .unwrap_or(0)
        };
        let used = if let Some(vol) = open_vols.get(name) {
            vol.backend.used_space().await.unwrap_or(0)
        } else {
            volume::read_volume_used_space(&state.config, name)
                .await
                .unwrap_or(0)
        };

        blocks_list.push(VolumeSummary {
            name: name.clone(),
            capacity,
            used,
            status: status.to_string(),
        });
    }

    Json(serde_json::json!(VolumesListResponse {
        blocks: blocks_list
    }))
}

async fn handle_create_volume(
    State(state): State<AppState>,
    Json(req): Json<CreateVolumeRequest>,
) -> impl IntoResponse {
    let backend_type = match req.backend.to_lowercase().as_str() {
        "sparse" => BackendType::Sparse,
        "s3" => BackendType::S3,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!(ApiError {
                    error: format!("unknown block device backend '{}'", req.backend),
                })),
            );
        }
    };

    let capacity = parse_size(&req.size);
    if capacity == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!(ApiError {
                error: format!("invalid size '{}'", req.size),
            })),
        );
    }

    let block_size = 4096u32;

    match volume::create_volume(&state.config, &req.name, backend_type, capacity, block_size).await
    {
        Ok(vol) => {
            state
                .blocks
                .write()
                .await
                .insert(req.name.clone(), vol.clone());
            (
                StatusCode::CREATED,
                Json(serde_json::json!(CreateVolumeResponse {
                    name: vol.meta.vol_name.clone(),
                    capacity: vol.meta.capacity,
                    block_size: vol.meta.block_size,
                    backend: vol.meta.backend_type.as_str().to_string(),
                })),
            )
        }
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!(ApiError {
                error: e.to_string(),
            })),
        ),
    }
}

async fn handle_volume_info(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let sb = match volume::read_volume_superblock(&state.config, &name).await {
        Ok(sb) => sb,
        Err(volume::DaemonError::VolumeNotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("block '{name}' not found"),
                })),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!(ApiError {
                    error: e.to_string(),
                })),
            );
        }
    };
    let meta = sb.vol_meta;

    let open_vols = state.blocks.read().await;
    let status = if open_vols.contains_key(&name) {
        "open"
    } else {
        "closed"
    };

    let stored = if let Some(vol) = open_vols.get(&name) {
        vol.backend.used_space().await.unwrap_or(0)
    } else {
        volume::read_volume_used_space(&state.config, &name)
            .await
            .unwrap_or(0)
    };

    let recovery = if status == "open" {
        let core = open_vols.get(&name).unwrap().inner.read().await;
        let (pass_done, passes_complete, passes_failing) = core.recovery_progress();
        Some(VolumeRecoveryInfo {
            pass_done: pass_done as u32,
            passes_complete: passes_complete as u32,
            passes_failing: passes_failing as u32,
        })
    } else {
        None
    };

    (
        StatusCode::OK,
        Json(serde_json::json!(VolumeInfo {
            name: meta.vol_name,
            id: meta.vol_id,
            capacity: meta.capacity,
            block_size: meta.block_size,
            backend: meta.backend_type.as_str().to_string(),
            created_at: meta.created_at,
            stored,
            status: status.to_string(),
            recovery,
        })),
    )
}

async fn handle_delete_volume(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match volume::delete_volume(&state.config, &name).await {
        Ok(()) => {
            state.blocks.write().await.remove(&name);
            (
                StatusCode::OK,
                Json(serde_json::json!(DeleteResponse { freed: 0 })),
            )
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!(ApiError {
                error: e.to_string(),
            })),
        ),
    }
}

async fn handle_mount(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let vol = match state.blocks.read().await.get(&name) {
        Some(v) => v.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("block '{name}' is not open"),
                })),
            );
        }
    };

    if !state.nbd_server.is_exported(&name).await {
        let nbd_export = volmount_nbd::NbdExport {
            name: vol.meta.vol_name.clone(),
            size: vol.meta.capacity,
            backend: vol.backend.clone(),
            flags: 0,
        };
        state.nbd_server.register_export(nbd_export).await;
    }

    (
        StatusCode::OK,
        Json(serde_json::json!(MountResponse {
            export_name: name.clone(),
            socket: state
                .config
                .resolved_nbd_socket()
                .to_string_lossy()
                .to_string(),
            size: vol.meta.capacity,
        })),
    )
}

async fn handle_umount(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    state.nbd_server.unregister_export(&name).await;

    (
        StatusCode::OK,
        Json(serde_json::json!(UmountResponse {
            status: "unmounted".to_string(),
        })),
    )
}

async fn handle_list_snapshots(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let vol = match get_volume(&state, &name).await {
        Some(v) => v,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("block '{name}' not found"),
                })),
            );
        }
    };

    let core = vol.inner.read().await;
    let snaps: Vec<SnapshotSummary> = core
        .list_snapshots()
        .into_iter()
        .map(|s| SnapshotSummary {
            id: s.id as u64,
            description: String::new(),
            root_set_count: 0,
            timestamp: s.created_at.to_string(),
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!(SnapshotsResponse { snapshots: snaps })),
    )
}

async fn handle_create_snapshot(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<CreateSnapshotRequest>,
) -> impl IntoResponse {
    let vol = match get_volume(&state, &name).await {
        Some(v) => v,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("block '{name}' not found"),
                })),
            );
        }
    };

    let mut core = vol.inner.write().await;
    match core.create_snapshot(&req.description) {
        Ok(id) => (
            StatusCode::CREATED,
            Json(serde_json::json!(CreateSnapshotResponse { id: id as u64 })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!(ApiError {
                error: e.to_string()
            })),
        ),
    }
}

async fn handle_delete_snapshot(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, u64)>,
) -> impl IntoResponse {
    let vol = match get_volume(&state, &name).await {
        Some(v) => v,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("block '{name}' not found"),
                })),
            );
        }
    };

    let mut core = vol.inner.write().await;
    match core.delete_snapshot(id as u32) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!(serde_json::json!({"status": "deleted"}))),
        ),
        Err(StorageError::NotFound(msg)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!(ApiError { error: msg })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!(ApiError {
                error: e.to_string()
            })),
        ),
    }
}

async fn handle_rollback(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, u64)>,
) -> impl IntoResponse {
    let vol = match get_volume(&state, &name).await {
        Some(v) => v,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("block '{name}' not found"),
                })),
            );
        }
    };

    let mut core = vol.inner.write().await;
    match core.rollback(id as u32) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!(RollbackResponse {
                status: "rolled back".to_string(),
                entries: 0,
            })),
        ),
        Err(StorageError::NotFound(msg)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!(ApiError { error: msg })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!(ApiError {
                error: e.to_string()
            })),
        ),
    }
}

async fn handle_clone_from_snapshot(
    State(state): State<AppState>,
    Path((name, _id)): Path<(String, u64)>,
    Json(req): Json<CloneBlockRequest>,
) -> impl IntoResponse {
    match volume::clone_volume(&state.config, &name, &req.name).await {
        Ok(vol) => {
            state
                .blocks
                .write()
                .await
                .insert(req.name.clone(), vol.clone());
            (
                StatusCode::CREATED,
                Json(serde_json::json!(serde_json::json!({
                    "name": vol.meta.vol_name,
                    "capacity": vol.meta.capacity,
                    "block_size": vol.meta.block_size,
                    "backend": vol.meta.backend_type.as_str(),
                }))),
            )
        }
        Err(volume::DaemonError::VolumeExists(_)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!(ApiError {
                error: format!("block '{}' already exists", req.name),
            })),
        ),
        Err(volume::DaemonError::VolumeNotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!(ApiError {
                error: format!("block '{name}' not found"),
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!(ApiError {
                error: e.to_string()
            })),
        ),
    }
}

// ─── D3: Recovery 进度 API ───

#[derive(Serialize)]
struct RecoveryProgress {
    pass_done: u32,
    passes_complete: u32,
    passes_failing: u32,
}

async fn handle_recovery_progress(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let vol = match get_volume(&state, &name).await {
        Some(v) => v,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("block '{name}' not found"),
                })),
            );
        }
    };

    let core = vol.inner.read().await;
    let (pass_done, passes_complete, passes_failing) = core.recovery_progress();
    (
        StatusCode::OK,
        Json(serde_json::json!(RecoveryProgress {
            pass_done: pass_done as u32,
            passes_complete: passes_complete as u32,
            passes_failing: passes_failing as u32,
        })),
    )
}

// ─── D6: NBD handler ───

async fn handle_nbd_list_exports(State(state): State<AppState>) -> impl IntoResponse {
    let exports = state.nbd_server.list_exports().await;
    let list: Vec<NbdExportInfo> = exports
        .into_iter()
        .map(|(name, size)| NbdExportInfo {
            name,
            size,
            status: "exported".to_string(),
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!(NbdExportsResponse { exports: list })),
    )
}

// ─── D2: 子卷 handlers ───

async fn handle_list_subvols(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let vol = match get_volume(&state, &name).await {
        Some(v) => v,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("block '{name}' not found"),
                })),
            );
        }
    };

    let core = vol.inner.read().await;
    let subvols: Vec<SubvolSummary> = core
        .list_subvols()
        .into_iter()
        .map(|(id, sv)| SubvolSummary {
            id: id as u64,
            snapshot_id: sv.snapshot,
            size: sv.size,
            status: if sv.is_unlinked() {
                "unlinked"
            } else {
                "alive"
            }
            .to_string(),
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!(SubvolsResponse { subvols })),
    )
}

async fn handle_create_subvol(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<CreateSubvolRequest>,
) -> impl IntoResponse {
    let vol = match get_volume(&state, &name).await {
        Some(v) => v,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("block '{name}' not found"),
                })),
            );
        }
    };

    let size = parse_size(&req.size);
    let mut core = vol.inner.write().await;
    match core.create_subvol(&req.name, size) {
        Ok(id) => (
            StatusCode::CREATED,
            Json(serde_json::json!(CreateSubvolResponse { id: id as u64 })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!(ApiError {
                error: e.to_string()
            })),
        ),
    }
}

async fn handle_delete_subvol(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, u64)>,
) -> impl IntoResponse {
    let vol = match get_volume(&state, &name).await {
        Some(v) => v,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ApiError {
                    error: format!("volume '{name}' not found"),
                })),
            );
        }
    };

    let mut core = vol.inner.write().await;
    match core.delete_subvol(id as u32) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!(serde_json::json!({"status": "deleted"}))),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!(ApiError {
                error: e.to_string()
            })),
        ),
    }
}

// ─── 工具函数 ───

/// 从 state.blocks 获取已 open 的块设备
async fn get_volume(state: &AppState, name: &str) -> Option<Arc<Volume>> {
    state.blocks.read().await.get(name).cloned()
}

fn parse_size(s: &str) -> u64 {
    let s = s.trim().to_uppercase();
    if let Some(rest) = s.strip_suffix('G') {
        rest.parse::<u64>()
            .ok()
            .map(|v| v * 1024 * 1024 * 1024)
            .unwrap_or(0)
    } else if let Some(rest) = s.strip_suffix('M') {
        rest.parse::<u64>()
            .ok()
            .map(|v| v * 1024 * 1024)
            .unwrap_or(0)
    } else if let Some(rest) = s.strip_suffix('K') {
        rest.parse::<u64>().ok().map(|v| v * 1024).unwrap_or(0)
    } else {
        s.parse::<u64>().unwrap_or(0)
    }
}
