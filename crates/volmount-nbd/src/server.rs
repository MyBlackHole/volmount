use std::collections::HashMap;
use std::sync::Arc;

use bytes::BytesMut;
use tokio::net::UnixListener;
use tokio::sync::RwLock;

use volmount_core::types::BlockAddr;

use crate::error::{NbdError, NbdResult};
use crate::export::NbdExport;
use crate::handshake;
use crate::protocol::*;

pub struct NbdServer {
    socket_path: String,
    exports: Arc<RwLock<HashMap<String, NbdExport>>>,
}

impl NbdServer {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            exports: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register_export(&self, export: NbdExport) {
        let mut exports = self.exports.write().await;
        exports.insert(export.name.clone(), export);
    }

    pub async fn unregister_export(&self, name: &str) {
        let mut exports = self.exports.write().await;
        exports.remove(name);
    }

    pub async fn is_exported(&self, name: &str) -> bool {
        self.exports.read().await.contains_key(name)
    }

    pub async fn list_exports(&self) -> Vec<(String, u64)> {
        self.exports
            .read()
            .await
            .values()
            .map(|e| (e.name.clone(), e.size))
            .collect()
    }

    pub async fn run(&self) -> NbdResult<()> {
        let socket_path = &self.socket_path;
        let _ = tokio::fs::remove_file(socket_path).await;

        let listener = UnixListener::bind(socket_path).map_err(|e| {
            NbdError::Io(std::io::Error::new(
                e.kind(),
                format!("bind NBD socket {socket_path}: {e}"),
            ))
        })?;

        tracing::info!("NBD server listening on {}", socket_path);

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let exports = self.exports.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, exports).await {
                            tracing::warn!("NBD connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("NBD accept error: {e}");
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }
}

const BLOCK_SIZE: u64 = 4096;

async fn handle_connection(
    mut stream: tokio::net::UnixStream,
    exports: Arc<RwLock<HashMap<String, NbdExport>>>,
) -> NbdResult<()> {
    let exports_guard = exports.read().await;
    let export = handshake::negotiate(&mut stream, &exports_guard).await?;
    drop(exports_guard);

    tracing::info!(
        "NBD client connected to export '{}', size={}, flags={:#x}",
        export.name,
        export.size,
        export.flags,
    );

    loop {
        let Some(req) = read_request(&mut stream).await? else {
            break;
        };

        match req.r#type {
            NBD_CMD_READ => handle_read(&mut stream, &export, &req).await?,
            NBD_CMD_WRITE => handle_write(&mut stream, &export, &req).await?,
            NBD_CMD_TRIM => handle_trim(&mut stream, &export, &req).await?,
            NBD_CMD_FLUSH => handle_flush(&mut stream, &export, &req).await?,
            NBD_CMD_DISC => {
                tracing::info!("NBD client disconnected");
                break;
            }
            t => {
                tracing::warn!("unknown NBD command type: {t}");
                send_response(&mut stream, req.handle, 0xffff).await?;
            }
        }
    }

    Ok(())
}

async fn handle_read(
    stream: &mut tokio::net::UnixStream,
    export: &NbdExport,
    req: &NbdRequest,
) -> NbdResult<()> {
    let offset = req.offset;
    let end = (offset + req.len as u64).min(export.size);
    let actual_len = end.saturating_sub(offset) as usize;
    let mut buf = BytesMut::zeroed(actual_len);

    let start_block = offset / BLOCK_SIZE;
    let end_block = end.div_ceil(BLOCK_SIZE);

    // 任何块读失败都会导致 EIO，此时需要先读完数据再发错误
    let mut read_error = false;
    for block_idx in start_block..end_block {
        let block_off = block_idx * BLOCK_SIZE;

        let src_start = if offset > block_off {
            (offset - block_off) as usize
        } else {
            0
        };
        let src_end = BLOCK_SIZE.min(end - block_off) as usize;
        if src_start >= src_end {
            continue;
        }

        let mut block_buf = vec![0u8; BLOCK_SIZE as usize];
        if export
            .backend
            .read_block(BlockAddr::new(block_idx), &mut block_buf)
            .await
            .is_err()
        {
            read_error = true;
            continue;
        }

        let copy_len = src_end - src_start;
        let dst_start = (block_off + src_start as u64 - offset) as usize;
        if dst_start + copy_len <= buf.len() {
            buf[dst_start..dst_start + copy_len].copy_from_slice(&block_buf[src_start..src_end]);
        }
    }

    if read_error {
        // EIO: 后端读失败，NBD 协议要求 error != 0 时不发送数据
        send_response(stream, req.handle, NBD_EIO).await?;
    } else {
        send_response_with_data(stream, req.handle, 0, &buf).await?;
    }
    Ok(())
}

async fn handle_write(
    stream: &mut tokio::net::UnixStream,
    export: &NbdExport,
    req: &NbdRequest,
) -> NbdResult<()> {
    let offset = req.offset;
    let write_data = read_write_data(stream, req.len).await?;
    let end = (offset + req.len as u64).min(export.size);
    if end <= offset {
        send_response(stream, req.handle, 0).await?;
        return Ok(());
    }

    let start_block = offset / BLOCK_SIZE;
    let end_block = end.div_ceil(BLOCK_SIZE);

    for block_idx in start_block..end_block {
        let block_off = block_idx * BLOCK_SIZE;
        let write_start = if offset > block_off {
            (offset - block_off) as usize
        } else {
            0
        };
        let write_end = BLOCK_SIZE.min(end - block_off) as usize;
        if write_start >= write_end {
            continue;
        }

        let full_block = write_start == 0 && write_end == BLOCK_SIZE as usize;
        let mut block_buf = vec![0u8; BLOCK_SIZE as usize];

        if !full_block {
            let _ = export
                .backend
                .read_block(BlockAddr::new(block_idx), &mut block_buf)
                .await;
        }

        let data_start = (block_off + write_start as u64 - offset) as usize;
        let copy_len = write_end - write_start;
        if data_start + copy_len <= write_data.len() {
            block_buf[write_start..write_end]
                .copy_from_slice(&write_data[data_start..data_start + copy_len]);
        }

        export
            .backend
            .write_block(BlockAddr::new(block_idx), &block_buf)
            .await
            .map_err(NbdError::Storage)?;
    }

    send_response(stream, req.handle, 0).await?;
    Ok(())
}

async fn handle_trim(
    stream: &mut tokio::net::UnixStream,
    export: &NbdExport,
    req: &NbdRequest,
) -> NbdResult<()> {
    let start_block = req.offset / BLOCK_SIZE;
    let end_block = (req.offset + req.len as u64).div_ceil(BLOCK_SIZE);
    for block_idx in start_block..end_block {
        let _ = export.backend.trim_block(BlockAddr::new(block_idx)).await;
    }
    send_response(stream, req.handle, 0).await?;
    Ok(())
}

async fn handle_flush(
    stream: &mut tokio::net::UnixStream,
    export: &NbdExport,
    req: &NbdRequest,
) -> NbdResult<()> {
    export.backend.flush().await.map_err(NbdError::Storage)?;
    send_response(stream, req.handle, 0).await?;
    Ok(())
}
