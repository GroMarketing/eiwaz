//! Makes sure nothing a terminal started outlives it.
//!
//! portable_pty starts every terminal's shell with `setsid`, so the shell's pid is
//! also the session id of everything started inside it: Claude Code, its MCP
//! servers, dev servers it backgrounded, `nohup` jobs. Ending only the shell (what
//! `Child::kill` does: one SIGHUP) leaves anything that ignores SIGHUP running with
//! no terminal. So we end the whole session.
//!
//! If Eiwaz crashes or is force-quit, its cleanup never runs. Each running Eiwaz
//! therefore records its terminals in `live-sessions.json`; on the next launch,
//! entries whose Eiwaz is gone are swept. Programs that detached on purpose
//! (called setsid themselves, like a daemon) are left alone.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, Signal, System};

static LEDGER_LOCK: Mutex<()> = Mutex::new(());
const LEDGER: &str = "live-sessions.json";

pub(crate) fn fresh_system() -> System {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing(),
    );
    sys
}

pub(crate) fn alive(p: &sysinfo::Process) -> bool {
    !matches!(p.status(), ProcessStatus::Zombie | ProcessStatus::Dead)
}

fn name_of(p: &sysinfo::Process) -> String {
    p.name().to_string_lossy().into_owned()
}

/// One terminal session to end. `born` and `before` (unix seconds) guard against
/// pid reuse when sweeping after a crash: only processes started inside that
/// window count, and a different process now holding the leader's pid means the
/// session is long gone.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub sid: u32,
    pub born: Option<u64>,
    pub before: Option<u64>,
}

impl Target {
    pub fn live(sid: u32) -> Self {
        Target { sid, born: None, before: None }
    }
}

fn members(sys: &System, t: &Target) -> Vec<Pid> {
    let me = Pid::from_u32(std::process::id());
    if let (Some(born), Some(leader)) = (t.born, sys.process(Pid::from_u32(t.sid))) {
        // The leader's pid now belongs to a newer process: not our session.
        if leader.start_time() > born + 1 {
            return vec![];
        }
    }
    sys.processes()
        .iter()
        .filter(|(pid, p)| {
            **pid != me
                && alive(p)
                && p.session_id().map(|s| s.as_u32()) == Some(t.sid)
                && t.born.map_or(true, |b| p.start_time() + 1 >= b)
                && t.before.map_or(true, |b| p.start_time() <= b)
        })
        .map(|(pid, _)| *pid)
        .collect()
}

