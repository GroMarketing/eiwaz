//! Save points: snapshots of a project folder kept in a private "shadow" git
//! repository inside the app's data folder. The project's own .git (if any) is
//! never touched, so this works for folders that aren't git repos too.
//!
//! What it can't undo (said plainly in the UI): anything outside the folder,
//! files ignored by .gitignore, and side effects such as deploys or sent messages.

use crate::store::data_dir;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use tauri::AppHandle;

static GIT_LOCK: Mutex<()> = Mutex::new(());

const DEFAULT_EXCLUDES: &str = "\
# Eiwaz save points skip these by default
node_modules/
.git/
target/
.venv/
venv/
__pycache__/
.next/
.nuxt/
.svelte-kit/
.turbo/
.cache/
.parcel-cache/
.DS_Store
*.log
# Secrets are never copied into save points (and so never undone or deleted)
.env
.env.*
*.pem
*.key
*.p12
id_rsa*
id_ed25519*
.npmrc
.netrc
credentials.json
";

pub fn project_key(project: &str) -> String {
    let mut h = DefaultHasher::new();
    project.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn shadow_dir<R: tauri::Runtime>(app: &AppHandle<R>, project: &str) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("savepoints").join(project_key(project)))
}

pub fn shots_dir<R: tauri::Runtime>(app: &AppHandle<R>, project: &str) -> Result<PathBuf, String> {
    let d = data_dir(app)?.join("shots").join(project_key(project));
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d)
}

struct Repo {
    git_dir: PathBuf,
    work: PathBuf,
}

