//! Routing log: what Eiwaz decided when it sent a message to Claude Code, joined
//! with what Claude Code's own session log says happened afterwards.
//!
//! Eiwaz starts each Claude Code session with `--session-id <uuid>`, so it knows
//! which log file belongs to which terminal. For every message it sends, it
//! appends one line to routes.jsonl (time, project, session, estimate, split
//! setting, plan; never the message text). `route_report` reads Claude Code's
//! log (~/.claude/projects/<project>/<session>.jsonl and its subagents) and
//! reports, per message: turns, cloud input, tool output, local steps, context
//! size, and whether Claude wrote its Split lines.
//!
//! Everything stays on this Mac: it only reads files that are already on disk.

use crate::store::data_dir;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use tauri::AppHandle;

fn routes_path<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("routes.jsonl"))
}

#[tauri::command]
pub fn route_log<R: tauri::Runtime>(app: AppHandle<R>, entry: Value) -> Result<(), String> {
    if !entry.is_object() {
        return Err("Not a route entry".into());
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(routes_path(&app)?).map_err(|e| e.to_string())?;
    writeln!(f, "{entry}").map_err(|e| e.to_string())
}

/// Claude Code's folder name for a project: every character that isn't a letter or digit becomes '-'.
pub fn claude_dir(project: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let enc: String = project.trim_end_matches('/').chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    Some(Path::new(&home).join(".claude/projects").join(enc))
}

/// "2026-11-21T21:32:46.391Z" -> milliseconds since 1970.
pub fn iso_ms(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = rest.trim_end_matches('Z');
    let mut t = time.split(':');
    let (hh, mm) = (t.next()?.parse::<i64>().ok()?, t.next()?.parse::<i64>().ok()?);
    let secs: f64 = t.next().unwrap_or("0").parse().ok()?;
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    Some(((days * 86400 + hh * 3600 + mm * 60) as f64 * 1000.0 + secs * 1000.0) as i64)
}

#[derive(Serialize, Default, Clone)]
pub struct Msg {
    pub ts: i64,
    pub prompt_id: String,
    pub preview: String,
    pub words: usize,
    pub turns: u64,
    pub input: u64,          // all input tokens processed (fresh + cache writes + cache reads)
    pub output: u64,
    pub context_end: u64,    // context size on the last turn
    pub tool_tokens: u64,    // tool output that entered the conversation
    pub tool_calls: u64,
    pub local_calls: u64,    // eiwaz tool calls
    pub local_read: u64,     // tokens the local model read
    pub subagents: u64,
    pub split_line: bool,
    pub split_result: bool,
}

fn text_of(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            if a.iter().any(|c| c["type"] == "tool_result") {
                return None;
            }
            let t: Vec<&str> = a.iter().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
            (!t.is_empty()).then(|| t.join(" "))
        }
        _ => None,
    }
}

fn result_tokens(c: &Value) -> u64 {
    let body = match &c["content"] {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).map(str::len).sum(),
        _ => 0,
    };
    (body / 4) as u64
}

/// Add one log line's usage and tool activity to `m`.
fn tally(m: &mut Msg, e: &Value, ids: &mut HashMap<String, String>) {
    let msg = &e["message"];
    match e["type"].as_str() {
        Some("assistant") => {
            let u = &msg["usage"];
            if u.is_object() {
                let inp = u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0) + u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                m.turns += 1;
                m.input += inp;
                m.output += u["output_tokens"].as_u64().unwrap_or(0);
                m.context_end = inp;
            }
            for c in msg["content"].as_array().into_iter().flatten() {
                if c["type"] == "tool_use" {
                    let name = c["name"].as_str().unwrap_or("").to_string();
                    if name.starts_with("mcp__eiwaz__") || name.starts_with("mcp__plado__") {
                        m.local_calls += 1;
                    } else {
                        m.tool_calls += 1;
                    }
                    if name == "Agent" || name == "Task" {
                        m.subagents += 1;
                    }
                    ids.insert(c["id"].as_str().unwrap_or("").to_string(), name);
                } else if c["type"] == "text" {
                    let t = c["text"].as_str().unwrap_or("");
                    if t.contains("Split ·") || t.contains("Split:") {
                        m.split_line = true;
                    }
                    if t.contains("Split result") {
                        m.split_result = true;
                    }
                }
            }
        }
        Some("user") => {
            for c in msg["content"].as_array().into_iter().flatten() {
                if c["type"] == "tool_result" {
                    let n = ids.get(c["tool_use_id"].as_str().unwrap_or("")).cloned().unwrap_or_default();
                    let tok = result_tokens(c);
                    if n.starts_with("mcp__eiwaz__") || n.starts_with("mcp__plado__") {
                        // the brief that came back; what was read locally is in the brief's footer
                        let txt = c["content"].to_string();
                        if let Some(i) = txt.find("about ") {
                            m.local_read += txt[i + 6..].split(|ch: char| !ch.is_ascii_digit()).next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
                        }
                    }
                    m.tool_tokens += tok;
                }
            }
        }
        _ => {}
    }
}

