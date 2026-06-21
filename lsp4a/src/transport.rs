//! LSP framing: JSON-RPC bodies prefixed with `Content-Length` headers.
//!
//! This is the Rust mirror of the Python testbed's `Framer`. The body is read
//! by exact byte count (never line-by-line) because the JSON can contain
//! newlines. Headers are ASCII, terminated by `\r\n`, with a blank line before
//! the body.

use std::io::{BufRead, Write};
// `Read::read_exact` is reachable via the `R: BufRead` supertrait bound, so the
// `Read` trait does not need to be imported explicitly.

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Write one framed message: `Content-Length: N\r\n\r\n` + N bytes of JSON.
pub fn send_message<W: Write>(w: &mut W, msg: &Value) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Read one framed message: parse headers until the blank line, then read
/// exactly `Content-Length` bytes and decode them as JSON.
pub fn read_message<R: BufRead>(reader: &mut R) -> Result<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            bail!("server closed stdout before sending a complete frame");
        }
        let trimmed = line.trim_end(); // strips the trailing \r\n
        if trimmed.is_empty() {
            break; // blank line ends the header block
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse().context("bad Content-Length")?);
        }
    }
    let len = content_length.context("frame had no Content-Length header")?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).context("frame body was not valid JSON")
}
