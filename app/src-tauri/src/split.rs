//! Splitting token-heavy requests between Claude Code (cloud) and this Mac.
//!
//! - `local_mcp_config` writes the MCP config Claude Code is started with, so it
//!   can call the read-only "do it on this Mac" tools (see local_mcp.rs).
//! - `local_mcp_events` reads what those tools did, for the status bar and Savings.
//! - `estimate_request` estimates how much a message would send to the cloud:
//!   the message itself, attached files, and project files it names.

use crate::local_mcp::is_secret;
use crate::store::data_dir;
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use tauri::AppHandle;

fn log_path<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("local-steps.jsonl"))
}

#[derive(Serialize)]
pub struct LocalConfig {
    config: String,
    rules: String,
}

/// Write the MCP config for this project and model, and the split rules Claude
/// Code is started with. Both go in files so the launch command stays short
/// (a terminal line over about 1,024 characters blocks).
#[tauri::command]
pub fn local_mcp_config<R: tauri::Runtime>(app: AppHandle<R>, project: String, model: String, rules: String) -> Result<LocalConfig, String> {
    if !Path::new(&project).is_dir() {
        return Err("Project folder not found".into());
    }
    if model.is_empty() || !model.chars().all(|c| c.is_ascii_alphanumeric() || "._:/-=,".contains(c)) {
        return Err("Unusual model name".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = log_path(&app)?;
    let cfg = json!({"mcpServers": {"eiwaz": {
        "type": "stdio",
        "command": exe.to_string_lossy(),
        "args": ["--local-mcp", project, model, log.to_string_lossy()]
    }}});
    let path = data_dir(&app)?.join("claude-local-tools.json");
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap_or_default()).map_err(|e| e.to_string())?;
    let rules_path = data_dir(&app)?.join("claude-split-rules.md");
    std::fs::write(&rules_path, rules).map_err(|e| e.to_string())?;
    Ok(LocalConfig { config: path.to_string_lossy().into_owned(), rules: rules_path.to_string_lossy().into_owned() })
}

#[derive(Serialize)]
pub struct Events {
    events: Vec<Value>,
    offset: u64,
}

/// New log entries since `offset` (a byte position in the log).
#[tauri::command]
pub fn local_mcp_events<R: tauri::Runtime>(app: AppHandle<R>, offset: u64) -> Result<Events, String> {
    let path = log_path(&app)?;
    let Ok(mut f) = std::fs::File::open(&path) else { return Ok(Events { events: vec![], offset: 0 }) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = if offset > len { 0 } else { offset };
    f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let mut s = String::new();
    f.read_to_string(&mut s).map_err(|e| e.to_string())?;
    let events = s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    Ok(Events { events, offset: len })
}

#[derive(Serialize)]
pub struct FileCost {
    path: String,
    tokens: u64,
}

#[derive(Serialize)]
pub struct Estimate {
    message: u64,
    files: Vec<FileCost>,
    total: u64,
}

fn words_like_files(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || "`'\"()[]{}<>,;".contains(c))
        .map(|w| w.trim_end_matches(['.', ':', '!', '?']))
        .filter(|w| {
            let ext = w.rsplit('.').next().unwrap_or("");
            w.contains('.') && (1..=6).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) && !ext.chars().all(|c| c.is_ascii_digit()) && w.len() < 200
        })
        .map(String::from)
        .collect()
}

fn find_by_name(root: &Path, name: &str, budget: &mut usize) -> Option<PathBuf> {
    let direct = root.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    let base = Path::new(name).file_name()?.to_string_lossy().to_string();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if *budget == 0 {
                return None;
            }
            *budget -= 1;
            let p = e.path();
            let n = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !n.starts_with('.') && !["node_modules", "target", "dist", "build", "vendor"].contains(&n.as_str()) {
                    stack.push(p);
                }
            } else if n == base {
                return Some(p);
            }
        }
    }
    None
}

/// Rough tokens a message would send: its own text, attached files, and project
/// files it names (about 4 characters per token). Agents usually read more on
/// their own, so this is a floor, not a ceiling.
#[tauri::command(async)]
pub fn estimate_request(project: String, text: String, attachments: Vec<String>) -> Estimate {
    let message = (text.chars().count() as u64).div_ceil(4);
    let root = PathBuf::from(&project);
    let mut files = vec![];
    let mut seen = std::collections::HashSet::new();
    let mut budget = 20_000usize;
    for a in attachments.iter().take(20) {
        let p = Path::new(a);
        let lower = a.to_lowercase();
        // images are sent as pictures, roughly a fixed cost each
        let tokens = if [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|x| lower.ends_with(x)) { 1600 } else { std::fs::metadata(p).map(|m| m.len() / 4).unwrap_or(0) };
        if seen.insert(a.clone()) {
            files.push(FileCost { path: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), tokens });
        }
    }
    if root.is_dir() {
        for w in words_like_files(&text).into_iter().take(30) {
            if let Some(p) = find_by_name(&root, &w, &mut budget) {
                let rel = p.strip_prefix(&root).unwrap_or(&p).to_path_buf();
                if is_secret(&rel) || !seen.insert(p.to_string_lossy().into_owned()) {
                    continue;
                }
                let tokens = std::fs::metadata(&p).map(|m| m.len() / 4).unwrap_or(0);
                files.push(FileCost { path: rel.to_string_lossy().into_owned(), tokens });
            }
        }
    }
    let total = message + files.iter().map(|f| f.tokens).sum::<u64>();
    Estimate { message, files, total }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_message_and_named_files() {
        let dir = std::env::temp_dir().join(format!("eiwaz-est-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/price.js"), "x".repeat(4000)).unwrap();
        std::fs::write(dir.join(".env"), "x".repeat(4000)).unwrap();
        let e = estimate_request(dir.to_string_lossy().into(), "Fix price.js, it shows 4.5 not $4.50. Also see .env and v1.2".into(), vec![]);
        assert_eq!(e.files.len(), 1, "{:?}", e.files.iter().map(|f| &f.path).collect::<Vec<_>>());
        assert_eq!(e.files[0].path, "src/price.js");
        assert_eq!(e.files[0].tokens, 1000);
        assert!(e.total > 1000 && e.total < 1100);
        let _ = std::fs::remove_dir_all(dir);
    }
}
