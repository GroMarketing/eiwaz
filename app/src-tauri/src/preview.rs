//! Preview: a tiny local file server for the open project (with range requests,
//! so large videos stream instead of loading whole), dev-server discovery, and
//! page screenshots through headless Chrome.
//!
//! The server listens on 127.0.0.1 only, under a random secret path, and refuses
//! dotfiles (.env, .git, keys), so other apps and web pages can't read the project.

use crate::savepoints::shots_dir;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Serialize;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::AppHandle;

struct Server {
    port: u16,
    token: String,
    root: Mutex<PathBuf>,
}

static SERVER: OnceLock<Server> = OnceLock::new();

fn content_type(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(s: &mut TcpStream, code: &str, ctype: &str, body: &[u8]) {
    let _ = write!(s, "HTTP/1.1 {code}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
    let _ = s.write_all(body);
}

fn handle(mut s: TcpStream, srv: &Server) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(match s.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    let mut range: Option<(u64, Option<u64>)> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
            break;
        }
        let l = line.to_ascii_lowercase();
        if let Some(v) = l.strip_prefix("range: bytes=") {
            let v = v.trim();
            if let Some((a, b)) = v.split_once('-') {
                if let Ok(a) = a.parse::<u64>() {
                    range = Some((a, b.parse::<u64>().ok()));
                }
            }
        }
    }
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method != "GET" && method != "HEAD" {
        return respond(&mut s, "405 Method Not Allowed", "text/plain", b"");
    }
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let prefix = format!("/{}/", srv.token);
    let Some(rel) = path.strip_prefix(&prefix) else {
        return respond(&mut s, "404 Not Found", "text/plain", b"Not found");
    };
    let rel = pct_decode(rel);
    if rel.split('/').any(|c| c.starts_with('.') || c == "..") {
        return respond(&mut s, "403 Forbidden", "text/plain", b"Hidden files are not served");
    }
    let root = srv.root.lock().unwrap().clone();
    let mut full = root.join(&rel);
    if full.is_dir() {
        full = full.join("index.html");
    }
    let (Ok(canon), Ok(croot)) = (full.canonicalize(), root.canonicalize()) else {
        return respond(&mut s, "404 Not Found", "text/plain", b"Not found");
    };
    if !canon.starts_with(&croot) {
        return respond(&mut s, "403 Forbidden", "text/plain", b"Outside the project");
    }
    let Ok(mut f) = std::fs::File::open(&canon) else {
        return respond(&mut s, "404 Not Found", "text/plain", b"Not found");
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let ctype = content_type(&canon);
    let (start, end, status) = match range {
        Some((a, b)) if a < len => (a, b.unwrap_or(len - 1).min(len - 1), "206 Partial Content"),
        Some(_) => {
            let _ = write!(s, "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\n\r\n");
            return;
        }
        None => (0, len.saturating_sub(1), "200 OK"),
    };
    let count = if len == 0 { 0 } else { end - start + 1 };
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {count}\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n");
    // Page files are previewed in a sandbox with no origin ("null"). Let those
    // pages load their own module scripts and fonts; the secret path still
    // keeps everyone else out.
    head.push_str("Access-Control-Allow-Origin: null\r\nX-Content-Type-Options: nosniff\r\n");
    if status.starts_with("206") {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
    }
    head.push_str("\r\n");
    if s.write_all(head.as_bytes()).is_err() || method == "HEAD" || count == 0 {
        return;
    }
    if f.seek(SeekFrom::Start(start)).is_err() {
        return;
    }
    let mut left = count;
    let mut buf = vec![0u8; 256 * 1024];
    while left > 0 {
        let n = (left as usize).min(buf.len());
        match f.read(&mut buf[..n]) {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                if s.write_all(&buf[..k]).is_err() {
                    break;
                }
                left -= k as u64;
            }
        }
    }
}

fn token() -> String {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let pid = std::process::id() as u128;
    let mut x = n ^ (pid << 64) ^ 0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C834;
    let mut out = String::new();
    for _ in 0..24 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(char::from(b"abcdefghijklmnopqrstuvwxyz0123456789"[(x % 36) as usize]));
    }
    out
}

