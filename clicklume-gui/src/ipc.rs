//! Thin wrapper over `common::GuiToBackend` for the GUI side.
//!
//! All commands are JSON-encoded and sent over the same `/tmp/clicklume.sock`
//! Unix domain socket the backend already speaks. Two flavors:
//!   - `send` — fire-and-forget on a detached thread (use in normal UI flow)
//!   - `send_blocking` — synchronous, used during shutdown so the message
//!     actually lands before the process tears down

use common::GuiToBackend;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const SOCKET_PATH: &str = "/tmp/clicklume.sock";

pub fn send(cmd: GuiToBackend) -> std::io::Result<()> {
    try_send(&cmd, Some(Duration::from_millis(500)))
}

pub fn send_blocking(cmd: GuiToBackend) -> std::io::Result<()> {
    try_send(&cmd, Some(Duration::from_millis(750)))
}

fn try_send(cmd: &GuiToBackend, write_timeout: Option<Duration>) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    if let Some(t) = write_timeout {
        stream.set_write_timeout(Some(t))?;
    }
    let json = serde_json::to_vec(cmd)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    stream.write_all(&json)?;

    // Normal UI commands receive a full Status response. Reading it here
    // makes button presses deterministic and prevents the backend from
    // writing into a socket the UI has already discarded. A timeout is still
    // treated as success because the command itself was already delivered.
    stream.set_read_timeout(Some(write_timeout.unwrap_or(Duration::from_millis(250))))?;
    let mut response = [0u8; 4096];
    let _ = stream.read(&mut response);
    Ok(())
}
