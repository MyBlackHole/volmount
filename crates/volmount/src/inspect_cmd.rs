//! volmount inspect — 离线调试工具
//!
//! 不依赖 daemon 直接读取存储文件，用于开发阶段验证数据正确性。

use crate::config::VolmountdConfig;
use volmount_core::storage::superblock::{BchSb, SUPERBLOCK_SIZE};

/// 自动检测文件类型并 dump
pub fn auto_inspect(config: &VolmountdConfig, path: &str) {
    if let Some(backend_path) = find_backend_file(config, path) {
        if let Ok(sb) = read_superblock_file(&backend_path) {
            println!("=== Superblock ===");
            println!("  magic:       {:?}", &sb.magic);
            println!("  version:     {}", sb.version);
            println!("  clean:       {}", sb.clean_shutdown);
            let vol = sb.vol_meta;
            println!("  vol_name:    {}", vol.vol_name);
            println!("  block_size:  {}", vol.block_size);
            println!("  journal_seq: {}", sb.journal_seq);
            println!("  root_addrs:  {:?}", sb.root_addrs);
            println!("  root_levels: {:?}", sb.root_levels);
            println!("  root_ptrs:   {:?}", sb.root_ptrs);
            return;
        }
    }

    println!("inspect: unknown file format (auto-detection attempted)");
    println!("Try: volmount inspect btree <path>");
    println!("     volmount inspect wal   <path>");
    println!("     volmount inspect meta  <path>");
    println!("     volmount inspect block <path> --paddr <n>");
}

pub fn inspect_meta(config: &VolmountdConfig, path: &str) {
    match find_backend_file(config, path) {
        Some(backend_path) => match read_superblock_file(&backend_path) {
            Ok(sb) => {
                let vm = sb.vol_meta;
                println!("=== Block Device Metadata ===");
                println!("  magic:      {:?}", &vm.magic[..]);
                println!("  name:       {}", vm.vol_name);
                println!("  id:         {}", vm.vol_id);
                println!("  pool:       {}", vm.pool_name);
                println!("  block_size: {}", vm.block_size);
                println!("  capacity:   {} bytes", vm.capacity);
                println!("  block device backend: {}", vm.backend_type.as_str());
                println!("  created_at: {}", vm.created_at);
            }
            Err(e) => eprintln!("inspect meta: {e}"),
        },
        None => eprintln!("inspect meta: cannot find backend file at '{path}'"),
    }
}

pub fn inspect_btree(config: &VolmountdConfig, path: &str) {
    let backend_path = match find_backend_file(config, path) {
        Some(p) => p,
        None => {
            eprintln!("inspect btree: cannot find backend file at '{}'", path);
            return;
        }
    };

    let data = match std::fs::read(&backend_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("inspect btree: read error: {e}");
            return;
        }
    };

    if data.len() < SUPERBLOCK_SIZE {
        eprintln!("inspect btree: file too small ({} bytes)", data.len());
        return;
    }

    let sb = match BchSb::deserialize(&data[..SUPERBLOCK_SIZE]) {
        Ok(sb) => sb,
        Err(e) => {
            eprintln!("inspect btree: deserialize superblock: {e}");
            return;
        }
    };

    println!("=== Btree Roots ===");
    println!("  root_addrs:      {:?}", sb.root_addrs);
    println!("  root_levels:     {:?}", sb.root_levels);
    println!("  root_ptrs:       {:?}", sb.root_ptrs);

    if sb.root_ptrs.is_empty() {
        println!("  (no root pointers)");
        return;
    }

    let valid = sb.root_ptrs.iter().filter(|ptr| ptr.is_valid()).count();
    println!("  valid roots:     {}", valid);
}

