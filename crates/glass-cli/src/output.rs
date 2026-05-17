//! CLI output framework.
//!
//! Every verb produces a structured result that flows through here
//! on its way to stdout. Three formats:
//!
//!   * `json` (default) — single JSON object with `data` + `meta`
//!     envelope. Errors go to a parallel `error` shape on stderr +
//!     non-zero exit.
//!   * `ndjson` — one JSON object per line. Useful for huge result
//!     lists that would otherwise blow up `jq`'s memory.
//!   * `text` — human-readable rendering. Per-verb; falls back to
//!     pretty-printed JSON if the verb doesn't implement a text
//!     formatter.
//!
//! Addresses serialise as `"0x..."` strings throughout; raw `u64`
//! would overflow JS number precision once we cross the 2^53
//! threshold.

// First consumers land in #78 and following; module is intentionally
// complete up-front so verbs don't each invent their own emit.
#![allow(dead_code)]

use std::io::Write;
use std::time::Instant;

use anyhow::Result;
use clap::ValueEnum;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Default)]
#[clap(rename_all = "lowercase")]
pub enum Format {
    /// One JSON object per invocation, with `data` + `meta`.
    #[default]
    Json,
    /// One JSON object per line. Lossless re. data but no
    /// envelope; suitable for streaming and `jq` pipelines.
    Ndjson,
    /// Human-readable text. Best-effort; falls back to
    /// pretty-printed JSON when the verb has no text formatter.
    Text,
}

/// The standard CLI envelope. `data` is the verb's typed payload;
/// `meta` carries timing + any per-verb metadata (counts, paging
/// state, etc.).
#[derive(Serialize)]
pub struct Envelope<T: Serialize> {
    pub data: T,
    pub meta: Meta,
}

#[derive(Serialize, Default)]
pub struct Meta {
    pub duration_ms: u128,
}

/// Render an `Envelope` to stdout in the requested format. The
/// `text_renderer` closure produces the verb-specific human output;
/// pass a no-op closure to fall back to pretty JSON.
pub fn emit<T, F>(envelope: Envelope<T>, format: Format, text_renderer: F) -> Result<()>
where
    T: Serialize,
    F: FnOnce(&T, &mut dyn Write) -> std::io::Result<()>,
{
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match format {
        Format::Json => {
            serde_json::to_writer(&mut out, &envelope)?;
            writeln!(&mut out)?;
        }
        Format::Ndjson => {
            // ndjson elides the envelope. Caller emits the
            // payload as one object per line via `emit_ndjson`;
            // this branch is taken when the verb doesn't have
            // per-row output, so the envelope still goes out as
            // a single line for consistency.
            serde_json::to_writer(&mut out, &envelope)?;
            writeln!(&mut out)?;
        }
        Format::Text => {
            text_renderer(&envelope.data, &mut out)
                .or_else(|_| {
                    let pretty = serde_json::to_string_pretty(&envelope)?;
                    writeln!(&mut out, "{}", pretty)?;
                    Ok::<_, std::io::Error>(())
                })?;
        }
    }
    Ok(())
}

/// Streaming variant — emit one row per line as ndjson. The caller
/// is responsible for the meta envelope (if any).
#[allow(dead_code)] // wired up once the first large-result verb lands.
pub fn emit_ndjson<T, I>(rows: I) -> Result<()>
where
    T: Serialize,
    I: IntoIterator<Item = T>,
{
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for row in rows {
        serde_json::to_writer(&mut out, &row)?;
        writeln!(&mut out)?;
    }
    Ok(())
}

/// Convenience: time a closure, return the result wrapped in an
/// `Envelope` with `duration_ms` populated.
pub fn measured<T, F>(f: F) -> Result<Envelope<T>>
where
    T: Serialize,
    F: FnOnce() -> Result<T>,
{
    let start = Instant::now();
    let data = f()?;
    let duration_ms = start.elapsed().as_millis();
    Ok(Envelope {
        data,
        meta: Meta { duration_ms },
    })
}

/// Error envelope — single shape we emit to stderr on failure.
#[derive(Serialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Serialize)]
pub struct ErrorBody {
    pub message: String,
}

/// Print an error in the requested format and return a process
/// exit code. Use as the catch-all in `main`.
pub fn emit_error(err: &anyhow::Error, format: Format) -> i32 {
    let body = ErrorBody {
        message: format!("{:#}", err),
    };
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    match format {
        Format::Json | Format::Ndjson => {
            let _ =
                serde_json::to_writer(&mut out, &ErrorEnvelope { error: body });
            let _ = writeln!(&mut out);
        }
        Format::Text => {
            let _ = writeln!(&mut out, "error: {}", body.message);
        }
    }
    1
}
