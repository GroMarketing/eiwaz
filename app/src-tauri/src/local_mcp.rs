//! "Do it on this Mac": a tiny MCP server that Claude Code starts (via
//! `--mcp-config`) so it can hand the bulky, read-only parts of a job to a
//! local model: reading and summarizing files, and digesting long output such
//! as logs and test results. Claude Code then gets a short brief instead of the
//! raw text, which is where the cloud tokens go.
//!
//! Safety: it only reads files inside the project folder, never secrets (.env,
//! keys, .ssh, .git), and never runs commands. The one thing it may write is
//! prose (write_draft): emails, copy and docs, into text-type files inside the
//! project, never code. Eiwaz's save points (Changes) let you undo any of it.
//!
//! Each call is appended to a log (one JSON object per line) that Eiwaz reads
//! to show progress in the status bar and to count the savings.
//!
//! Run: eiwaz --local-mcp <project> <models> <log file>
//! <models> is either one model for everything, or per job category:
//! "summarize=<model>,digest=<model>,write=<model>". A category with no model
//! isn't offered to Claude Code, so it does that work itself.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_CHARS: usize = 150_000; // what one local call reads, about 40K tokens
const MAX_FILES: usize = 40;

/// Paths that must never leave the user's hands, even to a local model.
pub fn is_secret(rel: &Path) -> bool {
    rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy().to_lowercase();
        s.starts_with(".env") || s == ".ssh" || s == ".git" || s == ".aws" || s == ".gnupg" || s.ends_with(".pem") || s.ends_with(".key")
            || s.ends_with(".p12") || s.starts_with("id_rsa") || s.starts_with("id_ed25519") || s.contains("secret") || s.contains("credential")
    })
}

fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || ["node_modules", "target", "dist", "build", "vendor", "__pycache__"].contains(&name)
}

/// Resolve a path the agent gave us to a file or folder inside the project.
fn inside(root: &Path, p: &str) -> Result<PathBuf, String> {
    let joined = if Path::new(p).is_absolute() { PathBuf::from(p) } else { root.join(p) };
    let real = joined.canonicalize().map_err(|_| format!("{p}: not found"))?;
    let rel = real.strip_prefix(root).map_err(|_| format!("{p}: outside the project, so it wasn't read"))?;
    if is_secret(rel) {
        return Err(format!("{p}: looks like secrets, so it wasn't read"));
    }
    Ok(real)
}

fn collect(root: &Path, dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 || out.len() >= MAX_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let rel = p.strip_prefix(root).unwrap_or(&p);
        if is_secret(rel) {
            continue;
        }
        if p.is_dir() {
            if !skip_dir(&name) {
                collect(root, &p, depth + 1, out);
            }
        } else {
            out.push(p);
        }
    }
}

/// Read the requested files as text. Returns (text with file headers, files read, notes).
pub fn read_files(root: &Path, paths: &[String]) -> (String, Vec<String>, Vec<String>) {
    let mut files = vec![];
    let mut notes = vec![];
    for p in paths.iter().take(MAX_FILES) {
        match inside(root, p) {
            Ok(real) if real.is_dir() => collect(root, &real, 0, &mut files),
            Ok(real) => files.push(real),
            Err(e) => notes.push(e),
        }
    }
    let mut text = String::new();
    let mut read = vec![];
    for f in files {
        let rel = f.strip_prefix(root).unwrap_or(&f).to_string_lossy().to_string();
        let Ok(bytes) = std::fs::read(&f) else { continue };
        if bytes.contains(&0) {
            notes.push(format!("{rel}: not a text file, skipped"));
            continue;
        }
        let body = String::from_utf8_lossy(&bytes);
        if text.len() + body.len() > MAX_CHARS {
            let room = MAX_CHARS.saturating_sub(text.len());
            if room > 2000 {
                let cut: String = body.chars().take(room).collect();
                text.push_str(&format!("\n===== {rel} (cut short) =====\n{cut}\n"));
                read.push(rel.clone());
            }
            notes.push(format!("Stopped at about {} characters; the rest wasn't read.", MAX_CHARS));
            break;
        }
        text.push_str(&format!("\n===== {rel} =====\n{body}\n"));
        read.push(rel);
    }
    (text, read, notes)
}

/// File types write_draft may create or replace: prose, never code.
const PROSE: [&str; 9] = ["md", "markdown", "txt", "html", "htm", "mjml", "eml", "csv", "rtf"];

