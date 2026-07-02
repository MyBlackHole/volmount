//! HTTP API 客户端 — 与 volmountd REST API 通信

use std::io::Read;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::VolmountdConfig;

fn base_url(config: &VolmountdConfig) -> String {
    format!("http://127.0.0.1:{}", config.http_port)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout_read(std::time::Duration::from_secs(30))
        .build()
}

fn handle_response<T: DeserializeOwned>(response: ureq::Response) -> Result<T, String> {
    let status = response.status();
    let mut body = String::new();
    let _ = response.into_reader().read_to_string(&mut body);

    if (200..300).contains(&status) {
        serde_json::from_str::<T>(&body).map_err(|e| format!("parse error: {e}"))
    } else if let Ok(err) = serde_json::from_str::<ApiError>(&body) {
        Err(err.error)
    } else {
        Err(format!("HTTP {status}: {body}"))
    }
}

// ─── 响应类型 ───

#[derive(Deserialize)]
struct ApiError {
    error: String,
}

#[derive(Deserialize)]
pub struct StatusResponse {
    pub version: String,
    pub blocks: Vec<String>,
}

#[derive(Deserialize)]
pub struct BlocksListResponse {
    pub blocks: Vec<BlockSummary>,
}

#[derive(Deserialize)]
pub struct BlockSummary {
    pub name: String,
    pub capacity: u64,
    pub used: u64,
    pub status: String,
}

