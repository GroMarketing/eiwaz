//! Eiwaz Guide: looks at the open project (what it's built with, which agent
//! add-ons are already set up) and matches it against the public Plado catalog.
//! The catalog is downloaded as a whole file and cached for a day; nothing about
//! the project is sent anywhere.

use crate::store::data_dir;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};
use tauri::AppHandle;

const CATALOG_URL: &str = "https://plado.pages.dev/data/catalog.json";

#[derive(Serialize, Clone)]
pub struct Signal {
    integration: String,
    evidence: String,
}

#[derive(Serialize, Clone)]
pub struct Installed {
    name: String,
    app: String,   // Claude Code | Codex
    scope: String, // this project | all projects
    detail: String,
}

#[derive(Serialize)]
pub struct ProjectFacts {
    signals: Vec<Signal>,
    stack: Vec<String>,
    has_claude_md: bool,
    has_agents_md: bool,
    installed: Vec<Installed>,
    skills: u32,
    subagents: u32,
    git_remote: String,
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// dependency-name fragment -> integration tag used by the catalog
const DEP_MAP: &[(&str, &str)] = &[
    ("stripe", "stripe"),
    ("@stripe/", "stripe"),
    ("@supabase/", "supabase"),
    ("supabase", "supabase"),
    ("pg", "postgres"),
    ("postgres", "postgres"),
    ("@neondatabase", "postgres"),
    ("@playwright/test", "playwright"),
    ("playwright", "playwright"),
    ("@sentry/", "sentry"),
    ("@notionhq/client", "notion"),
    ("@slack/", "slack"),
    ("@linear/sdk", "linear"),
    ("@shopify/", "shopify"),
    ("mongodb", "mongodb"),
    ("mongoose", "mongodb"),
    ("redis", "redis"),
    ("ioredis", "redis"),
    ("@aws-sdk/", "aws"),
    ("aws-sdk", "aws"),
    ("@vercel/", "vercel"),
    ("vercel", "vercel"),
    ("remotion", "remotion"),
    ("@hubspot/", "hubspot"),
    ("wrangler", "cloudflare"),
    ("@cloudflare/", "cloudflare"),
    ("figma", "figma"),
];

fn dep_matches(dep: &str, frag: &str) -> bool {
    if frag.ends_with('/') {
        dep.starts_with(frag)
    } else {
        dep == frag || dep.starts_with(&format!("{frag}-")) || dep.ends_with(&format!("/{frag}"))
    }
}

fn add(sig: &mut Vec<Signal>, integ: &str, ev: String) {
    if !sig.iter().any(|s| s.integration == integ) {
        sig.push(Signal { integration: integ.into(), evidence: ev });
    }
}

fn mcp_names(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|o| {
            o.iter()
                .map(|(k, s)| {
                    let detail = s["url"].as_str().map(String::from).unwrap_or_else(|| {
                        let cmd = s["command"].as_str().unwrap_or("");
                        let args: Vec<String> = s["args"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                        format!("{cmd} {}", args.join(" ")).trim().to_string()
                    });
                    (k.clone(), detail)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn count_dir(p: &Path, want_dirs: bool) -> u32 {
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().filter(|e| if want_dirs { e.path().is_dir() } else { e.path().extension().map(|x| x == "md").unwrap_or(false) }).count() as u32)
        .unwrap_or(0)
}

#[tauri::command(async)]
pub fn guide_scan(project: String) -> Result<ProjectFacts, String> {
    let root = Path::new(&project);
    if !root.is_dir() {
        return Err("Folder not found".into());
    }
    let mut signals = vec![];
    let mut stack = BTreeSet::new();
    if let Some(pkg) = read_json(&root.join("package.json")) {
        stack.insert("JavaScript / Node".to_string());
        let mut deps: Vec<String> = vec![];
        for k in ["dependencies", "devDependencies"] {
            if let Some(o) = pkg[k].as_object() {
                deps.extend(o.keys().cloned());
            }
        }
        for d in &deps {
            for &(frag, integ) in DEP_MAP {
                if dep_matches(d, frag) {
                    add(&mut signals, integ, format!("`{d}` in package.json"));
                }
            }
            for (frag, label) in [("next", "Next.js"), ("astro", "Astro"), ("react", "React"), ("vue", "Vue"), ("svelte", "Svelte"), ("vite", "Vite"), ("tailwindcss", "Tailwind"), ("@tauri-apps/api", "Tauri"), ("express", "Express")] {
                if d == frag {
                    stack.insert(label.to_string());
                }
            }
        }
    }
    let exists = |f: &str| root.join(f).exists();
    if exists("wrangler.toml") || exists("wrangler.jsonc") || exists("wrangler.json") {
        add(&mut signals, "cloudflare", "a wrangler config file".into());
        stack.insert("Cloudflare Workers".into());
    }
    if exists("vercel.json") || exists(".vercel") {
        add(&mut signals, "vercel", "vercel.json".into());
    }
    if exists("Dockerfile") || exists("docker-compose.yml") || exists("compose.yaml") {
        add(&mut signals, "docker", "a Dockerfile".into());
    }
    if exists("supabase") {
        add(&mut signals, "supabase", "a supabase/ folder".into());
    }
    if let Ok(s) = std::fs::read_to_string(root.join("prisma/schema.prisma")) {
        if s.contains("postgresql") {
            add(&mut signals, "postgres", "Prisma set to PostgreSQL".into());
        }
    }
    if exists("playwright.config.ts") || exists("playwright.config.js") {
        add(&mut signals, "playwright", "playwright.config".into());
    }
    if exists("Cargo.toml") {
        stack.insert("Rust".into());
    }
    if exists("pyproject.toml") || exists("requirements.txt") {
        stack.insert("Python".into());
    }
    if exists("go.mod") {
        stack.insert("Go".into());
    }
    if exists("index.html") && stack.is_empty() {
        stack.insert("Static website".into());
    }
    let git_remote = Command::new("git")
        .args(["-C", &project, "config", "--get", "remote.origin.url"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if git_remote.contains("github.com") {
        add(&mut signals, "github", "the project is on GitHub".into());
    }

    // What's already installed, per app and scope.
    let mut installed = vec![];
    if let Some(v) = read_json(&root.join(".mcp.json")) {
        for (n, d) in mcp_names(&v["mcpServers"]) {
            installed.push(Installed { name: n, app: "Claude Code".into(), scope: "this project (shared file .mcp.json)".into(), detail: d });
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if let Some(v) = read_json(&Path::new(&home).join(".claude.json")) {
        for (n, d) in mcp_names(&v["mcpServers"]) {
            installed.push(Installed { name: n, app: "Claude Code".into(), scope: "all projects".into(), detail: d });
        }
        for (n, d) in mcp_names(&v["projects"][&project]["mcpServers"]) {
            installed.push(Installed { name: n, app: "Claude Code".into(), scope: "this project, just you".into(), detail: d });
        }
    }
    if let Ok(toml) = std::fs::read_to_string(Path::new(&home).join(".codex/config.toml")) {
        for line in toml.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("[mcp_servers.") {
                let name = rest.trim_end_matches(']').trim_matches('"').to_string();
                if !name.contains('.') {
                    installed.push(Installed { name, app: "Codex".into(), scope: "all projects".into(), detail: String::new() });
                }
            }
        }
    }
    Ok(ProjectFacts {
        signals,
        stack: stack.into_iter().collect(),
        has_claude_md: exists("CLAUDE.md") || exists(".claude/CLAUDE.md"),
        has_agents_md: exists("AGENTS.md"),
        installed,
        skills: count_dir(&root.join(".claude/skills"), true),
        subagents: count_dir(&root.join(".claude/agents"), false),
        git_remote,
    })
}

#[derive(Serialize)]
pub struct CatalogPick {
    integration: String,
    entries: Vec<Value>,
}

#[derive(Serialize)]
pub struct CatalogResult {
    picks: Vec<CatalogPick>,
    built_at: String,
    fetched_at: u64,
    offline: bool,
    error: String,
}

fn load_catalog<R: tauri::Runtime>(app: &AppHandle<R>, refresh: bool) -> (Option<Value>, u64, bool, String) {
    let Ok(dir) = data_dir(app) else { return (None, 0, true, "No data folder".into()) };
    let path = dir.join("plado-catalog.json");
    let age = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok()).and_then(|t| SystemTime::now().duration_since(t).ok());
    let fresh = age.map(|a| a < Duration::from_secs(24 * 3600)).unwrap_or(false);
    let mut err = String::new();
    let mut offline = false;
    if refresh || !fresh {
        match ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).user_agent("Eiwaz/0.1").build().get(CATALOG_URL).call() {
            Ok(r) => {
                let mut body = String::new();
                if std::io::Read::read_to_string(&mut r.into_reader(), &mut body).is_ok() && body.starts_with('{') {
                    let _ = std::fs::write(&path, &body);
                }
            }
            Err(e) => {
                offline = true;
                err = e.to_string();
            }
        }
    }
    let fetched = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let v = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok());
    (v, fetched, offline, err)
}

/// Integrations the catalog doesn't tag: match on the add-on's own name/repo.
fn name_match(e: &Value, integ: &str) -> bool {
    let hay = format!("{} {} {}", e["name"].as_str().unwrap_or(""), e["registryName"].as_str().unwrap_or(""), e["repo"].as_str().unwrap_or("")).to_lowercase();
    hay.contains(integ)
}

#[tauri::command(async)]
pub fn guide_catalog<R: tauri::Runtime>(app: AppHandle<R>, integrations: Vec<String>, refresh: bool) -> CatalogResult {
    let (cat, fetched_at, offline, error) = load_catalog(&app, refresh);
    let Some(cat) = cat else {
        return CatalogResult { picks: vec![], built_at: String::new(), fetched_at, offline: true, error: if error.is_empty() { "Catalog not downloaded yet".into() } else { error } };
    };
    let entries = cat["entries"].as_array().cloned().unwrap_or_default();
    let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for e in entries {
        if e["github"]["archived"].as_bool().unwrap_or(false) {
            continue;
        }
        let tags: Vec<&str> = e["integrations"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        for i in &integrations {
            if tags.contains(&i.as_str()) || (i == "cloudflare" && name_match(&e, "cloudflare")) {
                by.entry(i.clone()).or_default().push(e.clone());
            }
        }
    }
    let picks = integrations
        .iter()
        .filter_map(|i| {
            let mut v = by.remove(i)?;
            v.sort_by(|a, b| b["score"]["total"].as_i64().unwrap_or(0).cmp(&a["score"]["total"].as_i64().unwrap_or(0)));
            v.truncate(12);
            Some(CatalogPick { integration: i.clone(), entries: v })
        })
        .collect();
    CatalogResult { picks, built_at: cat["builtAt"].as_str().unwrap_or("").into(), fetched_at, offline, error }
}