/// Start (once) the local file server and point it at `project`.
/// Returns the base URL ending in "/".
#[tauri::command(async)]
pub fn preview_serve(project: String) -> Result<String, String> {
    if !Path::new(&project).is_dir() {
        return Err("Folder not found".into());
    }
    if let Some(srv) = SERVER.get() {
        *srv.root.lock().unwrap() = PathBuf::from(&project);
        return Ok(format!("http://127.0.0.1:{}/{}/", srv.port, srv.token));
    }
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let _ = SERVER.set(Server { port, token: token(), root: Mutex::new(PathBuf::from(&project)) });
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || handle(conn, SERVER.get().unwrap()));
        }
    });
    let srv = SERVER.get().unwrap();
    Ok(format!("http://127.0.0.1:{}/{}/", srv.port, srv.token))
}

#[derive(Serialize)]
pub struct DevServer {
    port: u16,
    url: String,
    command: String,
    from_project: bool,
}

#[derive(Serialize)]
pub struct Probe {
    servers: Vec<DevServer>,
    start_command: Option<String>,
    files: Vec<String>,
}

fn listening() -> Vec<(u32, u16, String)> {
    // pid, port, command name
    let out = Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"]).output();
    let mut v = vec![];
    if let Ok(o) = out {
        let (mut pid, mut cmd) = (0u32, String::new());
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            match line.chars().next() {
                Some('p') => pid = line[1..].parse().unwrap_or(0),
                Some('c') => cmd = line[1..].to_string(),
                Some('n') => {
                    let addr = &line[1..];
                    let local = addr.starts_with("127.") || addr.starts_with("*:") || addr.starts_with("[::1]") || addr.starts_with("[::]") || addr.starts_with("localhost");
                    if let Some(port) = addr.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) {
                        if local && !v.iter().any(|(_, p, _)| *p == port) {
                            v.push((pid, port, cmd.clone()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    v
}

fn pid_cwd(pid: u32) -> String {
    Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().find(|l| l.starts_with('n')).map(|l| l[1..].to_string()))
        .unwrap_or_default()
}

fn speaks_http(port: u16) -> bool {
    let Ok(mut addrs) = ("127.0.0.1", port).to_socket_addrs() else { return false };
    let Some(addr) = addrs.next() else { return false };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(250)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_millis(600)));
    if s.write_all(b"HEAD / HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = [0u8; 5];
    s.read_exact(&mut buf).is_ok() && &buf == b"HTTP/"
}

/// Is a web server answering on this port of this Mac? For addresses you type
/// yourself, which the probe may skip (uncommon port, started elsewhere).
#[tauri::command(async)]
pub fn preview_port_up(port: u16) -> bool {
    port >= 1024 && speaks_http(port)
}

fn start_command(project: &Path) -> Option<String> {
    let pkg = std::fs::read_to_string(project.join("package.json")).ok();
    if let Some(pkg) = pkg {
        let v: serde_json::Value = serde_json::from_str(&pkg).ok()?;
        let scripts = v.get("scripts")?.as_object()?;
        let pm = if project.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if project.join("yarn.lock").exists() {
            "yarn"
        } else if project.join("bun.lockb").exists() || project.join("bun.lock").exists() {
            "bun"
        } else {
            "npm"
        };
        for s in ["dev", "start", "serve", "preview"] {
            if scripts.contains_key(s) {
                return Some(if pm == "npm" && s != "start" { format!("npm run {s}") } else { format!("{pm} {s}") });
            }
        }
    }
    if project.join("manage.py").exists() {
        return Some("python3 manage.py runserver".into());
    }
    None
}

const PREVIEWABLE: [&str; 17] = ["html", "htm", "md", "markdown", "png", "jpg", "jpeg", "gif", "webp", "svg", "avif", "mp4", "mov", "webm", "m4v", "pdf", "mmd"];

fn walk(dir: &Path, root: &Path, depth: u32, out: &mut Vec<String>) {
    if depth > 5 || out.len() >= 400 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || ["node_modules", "target", "venv", "__pycache__", "Pods", "build", ".next"].contains(&name.as_str()) {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            walk(&p, root, depth + 1, out);
        } else if let Some(ext) = p.extension().and_then(|x| x.to_str()) {
            if PREVIEWABLE.contains(&ext.to_ascii_lowercase().as_str()) {
                if let Ok(r) = p.strip_prefix(root) {
                    out.push(r.to_string_lossy().into_owned());
                }
            }
        }
        if out.len() >= 400 {
            return;
        }
    }
}

#[tauri::command(async)]
pub fn preview_probe(project: String) -> Probe {
    let root = PathBuf::from(&project);
    let real = root.canonicalize().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| project.clone());
    let own = SERVER.get().map(|s| s.port).unwrap_or(0);
    let mut servers = vec![];
    for (pid, port, cmd) in listening() {
        // Skip our own server, local model engines and system services.
        if port == own || [11434, 1234, 5000, 7000, 631].contains(&port) || port < 1024 {
            continue;
        }
        let lc = cmd.to_lowercase();
        if ["ollama", "lm studio", "eiwaz", "plado", "controlce", "rapportd", "figma", "spotify", "dropbox", "code helper", "cursor", "slack"].iter().any(|n| lc.contains(n)) {
            continue;
        }
        let cwd = pid_cwd(pid);
        let from_project = !cwd.is_empty() && (cwd.starts_with(&project) || cwd.starts_with(&real));
        let common = [3000, 3001, 4000, 4200, 4321, 5173, 5174, 8000, 8080, 8787, 8888, 1313, 4173, 8081].contains(&port);
        if !(from_project || common) || !speaks_http(port) {
            continue;
        }
        servers.push(DevServer { port, url: format!("http://localhost:{port}/"), command: cmd, from_project });
    }
    servers.sort_by_key(|s| (!s.from_project, s.port));
    let mut files = vec![];
    walk(&root, &root, 0, &mut files);
    Probe { servers, start_command: start_command(&root), files }
}

fn find_chrome() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ]
    .iter()
    .map(PathBuf::from)
    .chain(std::iter::once(PathBuf::from(format!("{home}/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"))))
    .find(|p| p.exists())
}

