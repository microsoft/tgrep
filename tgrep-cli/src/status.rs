/// `tgrep status` — show index and server status.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;

use anyhow::Result;
use tgrep_core::builder;
use tgrep_core::meta::IndexMeta;
use tgrep_core::path_index;
use tgrep_core::reader::IndexReader;

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
        write_refresh_status(&mut std::io::stdout().lock(), &status)?;
        if status.indexing {
            println!(
                "  Indexing:   {}/{} files",
                status.index_progress, status.index_total
            );
        } else {
            println!("  Indexing:   complete");
        }
        println!(
            "  Hidden coverage: {}",
            coverage_label(status.hidden_complete.unwrap_or(false))
        );
        return Ok(());
    }

    // Fall back to on-disk metadata
    match IndexMeta::load(&index_dir) {
        Ok(meta) => {
            let hidden_complete = match local_hidden_coverage(&index_dir, &meta) {
                Ok(complete) => complete,
                Err(error) => {
                    eprintln!("warning: index coverage unavailable ({error})");
                    false
                }
            };
            println!("Index status for {}", root.display());
            println!("  Files:      {}", meta.num_files);
            println!("  Trigrams:   {}", meta.num_trigrams);
            println!("  Created:    {}", format_timestamp(meta.created_at));
            println!("  Updated:    {}", format_timestamp(meta.updated_at));
            println!("  Server:     not running");
            println!("  Hidden coverage: {}", coverage_label(hidden_complete));
        }
        Err(_) => {
            println!("No index found at {}", index_dir.display());
            println!("Run `tgrep index {}` to build one.", root.display());
        }
    }

    Ok(())
}

fn local_hidden_coverage(index_dir: &Path, meta: &IndexMeta) -> Result<bool> {
    let Some(index) = path_index::read_filename_index(index_dir)? else {
        return Ok(false);
    };
    let Some(visibility) = index.visibility else {
        return Ok(false);
    };
    let reader = IndexReader::open(index_dir)?;
    Ok(visibility.covers_index(meta, reader.file_table_id()))
}

#[derive(serde::Deserialize)]
struct StatusResult {
    num_files: u64,
    num_trigrams: u64,
    cache_size: u64,
    cache_capacity: u64,
    watcher_active: bool,
    watch_mode_requested: Option<String>,
    watch_mode_active: Option<String>,
    watch_fallback_reason: Option<String>,
    watch_budget: Option<usize>,
    poll_interval_secs: Option<u64>,
    last_reconcile_at: Option<u64>,
    last_reconcile_duration_ms: Option<u64>,
    last_reconcile_error: Option<String>,
    #[serde(default)]
    reconcile_running: bool,
    reconcile_pending: Option<bool>,
    reconcile_overdue: Option<bool>,
    #[serde(default)]
    indexing: bool,
    #[serde(default)]
    index_progress: u64,
    #[serde(default)]
    index_total: u64,
    hidden_complete: Option<bool>,
}

fn coverage_label(complete: bool) -> &'static str {
    if complete {
        "complete"
    } else {
        "unavailable (queries scan)"
    }
}

