//! Search inside files too big for a local model's window.
//!
//! When an attachment can't fit, the chat used to send only its first pages.
//! Instead, Eiwaz now splits the file into short passages, finds the ones that
//! answer the question, and sends those (in document order, labelled with their
//! pages). Two signals are fused so each covers the other's blind spot:
//! - meaning, from `nomic-embed-text` embeddings (finds a passage that answers
//!   the question in different words), and
//! - exact words, from BM25 (finds a code word, a name or an error string that
//!   embeddings blur).
//!
//! For a summary there is no question to search for, so Eiwaz sends an even
//! sample from across the whole file instead of the beginning.
//!
//! Embeddings are cached per file, so follow-up questions only embed the question.

use crate::chat::{api_err, http, ollama_up, start_ollama, stop_flag, OLLAMA};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter};

const EMBED_MODEL: &str = "nomic-embed-text";
/// Passage length in characters (~350 tokens): long enough to hold an answer, short enough to rank precisely.
const CHUNK: usize = 1500;
const BATCH: usize = 32;
/// Files whose embeddings are kept in memory.
const CACHE_FILES: usize = 6;

#[derive(Deserialize, Clone)]
pub struct Doc {
    pub name: String,
    pub text: String,
}

#[derive(Serialize, Debug, Default)]
pub struct Found {
    pub name: String,
    /// The passages, ready to put inside `<file>`.
    pub text: String,
    /// Where they came from, for people: "pages 3, 17–18 and 92" or "12 passages".
    pub places: String,
    pub passages: usize,
    pub total_passages: usize,
}

#[derive(Serialize, Debug, Default)]
pub struct Search {
    pub docs: Vec<Found>,
    /// True when meaning-based search ran; false means exact words only.
    pub semantic: bool,
    /// Why meaning-based search didn't run, when it didn't.
    pub note: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chunk {
    pub doc: usize,
    pub start: usize,
    pub end: usize,
    /// First and last page (1-based), or 0 when the file has no pages.
    pub page_from: u32,
    pub page_to: u32,
}

/// Split text into passages of about `CHUNK` characters, breaking at a page,
/// paragraph, line or sentence end where one is near. PDF pages are separated by form feeds.
pub fn chunks(doc: usize, text: &str) -> Vec<Chunk> {
    let breaks: Vec<usize> = text.match_indices('\u{c}').map(|(i, _)| i).collect();
    // Page of the byte at `i`: one more than the page breaks before it.
    let page = |i: usize| if breaks.is_empty() { 0 } else { breaks.partition_point(|&b| b < i) as u32 + 1 };
    let mut out = Vec::new();
    let (mut start, len) = (0usize, text.len());
    while start < len {
        let mut end = (start + CHUNK).min(len);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < len {
            let floor = start + CHUNK * 6 / 10;
            let window = &text[start..end];
            let cut = ["\u{c}", "\n\n", "\n", ". ", " "]
                .iter()
                .find_map(|sep| window.rfind(sep).map(|i| start + i + sep.len()).filter(|&i| i > floor));
            if let Some(c) = cut {
                end = c;
            }
        }
        let body = &text[start..end];
        // Pages are those of the first and last visible characters, so a break at either edge doesn't count.
        if let (Some(f), Some(l)) = (body.find(|c: char| !c.is_whitespace()), body.rfind(|c: char| !c.is_whitespace())) {
            out.push(Chunk { doc, start, end, page_from: page(start + f), page_to: page(start + l) });
        }
        start = end;
    }
    out
}

// ---------- exact words (BM25) ----------

const STOP: [&str; 48] = [
    "the", "and", "for", "are", "was", "were", "this", "that", "with", "from", "what", "which", "who", "whom", "how", "why", "when",
    "where", "does", "did", "can", "could", "would", "should", "about", "into", "than", "then", "there", "their", "they", "them",
    "you", "your", "our", "has", "have", "had", "not", "but", "its", "his", "her", "any", "all", "tell", "file", "document",
];

fn words(t: &str) -> impl Iterator<Item = String> + '_ {
    t.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 2).map(|w| w.to_lowercase()).filter(|w| !STOP.contains(&w.as_str()))
}

