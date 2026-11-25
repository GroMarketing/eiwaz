//! Eiwaz core engine.
//!
//! Owns real terminal sessions (PTYs), saves pasted/dropped attachments under
//! safe file names, reports Mac memory and agent process memory, and talks to
//! the local Ollama server to list and unload models.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{ProcessesToUpdate, System};
use tauri::{AppHandle, Emitter, Manager, State};

mod bench;
mod chat;
mod search;
mod guide;
mod preview;
mod savepoints;
mod security;
mod models;
pub mod local_mcp;
mod split;
mod procs;
mod routes;
mod jobs;
mod store;

const OLLAMA: &str = "http://127.0.0.1:11434";

struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// The shell's pid, which is also the session id of everything started in it.
    sid: Option<u32>,
}

#[derive(Default)]
struct AppState {
    sessions: Mutex<HashMap<u32, Session>>,
    next_id: AtomicU32,
    sys: Mutex<Option<System>>,
}

#[derive(Clone, Serialize)]
struct OutputEvent {
    id: u32,
    data: String,
}

fn user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into())
}

// ---------- terminals ----------

// Runs off the main thread: a terminal that stops reading must never freeze the app.
#[tauri::command(async)]
fn spawn_session(
    app: AppHandle,
    state: State<AppState>,
    cwd: String,
    startup: Option<String>,
    cols: u16,
    rows: u16,
) -> Result<u32, String> {
    if !Path::new(&cwd).is_dir() {
        return Err(format!("Folder not found: {cwd}"));
    }
    let pair = native_pty_system()
        .openpty(PtySize { rows: rows.max(5), cols: cols.max(20), pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;

    let mut cmd = CommandBuilder::new(user_shell());
    cmd.arg("-l"); // login shell so PATH matches the user's normal Terminal
    cmd.cwd(&cwd);
    // If Eiwaz itself was launched from inside a Claude Code session, that session's
    // markers would leak into every terminal and make Claude Code think it's a child
    // session (which turns off transcript saving). Each terminal starts clean.
    for (key, _) in std::env::vars_os() {
        let k = key.to_string_lossy();
        if k.starts_with("CLAUDE_CODE_") || k == "CLAUDECODE" || k == "CLAUDE_PID" || k == "CLAUDE_EFFORT" {
            cmd.env_remove(&key);
        }
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "Eiwaz");
    if std::env::var("LANG").is_err() {
        cmd.env("LANG", "en_US.UTF-8");
    }

    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);
    let sid = child.process_id();
    if let (Some(sid), Ok(dir)) = (sid, store::data_dir(&app)) {
        procs::track(&dir, sid);
    }
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let mut writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    if let Some(s) = startup.filter(|s| !s.trim().is_empty()) {
        let _ = writer.write_all(format!("{s}\r").as_bytes());
        let _ = writer.flush();
    }

    let id = state.next_id.fetch_add(1, Ordering::SeqCst) + 1;
    let emitter = app.clone();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = emitter.emit("pty-output", OutputEvent { id, data: B64.encode(&buf[..n]) });
                }
            }
        }
        let _ = emitter.emit("pty-exit", id);
    });

    state.sessions.lock().unwrap().insert(id, Session { master: pair.master, writer, child, sid });
    Ok(id)
}

// Runs off the main thread: a terminal that stops reading must never freeze the app.
#[tauri::command(async)]
fn write_session(state: State<AppState>, id: u32, data: String) -> Result<(), String> {
    let mut sessions = state.sessions.lock().unwrap();
    let s = sessions.get_mut(&id).ok_or("Session ended")?;
    s.writer.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    s.writer.flush().map_err(|e| e.to_string())
}