fn write_refresh_status(writer: &mut impl Write, status: &StatusResult) -> std::io::Result<()> {
    if let Some(mode) = &status.watch_mode_active {
        write!(writer, "  Watch mode: {mode}")?;
        if let Some(requested) = &status.watch_mode_requested {
            write!(writer, " (requested: {requested})")?;
        }
        writeln!(writer)?;
    } else if let Some(requested) = &status.watch_mode_requested {
        writeln!(writer, "  Requested:  {requested}")?;
    }
    if let Some(reason) = &status.watch_fallback_reason {
        writeln!(writer, "  Fallback:   {reason}")?;
    }
    if let Some(budget) = status.watch_budget {
        writeln!(writer, "  Watch budget: {budget}")?;
    }
    if let Some(interval) = status.poll_interval_secs {
        writeln!(writer, "  Poll interval: {interval}s (after completion)")?;
    }
    if status.watch_mode_active.is_some()
        || status.watch_mode_requested.is_some()
        || status.reconcile_running
    {
        writeln!(
            writer,
            "  Reconcile:  {}",
            if status.reconcile_running {
                "running"
            } else {
                "idle"
            }
        )?;
        if status.last_reconcile_at.is_none() {
            writeln!(writer, "  Last successful reconcile: never")?;
        }
    }
    if let Some(timestamp) = status.last_reconcile_at {
        writeln!(
            writer,
            "  Last successful reconcile: {}",
            format_timestamp(timestamp)
        )?;
    }
    if let Some(pending) = status.reconcile_pending {
        writeln!(
            writer,
            "  Reconcile pending: {}",
            if pending { "yes" } else { "no" }
        )?;
    }
    if let Some(overdue) = status.reconcile_overdue {
        writeln!(
            writer,
            "  Reconcile overdue: {}",
            if overdue { "yes" } else { "no" }
        )?;
    }
    if let Some(duration) = status.last_reconcile_duration_ms {
        writeln!(writer, "  Last reconcile duration: {duration}ms")?;
    }
    if let Some(error) = &status.last_reconcile_error {
        writeln!(writer, "  Last reconcile error: {error}")?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_status() -> serde_json::Value {
        serde_json::json!({
            "num_files": 152,
            "num_trigrams": 12265,
            "cache_size": 2,
            "cache_capacity": 50000,
            "watcher_active": true
        })
    }

    fn render(status: &StatusResult) -> String {
        let mut output = Vec::new();
        write_refresh_status(&mut output, status).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn refresh_legacy_status_remains_compatible() {
        let status: StatusResult = serde_json::from_value(legacy_status()).unwrap();
        assert!(status.watcher_active);
        assert!(!status.indexing);
        assert!(status.hidden_complete.is_none());
        assert!(!status.reconcile_running);
        assert!(status.watch_mode_requested.is_none());
        assert!(status.watch_mode_active.is_none());
        assert!(status.watch_budget.is_none());
        assert!(status.poll_interval_secs.is_none());
        assert!(status.reconcile_pending.is_none());
        assert!(status.reconcile_overdue.is_none());
        assert_eq!(render(&status), "");
    }

    #[test]
    fn refresh_status_renders_fallback_timing_and_failure() {
        let mut json = legacy_status();
        json.as_object_mut().unwrap().extend(
            serde_json::json!({
                "watcher_active": false,
                "watch_mode_requested": "auto",
                "watch_mode_active": "poll",
                "watch_fallback_reason": "watch_budget_exceeded",
                "watch_budget": 8192,
                "poll_interval_secs": 120,
                "last_reconcile_at": 1,
                "last_reconcile_duration_ms": 42,
                "last_reconcile_error": "reconcile failed",
                "reconcile_running": true,
                "reconcile_pending": true,
                "reconcile_overdue": true,
                "future_status_field": "ignored"
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let status: StatusResult = serde_json::from_value(json).unwrap();
        assert!(!status.watcher_active);
        let output = render(&status);
        for expected in [
            "Watch mode: poll (requested: auto)",
            "Fallback:   watch_budget_exceeded",
            "Watch budget: 8192",
            "Poll interval: 120s (after completion)",
            "Reconcile:  running",
            "Reconcile pending: yes",
            "Reconcile overdue: yes",
            "Last reconcile duration: 42ms",
            "Last reconcile error: reconcile failed",
        ] {
            assert!(
                output.contains(expected),
                "missing {expected:?} in {output}"
            );
        }
        assert!(output.contains(&format!(
            "Last successful reconcile: {}",
            format_timestamp(1)
        )));
    }

    #[test]
    fn refresh_status_accepts_null_optional_fields_and_each_mode() {
        for (requested, active) in [
            ("auto", "native"),
            ("auto", "starting"),
            ("poll", "poll"),
            ("disabled", "disabled"),
        ] {
            let mut json = legacy_status();
            json.as_object_mut().unwrap().extend(
                serde_json::json!({
                    "watch_mode_requested": requested,
                    "watch_mode_active": active,
                    "watch_fallback_reason": null,
                    "watch_budget": null,
                    "poll_interval_secs": null,
                    "last_reconcile_at": null,
                    "last_reconcile_duration_ms": null,
                    "last_reconcile_error": null,
                    "reconcile_running": false,
                    "reconcile_pending": false,
                    "reconcile_overdue": false
                })
                .as_object()
                .unwrap()
                .clone(),
            );
            let status: StatusResult = serde_json::from_value(json).unwrap();
            let output = render(&status);
            assert!(output.contains(&format!("Watch mode: {active} (requested: {requested})")));
            assert!(output.contains("Reconcile:  idle"));
            assert!(output.contains("Reconcile pending: no"));
            assert!(output.contains("Reconcile overdue: no"));
            assert!(output.contains("Last successful reconcile: never"));
            assert!(!output.contains("Fallback:"));
            assert!(!output.contains("Last reconcile error:"));
        }
    }
}
