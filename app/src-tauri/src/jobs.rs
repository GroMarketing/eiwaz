//! Local job tests: how reliable is a model at each kind of work Eiwaz hands to
//! this Mac? One test per category, graded by rules (not by another model):
//!
//! - summarize: a long project doc with planted facts. Did the brief keep them,
//!   stay short, and avoid numbers that aren't in the doc?
//! - digest: a long test log with known failures. Did it name each failing test
//!   with its file:line, and not report passing tests as failures?
//! - write: a copy brief with required facts. Are they all in the email, with no
//!   invented prices or links, a Subject line, the right length, no stray
//!   file names or code fences?
//!
//! Each category runs twice; the score is the average (0-100). Eiwaz routes a
//! category to the best model scoring at least PASS, or keeps it in the cloud.

use serde::Serialize;

/// Score a model needs in a category before Eiwaz gives it that job (the app uses the same number).
#[allow(dead_code)]
pub const PASS: u32 = 80;
pub const CATEGORIES: [&str; 3] = ["summarize", "digest", "write"];

#[derive(Serialize, Clone, Default)]
pub struct JobScore {
    pub category: String,
    pub score: u32,
    pub seconds: f64,
    pub detail: String,
}

// ---------- fixtures ----------

const FACTS: [(&str, &str); 8] = [
    ("launch date", "March 14"),
    ("price", "$29"),
    ("trial", "21-day"),
    ("owner", "Dana Whitfield"),
    ("region", "Pacific Northwest"),
    ("limit", "500 orders"),
    ("vendor", "Larkspur Mills"),
    ("goal", "1,200 subscribers"),
];

fn filler(n: usize, seed: u64) -> String {
    let words = ["the", "team", "reviewed", "notes", "from", "last", "week", "and", "agreed", "that", "work", "should", "continue", "on", "current", "tasks", "while", "keeping", "an", "eye", "on", "quality", "feedback", "planning"];
    let mut x = seed | 1;
    let mut s = String::new();
    for i in 0..n {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        s.push_str(words[(x as usize) % words.len()]);
        s.push(if i % 14 == 13 { '.' } else { ' ' });
        if i % 70 == 69 { s.push_str("\n\n"); }
    }
    s
}

fn summarize_doc(seed: u64) -> String {
    let mut s = String::from("# Harvest Box launch notes\n\n");
    for (i, (k, v)) in FACTS.iter().enumerate() {
        s.push_str(&filler(500, seed + i as u64));
        s.push_str(&format!("\n\nDecision: the {k} is {v}.\n\n"));
    }
    s.push_str(&filler(400, seed + 99));
    s
}

fn digest_log(seed: u64) -> String {
    let mut s = String::new();
    let fails = [("formats prices with two decimals", "src/price.test.ts", 42), ("rejects expired coupons", "src/coupon.test.ts", 17), ("sends the welcome email once", "src/email.test.ts", 88)];
    let mut x = seed | 1;
    for i in 0..160 {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        s.push_str(&format!("  ✓ case {i}: handles input variant {} ({} ms)\n", x % 1000, x % 40));
        if i % 53 == 52 {
            let (name, file, line) = fails[i / 53];
            s.push_str(&format!("  ✕ {name}\n    AssertionError: expected values to match\n      at {file}:{line}:7\n"));
        }
    }
    s.push_str("\nTests: 3 failed, 160 passed, 163 total\n");
    s
}

const BRIEF: &str = "Write a launch email for Harvest Box, a weekly produce subscription. Audience: people on the waitlist. Goal: get them to start a trial. Tone: warm, plain, short (120 to 220 words). Must include: the price $29 per week, a 21-day free trial, the launch date March 14, the link https://harvestbox.example/start, and that boxes come from Larkspur Mills. Start with a Subject line. Do not mention any other prices, discounts or links.";

// ---------- grading ----------

fn numbers(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || (c == ',' && !cur.is_empty()) { cur.push(c); } else { if cur.chars().any(|d| d.is_ascii_digit()) { out.push(cur.trim_end_matches(',').to_string()); } cur.clear(); }
    }
    if cur.chars().any(|d| d.is_ascii_digit()) { out.push(cur); }
    out
}

pub fn grade_summary(doc: &str, out: &str) -> (u32, String) {
    let low = out.to_lowercase();
    let kept = FACTS.iter().filter(|(_, v)| low.contains(&v.to_lowercase())).count();
    let invented: Vec<String> = numbers(out).into_iter().filter(|n| !doc.contains(n.as_str()) && n.len() > 1).collect();
    let short = out.len() * 4 < doc.len(); // under a quarter of the doc
    let mut score = (kept as u32 * 70) / FACTS.len() as u32;
    if invented.is_empty() { score += 20 } else { score = score.saturating_sub(10 * invented.len() as u32) }
    if short { score += 10 }
    (score.min(100), format!("kept {kept} of {} facts{}{}", FACTS.len(), if invented.is_empty() { String::new() } else { format!(", invented numbers: {}", invented.join(" ")) }, if short { "" } else { ", too long" }))
}

pub fn grade_digest(out: &str) -> (u32, String) {
    let want = [("price", "src/price.test.ts:42"), ("coupon", "src/coupon.test.ts:17"), ("welcome", "src/email.test.ts:88")];
    let low = out.to_lowercase();
    let named = want.iter().filter(|(k, _)| low.contains(k)).count();
    let located = want.iter().filter(|(_, loc)| out.contains(loc)).count();
    let false_alarm = low.contains("case ") && (low.contains("fail") && low.matches("case ").count() > 2);
    let mut score = named as u32 * 15 + located as u32 * 15;
    if !false_alarm { score += 10 }
    (score.min(100), format!("named {named}/3 failures, {located}/3 with file:line{}", if false_alarm { ", listed passing cases as failures" } else { "" }))
}

