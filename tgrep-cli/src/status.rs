/// `tgrep status` — show index and server status.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;

use anyhow::Result;
use tgrep_core::builder;
use tgrep_core::meta::IndexMeta;

use crate::serve::ServerInfo;

pub fn run(root: &Path, index_path: Option<&Path>) -> Result<()> {
    let root = std::fs::canonicalize(root)?;
    let index_dir = index_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| builder::default_index_dir(&root));

    // Try connecting to a running server
    if let Ok(info) = ServerInfo::load(&index_dir)
        && let Ok(status) = query_server_status(&info)
    {
        println!("Server status for {}", root.display());
        println!("  PID:        {}", info.pid);
        println!("  Port:       {}", info.port);
        println!("  Files:      {}", status.num_files);
        println!("  Trigrams:   {}", status.num_trigrams);
        println!(
            "  Cache:      {}/{}",
            status.cache_size, status.cache_capacity
        );
        println!(
            "  Watcher:    {}",
            if status.watcher_active {
                "active"
            } else {
                "inactive"
            }
        );
        if status.indexing {
            println!(
                "  Indexing:   {}/{} files",
                status.index_progress, status.index_total
            );
        } else {
            println!("  Indexing:   complete");
        }
        return Ok(());
    }

    // Fall back to on-disk metadata
    match IndexMeta::load(&index_dir) {
        Ok(meta) => {
            println!("Index status for {}", root.display());
            println!("  Files:      {}", meta.num_files);
            println!("  Trigrams:   {}", meta.num_trigrams);
            println!("  Created:    {}", format_timestamp(meta.created_at));
            println!("  Updated:    {}", format_timestamp(meta.updated_at));
            println!("  Server:     not running");
        }
        Err(_) => {
            println!("No index found at {}", index_dir.display());
            println!("Run `tgrep index {}` to build one.", root.display());
        }
    }

    Ok(())
}

#[derive(serde::Deserialize)]
struct StatusResult {
    num_files: u64,
    num_trigrams: u64,
    cache_size: u64,
    cache_capacity: u64,
    watcher_active: bool,
    #[serde(default)]
    indexing: bool,
    #[serde(default)]
    index_progress: u64,
    #[serde(default)]
    index_total: u64,
}

fn query_server_status(info: &ServerInfo) -> Result<StatusResult> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", info.port))?;
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "status",
        "id": 1,
    });
    writeln!(stream, "{}", request)?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let response: serde_json::Value = serde_json::from_str(&line)?;
    let result = response
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("no result in response"))?;
    let status: StatusResult = serde_json::from_value(result.clone())?;
    Ok(status)
}

fn format_timestamp(ts: u64) -> String {
    // Simple human-readable timestamp
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = secs.saturating_sub(ts);
    if age < 60 {
        format!("{age}s ago")
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else if age < 86400 {
        format!("{}h ago", age / 3600)
    } else {
        format!("{}d ago", age / 86400)
    }
}
