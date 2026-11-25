//! Finding, vetting and downloading new local models from Hugging Face, so Bench
//! can test them. Vetting happens before anything is downloaded: who published
//! it, how established they are, whether it's a re-upload of someone else's
//! model, what Hugging Face's scanners said about each file, and whether the
//! chat template it ships with contains anything that doesn't belong.
//!
//! Only public metadata is requested from Hugging Face. Downloads go through
//! Ollama's own `pull`, which verifies each file's checksum.

use crate::security::{red_flags, Check};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

const HF: &str = "https://huggingface.co";
static PULLING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

fn hf_get(path: &str) -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).user_agent("Eiwaz/0.1").build();
    agent.get(&format!("{HF}{path}")).call().map_err(|e| e.to_string())?.into_json().map_err(|e| e.to_string())
}

fn hf_text(path: &str) -> Option<String> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).user_agent("Eiwaz/0.1").build();
    agent.get(&format!("{HF}{path}")).call().ok()?.into_string().ok()
}

/// Repo ids are "owner/name" with a narrow character set; refuse anything else
/// before it goes into a URL.
fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |s: Option<&str>| s.map(|s| !s.is_empty() && s.len() <= 96 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !s.starts_with('.')).unwrap_or(false);
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

fn days_since(iso: &str) -> Option<i64> {
    // "2026-06-13T18:04:00.000Z" -> days ago (good enough without a date crate)
    let d = iso.get(0..10)?;
    let mut it = d.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (it.next()??, it.next()??, it.next()??);
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let then = era * 146097 + doe - 719468;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64 / 86400;
    Some(now - then)
}

fn ago(days: i64) -> String {
    match days {
        d if d < 1 => "today".into(),
        1 => "yesterday".into(),
        d if d < 60 => format!("{d} days ago"),
        d if d < 730 => format!("{} months ago", d / 30),
        d => format!("{} years ago", d / 365),
    }
}

// ---------- search ----------

#[derive(Serialize)]
pub struct Hit {
    id: String,
    downloads: u64,
    likes: u64,
    updated: String,
    gated: bool,
}

#[tauri::command(async)]
pub fn hf_search(query: String) -> Result<Vec<Hit>, String> {
    let q: String = query.trim().chars().filter(|c| c.is_ascii_alphanumeric() || " -_./:".contains(*c)).take(80).collect();
    if q.is_empty() {
        return Ok(vec![]);
    }
    let enc = q.replace(' ', "%20").replace(':', "%3A").replace('/', "%2F");
    let v = hf_get(&format!("/api/models?search={enc}&filter=gguf&sort=downloads&limit=15&full=false"))?;
    Ok(v.as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|m| Hit {
            id: m["id"].as_str().unwrap_or("").into(),
            downloads: m["downloads"].as_u64().unwrap_or(0),
            likes: m["likes"].as_u64().unwrap_or(0),
            updated: m["lastModified"].as_str().and_then(days_since).map(ago).unwrap_or_default(),
            gated: m["gated"].as_bool().unwrap_or(m["gated"].is_string()),
        })
        .collect())
}

// ---------- vet ----------

#[derive(Serialize)]
pub struct VetFile {
    path: String,
    tag: String, // the Ollama tag for `hf.co/repo:TAG`
    size: u64,
    scans: Vec<(String, String)>,
    overall: String, // Hugging Face's overall scan status
    pullable: bool,
    why_not: String,
}

#[derive(Serialize)]
pub struct Vet {
    id: String,
    author: String,
    base_model: Vec<String>,
    license: String,
    downloads: u64,
    likes: u64,
    gated: bool,
    checks: Vec<Check>,
    files: Vec<VetFile>,
    commits: Vec<(String, String, String)>, // (when, title, who)
}

/// The quant tag Ollama uses for a GGUF file name, e.g. "...-Q4_K_M.gguf" -> "Q4_K_M".
pub fn quant_tag(file: &str) -> Option<String> {
    let stem = file.rsplit('/').next()?.strip_suffix(".gguf")?;
    let last = stem.rsplit(|c| c == '-' || c == '.').next()?;
    let up = last.to_uppercase();
    let looks = (up.starts_with('Q') || up.starts_with("IQ") || up.starts_with("TQ")) && up.chars().any(|c| c.is_ascii_digit())
        || ["F16", "BF16", "F32", "MXFP4"].contains(&up.as_str());
    looks.then_some(up)
}

