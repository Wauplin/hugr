//! Newline-delimited JSON framing shared by stdio transports (e.g. the host's
//! MCP client and `--mcp-serve`).
//!
//! Callers keep their own protocol semantics (what a message *means*) and error
//! taxonomy — a [`FramingError`] splits cleanly into an IO half and a JSON half
//! so it maps onto whatever error enum the caller already has.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, Lines};

/// A framing-level failure: either the underlying stream broke (IO) or a line
/// was not valid JSON for the expected type.
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    /// Reading from or writing to the underlying stream failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Serializing the outgoing value or parsing an incoming line failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Serialize `value` as a single JSON line (`<json>\n`), write it, and flush.
pub async fn write_json_line<W, T>(writer: &mut W, value: &T) -> Result<(), FramingError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read the next non-blank line and parse it as JSON. Blank lines (including
/// lines that are only whitespace/CR) are skipped; `Ok(None)` signals EOF.
/// `Lines::next_line` already strips the trailing `\n`/`\r\n`.
pub async fn read_json_line<R, T>(lines: &mut Lines<R>) -> Result<Option<T>, FramingError>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_str(&line)?));
    }
    Ok(None)
}

/// Like [`read_json_line`], but skip lines that are not valid JSON instead of
/// failing. For reading from processes we do not control (an MCP server that
/// prints a banner or log line to stdout must not fail the whole session);
/// our own `--mcp-serve` loop stays strict.
pub async fn read_json_line_lenient<R, T>(lines: &mut Lines<R>) -> Result<Option<T>, FramingError>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(value) => return Ok(Some(value)),
            Err(_) => {
                eprintln!("mcp: ignoring non-JSON stdout line: {line}");
            }
        }
    }
    Ok(None)
}