/// Where write_draft may write: inside the project, not secrets, a prose file type.
pub fn draft_target(root: &Path, p: &str) -> Result<PathBuf, String> {
    let rel = Path::new(p);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("{p}: give a path inside the project"));
    }
    if is_secret(rel) {
        return Err(format!("{p}: looks like secrets, so it wasn't written"));
    }
    let ext = rel.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if !PROSE.contains(&ext.as_str()) {
        return Err(format!("{p}: write_draft only writes prose files ({}). Write code yourself.", PROSE.join(", ")));
    }
    let full = root.join(rel);
    // The folder it goes in must already be inside the project (no symlink escapes).
    let parent = full.parent().ok_or("bad path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let real_parent = parent.canonicalize().map_err(|e| e.to_string())?;
    if !real_parent.starts_with(root) {
        return Err(format!("{p}: outside the project, so it wasn't written"));
    }
    if full.is_symlink() {
        return Err(format!("{p}: is a link, so it wasn't written"));
    }
    Ok(full)
}

/// Which model does each category; a single name means all three.
pub fn parse_models(spec: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    if !spec.contains('=') {
        for c in ["summarize", "digest", "write"] {
            if !spec.is_empty() { m.insert(c.to_string(), spec.to_string()); }
        }
        return m;
    }
    for part in spec.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            if !v.trim().is_empty() { m.insert(k.trim().to_string(), v.trim().to_string()); }
        }
    }
    m
}

fn tool_category(name: &str) -> &str {
    match name { "read_and_summarize" => "summarize", "digest_output" => "digest", "write_draft" => "write", _ => "" }
}

fn tools_for(models: &std::collections::HashMap<String, String>) -> Value {
    let all = tools();
    Value::Array(all.as_array().cloned().unwrap_or_default().into_iter().filter(|t| models.contains_key(tool_category(t["name"].as_str().unwrap_or("")))).collect())
}

fn tools() -> Value {
    json!([
        {
            "name": "write_draft",
            "description": "Write prose ON THIS MAC with a local model, straight into a file: emails and email sequences, newsletters, landing-page and ad copy, posts, docs, READMEs, plans. You get back only the path and a short outline, so the text never enters your context. Give a precise brief (audience, goal, tone, length, the exact facts, names, links and offers to include) and reference files whose voice and facts to follow. Only prose file types (.md .txt .html .mjml .eml .csv .rtf) inside the project; never code. Set replace=true to rewrite an existing file.",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string", "description": "File to write, relative to the project root"},
                "brief": {"type": "string", "description": "What to write: audience, goal, tone, length, structure, must-include facts"},
                "reference_paths": {"type": "array", "items": {"type": "string"}, "description": "Files to match for voice and facts (brand guide, earlier emails, product notes)"},
                "replace": {"type": "boolean", "description": "Rewrite the file if it already exists (default false)"}
            }, "required": ["path", "brief"]}
        },
        {
            "name": "read_and_summarize",
            "description": "Read files or folders in this project ON THIS MAC with a local model and return a compact brief instead of their full text. Use it before reading large files, many files, or whole folders yourself, to save context. The brief keeps exact names (functions, variables, routes, file paths) and the parts relevant to `focus`. Read-only; it can't see .env or other secrets. If you need exact lines to edit, read just those lines yourself afterwards.",
            "inputSchema": {"type": "object", "properties": {
                "paths": {"type": "array", "items": {"type": "string"}, "description": "Files or folders, relative to the project root"},
                "focus": {"type": "string", "description": "What you need to know from them, e.g. 'how prices are formatted'"}
            }, "required": ["paths", "focus"]}
        },
        {
            "name": "digest_output",
            "description": "Digest long text ON THIS MAC with a local model: a test run, build log, stack trace or command output. Returns the few lines that matter (errors, failing tests, file:line locations) and a short explanation. Use it instead of reading long output yourself.",
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string", "description": "The long output"},
                "question": {"type": "string", "description": "What you want to know, e.g. 'which tests fail and why'"}
            }, "required": ["text", "question"]}
        }
    ])
}

const SUMMARIZE: &str = "You prepare briefs for a coding agent that will edit this code. From the files below, report only what matters for the agent's focus. Keep exact identifiers: file paths, function and variable names, routes, config keys, and short code snippets that are directly relevant (quote them exactly, with the file name). Mention where things are (file and roughly which function). Skip boilerplate. Do not invent anything; if the files don't answer the focus, say so. Be compact: bullet points, no introduction.";
const WRITE: &str = "You are a careful copywriter. Write exactly what the brief asks for, ready to use, in the voice of the reference material. Use only facts, names, numbers, links and offers that appear in the brief or the references; never invent any. Where a needed fact is missing, write [CHECK: what's missing] instead. Match the requested format (for HTML, write clean simple HTML; for email, start with a 'Subject:' line, then a 'Preview:' line). Output only the finished text, with no notes before or after.";
const DIGEST: &str = "You digest long program output for a coding agent. Report: what failed or errored (quote the exact error lines and any file:line locations), the most likely cause, and anything else the agent's question asks. Ignore noise and passing items. Do not invent anything. Be compact: bullet points.";

