//! clicklume-cli — minimal CLI to send IPC commands to the clicklume backend
//! Usage: clicklume-cli {toggle|inc|dec|start|stop|quit}
//!
//! Connects to /tmp/clicklume.sock, sends a JSON GuiToBackend message, exits.

use common::GuiToBackend;
use std::io::Write;
use std::os::unix::net::UnixStream;

const SOCKET_PATH: &str = "/tmp/clicklume.sock";

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: clicklume-cli <toggle|inc|dec|start|stop|quit>");
        eprintln!("  toggle  — flip enabled state");
        eprintln!("  inc     — increase CPS");
        eprintln!("  dec     — decrease CPS");
        eprintln!("  start   — enable");
        eprintln!("  stop    — disable");
        eprintln!("  quit    — exit the backend");
        std::process::exit(1);
    });

    // Map CLI arg to GuiToBackend variant
    let msg = match arg.as_str() {
        "toggle" => GuiToBackend::Toggle,
        "inc" => GuiToBackend::IncreaseCps,
        "dec" => GuiToBackend::DecreaseCps,
        "start" => GuiToBackend::Start,
        "stop" => GuiToBackend::Stop,
        "quit" => GuiToBackend::Quit,
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(2);
        }
    };

    // Verify the backend is actually listening (not just that a stale socket
    // file exists). If the socket file is stale (left over from a crash),
    // remove it so the GUI's next auto-respawn can rebind cleanly.
    if UnixStream::connect(SOCKET_PATH).is_err() {
        let _ = std::fs::remove_file(SOCKET_PATH);
        eprintln!("clicklume backend not running (cannot connect to {SOCKET_PATH})");
        std::process::exit(3);
    }

    let mut stream = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to connect to {SOCKET_PATH}: {e}");
            std::process::exit(4);
        }
    };

    let json = match serde_json::to_vec(&msg) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("failed to serialize message: {e}");
            std::process::exit(5);
        }
    };

    if let Err(e) = stream.write_all(&json) {
        eprintln!("failed to write to socket: {e}");
        std::process::exit(6);
    }

    // Read response if any (best-effort, don't block forever)
    use std::io::Read;
    let mut buf = [0u8; 1024];
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => {
            if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                print!("{s}");
            }
        }
        _ => {}
    }

    // Exit cleanly
}