/// Per-message activity in one Claude Code session.
pub fn session_messages(project: &str, session: &str) -> Vec<Msg> {
    match claude_dir(project) {
        Some(dir) => session_messages_in(&dir, session),
        None => vec![],
    }
}

fn session_messages_in(dir: &Path, session: &str) -> Vec<Msg> {
    let main = dir.join(format!("{session}.jsonl"));
    let Ok(f) = std::fs::File::open(&main) else { return vec![] };
    let mut out: Vec<Msg> = vec![];
    let mut by_prompt: HashMap<String, usize> = HashMap::new();
    let mut ids = HashMap::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(e) = serde_json::from_str::<Value>(&line) else { continue };
        if e["type"] == "user" && !e["isMeta"].as_bool().unwrap_or(false) && !e["isSidechain"].as_bool().unwrap_or(false) {
            if let Some(text) = text_of(&e["message"]["content"]) {
                if !text.trim_start().starts_with('<') {
                    let pid = e["promptId"].as_str().unwrap_or("").to_string();
                    let clean = text.split("\n\n(Eiwaz").next().unwrap_or(&text).split("\n\n(Plado").next().unwrap_or(&text).trim().to_string();
                    by_prompt.insert(pid.clone(), out.len());
                    out.push(Msg {
                        ts: e["timestamp"].as_str().and_then(iso_ms).unwrap_or(0),
                        prompt_id: pid,
                        words: clean.split_whitespace().count(),
                        preview: clean.chars().take(90).collect(),
                        ..Default::default()
                    });
                    continue;
                }
            }
        }
        if let Some(m) = out.last_mut() {
            tally(m, &e, &mut ids);
        }
    }
    // Subagents: each log's first line carries the promptId of the message that started it.
    if let Ok(rd) = std::fs::read_dir(dir.join(session).join("subagents")) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().map(|x| x != "jsonl").unwrap_or(true) {
                continue;
            }
            let Ok(f) = std::fs::File::open(&p) else { continue };
            let mut lines = BufReader::new(f).lines().map_while(Result::ok);
            let Some(first) = lines.next() else { continue };
            let pid = serde_json::from_str::<Value>(&first).ok().and_then(|v| v["promptId"].as_str().map(String::from)).unwrap_or_default();
            let Some(&i) = by_prompt.get(&pid) else { continue };
            let mut sub = Msg::default();
            let mut sids = HashMap::new();
            for line in lines {
                if let Ok(e) = serde_json::from_str::<Value>(&line) {
                    tally(&mut sub, &e, &mut sids);
                }
            }
            let m = &mut out[i];
            m.turns += sub.turns;
            m.input += sub.input;
            m.output += sub.output;
            m.tool_tokens += sub.tool_tokens;
            m.tool_calls += sub.tool_calls;
            m.local_calls += sub.local_calls;
            m.local_read += sub.local_read;
        }
    }
    out
}

#[derive(Serialize)]
pub struct RouteRow {
    route: Value,
    msg: Option<Msg>,
}

