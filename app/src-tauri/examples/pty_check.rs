// Sanity check: open a login shell in a PTY like the app does, run a command, read the output.
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
fn main() {
    let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).unwrap();
    let mut cmd = CommandBuilder::new(std::env::var("SHELL").unwrap_or("/bin/zsh".into()));
    cmd.arg("-l");
    cmd.cwd(std::env::var("HOME").unwrap());
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut r = pair.master.try_clone_reader().unwrap();
    let mut w = pair.master.take_writer().unwrap();
    w.write_all(b"echo EIWAZ_OK $(command -v claude) $(command -v ollama); exit\r").unwrap();
    let mut out = String::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = r.read(&mut buf) { if n == 0 { break; } out.push_str(&String::from_utf8_lossy(&buf[..n])); }
    let _ = child.wait();
    let line = out.lines().filter(|l| l.contains("EIWAZ_OK") && !l.contains("echo")).last().unwrap_or("NOT FOUND");
    println!("{}", line.trim());
}