impl Repo {
    fn cmd(&self) -> Command {
        let mut c = Command::new("git");
        c.arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.work.display()))
            .args(["-c", "core.quotepath=off", "-c", "core.autocrlf=false", "-c", "user.name=Eiwaz", "-c", "user.email=eiwaz@localhost", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
            .current_dir(&self.work)
            .env("GIT_TERMINAL_PROMPT", "0");
        c
    }
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self.cmd().args(args).output().map_err(|e| format!("git couldn't start: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
    fn ok(&self, args: &[&str]) -> bool {
        self.cmd().args(args).output().map(|o| o.status.success()).unwrap_or(false)
    }
    fn has_head(&self) -> bool {
        self.ok(&["rev-parse", "--verify", "-q", "HEAD"])
    }
    fn stage_all(&self) -> Result<(), String> {
        // --ignore-errors: an unreadable file must not block the whole snapshot.
        self.run(&["add", "-A", "--ignore-errors", "."]).map(|_| ())
    }
}

fn git_available() -> Result<(), String> {
    // On a Mac without the command line tools, /usr/bin/git is a stub that pops
    // an installer dialog; check first so we never trigger it by surprise.
    let clt = Command::new("xcode-select").arg("-p").output().map(|o| o.status.success()).unwrap_or(false);
    let brew = Path::new("/opt/homebrew/bin/git").exists() || Path::new("/usr/local/bin/git").exists();
    if clt || brew {
        Ok(())
    } else {
        Err("Save points need Apple's command line tools. Install them with: xcode-select --install".into())
    }
}

fn open<R: tauri::Runtime>(app: &AppHandle<R>, project: &str) -> Result<Repo, String> {
    git_available()?;
    if !Path::new(project).is_dir() {
        return Err(format!("Folder not found: {project}"));
    }
    let repo = Repo { git_dir: shadow_dir(app, project)?, work: PathBuf::from(project) };
    if !repo.git_dir.join("HEAD").exists() {
        std::fs::create_dir_all(&repo.git_dir).map_err(|e| e.to_string())?;
        let out = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&repo.git_dir)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        let _ = std::fs::write(repo.git_dir.join("plado-project"), project);
    }
    // Keep the skip list current (older stores lacked the secrets rules).
    let _ = std::fs::create_dir_all(repo.git_dir.join("info"));
    let ex = repo.git_dir.join("info").join("exclude");
    if std::fs::read_to_string(&ex).map(|c| c != DEFAULT_EXCLUDES).unwrap_or(true) {
        std::fs::write(&ex, DEFAULT_EXCLUDES).map_err(|e| e.to_string())?;
        // stop tracking anything now excluded (files on disk are untouched)
        let _ = repo.run(&["rm", "-r", "-q", "--cached", "--ignore-unmatch", "--", ".env", ".env.*", "*.pem", "*.key", "*.p12", "id_rsa*", "id_ed25519*", ".npmrc", ".netrc", "credentials.json"]);
    }
    Ok(repo)
}

#[derive(Serialize, Clone)]
pub struct SavePoint {
    id: String,
    ts: i64,
    label: String,
    agent: String,
    files: u32,
}

fn parse_log(s: &str) -> Vec<SavePoint> {
    s.split('\x1e')
        .filter_map(|rec| {
            let rec = rec.trim_matches(|c| c == '\n' || c == '\r');
            if rec.is_empty() {
                return None;
            }
            let mut parts = rec.split('\x1f');
            let id = parts.next()?.trim().to_string();
            let ts = parts.next()?.trim().parse().unwrap_or(0);
            let label = parts.next().unwrap_or("").to_string();
            let body = parts.next().unwrap_or("");
            let mut agent = String::new();
            let mut files = 0;
            for line in body.lines() {
                if let Some(a) = line.strip_prefix("agent=") {
                    agent = a.trim().to_string();
                }
                if let Some(f) = line.strip_prefix("files=") {
                    files = f.trim().parse().unwrap_or(0);
                }
            }
            Some(SavePoint { id, ts, label, agent, files })
        })
        .collect()
}

fn create(repo: &Repo, label: &str, agent: &str, force: bool) -> Result<Option<SavePoint>, String> {
    repo.stage_all()?;
    let had_head = repo.has_head();
    let files = if had_head {
        let names = repo.run(&["diff", "--cached", "--name-only", "HEAD"])?;
        names.lines().filter(|l| !l.is_empty()).count()
    } else {
        repo.run(&["ls-files"])?.lines().count()
    };
    if had_head && files == 0 && !force {
        return Ok(None);
    }
    let label = if label.trim().is_empty() { "Save point" } else { label.trim() };
    let body = format!("agent={}\nfiles={}", agent.replace('\n', " "), files);
    repo.run(&["commit", "-q", "--allow-empty", "--no-verify", "-m", label, "-m", &body])?;
    let log = repo.run(&["log", "-1", "--format=%H%x1f%ct%x1f%s%x1f%b%x1e"])?;
    Ok(parse_log(&log).into_iter().next())
}

#[tauri::command(async)]
pub fn sp_create<R: tauri::Runtime>(app: AppHandle<R>, project: String, label: String, agent: String, force: bool) -> Result<Option<SavePoint>, String> {
    let _g = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = open(&app, &project)?;
    create(&repo, &label, &agent, force)
}

#[tauri::command(async)]
pub fn sp_list<R: tauri::Runtime>(app: AppHandle<R>, project: String) -> Result<Vec<SavePoint>, String> {
    let _g = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = open(&app, &project)?;
    if !repo.has_head() {
        return Ok(vec![]);
    }
    Ok(parse_log(&repo.run(&["log", "-n", "200", "--format=%H%x1f%ct%x1f%s%x1f%b%x1e"])?))
}

#[derive(Serialize)]
pub struct FileChange {
    path: String,
    status: String, // added | changed | removed
    added: i64,     // -1 = binary
    removed: i64,
    agents: Vec<String>,
}

fn valid_rev(r: &str) -> bool {
    !r.is_empty() && r.len() <= 64 && r.chars().all(|c| c.is_ascii_hexdigit())
}

/// Who touched each file between `base` and now, from the agent recorded on
/// each turn-end save point. Edits made while two agents ran at once can be
/// credited to the wrong one; the UI says so.
fn attribution(repo: &Repo, base: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(out) = repo.run(&["log", "--format=@@%x1f%b", "--name-only", &format!("{base}..HEAD")]) else {
        return map;
    };
    let mut agent = String::new();
    for line in out.lines() {
        if line.starts_with("@@\x1f") {
            agent.clear();
            if let Some(a) = line.trim_start_matches("@@\x1f").strip_prefix("agent=") {
                agent = a.trim().to_string();
            }
        } else if let Some(a) = line.strip_prefix("agent=") {
            agent = a.trim().to_string();
        } else if line.starts_with("files=") || line.trim().is_empty() {
        } else {
            let who = if agent.is_empty() { "You or another app".to_string() } else { agent.clone() };
            let v = map.entry(line.to_string()).or_default();
            if !v.contains(&who) {
                v.push(who);
            }
        }
    }
    map
}

#[tauri::command(async)]
pub fn sp_changes<R: tauri::Runtime>(app: AppHandle<R>, project: String, base: String) -> Result<Vec<FileChange>, String> {
    if !valid_rev(&base) {
        return Err("Unknown save point".into());
    }
    let _g = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = open(&app, &project)?;
    repo.stage_all()?;
    let status = repo.run(&["diff", "--cached", "--no-renames", "--name-status", "-z", &base])?;
    let numstat = repo.run(&["diff", "--cached", "--no-renames", "--numstat", "-z", &base])?;
    let mut counts: HashMap<String, (i64, i64)> = HashMap::new();
    for rec in numstat.split('\0').filter(|s| !s.is_empty()) {
        let mut p = rec.splitn(3, '\t');
        let a = p.next().unwrap_or("-");
        let r = p.next().unwrap_or("-");
        let path = p.next().unwrap_or("").to_string();
        counts.insert(path, (a.parse().unwrap_or(-1), r.parse().unwrap_or(-1)));
    }
    let who = attribution(&repo, &base);
    let mut out = vec![];
    let mut it = status.split('\0').filter(|s| !s.is_empty());
    while let (Some(code), Some(path)) = (it.next(), it.next()) {
        let st = match code.chars().next() {
            Some('A') => "added",
            Some('D') => "removed",
            _ => "changed",
        };
        let (a, r) = counts.get(path).copied().unwrap_or((-1, -1));
        out.push(FileChange { path: path.to_string(), status: st.into(), added: a, removed: r, agents: who.get(path).cloned().unwrap_or_default() });
    }
    Ok(out)
}

#[tauri::command(async)]
pub fn sp_diff<R: tauri::Runtime>(app: AppHandle<R>, project: String, base: String, path: Option<String>, max_bytes: Option<usize>) -> Result<String, String> {
    if !valid_rev(&base) {
        return Err("Unknown save point".into());
    }
    let _g = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = open(&app, &project)?;
    repo.stage_all()?;
    let mut args = vec!["diff", "--cached", "--no-renames", "-U3", "--no-color", base.as_str()];
    if let Some(p) = path.as_deref() {
        args.push("--");
        args.push(p);
    }
    let mut d = repo.run(&args)?;
    let cap = max_bytes.unwrap_or(400_000);
    if d.len() > cap {
        let mut cut = cap;
        while !d.is_char_boundary(cut) {
            cut -= 1;
        }
        d.truncate(cut);
        d.push_str("\n\\ (cut here: the rest of this change is too long to show)\n");
    }
    Ok(d)
}

/// Contents of a file at a save point (or on disk when rev is empty), as base64.
#[tauri::command(async)]
pub fn sp_file<R: tauri::Runtime>(app: AppHandle<R>, project: String, rev: String, path: String) -> Result<String, String> {
    if path.contains("..") {
        return Err("Bad path".into());
    }
    if rev.is_empty() {
        let p = Path::new(&project).join(&path);
        let meta = std::fs::metadata(&p).map_err(|e| e.to_string())?;
        if meta.len() > 12_000_000 {
            return Err("Too large to show".into());
        }
        return std::fs::read(&p).map(|b| B64.encode(b)).map_err(|e| e.to_string());
    }
    if !valid_rev(&rev) {
        return Err("Unknown save point".into());
    }
    let _g = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = open(&app, &project)?;
    let out = repo.cmd().args(["show", &format!("{rev}:{path}")]).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("Not in that save point".into());
    }
    if out.stdout.len() > 12_000_000 {
        return Err("Too large to show".into());
    }
    Ok(B64.encode(out.stdout))
}

