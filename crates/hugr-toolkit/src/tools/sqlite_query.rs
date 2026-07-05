//! `sqlite_query` — a read-only, file-scoped SQLite tool (ROADMAP T1.2,
//! ARCHITECTURE §20.2). Privilege class: **read-only** (the database file is
//! opened `SQLITE_OPEN_READ_ONLY`, so writes fail at the engine).
//!
//! ```toml
//! [tools.sqlite_query]
//! file = "./expenses.db"
//! max_rows = 1000            # optional row cap (default 1000)
//! ```
//!
//! The connection is opened fresh per call (on a blocking thread) against the
//! one manifest-declared file.
//!
//! ## Sandbox hardening (ROADMAP T3.6)
//!
//! - **Second-file access is blocked.** `ATTACH DATABASE` would let a query
//!   reach a file outside the manifest scope even under a read-only main
//!   connection, so it is rejected before the query runs. The guard is
//!   **token-based** (`attach` as a whole SQL word) rather than a substring
//!   scan, so a column named `attachment` is not falsely rejected while a real
//!   `ATTACH` statement — in any casing or spacing — still is.
//! - **Symlink scope.** The manifest `file` is canonicalized at construction,
//!   so a symlinked path resolves to its real target once, up front (the tool
//!   is bound to that one resolved file for its lifetime).
//! - **Read-only engine.** The file is opened `SQLITE_OPEN_READ_ONLY`, so
//!   writes/DDL fail at the engine regardless of the SQL text.
//!
//! See `docs/THREAT_MODEL.md`. (This module is behind the `sqlite` cargo
//! feature; the guard is exercised by the unit tests below when it is enabled.)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use hugr_host::{Capability, ChunkSink};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;

const DEFAULT_MAX_ROWS: usize = 1000;

/// A read-only query tool bound to a single SQLite file.
pub struct SqliteQuery {
    file: Arc<PathBuf>,
    max_rows: usize,
}

impl SqliteQuery {
    /// Build from a manifest `[tools.sqlite_query]` config; `base_dir` resolves
    /// a relative `file`. The file must exist (opened read-only).
    pub fn from_config(config: &Value, base_dir: &Path) -> Result<Self> {
        let file = config
            .get("file")
            .and_then(Value::as_str)
            .context("[tools.sqlite_query] requires string `file`")?;
        let path = {
            let p = Path::new(file);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base_dir.join(p)
            }
        };
        let path = path
            .canonicalize()
            .with_context(|| format!("sqlite_query file not found: {}", path.display()))?;
        anyhow::ensure!(
            path.is_file(),
            "sqlite_query file is not a file: {}",
            path.display()
        );
        let max_rows = config
            .get("max_rows")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_ROWS)
            .max(1);
        Ok(Self {
            file: Arc::new(path),
            max_rows,
        })
    }
}

#[async_trait]
impl Capability for SqliteQuery {
    fn name(&self) -> &str {
        "sqlite_query"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "sqlite_query",
            "Run a read-only SQL query against the agent's SQLite database and return the result rows. The database is opened read-only; writes and ATTACH are rejected.",
            json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "A read-only SQL query (SELECT / WITH / PRAGMA / EXPLAIN)." }
                },
                "required": ["sql"],
                "additionalProperties": false
            }),
        )
    }

    fn requires_permission(&self) -> bool {
        false
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        let sql = match args.get("sql").and_then(Value::as_str) {
            Some(sql) => sql.to_string(),
            None => return Err(json!({ "error": "sqlite_query requires string `sql`" })),
        };
        let file = self.file.clone();
        let max_rows = self.max_rows;
        let result = tokio::task::spawn_blocking(move || run_query(&file, &sql, max_rows)).await;
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(json!({ "error": error.to_string() })),
            Err(join) => Err(json!({ "error": format!("query task failed: {join}") })),
        }
    }
}

/// Whether `sql` uses `attach` as a SQL keyword (whole word, any casing),
/// rather than merely containing the substring (e.g. a column `attachment`).
fn mentions_attach_keyword(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|token| token == "attach")
}

fn run_query(file: &Path, sql: &str, max_rows: usize) -> Result<Value> {
    if mentions_attach_keyword(sql) {
        anyhow::bail!("ATTACH is not permitted (sqlite_query is scoped to one file)");
    }
    let conn = Connection::open_with_flags(
        file,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", file.display()))?;

    let mut stmt = conn.prepare(sql).context("preparing query")?;
    let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let col_count = columns.len();

    let mut rows = stmt.query([]).context("executing query")?;
    let mut out_rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = rows.next().context("reading row")? {
        if out_rows.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut obj = serde_json::Map::with_capacity(col_count);
        for (i, name) in columns.iter().enumerate() {
            obj.insert(name.clone(), value_ref_to_json(row.get_ref(i)?));
        }
        out_rows.push(Value::Object(obj));
    }
    Ok(json!({
        "columns": columns,
        "rows": out_rows,
        "row_count": out_rows.len(),
        "truncated": truncated,
    }))
}

fn value_ref_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(bytes) => json!(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => json!({ "blob_bytes": bytes.len() }),
    }
}

#[cfg(test)]
mod tests {
    use super::mentions_attach_keyword;

    #[test]
    fn attach_keyword_is_detected_across_casing_and_spacing() {
        assert!(mentions_attach_keyword("ATTACH DATABASE 'x' AS y"));
        assert!(mentions_attach_keyword("  attach   database 'x'"));
        assert!(mentions_attach_keyword("select 1;\nAttAch database 'x'"));
    }

    #[test]
    fn substring_attach_in_an_identifier_is_not_a_false_positive() {
        // A column/table named `attachment` must not trip the guard.
        assert!(!mentions_attach_keyword("SELECT attachment FROM emails"));
        assert!(!mentions_attach_keyword("SELECT id, attachments_count FROM t"));
    }
}