/// The last `limit` messages Eiwaz sent to Claude Code in this project, each with what happened.
#[tauri::command(async)]
pub fn route_report<R: tauri::Runtime>(app: AppHandle<R>, project: String, limit: usize) -> Result<Vec<RouteRow>, String> {
    let Ok(f) = std::fs::File::open(routes_path(&app)?) else { return Ok(vec![]) };
    let routes: Vec<Value> = BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|r| r["project"].as_str() == Some(project.as_str()) && r["session_id"].as_str().map(|s| !s.is_empty()).unwrap_or(false))
        .collect();
    let routes: Vec<Value> = routes.into_iter().rev().take(limit.clamp(1, 200)).collect();
    let mut cache: HashMap<String, Vec<Msg>> = HashMap::new();
    let mut used: std::collections::HashSet<String> = Default::default();
    let mut out = vec![];
    for r in routes.into_iter().rev() {
        let sid = r["session_id"].as_str().unwrap_or("").to_string();
        let msgs = cache.entry(sid.clone()).or_insert_with(|| session_messages(&project, &sid));
        let ts = r["ts"].as_i64().unwrap_or(0);
        // the first message Claude Code logged at or after the send (allow a little clock slack)
        let m = msgs.iter().find(|m| m.ts >= ts - 5000 && !used.contains(&m.prompt_id)).cloned();
        if let Some(m) = &m {
            used.insert(m.prompt_id.clone());
        }
        out.push(RouteRow { route: r, msg: m });
    }
    out.reverse();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_times_and_project_folders() {
        assert_eq!(iso_ms("1970-01-01T00:00:01.500Z"), Some(1500));
        assert_eq!(iso_ms("2026-11-21T21:32:46.391Z").map(|v| v / 1000), Some(1795296766));
        let d = claude_dir("/Users/x/Desktop/My Project").unwrap();
        assert!(d.ends_with("-Users-x-Desktop-My-Project"));
    }

    #[test]
    fn joins_a_session_log() {
        let home = std::env::temp_dir().join(format!("eiwaz-routes-{}", std::process::id()));
        let project = "/tmp/demo proj";
        let dir = home.join(".claude/projects/-tmp-demo-proj");
        std::fs::create_dir_all(dir.join("s1/subagents")).unwrap();
        let lines = [
            r#"{"type":"user","promptId":"p1","timestamp":"2026-11-21T10:00:00.000Z","message":{"content":"fix the price\n\n(Eiwaz split is on: start with your Split line.)"}}"#,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":1000,"cache_read_input_tokens":69000,"output_tokens":50},"content":[{"type":"text","text":"Split · Mac: read menu.js · Cloud: edit"},{"type":"tool_use","id":"t1","name":"mcp__eiwaz__read_and_summarize"}]}}"#,
            r#"{"type":"user","promptId":"p1","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"brief [Read on this Mac by gemma4:latest: 1 file(s), about 8000 tokens, summarized to about 300.]"}]}}"#,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":70500,"output_tokens":20},"content":[{"type":"tool_use","id":"t2","name":"Agent"},{"type":"text","text":"Split result · Mac: read · Cloud: edit"}]}}"#,
            r#"{"type":"user","promptId":"p2","timestamp":"2026-11-21T10:05:00.000Z","message":{"content":"thanks"}}"#,
        ];
        std::fs::write(dir.join("s1.jsonl"), lines.join("\n")).unwrap();
        std::fs::write(dir.join("s1/subagents/agent-a.jsonl"), [
            r#"{"type":"user","promptId":"p1","isSidechain":true,"message":{"content":"sub task"}}"#,
            r#"{"type":"assistant","message":{"usage":{"cache_read_input_tokens":20000,"output_tokens":5},"content":[]}}"#,
        ].join("\n")).unwrap();
        let _ = project;
        let ms = session_messages_in(&dir, "s1");
        assert_eq!(ms.len(), 2);
        let m = &ms[0];
        assert_eq!(m.words, 3, "Eiwaz's note isn't counted: {:?}", m.preview);
        assert_eq!(m.turns, 3);
        assert_eq!(m.input, 70010 + 70505 + 20000);
        assert_eq!(m.local_calls, 1);
        assert_eq!(m.local_read, 8000);
        assert_eq!(m.subagents, 1);
        assert!(m.split_line && m.split_result);
        assert_eq!(m.context_end, 70505);
        let _ = std::fs::remove_dir_all(home);
    }

    // cargo test -- --ignored live_session --nocapture   (EIWAZ_SESSION=<id> EIWAZ_PROJECT=<path>)
    #[test]
    #[ignore]
    fn live_session() {
        let ms = session_messages(&std::env::var("EIWAZ_PROJECT").unwrap(), &std::env::var("EIWAZ_SESSION").unwrap());
        for m in ms.iter().take(12) {
            println!("{:3}w turns={:4} input={:7.2}M ctx_end={:4}K tools={:3} out={:5}K local={} sub={} split={}/{}  {:?}", m.words, m.turns, m.input as f64 / 1e6, m.context_end / 1000, m.tool_calls, m.tool_tokens / 1000, m.local_calls, m.subagents, m.split_line, m.split_result, m.preview.chars().take(50).collect::<String>());
        }
        println!("{} messages", ms.len());
    }
}

/// The latest title Claude Code gave this session (it writes and updates one
/// as the work changes), or "". Reads only the end of the log when it's large.
#[tauri::command(async)]
pub fn claude_title(project: String, session_id: String) -> String {
    use std::io::{Read, Seek, SeekFrom};
    if session_id.is_empty() || !session_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return String::new();
    }
    let Some(dir) = claude_dir(&project) else { return String::new() };
    let Ok(mut f) = std::fs::File::open(dir.join(format!("{session_id}.jsonl"))) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut last = String::new();
    // Titles repeat through the log, so the last 512 KB almost always has one.
    for start in [len.saturating_sub(512 * 1024), 0] {
        let _ = f.seek(SeekFrom::Start(start));
        let mut s = String::new();
        let _ = f.read_to_string(&mut s);
        for line in s.lines().filter(|l| l.contains("\"ai-title\"")) {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(t) = v["aiTitle"].as_str() {
                    last = t.to_string();
                }
            }
        }
        if !last.is_empty() || start == 0 {
            break;
        }
    }
    last.chars().take(60).collect()
}