#[derive(Serialize)]
pub struct RestoreResult {
    restored: Vec<String>,
    removed: Vec<String>,
    failed: Vec<String>,
    safety_point: Option<String>,
}

fn restore(repo: &Repo, base: &str, paths: &[String]) -> Result<RestoreResult, String> {
    // A safety save point first, so an undo can itself be undone.
    let safety = create(repo, "Before an undo", "", false)?.map(|s| s.id);
    let mut res = RestoreResult { restored: vec![], removed: vec![], failed: vec![], safety_point: safety };
    for p in paths {
        if p.contains("..") || p.starts_with('/') {
            res.failed.push(p.clone());
            continue;
        }
        if repo.ok(&["cat-file", "-e", &format!("{base}:{p}")]) {
            if repo.ok(&["checkout", base, "--", p]) {
                res.restored.push(p.clone());
            } else {
                res.failed.push(p.clone());
            }
        } else {
            // Added after the save point: undoing it means removing it.
            let full = repo.work.join(p);
            match std::fs::remove_file(&full) {
                Ok(_) => res.removed.push(p.clone()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => res.removed.push(p.clone()),
                Err(_) => res.failed.push(p.clone()),
            }
        }
    }
    repo.stage_all()?;
    Ok(res)
}

/// Of `paths`, the ones edited after the latest save point. Save points are
/// taken after every agent turn, so these edits weren't part of any turn Eiwaz
/// recorded: most likely your own work, or an agent that's still going.
fn unsaved(repo: &Repo, paths: &[String]) -> Result<Vec<String>, String> {
    if !repo.has_head() || paths.is_empty() {
        return Ok(vec![]);
    }
    repo.stage_all()?;
    let changed: std::collections::HashSet<String> = repo
        .run(&["diff", "--cached", "--no-renames", "--name-only", "-z", "HEAD"])?
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    Ok(paths.iter().filter(|p| changed.contains(*p)).cloned().collect())
}

#[tauri::command(async)]
pub fn sp_unsaved<R: tauri::Runtime>(app: AppHandle<R>, project: String, paths: Vec<String>) -> Result<Vec<String>, String> {
    let _g = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = open(&app, &project)?;
    unsaved(&repo, &paths)
}

#[tauri::command(async)]
pub fn sp_restore<R: tauri::Runtime>(app: AppHandle<R>, project: String, base: String, paths: Option<Vec<String>>) -> Result<RestoreResult, String> {
    if !valid_rev(&base) {
        return Err("Unknown save point".into());
    }
    let _g = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = open(&app, &project)?;
    let paths = match paths {
        Some(p) => p,
        None => {
            repo.stage_all()?;
            repo.run(&["diff", "--cached", "--no-renames", "--name-only", "-z", &base])?
                .split('\0')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        }
    };
    restore(&repo, &base, &paths)
}

/// Path where a site screenshot for a save point is (or would be) kept.
#[tauri::command(async)]
pub fn sp_shot<R: tauri::Runtime>(app: AppHandle<R>, project: String, id: String) -> Result<Option<String>, String> {
    if !valid_rev(&id) {
        return Err("Unknown save point".into());
    }
    let p = shots_dir(&app, &project)?.join(format!("{id}.png"));
    Ok(if p.exists() { Some(B64.encode(std::fs::read(&p).map_err(|e| e.to_string())?)) } else { None })
}