fn check(name: &str, status: &str, detail: impl Into<String>) -> Check {
    Check { name: name.into(), status: status.into(), detail: detail.into() }
}

#[tauri::command(async)]
pub fn hf_vet(repo: String) -> Result<Vet, String> {
    if !valid_repo(&repo) {
        return Err("That doesn't look like a Hugging Face repo (owner/name).".into());
    }
    let info = hf_get(&format!("/api/models/{repo}"))?;
    let tree = hf_get(&format!("/api/models/{repo}/tree/main?expand=true")).unwrap_or(Value::Null);
    let commits = hf_get(&format!("/api/models/{repo}/commits/main")).unwrap_or(Value::Null);
    let author = info["author"].as_str().unwrap_or("").to_string();
    let gated = info["gated"].as_bool().unwrap_or(info["gated"].is_string());
    let card = &info["cardData"];
    let base_model: Vec<String> = match &card["base_model"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    };
    let mut checks = vec![];

    // Format: what kinds of model files are in the repo?
    let all: Vec<Value> = tree.as_array().cloned().unwrap_or_default().into_iter().filter(|f| f["type"] == "file").collect();
    let pickles: Vec<&str> = all.iter().filter_map(|f| f["path"].as_str()).filter(|p| [".bin", ".pt", ".pth", ".pkl", ".ckpt"].iter().any(|x| p.ends_with(x))).collect();
    if pickles.is_empty() {
        checks.push(check("Format", "ok", "Only GGUF and text files. GGUF is data-only: loading it can't run code."));
    } else {
        checks.push(check("Format", "warn", format!("The repo also has {} file(s) in the pickle format ({}), which can run code when loaded. Eiwaz only downloads the GGUF files.", pickles.len(), pickles.iter().take(2).cloned().collect::<Vec<_>>().join(", "))));
    }

    // Who published it?
    let user = hf_get(&format!("/api/users/{author}/overview")).ok();
    let org = if user.is_none() { hf_get(&format!("/api/organizations/{author}/overview")).ok() } else { None };
    match (&user, &org) {
        (Some(u), _) => {
            let days = u["createdAt"].as_str().and_then(days_since);
            let models = u["numModels"].as_u64().unwrap_or(0);
            let followers = u["numFollowers"].as_u64().unwrap_or(0);
            let since = days.map(|d| format!("joined {}", ago(d))).unwrap_or_else(|| "join date unknown".into());
            let young = days.map(|d| d < 90).unwrap_or(true);
            checks.push(check("Publisher", if young { "warn" } else { "ok" }, format!("{author}: a personal account, {since}, {models} models, {followers} followers.{}", if young { " A new account is a common sign of a throwaway upload." } else { "" })));
        }
        (None, Some(o)) => {
            let verified = o["isVerified"].as_bool().unwrap_or(false);
            checks.push(check("Publisher", "ok", format!("{author}: an organization{}, {} models, {} followers.", if verified { " verified by Hugging Face" } else { " (not verified by Hugging Face)" }, o["numModels"].as_u64().unwrap_or(0), o["numFollowers"].as_u64().unwrap_or(0))));
        }
        _ => checks.push(check("Publisher", "warn", format!("Couldn't look up {author} on Hugging Face."))),
    }

    // Is this the maker's own file, or someone else re-packaging it?
    if let Some(base) = base_model.first() {
        let base_owner = base.split('/').next().unwrap_or("");
        if base_owner.eq_ignore_ascii_case(&author) {
            checks.push(check("Made by", "ok", format!("Published by the same account that made the model ({base}). Its test results apply to this file.")));
        } else {
            checks.push(check("Made by", "info", format!("{author} made these files from {base}; {base_owner} didn't publish them. Test results published for {base} may not apply to them.")));
        }
    } else {
        checks.push(check("Made by", "warn", "The repo doesn't say what model it's made from."));
    }

    // Gated: needs the person's own login.
    if gated {
        checks.push(check("Access", "warn", "Gated: you have to accept its terms on Hugging Face and sign Ollama in to Hugging Face yourself. Eiwaz can't download it for you."));
    }

    // The chat template it ships with (Hugging Face reads it out of the GGUF).
    let tpl = info["gguf"]["chat_template"].as_str().unwrap_or("");
    if tpl.is_empty() {
        checks.push(check("Built-in template", "info", "Hugging Face doesn't show this repo's chat template. Bench checks it after download."));
    } else {
        let flags = red_flags(tpl);
        if flags.is_empty() {
            checks.push(check("Built-in template", "ok", "No web addresses, commands, override phrases or hidden characters in its chat template."));
        } else {
            checks.push(check("Built-in template", "bad", format!("Its chat template contains {}. Don't download it.", flags.join(", "))));
        }
    }

    // Recent changes: a file swapped after it was scanned is a classic trick.
    let cs: Vec<(String, String, String)> = commits
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .take(5)
        .map(|c| {
            let when = c["date"].as_str().and_then(days_since).map(ago).unwrap_or_default();
            let who = c["authors"].as_array().map(|a| a.iter().filter_map(|x| x["user"].as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            (when, c["title"].as_str().unwrap_or("").chars().take(90).collect(), who)
        })
        .collect();
    let outsider = commits.as_array().map(|a| a.iter().take(5).any(|c| c["authors"].as_array().map(|x| x.iter().any(|u| u["user"].as_str().map(|n| !n.eq_ignore_ascii_case(&author)).unwrap_or(false))).unwrap_or(false))).unwrap_or(false);
    if outsider {
        checks.push(check("Changes", "warn", "Someone other than the publisher changed this repo recently. See the history below."));
    }

    // Files: GGUF models only, each with Hugging Face's scan results.
    let mut files = vec![];
    for f in &all {
        let path = f["path"].as_str().unwrap_or("");
        if !path.ends_with(".gguf") {
            continue;
        }
        let low = path.to_lowercase();
        let s = &f["securityFileStatus"];
        let scans: Vec<(String, String)> = [("avScan", "Antivirus"), ("pickleImportScan", "Pickle"), ("jFrogScan", "JFrog"), ("protectAiScan", "Protect AI")]
            .iter()
            .filter_map(|(k, label)| s[*k]["status"].as_str().map(|st| (label.to_string(), st.to_string())))
            .collect();
        let overall = s["status"].as_str().unwrap_or("not scanned").to_string();
        let tag = quant_tag(path);
        let (pullable, why_not) = if low.contains("mmproj") {
            (false, "Image/audio add-on, downloaded with the model when needed".to_string())
        } else if low.contains("-of-0") {
            (false, "Split into parts; download it with Ollama directly".to_string())
        } else if tag.is_none() {
            (false, "Can't tell which version this is".to_string())
        } else if scans.iter().any(|(_, st)| st == "unsafe" || st == "suspicious" || st == "caution") || overall == "unsafe" {
            (false, "A scanner flagged this file".to_string())
        } else if gated {
            (false, "Gated (see Access above)".to_string())
        } else {
            (true, String::new())
        };
        files.push(VetFile { path: path.into(), tag: tag.unwrap_or_default(), size: f["size"].as_u64().unwrap_or(0), scans, overall, pullable, why_not });
    }
    files.sort_by_key(|f| f.size);
    if files.iter().any(|f| f.why_not == "A scanner flagged this file") {
        checks.push(check("Scans", "bad", "Hugging Face's scanners flagged at least one file in this repo. Don't download it."));
    } else if files.iter().any(|f| f.pullable && f.scans.is_empty()) {
        checks.push(check("Scans", "warn", "Some files haven't been scanned by Hugging Face yet. Bench still checks the file after download."));
    }

    Ok(Vet {
        id: repo,
        author,
        base_model,
        license: card["license"].as_str().unwrap_or("not stated").into(),
        downloads: info["downloads"].as_u64().unwrap_or(0),
        likes: info["likes"].as_u64().unwrap_or(0),
        gated,
        checks,
        files,
        commits: cs,
    })
}

// ---------- template provenance (after download) ----------

/// The official chat template of `repo` (from chat_template.jinja or tokenizer_config.json).
fn repo_template(repo: &str) -> Option<String> {
    if !valid_repo(repo) {
        return None;
    }
    // hf_text only returns a body for a successful response, so a missing file is None.
    if let Some(t) = hf_text(&format!("/{repo}/resolve/main/chat_template.jinja")).filter(|t| t.contains("{%") || t.contains("{{")) {
        return Some(t);
    }
    let tc = hf_get(&format!("/{repo}/resolve/main/tokenizer_config.json")).ok()?;
    tc["chat_template"].as_str().map(String::from)
}

fn base_models(repo: &str) -> Vec<String> {
    let Ok(info) = hf_get(&format!("/api/models/{repo}")) else { return vec![] };
    match &info["cardData"]["base_model"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    }
}

/// Lines in `a` that aren't in `b` plus the reverse (a rough size of the difference).
fn line_diff(a: &str, b: &str) -> usize {
    use std::collections::HashMap;
    let mut m: HashMap<&str, i64> = HashMap::new();
    for l in a.lines().map(str::trim).filter(|l| !l.is_empty()) {
        *m.entry(l).or_default() += 1;
    }
    for l in b.lines().map(str::trim).filter(|l| !l.is_empty()) {
        *m.entry(l).or_default() -= 1;
    }
    m.values().map(|v| v.unsigned_abs() as usize).sum()
}

/// Compare a downloaded model's template with the models it says it's made from
/// (up to two steps back, e.g. GGUF repo -> abliterated model -> Google's original).
pub fn template_provenance(model: &str, template: &str) -> Check {
    let Some(rest) = model.strip_prefix("hf.co/") else {
        return check("Template source", "info", "From Ollama's library: Ollama supplies its own template.");
    };
    let repo = rest.split(':').next().unwrap_or(rest);
    if template.trim().is_empty() {
        return check("Template source", "info", "The file has no chat template of its own.");
    }
    let mut chain = base_models(repo);
    if let Some(first) = chain.first().cloned() {
        chain.extend(base_models(&first));
    }
    let mut closest: Option<(String, usize)> = None;
    for b in chain.iter().take(4) {
        if let Some(t) = repo_template(b) {
            let d = line_diff(template, &t);
            if d == 0 {
                return check("Template source", "ok", format!("Identical to the template of {b}, the model it's made from."));
            }
            if closest.as_ref().map(|(_, cd)| d < *cd).unwrap_or(true) {
                closest = Some((b.clone(), d));
            }
        }
    }
    match closest {
        Some((b, d)) => check("Template source", "warn", format!("Differs from {b}'s template in about {d} lines. No red flags were found in it, but it isn't an exact copy (often just an older or newer version).")),
        None => check("Template source", "info", "Couldn't find the original model's template on Hugging Face to compare."),
    }
}

// ---------- download ----------

#[derive(Serialize, Clone)]
struct PullProgress {
    name: String,
    status: String,
    completed: u64,
    total: u64,
}

#[derive(Serialize, Clone)]
struct PullDone {
    name: String,
    ok: bool,
    error: String,
}

#[tauri::command]
pub fn model_pull<R: tauri::Runtime>(app: AppHandle<R>, repo: String, tag: String) -> Result<(), String> {
    if !valid_repo(&repo) || tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("Not a downloadable model.".into());
    }
    if PULLING.swap(true, Ordering::SeqCst) {
        return Err("Another download is running.".into());
    }
    CANCEL.store(false, Ordering::SeqCst);
    let name = format!("hf.co/{repo}:{tag}");
    std::thread::spawn(move || {
        let res = (|| -> Result<(), String> {
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(600)).build();
            let r = agent.post("http://127.0.0.1:11434/api/pull").send_json(json!({"model": name, "stream": true})).map_err(|e| e.to_string())?;
            for line in BufReader::new(r.into_reader()).lines() {
                if CANCEL.load(Ordering::SeqCst) {
                    return Err("Cancelled".into());
                }
                let line = line.map_err(|e| e.to_string())?;
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                if let Some(e) = v["error"].as_str() {
                    return Err(e.into());
                }
                let _ = app.emit("pull-progress", PullProgress {
                    name: name.clone(),
                    status: v["status"].as_str().unwrap_or("").into(),
                    completed: v["completed"].as_u64().unwrap_or(0),
                    total: v["total"].as_u64().unwrap_or(0),
                });
            }
            Ok(())
        })();
        PULLING.store(false, Ordering::SeqCst);
        let _ = app.emit("pull-done", PullDone { name, ok: res.is_ok(), error: res.err().unwrap_or_default() });
    });
    Ok(())
}