fn log(path: &str, entry: Value) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{entry}");
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn call(root: &Path, model: &str, logfile: &str, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "read_and_summarize" => {
            let paths: Vec<String> = args["paths"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let focus = args["focus"].as_str().unwrap_or("what these files do");
            if paths.is_empty() {
                return Err("Give at least one path.".into());
            }
            let (text, read, notes) = read_files(root, &paths);
            if read.is_empty() {
                return Err(format!("Nothing was read. {}", notes.join(" ")));
            }
            let r = crate::bench::local_run_n(model.into(), SUMMARIZE.into(), format!("Focus: {focus}\n\nFiles:\n{text}"), 2500)?;
            log(logfile, json!({"ts": now(), "project": root, "tool": name, "model": model, "read_tokens": r.prompt_tokens, "returned_tokens": r.output_tokens, "files": read, "seconds": r.seconds, "focus": focus}));
            Ok(format!("{}\n\n[Read on this Mac by {model}: {} file(s), about {} tokens, summarized to about {}.{}{}]",
                r.text, read.len(), r.prompt_tokens, r.output_tokens,
                if notes.is_empty() { "" } else { " Notes: " }, notes.join(" ")))
        }
        "write_draft" => {
            let path = args["path"].as_str().unwrap_or("");
            let brief = args["brief"].as_str().unwrap_or("");
            if path.is_empty() || brief.trim().len() < 20 {
                return Err("Give a path and a real brief (audience, goal, tone, what to include).".into());
            }
            let target = draft_target(root, path)?;
            let replace = args["replace"].as_bool().unwrap_or(false);
            if target.exists() && !replace {
                return Err(format!("{path} already exists. Pass replace=true to rewrite it, or pick a new path."));
            }
            let refs: Vec<String> = args["reference_paths"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let (ref_text, ref_read, notes) = if refs.is_empty() { (String::new(), vec![], vec![]) } else { read_files(root, &refs) };
            let fmt = Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            let prompt = format!("Brief:\n{brief}\n\nFormat: {}. Do not include the file name.\n{}", if fmt == "md" || fmt == "markdown" { "Markdown" } else { fmt.as_str() }, if ref_text.is_empty() { String::new() } else { format!("\nReference material:\n{ref_text}") });
            let r = crate::bench::local_run_n(model.into(), WRITE.into(), prompt, 6000)?;
            let mut text = r.text.trim().to_string();
            // Models sometimes wrap the answer in a code fence; files shouldn't have one.
            if text.starts_with("```") {
                text = text.lines().skip(1).collect::<Vec<_>>().join("\n");
                if let Some(i) = text.rfind("```") { text.truncate(i); }
                text = text.trim().to_string();
            }
            // Drop a first line that just repeats the file name.
            if text.lines().next().map(|l| l.trim().trim_matches(['`', '*', '#', ' ']) == path || l.trim().ends_with(Path::new(path).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default().as_str())).unwrap_or(false) {
                text = text.lines().skip(1).collect::<Vec<_>>().join("\n").trim().to_string();
            }
            if text.len() < 40 {
                return Err("The local model didn't produce a usable draft. Write this one yourself.".into());
            }
            std::fs::write(&target, format!("{text}\n")).map_err(|e| e.to_string())?;
            let words = text.split_whitespace().count();
            let checks = text.matches("[CHECK").count();
            let outline: Vec<String> = text.lines().map(str::trim).filter(|l| l.starts_with('#') || l.to_lowercase().starts_with("subject:") || l.starts_with("<h1") || l.starts_with("<h2")).take(8).map(|l| l.chars().take(100).collect()).collect();
            log(logfile, json!({"ts": now(), "project": root, "tool": name, "model": model, "read_tokens": r.prompt_tokens, "returned_tokens": 60, "written_tokens": r.output_tokens, "files": [path], "seconds": r.seconds, "focus": brief.chars().take(120).collect::<String>()}));
            Ok(format!("Wrote {path} on this Mac ({words} words, about {} tokens you didn't have to write).{}\nOutline:\n{}\n{}\nDon't read it back in full. To check facts or tone, call read_and_summarize on it with that focus, then fix small things with Edit.{}",
                r.output_tokens,
                if checks > 0 { format!(" It left {checks} [CHECK: …] marker(s) where facts were missing: fill those in.") } else { String::new() },
                if outline.is_empty() { "(no headings)".to_string() } else { outline.join("\n") },
                if ref_read.is_empty() { String::new() } else { format!("Matched the voice of: {}.", ref_read.join(", ")) },
                if notes.is_empty() { String::new() } else { format!(" Notes: {}", notes.join(" ")) }))
        }
        "digest_output" => {
            let text = args["text"].as_str().unwrap_or("");
            let q = args["question"].as_str().unwrap_or("what went wrong");
            if text.trim().is_empty() {
                return Err("No text given.".into());
            }
            let body: String = text.chars().take(MAX_CHARS).collect();
            let r = crate::bench::local_run_n(model.into(), DIGEST.into(), format!("Question: {q}\n\nOutput:\n{body}"), 1500)?;
            log(logfile, json!({"ts": now(), "project": root, "tool": name, "model": model, "read_tokens": r.prompt_tokens, "returned_tokens": r.output_tokens, "files": [], "seconds": r.seconds, "focus": q}));
            Ok(format!("{}\n\n[Digested on this Mac by {model}: about {} tokens down to about {}.]", r.text, r.prompt_tokens, r.output_tokens))
        }
        _ => Err(format!("Unknown tool {name}")),
    }
}

/// The JSON-RPC loop over stdin/stdout.
pub fn serve(project: &str, spec: &str, logfile: &str) {
    let models = parse_models(spec);
    let root = match Path::new(project).canonicalize() {
        Ok(r) => r,
        Err(_) => return,
    };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications need no reply
        let method = msg["method"].as_str().unwrap_or("");
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "eiwaz-local", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "These tools run on the user's Mac with a local model, at no cloud cost. Use them for the bulky read-only parts of a task."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools_for(&models)})),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let args = &msg["params"]["arguments"];
                let model = models.get(tool_category(name)).map(String::as_str).unwrap_or("");
                Ok(if model.is_empty() {
                    json!({"content": [{"type": "text", "text": "No local model is reliable enough for this job on this Mac. Do it yourself."}], "isError": true})
                } else { match call(&root, model, logfile, name, args) {
                    Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                    Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
                }})
            }
            _ => Err(json!({"code": -32601, "message": format!("Unknown method {method}")})),
        };
        let reply = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_inside_the_project_but_never_secrets() {
        let dir = std::env::temp_dir().join(format!("eiwaz-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("src/price.js"), "export const fmt = (n) => n.toString();\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(dir.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(dir.join("logo.png"), [0x89u8, b'P', b'N', b'G', 0, 0]).unwrap();
        let root = dir.canonicalize().unwrap();
        let (text, read, notes) = read_files(&root, &["src/price.js".into(), ".env".into(), "../../etc/passwd".into()]);
        assert_eq!(read, vec!["src/price.js"]);
        assert!(text.contains("fmt"));
        assert!(notes.iter().any(|n| n.contains("secrets")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("outside") || n.contains("not found")), "{notes:?}");
        // a whole folder: skips .env, .git and binaries
        let (text, read, _) = read_files(&root, &[".".into()]);
        assert!(read.contains(&"src/price.js".to_string()));
        assert!(!text.contains("SECRET") && !read.iter().any(|r| r.contains(".git") || r.contains(".env") || r.contains("logo")), "{read:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn routes_each_category_to_its_model() {
        let m = parse_models("summarize=gemma4:latest,write=hf.co/x/y:Q4_K_M");
        assert_eq!(m.get("summarize").map(String::as_str), Some("gemma4:latest"));
        assert_eq!(m.get("write").map(String::as_str), Some("hf.co/x/y:Q4_K_M"));
        assert!(m.get("digest").is_none());
        let names: Vec<String> = tools_for(&m).as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert!(names.contains(&"read_and_summarize".into()) && names.contains(&"write_draft".into()) && !names.contains(&"digest_output".into()));
        assert_eq!(parse_models("gemma4:latest").len(), 3);
    }

    #[test]
    fn drafts_only_prose_inside_the_project() {
        let dir = std::env::temp_dir().join(format!("eiwaz-draft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.canonicalize().unwrap();
        assert!(draft_target(&root, "emails/welcome-01.md").is_ok());
        assert!(draft_target(&root, "emails/welcome.html").is_ok());
        assert!(draft_target(&root, "src/app.ts").unwrap_err().contains("prose"));
        assert!(draft_target(&root, "../outside.md").is_err());
        assert!(draft_target(&root, "/etc/x.md").is_err());
        assert!(draft_target(&root, ".env.md").unwrap_err().contains("secrets"));
        assert!(draft_target(&root, "notes/credentials.txt").unwrap_err().contains("secrets"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