/// Ends every process in the given sessions: SIGHUP first (what closing a
/// terminal sends, so programs save and exit cleanly), then SIGKILL for anything
/// still running after `grace`. Returns the names of what was running.
pub fn end_sessions(targets: &[Target], grace: Duration) -> Vec<String> {
    if targets.is_empty() {
        return vec![];
    }
    let mut sys = fresh_system();
    let mut pids: HashSet<Pid> = HashSet::new();
    let mut names = vec![];
    for t in targets {
        for pid in members(&sys, t) {
            if pids.insert(pid) {
                if let Some(p) = sys.process(pid) {
                    names.push(name_of(p));
                    let _ = p.kill_with(Signal::Hangup);
                }
            }
        }
    }
    let deadline = Instant::now() + grace;
    loop {
        // Pick up children forked while we were signalling, too.
        sys = fresh_system();
        let mut left: Vec<Pid> = pids.iter().copied().filter(|p| sys.process(*p).map_or(false, alive)).collect();
        for t in targets {
            for pid in members(&sys, t) {
                if pids.insert(pid) {
                    if let Some(p) = sys.process(pid) {
                        names.push(name_of(p));
                        let _ = p.kill_with(Signal::Hangup);
                    }
                    left.push(pid);
                }
            }
        }
        if left.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            for pid in left {
                if let Some(p) = sys.process(pid) {
                    let _ = p.kill_with(Signal::Kill);
                }
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    names
}

// ---------- the ledger: which Eiwaz owns which terminals ----------

fn read_ledger(dir: &Path) -> Value {
    std::fs::read_to_string(dir.join(LEDGER))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|v: &Value| v.is_object())
        .unwrap_or_else(|| json!({}))
}

fn write_ledger(dir: &Path, v: &Value) {
    let p = dir.join(LEDGER);
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn own_start() -> u64 {
    let sys = fresh_system();
    sys.process(Pid::from_u32(std::process::id())).map(|p| p.start_time()).unwrap_or_else(now)
}

/// Record a new terminal (its shell pid) under this Eiwaz.
pub fn track(dir: &Path, sid: u32) {
    let _g = LEDGER_LOCK.lock().unwrap();
    let mut v = read_ledger(dir);
    let me = std::process::id().to_string();
    let entry = v[&me].as_object().cloned().unwrap_or_default();
    let mut entry = Value::Object(entry);
    if entry["started"].is_null() {
        entry["started"] = json!(own_start());
    }
    let mut list = entry["sessions"].as_array().cloned().unwrap_or_default();
    list.push(json!({ "sid": sid, "born": now() }));
    entry["sessions"] = json!(list);
    v[&me] = entry;
    write_ledger(dir, &v);
}

/// Forget terminals this Eiwaz has ended itself (None = all of them).
pub fn untrack(dir: &Path, sid: Option<u32>) {
    let _g = LEDGER_LOCK.lock().unwrap();
    let mut v = read_ledger(dir);
    let me = std::process::id().to_string();
    match sid {
        None => {
            v.as_object_mut().map(|o| o.remove(&me));
        }
        Some(sid) => {
            if let Some(list) = v[&me]["sessions"].as_array_mut() {
                list.retain(|s| s["sid"].as_u64() != Some(sid as u64));
            }
        }
    }
    write_ledger(dir, &v);
}

#[derive(serde::Serialize, Debug, Default)]
pub struct Sweep {
    /// A previous Eiwaz left terminals behind without closing them (crash or force quit).
    pub unclean: bool,
    /// Programs that were still running from those terminals and have now been ended.
    pub ended: Vec<String>,
}

/// End whatever terminals of a Eiwaz that is no longer running left behind.
pub fn sweep(dir: &Path, grace: Duration) -> Sweep {
    let _g = LEDGER_LOCK.lock().unwrap();
    let mut v = read_ledger(dir);
    let sys = fresh_system();
    let me = std::process::id().to_string();
    let mut targets = vec![];
    let mut gone = vec![];
    for (owner, entry) in v.as_object().cloned().unwrap_or_default() {
        if owner == me {
            continue;
        }
        let started = entry["started"].as_u64().unwrap_or(0);
        let owner_alive = owner
            .parse::<u32>()
            .ok()
            .and_then(|pid| sys.process(Pid::from_u32(pid)))
            .map_or(false, |p| alive(p) && p.start_time().abs_diff(started) <= 1);
        if owner_alive {
            continue; // another Eiwaz (say, a dev build) still owns these
        }
        gone.push(owner);
        for s in entry["sessions"].as_array().cloned().unwrap_or_default() {
            if let Some(sid) = s["sid"].as_u64() {
                targets.push(Target { sid: sid as u32, born: s["born"].as_u64().or(Some(started)), before: Some(own_start()) });
            }
        }
    }
    let unclean = !targets.is_empty();
    let ended = end_sessions(&targets, grace);
    if let Some(o) = v.as_object_mut() {
        for g in gone {
            o.remove(&g);
        }
    }
    write_ledger(dir, &v);
    Sweep { unclean, ended }
}

/// What the app shows: names grouped with counts, e.g. "claude, node ×3".
pub fn describe(names: &[String]) -> String {
    let mut order: Vec<(String, usize)> = vec![];
    for n in names {
        match order.iter_mut().find(|(k, _)| k == n) {
            Some((_, c)) => *c += 1,
            None => order.push((n.clone(), 1)),
        }
    }
    order.iter().map(|(n, c)| if *c > 1 { format!("{n} ×{c}") } else { n.clone() }).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    fn is_running(pid: u32) -> bool {
        fresh_system().process(Pid::from_u32(pid)).map_or(false, alive)
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("eiwaz-procs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A terminal like Eiwaz's: a pty shell (session leader) that backgrounds a
    /// SIGHUP-immune job, the way `nohup npm run dev &` does. Returns the pty
    /// pieces (dropping them = closing the terminal), the shell pid and the job pid.
    fn terminal(dir: &Path) -> (Box<dyn portable_pty::MasterPty + Send>, Box<dyn portable_pty::Child + Send + Sync>, u32, u32) {
        let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).unwrap();
        let pidfile = dir.join("job.pid");
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(format!("nohup sleep 120 >/dev/null 2>&1 & echo $! > '{}'; sleep 120", pidfile.display()));
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let shell = child.process_id().unwrap();
        let t0 = Instant::now();
        let job = loop {
            if let Some(p) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
                break p;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "job never started");
            std::thread::sleep(Duration::from_millis(20));
        };
        (pair.master, child, shell, job)
    }

    #[test]
    fn closing_a_terminal_ends_everything_it_started() {
        let d = tmp("close");
        let (_master, mut child, shell, job) = terminal(&d);
        // What Eiwaz used to do: SIGHUP the shell only.
        child.kill().unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(!is_running(shell), "shell should be gone");
        assert!(is_running(job), "the old kill left the nohup job running (the bug)");
        // What Eiwaz does now: end the whole session.
        let ended = end_sessions(&[Target::live(shell)], Duration::from_secs(2));
        println!("ended: {}", describe(&ended));
        assert!(ended.iter().any(|n| n == "sleep"));
        assert!(!is_running(job), "the job must not outlive its terminal");
    }

    #[test]
    fn sweep_ends_what_a_crashed_studio_left() {
        let d = tmp("sweep");
        let (master, child, shell, job) = terminal(&d);
        // A Eiwaz that crashed: its pid is dead, its terminal closed without cleanup.
        let mut gone = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let dead_pid = gone.id();
        gone.wait().unwrap();
        let ledger = json!({ dead_pid.to_string(): { "started": now() - 5, "sessions": [{ "sid": shell, "born": now() - 2 }] } });
        write_ledger(&d, &ledger);
        drop(master);
        drop(child);
        std::thread::sleep(Duration::from_millis(400));
        assert!(is_running(job), "precondition: the job survived the crash");

        let s = sweep(&d, Duration::from_secs(2));
        println!("sweep: {s:?}");
        assert!(s.unclean);
        assert!(s.ended.iter().any(|n| n == "sleep"));
        assert!(!is_running(job));
        assert!(read_ledger(&d).as_object().unwrap().is_empty(), "swept owners are forgotten");
    }

    #[test]
    fn sweep_leaves_a_running_studio_alone() {
        let d = tmp("live");
        let (_master, _child, shell, job) = terminal(&d);
        // Another Eiwaz instance (say, a dev build) is still running and owns this terminal.
        let mut owner = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let started = fresh_system().process(Pid::from_u32(owner.id())).unwrap().start_time();
        write_ledger(&d, &json!({ owner.id().to_string(): { "started": started, "sessions": [{ "sid": shell, "born": now() }] } }));
        let s = sweep(&d, Duration::from_secs(1));
        assert!(!s.unclean && s.ended.is_empty());
        assert!(is_running(job));
        let _ = owner.kill();
        end_sessions(&[Target::live(shell)], Duration::from_secs(1));
    }

    #[test]
    fn sweep_ignores_a_reused_leader_pid() {
        let d = tmp("reuse");
        let (_master, _child, shell, job) = terminal(&d);
        // The ledger says the terminal was born long after this shell started:
        // i.e. the recorded leader pid now belongs to a different, older process.
        let mut gone = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let dead_pid = gone.id();
        gone.wait().unwrap();
        write_ledger(&d, &json!({ dead_pid.to_string(): { "started": 1, "sessions": [{ "sid": shell, "born": now() + 3600 }] } }));
        let s = sweep(&d, Duration::from_secs(1));
        assert!(s.ended.is_empty(), "must not touch a session that only shares the id");
        assert!(is_running(job));
        end_sessions(&[Target::live(shell)], Duration::from_secs(1));
    }

    #[test]
    fn describe_groups_names() {
        let n = ["claude", "node", "node", "zsh"].map(String::from);
        assert_eq!(describe(&n), "claude, node ×2, zsh");
    }
}