pub fn grade_write(out: &str) -> (u32, String) {
    let low = out.to_lowercase();
    let req = [("$29", "price"), ("21-day", "trial"), ("march 14", "date"), ("https://harvestbox.example/start", "link"), ("larkspur mills", "vendor")];
    let have: Vec<&str> = req.iter().filter(|(k, _)| low.contains(&k.to_lowercase()) || (*k == "21-day" && low.contains("21 day"))).map(|(_, n)| *n).collect();
    let missing: Vec<&str> = req.iter().map(|(_, n)| *n).filter(|n| !have.contains(n)).collect();
    let other_prices = numbers(out).iter().filter(|n| out.contains(&format!("${n}")) && n.as_str() != "29").count();
    let other_links = out.split_whitespace().filter(|w| w.contains("http") && !w.contains("harvestbox.example/start")).count();
    let subject = out.lines().next().map(|l| l.to_lowercase().starts_with("subject")).unwrap_or(false);
    let words = out.split_whitespace().count();
    let length_ok = (100..=260).contains(&words);
    let clean = !out.contains("```") && !low.lines().next().unwrap_or("").contains(".md");
    let mut score = have.len() as u32 * 12; // 60
    if other_prices == 0 && other_links == 0 { score += 15 }
    if subject { score += 10 }
    if length_ok { score += 10 }
    if clean { score += 5 }
    let mut d = vec![format!("{}/5 required facts", have.len())];
    if !missing.is_empty() { d.push(format!("missing {}", missing.join(", "))) }
    if other_prices + other_links > 0 { d.push(format!("invented {other_prices} price(s), {other_links} link(s)")) }
    if !subject { d.push("no Subject line".into()) }
    if !length_ok { d.push(format!("{words} words")) }
    if !clean { d.push("stray fence or file name".into()) }
    (score.min(100), d.join(", "))
}

// ---------- running ----------

fn run_one(model: &str, cat: &str, seed: u64) -> (u32, String, f64) {
    use crate::bench::local_run_n;
    let res = match cat {
        "summarize" => {
            let doc = summarize_doc(seed);
            local_run_n(model.into(), "You prepare briefs for a coding agent. Report only what matters for the focus, keeping exact names, numbers and dates. Do not invent anything. Be compact: bullet points.".into(),
                format!("Focus: every decision recorded in these notes\n\nFiles:\n===== notes/launch.md =====\n{doc}"), 800)
                .map(|r| { let (s, d) = grade_summary(&doc, &r.text); (s, d, r.seconds) })
        }
        "digest" => {
            let log = digest_log(seed);
            local_run_n(model.into(), "You digest long program output for a coding agent. Report what failed (quote the test names and file:line locations) and the likely cause. Ignore passing items. Be compact.".into(),
                format!("Question: which tests fail and where\n\nOutput:\n{log}"), 600)
                .map(|r| { let (s, d) = grade_digest(&r.text); (s, d, r.seconds) })
        }
        _ => local_run_n(model.into(), "You are a careful copywriter. Write exactly what the brief asks for. Use only facts, names, numbers, links and offers in the brief; never invent any. Output only the finished text.".into(),
                format!("Brief:\n{BRIEF}\n\nFormat: plain text email. Do not include a file name."), 900)
                .map(|r| { let (s, d) = grade_write(&r.text); (s, d, r.seconds) }),
    };
    res.unwrap_or_else(|e| (0, format!("error: {}", e.chars().take(80).collect::<String>()), 0.0))
}

/// Run every category twice and average.
pub fn run_jobs(model: &str, stop: &dyn Fn() -> bool, progress: &dyn Fn(&str, u32)) -> Vec<JobScore> {
    let mut out = vec![];
    let mut n = 0;
    for cat in CATEGORIES {
        let mut scores = vec![];
        let mut details = vec![];
        let mut secs = 0.0;
        for seed in [11u64, 23] {
            if stop() { return out; }
            progress(cat, n);
            n += 1;
            let (s, d, t) = run_one(model, cat, seed);
            scores.push(s);
            details.push(d);
            secs += t;
        }
        out.push(JobScore { category: cat.into(), score: scores.iter().sum::<u32>() / scores.len() as u32, seconds: secs / 2.0, detail: details.join(" | ") });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grades_good_and_bad_answers() {
        let doc = summarize_doc(11);
        let good = "- Launch March 14, $29, 21-day trial\n- Owner Dana Whitfield, Pacific Northwest\n- Limit 500 orders, vendor Larkspur Mills, goal 1,200 subscribers";
        assert!(grade_summary(&doc, good).0 >= 95, "{:?}", grade_summary(&doc, good));
        let bad = "- Launch April 2 at $49\n- Owner Dana Whitfield";
        assert!(grade_summary(&doc, bad).0 < 40, "{:?}", grade_summary(&doc, bad));

        let dg = "Failing: formats prices (src/price.test.ts:42), rejects expired coupons (src/coupon.test.ts:17), sends the welcome email once (src/email.test.ts:88).";
        assert_eq!(grade_digest(dg).0, 100);
        assert!(grade_digest("Some tests failed.").0 <= 10);

        let email = format!("Subject: Harvest Box opens March 14\n\n{} Start your 21-day free trial at https://harvestbox.example/start. It's $29 a week, with produce from Larkspur Mills.\n\nThe Harvest Box team", "word ".repeat(110));
        assert_eq!(grade_write(&email).0, 100, "{:?}", grade_write(&email));
        let worse = "Hi! Get 50% off, now $19 at https://other.example. Launching March 14.";
        assert!(grade_write(worse).0 < 40, "{:?}", grade_write(worse));
    }
}
