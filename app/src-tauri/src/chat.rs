//! Chat with a local model through Ollama's HTTP API, instead of typing into
//! `ollama run` in a terminal. The terminal chat can't open files (only images),
//! treats any message starting with "/" as a command, and can't change how much
//! text the model holds. Talking to the API directly fixes all three.
//!
//! - `chat_file` turns an attachment into something a model can read: text,
//!   PDF text (PDFKit), Word/RTF/HTML text (textutil), or an image (base64).
//! - `chat_plan` works out how much text the model can hold on this Mac.
//! - `chat_send` streams a reply as `chat-delta` events, then `chat-done`.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::System;
use tauri::{AppHandle, Emitter};

pub(crate) const OLLAMA: &str = "http://127.0.0.1:11434";
/// Biggest extracted text Eiwaz will hold for one file (~500K tokens), far past any model's window.
const MAX_CHARS: usize = 2_000_000;
/// Plain files bigger than this aren't read as text.
const MAX_TEXT_BYTES: u64 = 8_000_000;
const MAX_IMAGE_BYTES: u64 = 20_000_000;
const GB: u64 = 1024 * 1024 * 1024;

pub(crate) fn http(read_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(3)).timeout_read(Duration::from_secs(read_secs)).build()
}

pub(crate) fn api_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(_, r) => {
            let body = r.into_string().unwrap_or_default();
            serde_json::from_str::<Value>(&body).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(body)
        }
        e => e.to_string(),
    }
}

fn post(path: &str, body: Value) -> Result<Value, String> {
    http(15).post(&format!("{OLLAMA}{path}")).send_json(body).map_err(api_err)?.into_json().map_err(|e| e.to_string())
}

pub(crate) fn get(path: &str) -> Result<Value, String> {
    http(5).get(&format!("{OLLAMA}{path}")).call().map_err(api_err)?.into_json().map_err(|e| e.to_string())
}

// ---------- files ----------

#[derive(Serialize, Default, Debug)]
pub struct ChatFile {
    pub name: String,
    /// "text" or "image"
    pub kind: String,
    pub text: String,
    /// base64, for images
    pub image: String,
    /// PDF page count, when known
    pub pages: u32,
    /// Set when the text was cut to MAX_CHARS
    pub cut: bool,
}

const IMAGE_DIRECT: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
const IMAGE_CONVERT: [&str; 6] = ["heic", "heif", "tiff", "tif", "gif", "bmp"];
const TEXTUTIL: [&str; 8] = ["doc", "docx", "rtf", "rtfd", "odt", "html", "htm", "webarchive"];

fn tmp_path(ext: &str) -> std::path::PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    std::env::temp_dir().join(format!("eiwaz-chat-{n}.{ext}"))
}

fn cap(mut text: String) -> (String, bool) {
    if text.len() <= MAX_CHARS {
        return (text, false);
    }
    let mut end = MAX_CHARS;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

/// PDF text via PDFKit (built into macOS). Pages are separated by form feeds.
fn pdf_text(path: &Path) -> Result<(String, u32), String> {
    const JXA: &str = r#"ObjC.import("Quartz");
function run(argv) {
  const d = $.PDFDocument.alloc.initWithURL($.NSURL.fileURLWithPath(argv[0]));
  if (!d || d.isNil()) return "ERR";
  if (d.isLocked) return "LOCKED";
  const n = d.pageCount, out = [];
  for (let i = 0; i < n; i++) { const s = d.pageAtIndex(i).string; out.push(s && !s.isNil() ? s.js : ""); }
  return n + "\n" + out.join("\f");
}"#;
    let o = Command::new("osascript").args(["-l", "JavaScript", "-e", JXA]).arg(path).output().map_err(|e| e.to_string())?;
    let s = String::from_utf8_lossy(&o.stdout).to_string();
    let s = s.strip_suffix('\n').unwrap_or(&s);
    if !o.status.success() || s == "ERR" {
        return Err("Eiwaz couldn't open this PDF. It may be damaged.".into());
    }
    if s == "LOCKED" {
        return Err("This PDF is password-protected. Open it in Preview, export an unlocked copy, and attach that.".into());
    }
    let (n, body) = s.split_once('\n').unwrap_or((s, ""));
    let pages: u32 = n.trim().parse().unwrap_or(0);
    if body.trim().is_empty() {
        return Err("This PDF has no text in it, only pictures of pages (a scan). Eiwaz can't read scanned PDFs yet.".into());
    }
    Ok((body.to_string(), pages))
}

fn textutil(path: &Path) -> Result<String, String> {
    let o = Command::new("textutil").args(["-convert", "txt", "-stdout"]).arg(path).output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err("Eiwaz couldn't read the text in this document.".into());
    }
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}