/// BM25 score of each passage for the query's words.
pub fn bm25(texts: &[&str], query: &str) -> Vec<f64> {
    let q: HashSet<String> = words(query).collect();
    if q.is_empty() || texts.is_empty() {
        return vec![0.0; texts.len()];
    }
    let mut tf: Vec<HashMap<&str, f64>> = Vec::with_capacity(texts.len());
    let mut lens = Vec::with_capacity(texts.len());
    let mut df: HashMap<&str, f64> = HashMap::new();
    for t in texts {
        let mut m: HashMap<&str, f64> = HashMap::new();
        let mut n = 0.0;
        for w in words(t) {
            n += 1.0;
            if let Some(k) = q.get(&w) {
                *m.entry(k.as_str()).or_default() += 1.0;
            }
        }
        for k in m.keys() {
            *df.entry(k).or_default() += 1.0;
        }
        tf.push(m);
        lens.push(n);
    }
    let n = texts.len() as f64;
    let avg = (lens.iter().sum::<f64>() / n).max(1.0);
    let (k1, b) = (1.2, 0.75);
    tf.iter()
        .zip(&lens)
        .map(|(m, &dl)| {
            m.iter()
                .map(|(k, &f)| {
                    let idf = ((n - df[k] + 0.5) / (df[k] + 0.5) + 1.0).ln();
                    idf * f * (k1 + 1.0) / (f + k1 * (1.0 - b + b * dl / avg))
                })
                .sum()
        })
        .collect()
}

// ---------- meaning (embeddings) ----------

static CACHE: Mutex<Vec<(u64, Vec<Vec<f32>>)>> = Mutex::new(Vec::new());

fn key(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    (EMBED_MODEL, CHUNK, text).hash(&mut h);
    h.finish()
}

fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

fn embed(inputs: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
    let v: Value = http(180)
        .post(&format!("{OLLAMA}/api/embed"))
        .send_json(json!({ "model": EMBED_MODEL, "input": inputs, "truncate": true, "keep_alive": "15m" }))
        .map_err(api_err)?
        .into_json()
        .map_err(|e| e.to_string())?;
    let rows = v["embeddings"].as_array().ok_or("Ollama returned no embeddings")?;
    Ok(rows.iter().map(|r| unit(r.as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect()).unwrap_or_default())).collect())
}

fn has_embed_model() -> bool {
    crate::chat::get("/api/tags")
        .ok()
        .and_then(|t| t["models"].as_array().map(|m| m.iter().any(|x| x["name"].as_str().is_some_and(|n| n.starts_with(EMBED_MODEL)))))
        .unwrap_or(false)
}

// ---------- choosing passages ----------

/// Rank passages for a question: reciprocal-rank fusion of meaning and exact words.
/// A passage either signal ranks highly comes near the top.
pub fn rank(sem: Option<&[f32]>, words: &[f64]) -> Vec<usize> {
    let n = words.len();
    let mut score = vec![0.0f64; n];
    let mut add = |order: Vec<usize>, w: f64| {
        for (r, i) in order.into_iter().enumerate() {
            score[i] += w / (60.0 + r as f64);
        }
    };
    if let Some(s) = sem {
        let mut o: Vec<usize> = (0..n).collect();
        o.sort_by(|&a, &b| s[b].total_cmp(&s[a]));
        add(o, 1.0);
    }
    let mut o: Vec<usize> = (0..n).filter(|&i| words[i] > 0.0).collect();
    o.sort_by(|&a, &b| words[b].total_cmp(&words[a]));
    add(o, 1.0);
    let mut all: Vec<usize> = (0..n).collect();
    all.sort_by(|&a, &b| score[b].total_cmp(&score[a]).then(a.cmp(&b)));
    all
}

/// Evenly spaced passages from across everything, for a summary.
pub fn spread(lens: &[usize], budget: usize) -> Vec<usize> {
    let n = lens.len();
    if n == 0 {
        return vec![];
    }
    let avg = (lens.iter().sum::<usize>() / n).max(1);
    let k = (budget / (avg + 40)).clamp(1, n);
    (0..k).map(|j| j * n / k + (n / k) / 2).map(|i| i.min(n - 1)).collect()
}

