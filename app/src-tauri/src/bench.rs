//! Model Bench: tests each local Ollama model for what matters to a coding
//! agent, not top speed:
//!   1. Does it fit in this Mac's memory, with room to spare?
//!   2. Working memory: did it actually read the whole prompt? (a code word is
//!      hidden at the very start; the model must repeat it, and Ollama's own
//!      count of prompt tokens read is compared with what was sent)
//!   3. How long until the first reply at realistic prompt sizes
//!   4. Tool calls: does it call the right tool with the right arguments?
//! It unloads other models first, never changes system settings, and stops
//! when the Mac is low on memory or (unless allowed) on battery.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::System;
use tauri::{AppHandle, Emitter};

const OLLAMA: &str = "http://127.0.0.1:11434";
static RUNNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Clone, Default)]
pub struct SizeRun {
    asked: u32,        // tokens we aimed to send
    sent: u64,         // tokens actually sent, by this model's own count of a sample
    read: u64,         // tokens the runtime says it read
    recalled: bool,    // repeated the code word from the start
    first_reply_ms: u64,
    prefill_tps: f64,
}

#[derive(Serialize, Clone, Default)]
pub struct ToolCase {
    prompt: String,
    ok: bool,
    got: String,
}

#[derive(Serialize, Clone, Default)]
pub struct BenchResult {
    model: String,
    runtime: String,
    family: String,
    params: String,
    quant: String,
    file_bytes: u64,
    model_max_ctx: u64,
    requested_ctx: u32,
    reported_ctx: u64, // what Ollama says it loaded (0 = not reported)
    load_ms: u64,
    loaded_bytes: u64,
    gpu_bytes: u64,
    mem_total: u64,
    gen_tps: f64,
    sizes: Vec<SizeRun>,
    tools_supported: bool,
    tool_cases: Vec<ToolCase>,
    checks: Vec<crate::security::Check>,           // file checks, run before loading
    injection: Vec<crate::security::InjectionCase>, // planted-instruction tests
    jobs: Vec<crate::jobs::JobScore>,               // reliability at each local job category
    skipped: String,
    error: String,
    finished_at: u64,
}

#[derive(Serialize, Clone)]
struct Progress {
    model: String,
    step: String,
    done: u32,
    total: u32,
}

fn http(timeout_s: u64) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(3)).timeout(Duration::from_secs(timeout_s)).build()
}

fn post(path: &str, body: Value, timeout_s: u64) -> Result<Value, String> {
    http(timeout_s)
        .post(&format!("{OLLAMA}{path}"))
        .send_json(body)
        .map_err(|e| match e {
            ureq::Error::Status(_, r) => r.into_string().unwrap_or_default(),
            e => e.to_string(),
        })?
        .into_json()
        .map_err(|e| e.to_string())
}

fn get(path: &str) -> Result<Value, String> {
    http(5).get(&format!("{OLLAMA}{path}")).call().map_err(|e| e.to_string())?.into_json().map_err(|e| e.to_string())
}

fn on_battery() -> bool {
    Command::new("pmset").args(["-g", "batt"]).output().map(|o| String::from_utf8_lossy(&o.stdout).contains("'Battery Power'")).unwrap_or(false)
}