/// Read a file as text if it is text: no NUL bytes, and almost all valid UTF-8.
fn plain_text(path: &Path, len: u64) -> Result<String, String> {
    if len > MAX_TEXT_BYTES {
        return Err(format!("This file is {} MB, too big to read as text.", len / 1_000_000));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    std::fs::File::open(path).and_then(|mut f| f.read_to_end(&mut bytes)).map_err(|e| e.to_string())?;
    let head = &bytes[..bytes.len().min(8192)];
    let bad = String::from_utf8_lossy(&bytes).matches('\u{FFFD}').count();
    if head.contains(&0) || bad > bytes.len() / 100 + 2 {
        return Err("UNSUPPORTED".into());
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

pub fn read_file(path: &str) -> Result<ChatFile, String> {
    let p = Path::new(path);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string();
    let meta = std::fs::metadata(p).map_err(|_| format!("{name} isn't there any more."))?;
    if meta.is_dir() {
        return Err(format!("{name} is a folder. Attach the files inside it instead."));
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let mut f = ChatFile { name: name.clone(), kind: "text".into(), ..Default::default() };

    if IMAGE_DIRECT.contains(&ext.as_str()) || IMAGE_CONVERT.contains(&ext.as_str()) {
        if meta.len() > MAX_IMAGE_BYTES {
            return Err(format!("{name} is {} MB. Images up to 20 MB can be sent.", meta.len() / 1_000_000));
        }
        let bytes = if IMAGE_DIRECT.contains(&ext.as_str()) {
            std::fs::read(p).map_err(|e| e.to_string())?
        } else {
            // Ollama reads PNG, JPEG and WebP. Convert the rest with sips (built into macOS).
            let out = tmp_path("jpg");
            let ok = Command::new("sips").args(["-s", "format", "jpeg"]).arg(p).arg("--out").arg(&out).output().map(|o| o.status.success()).unwrap_or(false);
            let b = std::fs::read(&out);
            let _ = std::fs::remove_file(&out);
            match (ok, b) {
                (true, Ok(b)) => b,
                _ => return Err(format!("Eiwaz couldn't convert {name} to a picture the model can read.")),
            }
        };
        f.kind = "image".into();
        f.image = B64.encode(bytes);
        return Ok(f);
    }

    let text = if ext == "pdf" {
        let (t, pages) = pdf_text(p).map_err(|e| format!("{name}: {e}"))?;
        f.pages = pages;
        t
    } else if TEXTUTIL.contains(&ext.as_str()) {
        textutil(p).map_err(|e| format!("{name}: {e}"))?
    } else {
        match plain_text(p, meta.len()) {
            Ok(t) => t,
            Err(e) if e == "UNSUPPORTED" => {
                return Err(format!("{name} isn't something a model can read. It can read text and code files, PDFs, Word documents and images."))
            }
            Err(e) => return Err(format!("{name}: {e}")),
        }
    };
    let (text, cut) = cap(text);
    f.text = text;
    f.cut = cut;
    Ok(f)
}

#[tauri::command(async)]
pub fn chat_file(path: String) -> Result<ChatFile, String> {
    read_file(&path)
}

// ---------- how much the model can hold ----------

#[derive(Serialize, Debug)]
pub struct Plan {
    /// The most the model itself supports, in tokens.
    pub max_ctx: u32,
    /// The most that fits in this Mac's memory alongside the model, in tokens.
    pub safe_ctx: u32,
    pub vision: bool,
}

fn num(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        // Some models list a value per layer.
        Value::Array(a) => a.iter().filter_map(|x| x.as_u64()).max().unwrap_or(0),
        _ => 0,
    }
}

/// Upper bound on the memory one token of context takes (keys + values, 16-bit,
/// every layer at full attention). Real use is lower for models with
/// sliding-window layers, so sizing from this errs on the safe side.
pub fn bytes_per_token(info: &Value) -> u64 {
    let arch = info["general.architecture"].as_str().unwrap_or("");
    let k = |s: &str| num(&info[format!("{arch}.{s}")]);
    let blocks = k("block_count");
    let heads = k("attention.head_count").max(1);
    let kv = { let x = k("attention.head_count_kv"); if x == 0 { heads } else { x } };
    let per_head = k("embedding_length") / heads;
    let kl = { let x = k("attention.key_length"); if x == 0 { per_head } else { x } };
    let vl = { let x = k("attention.value_length"); if x == 0 { per_head } else { x } };
    let b = blocks * kv * (kl + vl) * 2;
    if b == 0 { 160 * 1024 } else { b }
}

/// Tokens of context that fit: what the GPU may use (and what's actually free),
/// minus the model itself, minus 2 GB of headroom for everything else.
pub fn safe_tokens(max_ctx: u64, per_token: u64, model_bytes: u64, gpu_limit: u64, free: u64) -> u64 {
    let room = (gpu_limit.saturating_sub(model_bytes)).min(free.saturating_sub(model_bytes)).saturating_sub(2 * GB);
    let fit = room / per_token.max(1);
    (fit.min(max_ctx) / 1024 * 1024).max(4096.min(max_ctx))
}

#[tauri::command(async)]
pub fn chat_plan(model: String) -> Result<Plan, String> {
    let show = post("/api/show", json!({ "model": model }))?;
    let info = &show["model_info"];
    let arch = info["general.architecture"].as_str().unwrap_or("");
    let max_ctx = num(&info[format!("{arch}.context_length")]).max(2048);
    let vision = show["capabilities"].as_array().map(|a| a.iter().any(|c| c == "vision")).unwrap_or(false);

    let size_of = |list: &Value| list["models"].as_array().and_then(|m| m.iter().find(|x| x["name"] == model.as_str()).and_then(|x| x["size"].as_u64())).unwrap_or(0);
    let model_bytes = get("/api/tags").map(|t| size_of(&t)).unwrap_or(0);
    let loaded = get("/api/ps").map(|t| size_of(&t)).unwrap_or(0);

    let mut sys = System::new();
    sys.refresh_memory();
    let total = sys.total_memory();
    // Memory this model already holds counts as free: reloading it releases that first.
    let free = sys.available_memory() + loaded;
    let safe = safe_tokens(max_ctx, bytes_per_token(info), model_bytes, crate::bench::gpu_limit(total), free);
    Ok(Plan { max_ctx: max_ctx as u32, safe_ctx: safe as u32, vision })
}

// ---------- making sure Ollama is running ----------

pub(crate) fn ollama_up() -> bool {
    ureq::AgentBuilder::new().timeout(Duration::from_millis(1200)).build().get(&format!("{OLLAMA}/api/version")).call().is_ok()
}

pub(crate) fn start_ollama() -> Result<(), String> {
    if Path::new("/Applications/Ollama.app").exists() {
        let _ = Command::new("open").args(["-g", "-a", "Ollama"]).status();
    } else {
        let bin = ["/opt/homebrew/bin/ollama", "/usr/local/bin/ollama"].into_iter().find(|b| Path::new(b).exists());
        let Some(bin) = bin else { return Err("Ollama isn't installed. Get it from ollama.com, then try again.".into()) };
        Command::new(bin)
            .arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    for _ in 0..60 {
        if ollama_up() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("Ollama didn't start. Open the Ollama app, then send your message again.".into())
}

// ---------- sending ----------

static STOPS: Mutex<Vec<(u32, Arc<AtomicBool>)>> = Mutex::new(Vec::new());

#[derive(Clone, Serialize)]
struct Delta {
    id: u32,
    content: String,
    thinking: String,
}

#[derive(Clone, Serialize)]
struct Status {
    id: u32,
    text: String,
}

#[derive(Clone, Serialize)]
struct Done {
    id: u32,
    ok: bool,
    stopped: bool,
    error: String,
    prompt_tokens: u64,
    reply_tokens: u64,
}

fn loaded_ctx(model: &str) -> Option<u64> {
    let ps = get("/api/ps").ok()?;
    ps["models"].as_array()?.iter().find(|m| m["name"] == model).map(|m| m["context_length"].as_u64().unwrap_or(0))
}

fn stream<R: tauri::Runtime>(app: &AppHandle<R>, id: u32, model: &str, messages: Value, num_ctx: u32, stop: &AtomicBool) -> Result<(u64, u64), String> {
    let status = |t: &str| { let _ = app.emit("chat-status", Status { id, text: t.into() }); };
    if !ollama_up() {
        status("starting");
        start_ollama()?;
    }
    // Load the model first, on its own, so the status can say when loading ends and reading begins.
    let need_load = match loaded_ctx(model) {
        None => { status("loading"); true }
        Some(c) if c != num_ctx as u64 => { status("resizing"); true }
        _ => false,
    };
    if need_load {
        http(600)
            .post(&format!("{OLLAMA}/api/generate"))
            .send_json(json!({ "model": model, "keep_alive": "15m", "options": { "num_ctx": num_ctx } }))
            .map_err(api_err)?;
    }
    if stop.load(Ordering::SeqCst) {
        return Err("stopped".into());
    }
    status("reading");
    let body = json!({
        "model": model, "stream": true, "keep_alive": "15m",
        "messages": messages,
        "options": { "num_ctx": num_ctx }
    });
    let resp = http(900).post(&format!("{OLLAMA}/api/chat")).send_json(body).map_err(api_err)?;
    let (mut prompt, mut reply) = (0, 0);
    for line in BufReader::new(resp.into_reader()).lines() {
        if stop.load(Ordering::SeqCst) {
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
        let content = v["message"]["content"].as_str().unwrap_or("").to_string();
        let thinking = v["message"]["thinking"].as_str().unwrap_or("").to_string();
        if !content.is_empty() || !thinking.is_empty() {
            let _ = app.emit("chat-delta", Delta { id, content, thinking });
        }
        if v["done"].as_bool().unwrap_or(false) {
            prompt = v["prompt_eval_count"].as_u64().unwrap_or(0);
            reply = v["eval_count"].as_u64().unwrap_or(0);
            break;
        }
    }
    Ok((prompt, reply))
}

/// A fresh stop flag for chat `id`, set by `chat_stop`. Shared by searching and answering.
pub(crate) fn stop_flag(id: u32) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    if let Ok(mut s) = STOPS.lock() {
        s.retain(|(i, _)| *i != id);
        s.push((id, flag.clone()));
    }
    flag
}

#[tauri::command]
pub fn chat_send(app: AppHandle, id: u32, model: String, messages: Value, num_ctx: u32) {
    let flag = stop_flag(id);
    std::thread::spawn(move || {
        let res = stream(&app, id, &model, messages, num_ctx, &flag);
        let stopped = flag.load(Ordering::SeqCst);
        let (ok, error, (prompt_tokens, reply_tokens)) = match res {
            Ok(t) => (true, String::new(), t),
            Err(e) => (false, if stopped { String::new() } else { e }, (0, 0)),
        };
        let _ = app.emit("chat-done", Done { id, ok, stopped, error, prompt_tokens, reply_tokens });
    });
}

#[tauri::command]
pub fn chat_stop(id: u32) {
    if let Ok(s) = STOPS.lock() {
        for (i, f) in s.iter() {
            if *i == id {
                f.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_context_to_memory() {
        // Gemma 4 E4B on a 48 GB Mac: 42 layers, 2 KV heads, 512+512 dims.
        let info = json!({
            "general.architecture": "gemma4", "gemma4.block_count": 42, "gemma4.attention.head_count": 8,
            "gemma4.attention.head_count_kv": 2, "gemma4.attention.key_length": 512, "gemma4.attention.value_length": 512
        });
        let per = bytes_per_token(&info);
        assert_eq!(per, 42 * 2 * 1024 * 2);
        // Plenty of memory: capped at what the model supports.
        assert_eq!(safe_tokens(131072, per, 7 * GB, 36 * GB, 30 * GB), 131072);
        // Little free memory: fits less, never below 4K.
        let tight = safe_tokens(131072, per, 7 * GB, 36 * GB, 10 * GB);
        assert!(tight < 131072 && tight >= 4096 && tight % 1024 == 0, "{tight}");
        assert_eq!(safe_tokens(131072, per, 7 * GB, 36 * GB, 1 * GB), 4096);
    }

    #[test]
    fn reads_text_and_refuses_binary() {
        let dir = std::env::temp_dir().join(format!("eiwaz-chat-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let md = dir.join("note one.md");
        std::fs::write(&md, "# Hi\nThe code word is PINEAPPLE.").unwrap();
        let f = read_file(md.to_str().unwrap()).unwrap();
        assert_eq!(f.kind, "text");
        assert!(f.text.contains("PINEAPPLE"));

        let bin = dir.join("blob.bin");
        std::fs::write(&bin, [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
        assert!(read_file(bin.to_str().unwrap()).unwrap_err().contains("isn't something a model can read"));

        assert!(read_file(dir.to_str().unwrap()).unwrap_err().contains("is a folder"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod live {
    use super::*;
    /// Needs Ollama with gemma4:latest. Run: cargo test live_ -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_plan_and_files() {
        let p = chat_plan("gemma4:latest".into()).unwrap();
        println!("{p:?}");
        assert!(p.max_ctx >= 32768 && p.safe_ctx >= 4096 && p.vision);
        if let Ok(pdf) = std::env::var("EIWAZ_TEST_PDF") {
            let f = read_file(&pdf).unwrap();
            println!("pdf pages={} chars={}", f.pages, f.text.len());
            assert!(f.pages > 0 && !f.text.is_empty());
        }
        if let Ok(doc) = std::env::var("EIWAZ_TEST_DOCX") {
            let f = read_file(&doc).unwrap();
            println!("docx chars={}", f.text.len());
            assert!(!f.text.is_empty());
        }
    }
}

#[cfg(test)]
mod live_stream {
    use super::*;
    /// Needs Ollama + gemma4:latest. cargo test live_stream -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_stream_file_and_image() {
        let app = tauri::test::mock_app();
        let h = app.handle().clone();
        let stop = AtomicBool::new(false);
        let doc = "The secret code word is PINEAPPLE. ".repeat(3);
        let msgs = json!([{ "role": "user", "content": format!("/private/path/note.md\n<file name=\"note.md\">\n{doc}\n</file>\n\nWhat is the code word? One word.") }]);
        let (p, r) = stream(&h, 7, "gemma4:latest", msgs, 32768, &stop).unwrap();
        println!("text ok: prompt={p} reply={r}");
        assert!(p > 20 && r > 0);
        let png = std::env::var("EIWAZ_TEST_PNG").unwrap();
        let img = read_file(&png).unwrap();
        let msgs = json!([{ "role": "user", "content": "What text is in this image? Answer briefly.", "images": [img.image] }]);
        let (p, _) = stream(&h, 8, "gemma4:latest", msgs, 32768, &stop).unwrap();
        println!("image ok: prompt={p}");
        assert!(p > 100, "image tokens should be counted");
    }
}