/// Take passages in the given order until the budget is full.
pub fn fill(order: &[usize], lens: &[usize], budget: usize) -> Vec<usize> {
    let mut used = 0;
    let mut out = Vec::new();
    for &i in order {
        let cost = lens[i] + 40; // label and separator
        if used + cost > budget {
            continue;
        }
        used += cost;
        out.push(i);
    }
    out
}

fn pages_label(ranges: &[(u32, u32)]) -> String {
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for &(a, b) in ranges {
        match merged.last_mut() {
            Some(l) if a <= l.1 + 1 => l.1 = l.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let parts: Vec<String> = merged.iter().map(|&(a, b)| if a == b { a.to_string() } else { format!("{a}–{b}") }).collect();
    let single = merged.len() == 1 && merged[0].0 == merged[0].1;
    let list = match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        n => format!("{} and {}", parts[..n - 1].join(", "), parts[n - 1]),
    };
    format!("{} {list}", if single { "page" } else { "pages" })
}

/// Format the chosen passages of one file in document order, joining neighbours.
pub fn render(text: &str, picked: &[Chunk], total: usize) -> (String, String) {
    let mut ps = picked.to_vec();
    ps.sort_by_key(|c| c.start);
    let mut groups: Vec<Chunk> = Vec::new();
    for c in ps {
        match groups.last_mut() {
            Some(g) if g.end == c.start => {
                g.end = c.end;
                g.page_to = c.page_to;
            }
            _ => groups.push(c),
        }
    }
    let len = text.len().max(1);
    let body = groups
        .iter()
        .map(|g| {
            let label = if g.page_from > 0 {
                if g.page_from == g.page_to { format!("[page {}]", g.page_from) } else { format!("[pages {}–{}]", g.page_from, g.page_to) }
            } else {
                format!("[about {}% into the file]", g.start * 100 / len)
            };
            format!("{label}\n{}", text[g.start..g.end].replace('\u{c}', "\n").trim())
        })
        .collect::<Vec<_>>()
        .join("\n\n[…]\n\n");
    let places = if groups.iter().any(|g| g.page_from > 0) {
        pages_label(&groups.iter().map(|g| (g.page_from, g.page_to)).collect::<Vec<_>>())
    } else {
        format!("{} passage{} of {total}", picked.len(), if picked.len() == 1 { "" } else { "s" })
    };
    (body, places)
}

#[derive(Clone, Serialize)]
struct Progress {
    id: u32,
    done: usize,
    total: usize,
}

/// Find the passages of `docs` that best answer `query`, within `budget_chars`.
/// With `summary`, take an even sample instead. Emits `chat-search` progress while embedding.
#[tauri::command(async)]
pub fn chat_search(app: AppHandle, id: u32, docs: Vec<Doc>, query: String, summary: bool, budget_chars: usize) -> Result<Search, String> {
    let stop = stop_flag(id);
    let all: Vec<Chunk> = docs.iter().enumerate().flat_map(|(i, d)| chunks(i, &d.text)).collect();
    let texts: Vec<&str> = all.iter().map(|c| &docs[c.doc].text[c.start..c.end]).collect();
    let lens: Vec<usize> = all.iter().map(|c| c.end - c.start).collect();
    let mut res = Search::default();

    let chosen = if summary {
        res.semantic = true; // nothing to search for; the sample doesn't depend on it
        spread(&lens, budget_chars)
    } else {
        if !ollama_up() {
            start_ollama()?;
        }
        let sem = if has_embed_model() {
            match embed_all(&app, id, &docs, &all, &texts, &query, &stop) {
                Ok(s) => Some(s),
                Err(e) if e == "stopped" => return Err(e),
                Err(e) => {
                    res.note = format!("Search by meaning failed ({e}), so Eiwaz matched exact words only.");
                    None
                }
            }
        } else {
            res.note = format!("Eiwaz matched exact words only. For search by meaning too, install the free {EMBED_MODEL} model: run `ollama pull {EMBED_MODEL}`.");
            None
        };
        res.semantic = sem.is_some();
        fill(&rank(sem.as_deref(), &bm25(&texts, &query)), &lens, budget_chars)
    };

    for (i, d) in docs.iter().enumerate() {
        let picked: Vec<Chunk> = chosen.iter().map(|&k| all[k]).filter(|c| c.doc == i).collect();
        let total = all.iter().filter(|c| c.doc == i).count();
        let (text, places) = render(&d.text, &picked, total);
        res.docs.push(Found { name: d.name.clone(), text, places, passages: picked.len(), total_passages: total });
    }
    Ok(res)
}

/// Similarity of every passage to the query, embedding (and caching) each file's passages as needed.
fn embed_all<R: tauri::Runtime>(
    app: &AppHandle<R>,
    id: u32,
    docs: &[Doc],
    all: &[Chunk],
    texts: &[&str],
    query: &str,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<Vec<f32>, String> {
    let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(all.len());
    let total = all.len();
    let mut done = 0;
    for (i, d) in docs.iter().enumerate() {
        let k = key(&d.text);
        let idx: Vec<usize> = (0..all.len()).filter(|&j| all[j].doc == i).collect();
        let cached = CACHE.lock().ok().and_then(|c| c.iter().find(|(h, _)| *h == k).map(|(_, v)| v.clone()));
        let file_vecs = match cached {
            Some(v) if v.len() == idx.len() => v,
            _ => {
                let mut v = Vec::with_capacity(idx.len());
                for batch in idx.chunks(BATCH) {
                    if stop.load(Ordering::SeqCst) {
                        return Err("stopped".into());
                    }
                    let _ = app.emit("chat-search", Progress { id, done: done + v.len(), total });
                    v.extend(embed(batch.iter().map(|&j| format!("search_document: {}", texts[j])).collect())?);
                }
                if let Ok(mut c) = CACHE.lock() {
                    c.retain(|(h, _)| *h != k);
                    c.push((k, v.clone()));
                    if c.len() > CACHE_FILES {
                        c.remove(0);
                    }
                }
                v
            }
        };
        done += file_vecs.len();
        vecs.extend(file_vecs);
    }
    let _ = app.emit("chat-search", Progress { id, done: total, total });
    let q = embed(vec![format!("search_query: {query}")])?.pop().ok_or("no query embedding")?;
    Ok(vecs.iter().map(|v| v.iter().zip(&q).map(|(a, b)| a * b).sum()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf(pages: usize, f: impl Fn(usize) -> String) -> String {
        (1..=pages).map(f).collect::<Vec<_>>().join("\u{c}")
    }

    #[test]
    fn chunks_cover_text_and_track_pages() {
        let text = pdf(40, |p| format!("Page {p} opening line.\n\n") + &"Filler sentence about nothing much. ".repeat(60));
        let cs = chunks(0, &text);
        assert_eq!(cs.first().unwrap().start, 0);
        assert_eq!(cs.last().unwrap().end, text.len());
        assert!(cs.windows(2).all(|w| w[0].end == w[1].start), "no gaps or overlaps");
        assert!(cs.iter().all(|c| c.end - c.start <= CHUNK));
        for c in &cs {
            let body = &text[c.start..c.end];
            if let Some(i) = body.find("opening line") {
                let p: u32 = body[..i].rsplit("Page ").next().unwrap().trim().parse().unwrap();
                assert!(c.page_from <= p && p <= c.page_to, "page {p} labelled {}–{}", c.page_from, c.page_to);
            }
        }
        assert_eq!(cs.last().unwrap().page_to, 40);
        let plain = chunks(0, &"word ".repeat(2000));
        assert!(plain.iter().all(|c| c.page_from == 0));
    }

    #[test]
    fn exact_words_find_the_needle() {
        let docs: Vec<String> = (0..50).map(|i| if i == 37 { "The vault code is ZEBRA-7731.".into() } else { format!("Ordinary notes number {i} about budgets.") }).collect();
        let t: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();
        let s = bm25(&t, "What is the vault code?");
        assert_eq!(rank(None, &s)[0], 37);
    }

    #[test]
    fn fusion_keeps_both_signals() {
        // Meaning favours 2, exact words favour 5: both make the top two.
        let sem = [0.1, 0.2, 0.9, 0.1, 0.1, 0.3];
        let w = [0.0, 0.0, 0.0, 0.0, 0.0, 4.0];
        let top: HashSet<usize> = rank(Some(&sem), &w)[..2].iter().copied().collect();
        assert_eq!(top, HashSet::from([2, 5]));
    }

    #[test]
    fn spread_covers_whole_file_and_respects_budget() {
        let lens = vec![1000; 300];
        let s = spread(&lens, 20_000);
        assert!(s.len() >= 15 && s.len() <= 20);
        assert!(s[0] < 20 && *s.last().unwrap() > 280, "{s:?}");
        let f = fill(&s, &lens, 20_000);
        assert!(f.iter().map(|&i| lens[i] + 40).sum::<usize>() <= 20_000);
    }

    #[test]
    fn renders_in_order_with_pages() {
        let text = pdf(10, |p| format!("Text of page {p}. ") + &"More words here. ".repeat(100));
        let cs = chunks(0, &text);
        let pick: Vec<Chunk> = vec![cs[cs.len() - 1], cs[0], cs[1]];
        let (body, places) = render(&text, &pick, cs.len());
        assert!(body.starts_with("[page"), "{}", &body[..40]);
        assert!(body.find("Text of page 1.").unwrap() < body.rfind("[…]").unwrap());
        assert!(places.starts_with("pages 1") && places.ends_with("10"), "{places}");
        assert_eq!(pages_label(&[(3, 3)]), "page 3");
        assert_eq!(pages_label(&[(3, 3), (4, 5), (9, 9)]), "pages 3–5 and 9");
    }
}

#[cfg(test)]
mod live_search {
    use super::*;
    /// Needs Ollama + nomic-embed-text. cargo test live_search -- --ignored --nocapture
    #[test]
    #[ignore]
    fn finds_answer_in_a_long_file_by_meaning() {
        // ~200 pages. The answer is on page 163 and shares no key words with the question.
        let filler = |p: usize| {
            format!("Section {p}. ")
                + &[
                    "The committee reviewed the quarterly hiring plan and agreed to revisit headcount next month. ",
                    "Marketing spend was held flat while the team tested new landing pages. ",
                    "Operations noted that vendor invoices are now approved within five business days. ",
                ][p % 3]
                    .repeat(18)
        };
        let text = (1..=200)
            .map(|p| if p == 163 { filler(p) + "Our office cat, a grey tabby named Biscuit, sleeps on the server rack every afternoon. " } else { filler(p) })
            .collect::<Vec<_>>()
            .join("\u{c}");
        let docs = vec![Doc { name: "minutes.pdf".into(), text }];
        let all = chunks(0, &docs[0].text);
        let texts: Vec<&str> = all.iter().map(|c| &docs[0].text[c.start..c.end]).collect();
        let lens: Vec<usize> = all.iter().map(|c| c.end - c.start).collect();
        let app = tauri::test::mock_app();
        let h = app.handle().clone();
        let t0 = std::time::Instant::now();
        let sem = embed_all(&h, 1, &docs, &all, &texts, "What pet lives at the workplace?", &std::sync::atomic::AtomicBool::new(false)).unwrap();
        let first = t0.elapsed();
        let order = rank(Some(&sem), &bm25(&texts, "What pet lives at the workplace?"));
        let picked: Vec<Chunk> = fill(&order, &lens, 12_000).into_iter().map(|i| all[i]).collect();
        let (body, places) = render(&docs[0].text, &picked, all.len());
        println!("{} passages, embedded in {:?}; picked {places}", all.len(), first);
        assert!(body.contains("Biscuit"), "answer not in the excerpts: {places}");
        let t1 = std::time::Instant::now();
        embed_all(&h, 1, &docs, &all, &texts, "Who approves invoices?", &std::sync::atomic::AtomicBool::new(false)).unwrap();
        println!("follow-up (cached) in {:?}", t1.elapsed());
        assert!(t1.elapsed() < first / 3);
    }
}