/// How much memory the GPU may use. macOS caps it below total memory: the
/// `iogpu.wired_limit_mb` setting when someone has set it, otherwise about 3/4
/// on Macs with more than 36 GB and about 2/3 on smaller ones.
pub fn gpu_limit(total: u64) -> u64 {
    let set_mb = Command::new("sysctl")
        .args(["-n", "iogpu.wired_limit_mb"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
        .unwrap_or(0);
    if set_mb > 0 {
        return (set_mb * 1024 * 1024).min(total);
    }
    if total > 36 * 1024 * 1024 * 1024 { total / 4 * 3 } else { total / 3 * 2 }
}

static CAPS: std::sync::Mutex<BTreeMap<String, Vec<String>>> = std::sync::Mutex::new(BTreeMap::new());

/// A model's capabilities as Ollama reports them ("completion", "tools", "image",
/// "embedding"...). Cached per name, since it only changes if the model is re-pulled.
pub fn capabilities(model: &str) -> Vec<String> {
    if let Some(c) = CAPS.lock().ok().and_then(|m| m.get(model).cloned()) {
        return c;
    }
    let Ok(show) = post("/api/show", json!({"model": model}), 10) else { return vec![] };
    let caps: Vec<String> = show["capabilities"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    if let Ok(mut m) = CAPS.lock() {
        m.insert(model.to_string(), caps.clone());
    }
    caps
}

/// Can this model hold a conversation (and so act as an agent)? Image generators
/// and embedding models can't. Unknown (no capability list) counts as yes.
pub fn can_chat(model: &str) -> bool {
    let caps = capabilities(model);
    caps.is_empty() || caps.iter().any(|c| c == "completion")
}

fn mem() -> (u64, u64) {
    let mut s = System::new();
    s.refresh_memory();
    (s.available_memory(), s.total_memory())
}

fn unload_all() {
    if let Ok(ps) = get("/api/ps") {
        for m in ps["models"].as_array().cloned().unwrap_or_default() {
            if let Some(n) = m["name"].as_str() {
                let _ = post("/api/generate", json!({"model": n, "keep_alive": 0}), 30);
            }
        }
    }
    for _ in 0..40 {
        match get("/api/ps") {
            Ok(ps) if ps["models"].as_array().map(|a| a.is_empty()).unwrap_or(true) => return,
            Err(_) => return,
            _ => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

const WORDS: [&str; 64] = [
    "river", "lantern", "copper", "meadow", "signal", "harbor", "pencil", "orbit", "maple", "canvas", "thunder", "garden", "ledger", "violet", "anchor", "pixel",
    "summit", "quartz", "willow", "beacon", "marble", "falcon", "prairie", "cobalt", "saddle", "glacier", "tunnel", "velvet", "ember", "compass", "harvest", "pebble",
    "window", "spiral", "cotton", "island", "kettle", "mirror", "nectar", "planet", "rocket", "silver", "timber", "upland", "vessel", "walnut", "yonder", "zephyr",
    "budget", "invoice", "server", "module", "branch", "folder", "button", "header", "footer", "layout", "column", "record", "export", "import", "status", "review",
];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize
    }
}

fn filler(chars: usize, rng: &mut Rng) -> String {
    let mut s = String::with_capacity(chars + 64);
    while s.len() < chars {
        let n = 8 + rng.next() % 10;
        for i in 0..n {
            let w = WORDS[rng.next() % WORDS.len()];
            if i == 0 {
                let mut c = w.chars();
                s.push(c.next().unwrap().to_ascii_uppercase());
                s.push_str(c.as_str());
            } else {
                s.push_str(w);
            }
            s.push(if i + 1 == n { '.' } else { ' ' });
        }
        s.push(' ');
    }
    s
}

fn tool_defs() -> Value {
    let f = |name: &str, desc: &str, props: Value, req: Vec<&str>| json!({"type": "function", "function": {"name": name, "description": desc, "parameters": {"type": "object", "properties": props, "required": req}}});
    json!([
        f("read_file", "Read a file in the project and return its contents", json!({"path": {"type": "string", "description": "File path relative to the project"}}), vec!["path"]),
        f("list_files", "List the files in a folder of the project", json!({"directory": {"type": "string", "description": "Folder path relative to the project, '.' for the root"}}), vec!["directory"]),
        f("search_code", "Search the project's code for text", json!({"query": {"type": "string"}}), vec!["query"]),
        f("write_file", "Create or overwrite a file in the project", json!({"path": {"type": "string"}, "content": {"type": "string"}}), vec!["path", "content"]),
        f("run_command", "Run a shell command in the project folder", json!({"command": {"type": "string"}}), vec!["command"]),
    ])
}

// (prompt, expected tool or "" for none, argument key, expected substring)
const CASES: [(&str, &str, &str, &str); 10] = [
    ("Show me what's in src/app.ts", "read_file", "path", "src/app.ts"),
    ("What files are in the components folder?", "list_files", "directory", "components"),
    ("Find every place the function calculateTotal is used.", "search_code", "query", "calculateTotal"),
    ("Create a file called notes.txt that says hello world.", "write_file", "path", "notes.txt"),
    ("Run the tests with npm test.", "run_command", "command", "npm test"),
    ("Open README.md please.", "read_file", "path", "README.md"),
    ("Search the code for TODO comments.", "search_code", "query", "TODO"),
    ("List everything in the project's top folder.", "list_files", "directory", ""),
    ("What is 7 times 8? Just answer, no tools.", "", "", "56"),
    ("Say good morning in French.", "", "", ""),
];

fn run_tool_case(model: &str, think: bool, num_ctx: u32, case: &(&str, &str, &str, &str)) -> ToolCase {
    let (prompt, want, key, sub) = *case;
    let mut body = json!({
        "model": model, "stream": false, "keep_alive": "10m",
        "messages": [
            {"role": "system", "content": "You are a coding agent working in a project folder. Use a tool when the user asks for an action on files or commands. Otherwise answer directly."},
            {"role": "user", "content": prompt}
        ],
        "tools": tool_defs(),
        "options": {"temperature": 0, "num_predict": 300, "num_ctx": num_ctx}
    });
    if think {
        body["think"] = json!(false);
    }
    let r = match post("/api/chat", body, 180) {
        Ok(r) => r,
        Err(e) => return ToolCase { prompt: prompt.into(), ok: false, got: format!("error: {}", e.chars().take(120).collect::<String>()) },
    };
    let calls = r["message"]["tool_calls"].as_array().cloned().unwrap_or_default();
    let content = r["message"]["content"].as_str().unwrap_or("").trim().to_string();
    if want.is_empty() {
        let ok = calls.is_empty() && (sub.is_empty() || content.contains(sub)) && !content.is_empty();
        let got = if calls.is_empty() { format!("answered: {}", content.chars().take(60).collect::<String>()) } else { "called a tool when none was needed".into() };
        return ToolCase { prompt: prompt.into(), ok, got };
    }
    let Some(c) = calls.first() else {
        return ToolCase { prompt: prompt.into(), ok: false, got: if content.is_empty() { "no tool call".into() } else { format!("wrote text instead: {}", content.chars().take(60).collect::<String>()) } };
    };
    let name = c["function"]["name"].as_str().unwrap_or("");
    let args = &c["function"]["arguments"];
    let args = if let Some(s) = args.as_str() { serde_json::from_str::<Value>(s).unwrap_or(Value::Null) } else { args.clone() };
    let val = args.get(key).and_then(|v| v.as_str()).map(String::from);
    let ok = name == want && val.as_deref().map(|v| v.to_lowercase().contains(&sub.to_lowercase())).unwrap_or(false);
    ToolCase { prompt: prompt.into(), ok, got: format!("{name}({})", args.to_string().chars().take(80).collect::<String>()) }
}

/// One streamed request; returns (first reply ms, final stats json, full text).
fn stream_chat(model: &str, think: bool, num_ctx: u32, prompt: &str, num_predict: u32) -> Result<(u64, Value, String), String> {
    let mut body = json!({
        "model": model, "stream": true, "keep_alive": "10m",
        "messages": [{"role": "user", "content": prompt}],
        "options": {"temperature": 0, "num_predict": num_predict, "num_ctx": num_ctx}
    });
    if think {
        body["think"] = json!(false);
    }
    let started = Instant::now();
    let resp = http(900).post(&format!("{OLLAMA}/api/chat")).send_json(body).map_err(|e| e.to_string())?;
    let mut first = 0u64;
    let mut text = String::new();
    let mut last = Value::Null;
    for line in BufReader::new(resp.into_reader()).lines() {
        if STOP.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if let Some(err) = v["error"].as_str() {
            return Err(err.to_string());
        }
        let piece = v["message"]["content"].as_str().unwrap_or("");
        let thought = v["message"]["thinking"].as_str().unwrap_or("");
        if first == 0 && (!piece.is_empty() || !thought.is_empty()) {
            first = started.elapsed().as_millis() as u64;
        }
        text.push_str(piece);
        if v["done"].as_bool().unwrap_or(false) {
            last = v;
            break;
        }
    }
    Ok((first, last, text))
}

fn bench_model<R: tauri::Runtime>(app: &AppHandle<R>, model: &str, sizes: &[u32], allow_battery: bool, idx: u32, total_models: u32) -> BenchResult {
    let (_, mem_total) = mem();
    let mut r = BenchResult { model: model.into(), mem_total, requested_ctx: 0, ..Default::default() };
    r.runtime = get("/api/version").ok().and_then(|v| v["version"].as_str().map(|s| format!("Ollama {s}"))).unwrap_or_else(|| "Ollama".into());
    let steps = 4 + sizes.len() as u32 + CASES.len() as u32 + crate::security::CASE_COUNT + 2 * crate::jobs::CATEGORIES.len() as u32;
    let mut done = 0u32;
    let say = |step: &str, done: u32| {
        let _ = app.emit("bench-progress", Progress { model: model.into(), step: step.into(), done: idx * steps + done, total: total_models * steps });
    };

    say("Reading model details", done);
    let show = post("/api/show", json!({"model": model}), 30).unwrap_or(Value::Null);
    r.family = show["details"]["family"].as_str().unwrap_or("").into();
    r.params = show["details"]["parameter_size"].as_str().unwrap_or("").into();
    r.quant = show["details"]["quantization_level"].as_str().unwrap_or("").into();
    if let Some(info) = show["model_info"].as_object() {
        r.model_max_ctx = info.iter().find(|(k, _)| k.ends_with(".context_length")).and_then(|(_, v)| v.as_u64()).unwrap_or(0);
    }
    let caps: Vec<String> = show["capabilities"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let think = caps.iter().any(|c| c == "thinking");
    r.tools_supported = caps.iter().any(|c| c == "tools");
    r.file_bytes = get("/api/tags")
        .ok()
        .and_then(|t| t["models"].as_array().and_then(|a| a.iter().find(|m| m["name"].as_str() == Some(model)).and_then(|m| m["size"].as_u64())))
        .unwrap_or(0);

    // Security first: check the file before it's ever loaded.
    done += 1;
    say("Security: checking the file", done);
    r.checks = crate::security::file_checks(model, &show);
    if let Some(bad) = r.checks.iter().find(|c| c.status == "bad") {
        r.skipped = format!("Failed a security check, so it wasn't loaded. {}: {}", bad.name, bad.detail);
        return r;
    }

    if r.file_bytes > 0 && r.file_bytes as f64 > gpu_limit(mem_total) as f64 * 0.9 {
        r.skipped = "Too big for this Mac's memory. Loading it would slow everything down, so it wasn't tested.".into();
        return r;
    }
    let max_size = *sizes.iter().max().unwrap_or(&4096);
    let mut num_ctx = max_size + 4096;
    if r.model_max_ctx > 0 && (num_ctx as u64) > r.model_max_ctx {
        num_ctx = r.model_max_ctx as u32;
    }
    r.requested_ctx = num_ctx;

    done += 1;
    say("Unloading other models", done);
    unload_all();
    if STOP.load(Ordering::SeqCst) {
        return r;
    }
    say("Loading it from disk (cold start)", done);
    let t = Instant::now();
    if let Err(e) = post("/api/generate", json!({"model": model, "prompt": "", "keep_alive": "10m", "options": {"num_ctx": num_ctx}}), 600) {
        r.error = format!("Couldn't load: {e}");
        return r;
    }
    r.load_ms = t.elapsed().as_millis() as u64;
    if let Ok(ps) = get("/api/ps") {
        if let Some(m) = ps["models"].as_array().and_then(|a| a.iter().find(|m| m["name"].as_str() == Some(model)).cloned()) {
            r.loaded_bytes = m["size"].as_u64().unwrap_or(0);
            r.gpu_bytes = m["size_vram"].as_u64().unwrap_or(0);
            r.reported_ctx = m["context_length"].as_u64().unwrap_or(0);
        }
    }
    let (avail, _) = mem();
    if avail < 1_500_000_000 {
        r.error = "Stopped: your Mac got low on free memory with this model loaded.".into();
        let _ = post("/api/generate", json!({"model": model, "keep_alive": 0}), 30);
        return r;
    }

    // Working memory + first reply, smallest size first so it calibrates characters per token.
    let mut rng = Rng(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(7) | 1);
    // Measure how many characters make a token for this model, so a "20K"
    // prompt really is 20K tokens and "read" can be compared with what was sent.
    let mut chars_per_token = 4.0f64;
    {
        let sample = filler(6000, &mut rng);
        let mut body = json!({"model": model, "stream": false, "keep_alive": "10m", "messages": [{"role": "user", "content": sample}], "options": {"num_predict": 1, "num_ctx": num_ctx}});
        if think {
            body["think"] = json!(false);
        }
        if let Ok(v) = post("/api/chat", body, 120) {
            let n = v["prompt_eval_count"].as_u64().unwrap_or(0);
            if n > 200 {
                chars_per_token = (6000.0 / n as f64).clamp(2.0, 8.0);
            }
        }
    }
    let mut sorted = sizes.to_vec();
    sorted.sort();
    for size in sorted {
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        if !allow_battery && on_battery() {
            r.error = "Paused: the Mac switched to battery.".into();
            break;
        }
        done += 1;
        say(&format!("Sending a {}K-token prompt", size / 1000), done);
        let code = format!("{}-{}", WORDS[rng.next() % 48].to_uppercase(), 1000 + rng.next() % 9000);
        let nonce = rng.next() % 1_000_000;
        let head = format!("Note {nonce}. The secret code word is {code}. Remember it.\n\n");
        let tail = "\n\nQuestion: what is the secret code word given at the very start of this message? Reply with only the code word.";
        let body_chars = ((size as f64 - 60.0).max(100.0) * chars_per_token) as usize;
        let prompt = format!("{head}{}{tail}", filler(body_chars, &mut rng));
        match stream_chat(model, think, num_ctx, &prompt, 24) {
            Ok((first, stats, text)) => {
                let read = stats["prompt_eval_count"].as_u64().unwrap_or(0);
                let pe = stats["prompt_eval_duration"].as_u64().unwrap_or(0) as f64 / 1e9;
                r.sizes.push(SizeRun {
                    asked: size,
                    sent: (prompt.len() as f64 / chars_per_token).round() as u64,
                    read,
                    recalled: text.to_uppercase().contains(&code),
                    first_reply_ms: first,
                    prefill_tps: if pe > 0.0 { read as f64 / pe } else { 0.0 },
                });
            }
            Err(e) => {
                if e != "stopped" {
                    r.error = format!("A {}K prompt failed: {}", size / 1000, e.chars().take(160).collect::<String>());
                }
                break;
            }
        }
        let (avail, _) = mem();
        if avail < 1_000_000_000 {
            r.error = "Stopped: your Mac got low on free memory.".into();
            break;
        }
    }

    if !STOP.load(Ordering::SeqCst) && r.error.is_empty() {
        done += 1;
        say("Measuring writing speed", done);
        if let Ok((_, stats, _)) = stream_chat(model, think, num_ctx, "Write a short paragraph about how rivers form.", 160) {
            let n = stats["eval_count"].as_u64().unwrap_or(0) as f64;
            let d = stats["eval_duration"].as_u64().unwrap_or(0) as f64 / 1e9;
            if d > 0.0 {
                r.gen_tps = n / d;
            }
        }
    }

    if r.tools_supported && r.error.is_empty() {
        for case in CASES.iter() {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            done += 1;
            say(&format!("Tool test {} of {}", r.tool_cases.len() + 1, CASES.len()), done);
            r.tool_cases.push(run_tool_case(model, think, num_ctx, case));
        }
    }
    if r.tools_supported && r.error.is_empty() && !STOP.load(Ordering::SeqCst) {
        let base = done;
        let chat = |body: Value| post("/api/chat", body, 180);
        let progress = |n: u32| {
            say(&format!("Security: planted-instruction test {} of {}", n + 1, crate::security::CASE_COUNT), base + n + 1);
            !STOP.load(Ordering::SeqCst)
        };
        r.injection = crate::security::injection_cases(model, think, &chat, &progress);
    }
    // Local jobs: how reliable is it at the work Eiwaz hands to this Mac?
    if r.error.is_empty() && !STOP.load(Ordering::SeqCst) {
        let base = done + crate::security::CASE_COUNT;
        let stop = || STOP.load(Ordering::SeqCst);
        let progress = |cat: &str, n: u32| say(&format!("Local jobs: {cat} test {}", n % 2 + 1), base + n + 1);
        r.jobs = crate::jobs::run_jobs(model, &stop, &progress);
    }
    let _ = post("/api/generate", json!({"model": model, "keep_alive": 0}), 30);
    r.finished_at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    r
}

#[cfg(test)]
pub fn bench_model_for_test<R: tauri::Runtime>(app: &AppHandle<R>, model: &str, sizes: &[u32]) -> BenchResult {
    bench_model(app, model, sizes, true, 0, 1)
}

#[derive(Serialize, Clone)]
struct Done {
    stopped: bool,
    reason: String,
}

#[tauri::command]
pub fn bench_start<R: tauri::Runtime>(app: AppHandle<R>, models: Vec<String>, sizes: Vec<u32>, allow_battery: bool) -> Result<(), String> {
    if models.is_empty() {
        return Err("Pick at least one model".into());
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("A test is already running".into());
    }
    if get("/api/version").is_err() {
        RUNNING.store(false, Ordering::SeqCst);
        return Err("Ollama isn't running. Open the Ollama app, then try again.".into());
    }
    if !allow_battery && on_battery() {
        RUNNING.store(false, Ordering::SeqCst);
        return Err("Your Mac is on battery. Plug in, or choose to run on battery anyway.".into());
    }
    let sizes: Vec<u32> = sizes.into_iter().filter(|s| (1000..=200_000).contains(s)).collect();
    STOP.store(false, Ordering::SeqCst);
    std::thread::spawn(move || {
        let total = models.len() as u32;
        let mut reason = String::new();
        let mut results: BTreeMap<String, BenchResult> = BTreeMap::new();
        for (i, m) in models.iter().enumerate() {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            let res = bench_model(&app, m, &sizes, allow_battery, i as u32, total);
            if res.error.starts_with("Paused") || res.error.starts_with("Stopped") {
                reason = res.error.clone();
            }
            let _ = app.emit("bench-model", res.clone());
            results.insert(m.clone(), res);
            if !reason.is_empty() {
                break;
            }
        }
        let stopped = STOP.load(Ordering::SeqCst) || !reason.is_empty();
        if STOP.load(Ordering::SeqCst) && reason.is_empty() {
            reason = "You stopped the test.".into();
        }
        RUNNING.store(false, Ordering::SeqCst);
        let _ = app.emit("bench-done", Done { stopped, reason });
    });
    Ok(())
}

#[tauri::command]
pub fn bench_stop() {
    STOP.store(true, Ordering::SeqCst);
}

#[derive(Serialize)]
pub struct BenchEnv {
    on_battery: bool,
    mem_total: u64,
    mem_available: u64,
    gpu_limit: u64,
    chip: String,
    running: bool,
    models: Vec<Value>,
    ollama_up: bool,
    lmstudio_up: bool,
}

#[tauri::command(async)]
pub fn bench_env() -> BenchEnv {
    let (avail, total) = mem();
    let chip = Command::new("sysctl").args(["-n", "machdep.cpu.brand_string"]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let tags = get("/api/tags").ok();
    let lm = ureq::AgentBuilder::new().timeout(Duration::from_millis(800)).build().get("http://127.0.0.1:1234/v1/models").call().is_ok();
    BenchEnv {
        on_battery: on_battery(),
        mem_total: total,
        mem_available: avail,
        gpu_limit: gpu_limit(total),
        chip,
        running: RUNNING.load(Ordering::SeqCst),
        ollama_up: tags.is_some(),
        models: tags
            .and_then(|t| t["models"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|m| can_chat(m["name"].as_str().unwrap_or("")))
            .collect(),
        lmstudio_up: lm,
    }
}

/// One plain, non-streamed local request (used by Savings' "do it on this Mac").
#[derive(Serialize)]
pub struct LocalReply {
    pub text: String,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub seconds: f64,
}

#[tauri::command(async)]
pub fn local_run(model: String, system: String, prompt: String) -> Result<LocalReply, String> {
    local_run_n(model, system, prompt, 1200)
}

/// Same, with a cap on how long the reply may be (in tokens).
pub fn local_run_n(model: String, system: String, prompt: String, max_reply: u32) -> Result<LocalReply, String> {
    let show = post("/api/show", json!({"model": model}), 30).unwrap_or(Value::Null);
    let think = show["capabilities"].as_array().map(|a| a.iter().any(|c| c.as_str() == Some("thinking"))).unwrap_or(false);
    let need = (prompt.len() + system.len()) / 3 + 2048;
    let num_ctx = need.clamp(8192, 65536) as u32;
    let mut body = json!({
        "model": model, "stream": false, "keep_alive": "5m",
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": prompt}],
        "options": {"temperature": 0.2, "num_ctx": num_ctx, "num_predict": max_reply}
    });
    if think {
        body["think"] = json!(false);
    }
    let t = Instant::now();
    let r = post("/api/chat", body, 600)?;
    if let Some(e) = r["error"].as_str() {
        return Err(e.to_string());
    }
    Ok(LocalReply {
        text: r["message"]["content"].as_str().unwrap_or("").trim().to_string(),
        prompt_tokens: r["prompt_eval_count"].as_u64().unwrap_or(0),
        output_tokens: r["eval_count"].as_u64().unwrap_or(0),
        seconds: t.elapsed().as_secs_f64(),
    })
}