#[tauri::command]
pub fn model_pull_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

/// Delete a downloaded model from Ollama.
#[tauri::command(async)]
pub fn model_delete(name: String) -> Result<(), String> {
    if name.is_empty() || name.len() > 200 || name.chars().any(|c| c.is_whitespace()) {
        return Err("Not a model name.".into());
    }
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).build();
    agent.delete("http://127.0.0.1:11434/api/delete").send_json(json!({"model": name})).map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quant_tags_and_repo_names() {
        assert_eq!(quant_tag("gemma-4-12B-it-uncensored-Q4_K_M.gguf").as_deref(), Some("Q4_K_M"));
        assert_eq!(quant_tag("Qwen3.8-27B-Uncensored-IQ4_XS.gguf").as_deref(), Some("IQ4_XS"));
        assert_eq!(quant_tag("model.Q8_0.gguf").as_deref(), Some("Q8_0"));
        assert_eq!(quant_tag("x-BF16.gguf").as_deref(), Some("BF16"));
        assert_eq!(quant_tag("mmproj-model-f16.gguf").as_deref(), Some("F16"));
        assert!(quant_tag("README.md").is_none());
        assert!(valid_repo("TrevorJS/gemma-4-12B-it-uncensored-GGUF"));
        assert!(!valid_repo("../etc/passwd"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("a/b?x=1"));
        assert_eq!(line_diff("a\nb\nc", "a\nb\nc"), 0);
        assert_eq!(line_diff("a\nb", "a\nc"), 2);
        assert!(days_since("2020-01-01T00:00:00Z").unwrap() > 2000);
    }
}