/// Screenshot a page with headless Chrome (a separate throwaway profile, so the
/// user's own Chrome is untouched). Saved under the save point's id when given,
/// else as an attachment. Returns the file path.
#[tauri::command(async)]
pub fn preview_screenshot<R: tauri::Runtime>(app: AppHandle<R>, url: String, width: u32, height: u32, project: Option<String>, save_point: Option<String>) -> Result<String, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("Only web pages can be captured".into());
    }
    let chrome = find_chrome().ok_or("Screenshots need Google Chrome (or Chromium, Brave or Edge) installed. You can still press ⌘⇧4 and paste the picture with ⌘V.")?;
    let cache = tauri::Manager::path(&app).app_cache_dir().map_err(|e| e.to_string())?;
    let profile = cache.join("chrome-shot-profile");
    let dest = match (project, save_point) {
        (Some(p), Some(id)) if id.chars().all(|c| c.is_ascii_hexdigit()) && !id.is_empty() => shots_dir(&app, &p)?.join(format!("{id}.png")),
        _ => {
            let d = cache.join("attachments");
            std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
            let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            d.join(format!("preview-{ms}.png"))
        }
    };
    let mut child = Command::new(chrome)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--mute-audio",
            "--virtual-time-budget=4000",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--window-size={},{}", width.clamp(320, 2560), height.clamp(320, 4000)))
        .arg(format!("--screenshot={}", dest.display()))
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if started.elapsed() > Duration::from_secs(40) {
            let _ = child.kill();
            return Err("The page took too long to capture".into());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    if dest.exists() {
        Ok(dest.to_string_lossy().into_owned())
    } else {
        Err("Chrome couldn't capture that page. Is the site running?".into())
    }
}

/// Read a project text file (for markdown/diagram preview). Size-capped.
#[tauri::command(async)]
pub fn read_text(project: String, path: String) -> Result<String, String> {
    if path.contains("..") || path.split('/').any(|c| c.starts_with('.')) {
        return Err("Hidden files are not shown".into());
    }
    let p = Path::new(&project).join(&path);
    let meta = std::fs::metadata(&p).map_err(|e| e.to_string())?;
    if meta.len() > 3_000_000 {
        return Err("Too large to preview".into());
    }
    std::fs::read_to_string(&p).map_err(|e| e.to_string())
}

/// Read any image the app made (screenshots) as base64, for thumbnails.
#[tauri::command(async)]
pub fn read_image<R: tauri::Runtime>(app: AppHandle<R>, path: String) -> Result<String, String> {
    let cache = tauri::Manager::path(&app).app_cache_dir().map_err(|e| e.to_string())?;
    let data = crate::store::data_dir(&app)?;
    let p = PathBuf::from(&path);
    let ok = p.canonicalize().map(|c| c.starts_with(cache.canonicalize().unwrap_or(cache.clone())) || c.starts_with(data.canonicalize().unwrap_or(data.clone()))).unwrap_or(false);
    if !ok {
        return Err("Not an app image".into());
    }
    std::fs::read(&p).map(|b| B64.encode(b)).map_err(|e| e.to_string())
}
