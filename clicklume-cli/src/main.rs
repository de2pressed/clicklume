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
        eprintln!("usage: clicklume-cli <status|toggle|inc|dec|start|stop|quit>");
        eprintln!("  status  — get current status");
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
        "status" => GuiToBackend::GetStatus,
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

    // Connect to the backend socket. If the socket file is stale (left over
    // from a crash or killed backend), remove it so subsequent auto-respawns
    // can rebind cleanly.
    let mut stream = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("clicklume backend not running (cannot connect to {SOCKET_PATH}): {e}");
            std::process::exit(3);
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

    // Require a complete, valid reply so scripts can trust the exit status.
    use std::io::{BufRead, BufReader, Read};
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut response = String::new();
    let result = BufReader::new(stream.take(4096)).read_line(&mut response);
    if !matches!(result, Ok(n) if n > 0)
        || !matches!(
            serde_json::from_str::<common::BackendToGui>(&response),
            Ok(common::BackendToGui::Status { .. })
        )
    {
        eprintln!("backend did not return a valid response");
        std::process::exit(7);
    }
    print!("{response}");
}