#[cfg(test)]
mod live {
    // Hits Hugging Face: cargo test -- --ignored live_vet --nocapture
    #[test]
    #[ignore]
    fn live_vet() {
        for r in ["TrevorJS/gemma-4-12B-it-uncensored-GGUF", "llmfan46/gemma-4-E4B-it-ultra-uncensored-heretic-GGUF", "orcarouter/Qwen3.8-27B-Uncensored-GGUF"] {
            let v = serde_json::to_value(super::hf_vet(r.into()).unwrap()).unwrap();
            println!("\n== {r} gated={} base={}", v["gated"], v["base_model"]);
            for c in v["checks"].as_array().unwrap() { println!("  [{}] {}: {}", c["status"].as_str().unwrap(), c["name"].as_str().unwrap(), c["detail"].as_str().unwrap()); }
            for f in v["files"].as_array().unwrap().iter().take(4) { println!("  file {} tag={} {:.1}GB pullable={} {} scans={}", f["path"], f["tag"], f["size"].as_f64().unwrap() / 1e9, f["pullable"], f["why_not"], f["scans"]); }
        }
        let c = super::template_provenance("hf.co/TrevorJS/gemma-4-12B-it-uncensored-GGUF:Q4_K_M", &std::fs::read_to_string(std::env::var("TPL12").unwrap()).unwrap());
        println!("\nprovenance 12B: [{}] {}", c.status, c.detail);
        let c = super::template_provenance("hf.co/llmfan46/gemma-4-E4B-it-ultra-uncensored-heretic-GGUF:Q4_K_M", &std::fs::read_to_string(std::env::var("TPLE4").unwrap()).unwrap());
        println!("provenance E4B: [{}] {}", c.status, c.detail);
    }
}
