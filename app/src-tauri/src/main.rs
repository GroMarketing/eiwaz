#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Claude Code starts this same binary as its "do it on this Mac" tool server.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--local-mcp") && args.len() >= 5 {
        eiwaz_lib::local_mcp::serve(&args[2], &args[3], &args[4]);
        return;
    }
    eiwaz_lib::run()
}