// Runs off the main thread: a terminal that stops reading must never freeze the app.
#[tauri::command(async)]
fn resize_session(state: State<AppState>, id: u32, cols: u16, rows: u16) -> Result<(), String> {
    let sessions = state.sessions.lock().unwrap();
    if let Some(s) = sessions.get(&id) {
        s.master
            .resize(PtySize { rows: rows.max(5), cols: cols.max(20), pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// Runs off the main thread: a terminal that stops reading must never freeze the app.
#[tauri::command(async)]
fn kill_session(app: AppHandle, state: State<AppState>, id: u32) {
    let Some(s) = state.sessions.lock().unwrap().remove(&id) else { return };
    end_all(&app, vec![s], false);
}

/// Close terminals and end everything started in them (Claude Code, its MCP
/// servers, backgrounded dev servers), not just the shell. See procs.rs.
fn end_all<R: tauri::Runtime>(app: &AppHandle<R>, sessions: Vec<Session>, quitting: bool) {
    let mut targets = vec![];
    for mut s in sessions {
        let _ = s.child.kill();
        if let Some(sid) = s.sid {
            targets.push(procs::Target::live(sid));
        }
        drop(s); // closes the pty
    }
    procs::end_sessions(&targets, Duration::from_millis(if quitting { 1500 } else { 2500 }));
    if let Ok(dir) = store::data_dir(app) {
        if quitting {
            procs::untrack(&dir, None);
        } else {
            for t in targets {
                procs::untrack(&dir, Some(t.sid));
            }
        }
    }
}

/// On launch: end whatever a crashed or force-quit Eiwaz left running.
#[tauri::command(async)]
fn sessions_sweep(app: AppHandle) -> Value {
    let Ok(dir) = store::data_dir(&app) else { return json!({ "unclean": false, "ended": [] }) };
    let s = procs::sweep(&dir, Duration::from_millis(2500));
    json!({ "unclean": s.unclean, "ended": s.ended, "summary": procs::describe(&s.ended) })
}

// ---------- attachments ----------

fn attachments_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_cache_dir().map_err(|e| e.to_string())?.join("attachments");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn stamp() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn safe_ext(ext: &str) -> String {
    let e: String = ext.chars().filter(|c| c.is_ascii_alphanumeric()).take(5).collect::<String>().to_lowercase();
    if e.is_empty() { "png".into() } else { e }
}

/// Save a pasted image (base64) under a plain ASCII name and return its path.
#[tauri::command(async)]
fn save_attachment(app: AppHandle, b64: String, ext: String) -> Result<String, String> {
    let bytes = B64.decode(b64.as_bytes()).map_err(|e| e.to_string())?;
    let path = attachments_dir(&app)?.join(format!("paste-{}.{}", stamp(), safe_ext(&ext)));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

const IMAGE_EXTS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "webp", "heic", "tiff"];

/// Images whose names have spaces or odd characters (e.g. macOS screenshot
/// names with a hidden narrow space before AM/PM) are copied to a safe name,
/// because CLI agents often fail to read those paths. Other files keep their
/// real location so the agent edits the right file.
#[tauri::command(async)]
fn stage_file(app: AppHandle, path: String) -> Result<String, String> {
    let p = Path::new(&path);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let is_image = IMAGE_EXTS.contains(&ext.as_str());
    let clean = name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !is_image || clean || p.is_dir() {
        return Ok(path);
    }
    let dest = attachments_dir(&app)?.join(format!("drop-{}.{}", stamp(), safe_ext(&ext)));
    std::fs::copy(p, &dest).map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().into_owned())
}

// ---------- system + tools ----------

#[derive(Serialize)]
struct Stats {
    mem_used: u64,
    mem_total: u64,
    claude_mem: u64,
    codex_mem: u64,
    ollama_mem: u64,
}

fn proc_matches(p: &sysinfo::Process, needle: &str) -> bool {
    let name = p.name().to_string_lossy().to_lowercase();
    if name == needle || name.starts_with(&format!("{needle}-")) {
        return true;
    }
    // Node-based installs show up as "node .../claude" or ".../@anthropic-ai/claude-code/cli.js"
    name == "node"
        && p.cmd().iter().take(3).any(|a| {
            let a = a.to_string_lossy().to_lowercase();
            a.ends_with(&format!("/{needle}")) || a.contains(&format!("{needle}-code")) || a.contains(&format!("/{needle}/"))
        })
}

#[tauri::command(async)]
fn system_stats(state: State<AppState>) -> Stats {
    let mut guard = state.sys.lock().unwrap();
    let sys = guard.get_or_insert_with(System::new);
    sys.refresh_memory();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let mut claude = 0;
    let mut codex = 0;
    let mut ollama = 0;
    for p in sys.processes().values() {
        if proc_matches(p, "claude") {
            claude += p.memory();
        } else if proc_matches(p, "codex") {
            codex += p.memory();
        } else if p.name().to_string_lossy().to_lowercase().starts_with("ollama") {
            ollama += p.memory();
        }
    }
    Stats { mem_used: sys.used_memory(), mem_total: sys.total_memory(), claude_mem: claude, codex_mem: codex, ollama_mem: ollama }
}

/// Which agent tools are installed, as seen by the user's login shell.
#[tauri::command(async)]
fn detect_tools() -> HashMap<String, String> {
    let script = "for t in claude codex ollama lms; do printf '%s=%s\\n' \"$t\" \"$(command -v $t 2>/dev/null)\"; done";
    let out = std::process::Command::new(user_shell()).args(["-lic", script]).output();
    let mut map = HashMap::new();
    if let Ok(o) = out {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some((k, v)) = line.split_once('=') {
                if ["claude", "codex", "ollama", "lms"].contains(&k) {
                    map.insert(k.to_string(), v.trim().to_string());
                }
            }
        }
    }
    map
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_millis(1500)).build()
}

#[derive(Serialize)]
struct LoadedModel {
    name: String,
    size: u64,
}

#[derive(Serialize)]
struct OllamaStatus {
    up: bool,
    loaded: Vec<LoadedModel>,
    installed: Vec<String>,
}

#[tauri::command(async)]
fn ollama_status() -> OllamaStatus {
    let a = agent();
    let ps: Option<Value> = a.get(&format!("{OLLAMA}/api/ps")).call().ok().and_then(|r| r.into_json().ok());
    let Some(ps) = ps else {
        return OllamaStatus { up: false, loaded: vec![], installed: vec![] };
    };
    let loaded = ps["models"]
        .as_array()
        .map(|m| {
            m.iter()
                .map(|x| LoadedModel {
                    name: x["name"].as_str().unwrap_or("").to_string(),
                    size: x["size"].as_u64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    let tags: Option<Value> = a.get(&format!("{OLLAMA}/api/tags")).call().ok().and_then(|r| r.into_json().ok());
    let installed = tags
        .and_then(|t| t["models"].as_array().map(|m| m.iter().filter_map(|x| x["name"].as_str().map(String::from)).filter(|n| bench::can_chat(n)).collect()))
        .unwrap_or_default();
    OllamaStatus { up: true, loaded, installed }
}

#[tauri::command(async)]
fn unload_model(name: String) -> Result<(), String> {
    agent()
        .post(&format!("{OLLAMA}/api/generate"))
        .send_json(json!({ "model": name, "keep_alive": 0 }))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Open a web page in the user's default browser (http/https only).
#[tauri::command(async)]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.chars().any(|c| c.is_whitespace()) {
        return Err("Only web links can be opened".into());
    }
    std::process::Command::new("open").arg(&url).status().map(|_| ()).map_err(|e| e.to_string())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            spawn_session,
            write_session,
            resize_session,
            kill_session,
            sessions_sweep,
            save_attachment,
            stage_file,
            system_stats,
            detect_tools,
            ollama_status,
            unload_model,
            open_url,
            store::store_read,
            store::store_write,
            routes::route_log,
            routes::route_report,
            routes::claude_title,
            split::local_mcp_config,
            split::local_mcp_events,
            split::estimate_request,
            models::hf_search,
            models::hf_vet,
            models::model_pull,
            models::model_pull_cancel,
            models::model_delete,
            savepoints::sp_create,
            savepoints::sp_list,
            savepoints::sp_changes,
            savepoints::sp_diff,
            savepoints::sp_file,
            savepoints::sp_restore,
            savepoints::sp_unsaved,
            savepoints::sp_shot,
            preview::preview_serve,
            preview::preview_probe,
            preview::preview_port_up,
            preview::preview_screenshot,
            preview::read_text,
            preview::read_image,
            chat::chat_file,
            chat::chat_plan,
            chat::chat_send,
            chat::chat_stop,
            search::chat_search,
            bench::bench_start,
            bench::bench_stop,
            bench::bench_env,
            bench::local_run,
            guide::guide_scan,
            guide::guide_catalog
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                quit_cleanup(window.app_handle());
            }
        })
        .build(tauri::generate_context!())
        .expect("error while running Eiwaz")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                quit_cleanup(app);
            }
        });
}

fn quit_cleanup(app: &AppHandle) {
    let state = app.state::<AppState>();
    let all: Vec<Session> = state.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
    if !all.is_empty() {
        end_all(app, all, true);
    }
}

#[cfg(test)]
mod tests;
