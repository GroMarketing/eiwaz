//! End-to-end checks of the engine behind Changes, Preview, Guide and Bench.
//! Run: cargo test -- --nocapture --test-threads=1
use crate::{guide, preview, savepoints};
use std::io::Read;
use std::path::Path;

fn demo() -> String {
    // The bundled bakery site (fixtures/demo, see its README); EIWAZ_DEMO overrides it.
    let src = std::env::var("EIWAZ_DEMO").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/demo").to_string());
    // One folder per call, so tests can run in parallel.
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dst = std::env::temp_dir().join(format!("eiwaz-demo-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);
    let st = std::process::Command::new("cp").args(["-R", &src]).arg(&dst).status().unwrap();
    assert!(st.success());
    dst.to_string_lossy().into_owned()
}

fn v(x: impl serde::Serialize) -> serde_json::Value { serde_json::to_value(x).unwrap() }

#[test]
fn savepoints_flow() {
    let app = tauri::test::mock_app();
    let h = app.handle().clone();
    let p = demo();
    let base = v(savepoints::sp_create(h.clone(), p.clone(), "Eiwaz started watching".into(), "".into(), true).unwrap().unwrap());
    let base_id = base["id"].as_str().unwrap().to_string();
    // nothing changed: a non-forced save point is skipped
    assert!(savepoints::sp_create(h.clone(), p.clone(), "noop".into(), "".into(), false).unwrap().is_none());
    // an "agent" edits, adds and deletes
    std::fs::write(Path::new(&p).join("index.html"), "<h1>Changed by agent</h1>\n").unwrap();
    std::fs::write(Path::new(&p).join("new file.txt"), "hello\n").unwrap();
    std::fs::remove_file(Path::new(&p).join("PLAN.md")).unwrap();
    std::fs::write(Path::new(&p).join(".env"), "SECRET=1\n").unwrap();
    let after = v(savepoints::sp_create(h.clone(), p.clone(), "After Claude Code's turn".into(), "Claude Code".into(), false).unwrap().unwrap());
    assert_eq!(after["agent"], "Claude Code");
    let ch = v(savepoints::sp_changes(h.clone(), p.clone(), base_id.clone()).unwrap());
    println!("changes: {ch}");
    assert!(!ch.as_array().unwrap().iter().any(|f| f["path"] == ".env"), "secrets must not be tracked");
    let by: std::collections::HashMap<String, serde_json::Value> = ch.as_array().unwrap().iter().map(|f| (f["path"].as_str().unwrap().to_string(), f.clone())).collect();
    assert_eq!(by["index.html"]["status"], "changed");
    assert_eq!(by["new file.txt"]["status"], "added");
    assert_eq!(by["PLAN.md"]["status"], "removed");
    assert_eq!(by["index.html"]["agents"][0], "Claude Code");
    // user edits by hand after the turn -> attributed to "You or another app" at next snapshot
    std::fs::write(Path::new(&p).join("img/note.md"), "mine\n").unwrap();
    savepoints::sp_create(h.clone(), p.clone(), "Before you messaged".into(), "".into(), false).unwrap().unwrap();
    let ch = v(savepoints::sp_changes(h.clone(), p.clone(), base_id.clone()).unwrap());
    let note = ch.as_array().unwrap().iter().find(|f| f["path"] == "img/note.md").unwrap();
    assert_eq!(note["agents"][0], "You or another app");
    let d = savepoints::sp_diff(h.clone(), p.clone(), base_id.clone(), Some("index.html".into()), None).unwrap();
    assert!(d.contains("+<h1>Changed by agent</h1>"), "{d}");
    let before = savepoints::sp_file(h.clone(), p.clone(), base_id.clone(), "img/logo.png".into()).unwrap();
    assert!(before.len() > 50);
    // edits after the latest save point are flagged before an undo replaces them
    assert!(savepoints::sp_unsaved(h.clone(), p.clone(), vec!["index.html".into()]).unwrap().is_empty());
    std::fs::write(Path::new(&p).join("index.html"), "<h1>My own edit</h1>\n").unwrap();
    assert_eq!(savepoints::sp_unsaved(h.clone(), p.clone(), vec!["index.html".into(), "styles.css".into()]).unwrap(), vec!["index.html".to_string()]);
    // undo one file
    let r = v(savepoints::sp_restore(h.clone(), p.clone(), base_id.clone(), Some(vec!["index.html".into()])).unwrap());
    assert_eq!(r["restored"][0], "index.html");
    assert!(std::fs::read_to_string(Path::new(&p).join("index.html")).unwrap().contains("Fresh bread"));
    // ...and the safety save point kept that edit, so the undo can be undone
    let safety = r["safety_point"].as_str().unwrap().to_string();
    use base64::Engine;
    let kept = base64::engine::general_purpose::STANDARD.decode(savepoints::sp_file(h.clone(), p.clone(), safety, "index.html".into()).unwrap()).unwrap();
    assert!(String::from_utf8_lossy(&kept).contains("My own edit"));
    // undo everything
    let r = v(savepoints::sp_restore(h.clone(), p.clone(), base_id.clone(), None).unwrap());
    println!("restore all: {r}");
    assert!(Path::new(&p).join("PLAN.md").exists());
    assert!(!Path::new(&p).join("new file.txt").exists());
    assert!(r["safety_point"].is_string(), "undo must be undoable");
    let ch = v(savepoints::sp_changes(h.clone(), p.clone(), base_id.clone()).unwrap());
    assert_eq!(ch.as_array().unwrap().len(), 0, "{ch}");
    // secrets are never tracked, so undo never touches them
    assert!(Path::new(&p).join(".env").exists(), ".env must survive undo");
    // bad revisions are refused
    assert!(savepoints::sp_changes(h.clone(), p.clone(), "HEAD; rm -rf /".into()).is_err());
    let list = v(savepoints::sp_list(h.clone(), p.clone()).unwrap());
    println!("{} save points", list.as_array().unwrap().len());
}

fn get(url: &str, range: Option<&str>) -> (u16, Vec<u8>, String) {
    let mut req = ureq::get(url);
    if let Some(r) = range { req = req.set("Range", r); }
    match req.call() {
        Ok(r) => { let st = r.status(); let cr = r.header("Content-Range").unwrap_or("").to_string(); let mut b = vec![]; r.into_reader().read_to_end(&mut b).unwrap(); (st, b, cr) }
        Err(ureq::Error::Status(c, _)) => (c, vec![], String::new()),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn preview_server_and_probe() {
    let p = demo();
    std::fs::write(Path::new(&p).join(".env"), "SECRET=1\n").unwrap();
    let base = preview::preview_serve(p.clone()).unwrap();
    println!("served at {base}");
    let (s, b, _) = get(&format!("{base}index.html"), None);
    assert_eq!(s, 200);
    assert!(String::from_utf8_lossy(&b).contains("Fresh bread"));
    let (s, _, _) = get(&format!("{base}"), None);
    assert_eq!(s, 200, "folder serves index.html");
    let full = std::fs::read(Path::new(&p).join("img/logo.png")).unwrap();
    let (s, b, cr) = get(&format!("{base}img/logo.png"), Some("bytes=10-19"));
    assert_eq!(s, 206);
    assert_eq!(b, full[10..20].to_vec());
    assert_eq!(cr, format!("bytes 10-19/{}", full.len()));
    assert_eq!(get(&format!("{base}.env"), None).0, 403, "dotfiles never served");
    assert_eq!(get(&format!("{base}img/..%2F..%2Fetc/passwd"), None).0, 403);
    let wrong = base.rsplitn(3, '/').nth(2).unwrap().to_string() + "/wrongtoken/index.html";
    assert_eq!(get(&wrong, None).0, 404, "needs the secret path");
    let probe = v(preview::preview_probe(p.clone()));
    println!("probe: {probe}");
    assert_eq!(probe["start_command"], "npm run dev");
    let files: Vec<&str> = probe["files"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert!(files.contains(&"PLAN.md") && files.contains(&"img/logo.png") && files.contains(&"index.html"));
    // a real dev server started from the project is found
    let mut child = std::process::Command::new("python3").args(["-m", "http.server", "5199"]).current_dir(&p)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let probe = v(preview::preview_probe(p.clone()));
    let _ = child.kill();
    let found = probe["servers"].as_array().unwrap().iter().find(|s| s["port"] == 5199).cloned();
    println!("server: {found:?}");
    assert_eq!(found.expect("dev server found")["from_project"], true);
    let txt = preview::read_text(p.clone(), "PLAN.md".into()).unwrap();
    assert!(txt.contains("mermaid"));
    assert!(preview::read_text(p.clone(), ".env".into()).is_err());
}

#[test]
fn preview_screenshot() {
    let app = tauri::test::mock_app();
    let p = demo();
    let base = preview::preview_serve(p.clone()).unwrap();
    let shot = preview::preview_screenshot(app.handle().clone(), format!("{base}index.html"), 390, 844, None, None).unwrap();
    let bytes = std::fs::read(&shot).unwrap();
    println!("screenshot {shot} {} bytes", bytes.len());
    assert!(bytes.starts_with(b"\x89PNG") && bytes.len() > 2000);
    assert!(preview::preview_screenshot(app.handle().clone(), "file:///etc/passwd".into(), 390, 844, None, None).is_err());
}

#[test]
fn guide_scan_and_catalog() {
    let app = tauri::test::mock_app();
    let p = demo();
    let f = v(guide::guide_scan(p.clone()).unwrap());
    println!("facts: {f}");
    assert_eq!(f["signals"][0]["integration"], "stripe");
    assert_eq!(f["has_claude_md"], false);
    let c = v(guide::guide_catalog(app.handle().clone(), vec!["stripe".into()], false));
    let n = c["picks"][0]["entries"].as_array().map(|a| a.len()).unwrap_or(0);
    println!("catalog built {} offline={} error={} stripe entries={n}", c["built_at"], c["offline"], c["error"]);
    assert!(n > 0, "{c}");
    for e in c["picks"][0]["entries"].as_array().unwrap().iter().take(4) {
        println!("  {} by {} score {} :: {:?}", e["registryName"].as_str().or(e["name"].as_str()).unwrap(), e["github"]["owner"], e["score"]["total"], e["install"]["claude-code"]);
    }
}

#[test]
#[ignore] // loads real models; run with --ignored
fn bench_real_models() {
    let app = tauri::test::mock_app();
    for m in std::env::var("EIWAZ_BENCH_MODELS").unwrap_or("llama3.2:3b".into()).split(',') {
        let t = std::time::Instant::now();
        let r = v(crate::bench::bench_model_for_test(app.handle(), m, &[4000]));
        println!("{m} in {:.0}s:\n{}", t.elapsed().as_secs_f64(), serde_json::to_string_pretty(&r).unwrap());
        assert!(r["error"].as_str().unwrap_or("").is_empty(), "{}", r["error"]);
        assert_eq!(r["sizes"].as_array().unwrap().len(), 1);
        let (read, sent) = (r["sizes"][0]["read"].as_u64().unwrap(), r["sizes"][0]["sent"].as_u64().unwrap());
        println!("{m}: asked 4000, sent ~{sent}, read {read}, recalled {}, tools {}/10", r["sizes"][0]["recalled"], r["tool_cases"].as_array().unwrap().iter().filter(|c| c["ok"] == true).count());
        for c in r["tool_cases"].as_array().unwrap() { println!("   {} {} -> {}", if c["ok"] == true {"ok "} else {"BAD"}, c["prompt"], c["got"]); }
        assert!(read as f64 > 3400.0 && (read as f64) < 4800.0, "a 4K prompt should be ~4K tokens, got {read}");
        assert!(read as f64 >= sent as f64 * 0.9, "read {read} vs sent {sent}");
        assert!(r["sizes"][0]["first_reply_ms"].as_u64().unwrap() > 0);
        assert_eq!(r["tool_cases"].as_array().unwrap().len(), 10);
    }
    let lr = crate::bench::local_run("llama3.2:3b".into(), "Answer in one word.".into(), "What color is the sky on a clear day?".into()).unwrap();
    let lr = v(lr);
    println!("local_run: {lr}");
    assert!(lr["output_tokens"].as_u64().unwrap() > 0);
}
