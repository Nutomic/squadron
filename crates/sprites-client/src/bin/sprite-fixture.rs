// Test-fixture stand-in for the real `sprite` CLI: its contract is plain text
// on stdout/stderr, so the print lints do not apply here.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let program = PathBuf::from(args.first().cloned().unwrap_or_default());
    let log = match program.parent() {
        Some(parent) => parent.join("argv.log"),
        None => PathBuf::from("argv.log"),
    };
    append_log(&log, &args[1..]);
    let whole = args[1..].join(" ");
    if args.get(1).map(String::as_str) == Some("list") {
        print!("alpha-sprite  running  https://alpha.sprites.app\nbeta-sprite  running  https://beta.sprites.app\n");
        return;
    }
    if args.get(1).map(String::as_str) == Some("info") {
        let name = args.get(2).cloned().unwrap_or_default();
        print!("Name: {name}\nStatus: running\nURL: https://{name}-f1x2e.sprites.app\nurl-auth: public\n");
        return;
    }
    if whole.contains("failstdout") {
        // Failure round: emit stdout BEFORE failing so CliExit capture of the
        // stream can be asserted (the real CLI forwards both streams).
        println!("partial stdout before the crash");
        eprintln!("synthetic failure");
        std::process::exit(7);
    }
    if whole.contains("fail") {
        eprintln!("synthetic failure");
        std::process::exit(7);
    }
    if whole.contains("sleep 2") {
        thread::sleep(Duration::from_secs(2));
    }
}

fn append_log(path: &Path, args: &[String]) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{}", args.join(" "));
    }
}