#[derive(Serialize)]
pub struct CreateBlockRequest {
    pub name: String,
    pub size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateBlockResponse {
    pub name: String,
    pub capacity: u64,
    pub block_size: u32,
    pub backend: String,
}

#[derive(Deserialize)]
pub struct BlockInfo {
    pub name: String,
    pub id: u64,
    pub capacity: u64,
    pub block_size: u32,
    pub backend: String,
    pub created_at: String,
    pub stored: u64,
    pub status: String,
}

#[derive(Deserialize)]
pub struct DeleteResponse {
    pub freed: u64,
}

#[derive(Deserialize)]
pub struct MountResponse {
    pub export_name: String,
    pub socket: String,
    pub size: u64,
}

#[derive(Deserialize)]
pub struct SnapshotsResponse {
    pub snapshots: Vec<SnapshotSummary>,
}

#[derive(Deserialize)]
pub struct SnapshotSummary {
    pub id: u64,
    pub description: String,
    pub root_set_count: u64,
    pub timestamp: String,
}

#[derive(Serialize)]
pub struct CreateSnapshotRequest {
    pub description: String,
}

#[derive(Deserialize)]
pub struct CreateSnapshotResponse {
    pub id: u64,
}

#[derive(Deserialize)]
pub struct NbdExportsResponse {
    pub exports: Vec<NbdExportInfo>,
}

#[derive(Deserialize)]
pub struct NbdExportInfo {
    pub name: String,
    pub size: u64,
    pub status: String,
}

#[derive(Deserialize)]
pub struct SubvolsResponse {
    pub subvols: Vec<SubvolSummary>,
}

#[derive(Deserialize)]
pub struct SubvolSummary {
    pub id: u64,
    pub snapshot_id: u32,
    pub size: u64,
    pub status: String,
}

#[derive(Deserialize)]
pub struct CreateSubvolResponse {
    pub id: u64,
}

// ─── API 调用 ───

pub fn get_status(config: &VolmountdConfig) -> Result<StatusResponse, String> {
    let url = format!("{}/api/v1/daemon/status", base_url(config));
    let response = agent().get(&url).call().map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn list_blocks(config: &VolmountdConfig) -> Result<BlocksListResponse, String> {
    let url = format!("{}/api/v1/blocks", base_url(config));
    let response = agent().get(&url).call().map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn create_block(
    config: &VolmountdConfig,
    name: &str,
    size: &str,
    backend: Option<&str>,
) -> Result<CreateBlockResponse, String> {
    let url = format!("{}/api/v1/blocks", base_url(config));
    let req = CreateBlockRequest {
        name: name.to_string(),
        size: size.to_string(),
        backend: backend.map(|s| s.to_string()),
    };
    let body = serde_json::to_vec(&req).map_err(|e| format!("serialize: {e}"))?;
    let response = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_bytes(&body)
        .map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn get_block_info(config: &VolmountdConfig, name: &str) -> Result<BlockInfo, String> {
    let url = format!("{}/api/v1/blocks/{name}", base_url(config));
    let response = agent().get(&url).call().map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn delete_block(config: &VolmountdConfig, name: &str) -> Result<DeleteResponse, String> {
    let url = format!("{}/api/v1/blocks/{name}", base_url(config));
    let response = agent().delete(&url).call().map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn mount_block(config: &VolmountdConfig, name: &str) -> Result<MountResponse, String> {
    let url = format!("{}/api/v1/blocks/{name}/mount", base_url(config));
    let response = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_bytes(b"{}")
        .map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn umount_block(config: &VolmountdConfig, name: &str) -> Result<(), String> {
    let url = format!("{}/api/v1/blocks/{name}/umount", base_url(config));
    let response = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_bytes(b"{}")
        .map_err(|e| format!("{e}"))?;
    let _status = response.status();
    Ok(())
}

pub fn list_snapshots(
    config: &VolmountdConfig,
    vol_name: &str,
) -> Result<SnapshotsResponse, String> {
    let url = format!("{}/api/v1/blocks/{vol_name}/snapshots", base_url(config));
    let response = agent().get(&url).call().map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn list_nbd_exports(config: &VolmountdConfig) -> Result<NbdExportsResponse, String> {
    let url = format!("{}/api/v1/nbd/exports", base_url(config));
    let response = agent().get(&url).call().map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn create_snapshot(
    config: &VolmountdConfig,
    vol_name: &str,
    description: &str,
) -> Result<CreateSnapshotResponse, String> {
    let url = format!("{}/api/v1/blocks/{vol_name}/snapshots", base_url(config));
    let req = CreateSnapshotRequest {
        description: description.to_string(),
    };
    let body = serde_json::to_vec(&req).map_err(|e| format!("serialize: {e}"))?;
    let response = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_bytes(&body)
        .map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn delete_snapshot(
    config: &VolmountdConfig,
    vol_name: &str,
    snap_id: u64,
) -> Result<(), String> {
    let url = format!(
        "{}/api/v1/blocks/{vol_name}/snapshots/{snap_id}",
        base_url(config)
    );
    let response = agent().delete(&url).call().map_err(|e| format!("{e}"))?;
    let _status = response.status();
    Ok(())
}

pub fn clone_block_from_snapshot(
    config: &VolmountdConfig,
    vol_name: &str,
    snap_id: u64,
    new_name: &str,
) -> Result<CreateBlockResponse, String> {
    let url = format!(
        "{}/api/v1/blocks/{vol_name}/snapshots/{snap_id}/clone",
        base_url(config)
    );
    let req = serde_json::json!({ "name": new_name });
    let response = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_bytes(&serde_json::to_vec(&req).map_err(|e| format!("{e}"))?)
        .map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn list_subvols(config: &VolmountdConfig, vol_name: &str) -> Result<SubvolsResponse, String> {
    let url = format!("{}/api/v1/blocks/{vol_name}/subvols", base_url(config));
    let response = agent().get(&url).call().map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn create_subvol(
    config: &VolmountdConfig,
    vol_name: &str,
    name: &str,
    size: &str,
) -> Result<CreateSubvolResponse, String> {
    let url = format!("{}/api/v1/blocks/{vol_name}/subvols", base_url(config));
    let body = serde_json::json!({ "name": name, "size": size });
    let response = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_bytes(&serde_json::to_vec(&body).map_err(|e| format!("{e}"))?)
        .map_err(|e| format!("{e}"))?;
    handle_response(response)
}

pub fn delete_subvol(
    config: &VolmountdConfig,
    vol_name: &str,
    subvol_id: u64,
) -> Result<(), String> {
    let url = format!(
        "{}/api/v1/blocks/{vol_name}/subvols/{subvol_id}",
        base_url(config)
    );
    let response = agent().delete(&url).call().map_err(|e| format!("{e}"))?;
    let _status = response.status();
    Ok(())
}

pub fn rollback_snapshot(
    config: &VolmountdConfig,
    vol_name: &str,
    snap_id: u64,
) -> Result<(), String> {
    let url = format!(
        "{}/api/v1/blocks/{vol_name}/snapshots/{snap_id}/rollback",
        base_url(config)
    );
    let response = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_bytes(b"{}")
        .map_err(|e| format!("{e}"))?;
    let _status = response.status();
    Ok(())
}