pub fn inspect_wal(path: &str) {
    let dir = std::path::Path::new(path);
    let dir = if dir.is_dir() {
        dir
    } else {
        dir.parent().unwrap_or(dir)
    };

    let mut total_entries = 0usize;
    let mut min_seq = u64::MAX;
    let mut max_seq = 0u64;
    let mut file_count: u32 = 0;

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("journal.") {
                continue;
            }
            file_count += 1;
            let trimmed = name_str.trim_start_matches("journal.");
            if let Some(dash_pos) = trimmed.find('-') {
                if let Ok(seq) = trimmed[..dash_pos].parse::<u64>() {
                    min_seq = min_seq.min(seq);
                    max_seq = max_seq.max(seq);
                }
            }
            if let Ok(data) = std::fs::read(entry.path()) {
                let entry_count = data.split(|b| b == &0xBE).count().saturating_sub(1);
                total_entries += entry_count;
            }
        }
    }

    if file_count == 0 {
        println!("No journal WAL files found in '{}'", dir.display());
        println!("(looks for files matching journal.*)");
        return;
    }

    println!("=== Journal WAL ===");
    println!("  WAL files:   {}", file_count);
    println!("  seq range:   {} .. {}", min_seq, max_seq);
    println!("  entries:     ~{}", total_entries);
    println!();
    println!("  Files:");
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("journal.") {
                let size = std::fs::metadata(entry.path())
                    .map(|m| m.len())
                    .unwrap_or(0);
                println!("    {}  ({} bytes)", name, size);
            }
        }
    }
}

pub fn inspect_snapshot(config: &VolmountdConfig, path: &str) {
    let backend_path = match find_backend_file(config, path) {
        Some(p) => p,
        None => {
            eprintln!("inspect snapshot: cannot find backend file at '{}'", path);
            return;
        }
    };

    let data = match std::fs::read(&backend_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("inspect snapshot: read error: {e}");
            return;
        }
    };

    if data.len() < SUPERBLOCK_SIZE {
        eprintln!("inspect snapshot: file too small");
        return;
    }

    let sb = match BchSb::deserialize(&data[..SUPERBLOCK_SIZE]) {
        Ok(sb) => sb,
        Err(e) => {
            eprintln!("inspect snapshot: deserialize superblock: {e}");
            return;
        }
    };

    println!("=== Snapshot Info ===");
    println!("  snap tree roots: {:?}", sb.root_addrs);
    println!("  journal_seq:     {}", sb.journal_seq);
    println!();
    println!("  (detailed enumeration: volmount snap list <vol>)");
}

pub fn inspect_block(config: &VolmountdConfig, path: &str, paddr: u64) {
    let backend_path = match find_backend_file(config, path) {
        Some(p) => p,
        None => {
            eprintln!("inspect block: cannot find backend file at '{}'", path);
            return;
        }
    };

    let data = match std::fs::read(&backend_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("inspect block: read error: {e}");
            return;
        }
    };

    if data.len() < SUPERBLOCK_SIZE {
        eprintln!("inspect block: file too small");
        return;
    }

    let sb = match BchSb::deserialize(&data[..SUPERBLOCK_SIZE]) {
        Ok(sb) => sb,
        Err(e) => {
            eprintln!("inspect block: deserialize superblock: {e}");
            return;
        }
    };

    let block_size = sb.vol_meta.block_size as usize;
    let offset = paddr as usize * block_size;

    if offset + block_size > data.len() {
        eprintln!("inspect block: paddr={} out of range", paddr);
        return;
    }

    let block = &data[offset..offset + block_size];

    println!("=== Block at paddr={} ===", paddr);
    println!("  block_size:  {} bytes", block_size);
    println!("  file_offset: {}", offset);
    println!();

    let dump_len = block.len().min(256);
    for chunk in block[..dump_len].chunks(16) {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{:02x}", b)).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("  {:08x}  {:47}  |{}|", offset, hex.join(" "), ascii);
    }
    if dump_len < block.len() {
        println!("  ... ({} more bytes)", block.len() - dump_len);
    }
}

fn find_backend_file(config: &VolmountdConfig, path: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(path);

    if p.is_dir() {
        let backend = p.join(&config.backend_file_name);
        if backend.exists() {
            return Some(backend);
        }
        None
    } else if p
        .file_name()
        .map_or(false, |n| n == config.backend_file_name.as_str())
    {
        Some(p.to_path_buf())
    } else {
        if let Some(parent) = p.parent() {
            let backend = parent.join(&config.backend_file_name);
            if backend.exists() {
                return Some(backend);
            }
        }
        if p.exists() {
            Some(p.to_path_buf())
        } else {
            None
        }
    }
}

fn read_superblock_file(path: &std::path::Path) -> Result<BchSb, String> {
    let data = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
    if data.len() < SUPERBLOCK_SIZE {
        return Err("file too small".to_string());
    }
    BchSb::deserialize(&data[..SUPERBLOCK_SIZE]).map_err(|e| format!("deserialize: {e}"))
}
