//! Custom Rust tool for the hugr-sqlite agent — ILLUSTRATIVE SKETCH.
//!
//! This is what "I need a tool the predefined library doesn't have" looks
//! like (ARCHITECTURE §20.3, path 3). The rules of the game:
//!
//! - A tool is an ordinary host `Capability` (ARCHITECTURE §7.1) — the same
//!   trait `shell`, `fs_read`, and every library tool implement. There are no
//!   privileged tools, so a custom tool is a first-class citizen: same
//!   permission path, same tracing, same replay story.
//! - It never touches `hugr-core`. The brain only ever sees the tool's
//!   `ToolSchema` and opaque JSON args/results (the narrow waist, §2.4), so a
//!   custom tool cannot require a core change by construction.
//! - The manifest owns the *scope* (which db file, read-only, row cap); the
//!   Rust owns the *mechanism*. The toolkit hands the `[tools.rust.sqlite.config]`
//!   table to the exported constructor below at artifact startup.

use hugr_host::{CapResult, Capability, ChunkSink, ToolSchema};
use serde_json::{json, Value};

/// The entry point named by the manifest. `hugr build` generates a shim that
/// calls every exported constructor with its manifest `config` table and
/// registers the returned capabilities — this is the whole integration
/// surface between the toolkit and custom Rust.
#[hugr_toolkit::export_tool] // illustrative attribute; resolution via the shim
pub fn sqlite(config: Value) -> Result<Vec<Box<dyn Capability>>, String> {
    let db = config["db"].as_str().ok_or("missing `db` in tool config")?;
    if config["read_only"].as_bool() != Some(true) {
        // The constructor is where a tool enforces its own privilege claims:
        // this crate simply refuses to exist in a writable configuration.
        return Err("hugr-sqlite-tools only supports read_only = true".into());
    }
    let max_rows = config["max_rows"].as_u64().unwrap_or(200) as usize;
    Ok(vec![
        Box::new(SqliteSchema::open(db)?),
        Box::new(SqliteQuery::open(db, max_rows)?),
    ])
}

/// `sqlite_schema` — list tables, columns, indexes. No arguments.
pub struct SqliteSchema {
    conn: rusqlite::Connection, // opened with OPEN_READ_ONLY | OPEN_NO_MUTEX
}

/// `sqlite_query` — run one SELECT. The read-only connection makes writes
/// impossible at the sqlite layer; on top of that we reject anything that
/// isn't a single SELECT/WITH statement (defense in depth: no ATTACH, no
/// PRAGMA, no multi-statement smuggling).
pub struct SqliteQuery {
    conn: rusqlite::Connection,
    max_rows: usize,
}

impl Capability for SqliteQuery {
    fn name(&self) -> String {
        "sqlite_query".into()
    }

    fn schema(&self) -> ToolSchema {
        // What the model sees. Declared read-only, so the host's permission
        // policy skips gating it — same treatment as the library fs_read.
        ToolSchema::new("sqlite_query")
            .description("Run a single read-only SELECT (or WITH…SELECT) against the database and return rows as JSON.")
            .read_only(true)
            .arg("sql", "string", "One SELECT statement. No writes, no PRAGMA, no ATTACH.")
    }

    fn invoke(&self, args: Value, _sink: &mut dyn ChunkSink) -> CapResult {
        let sql = args["sql"].as_str().unwrap_or_default();
        if !is_single_select(sql) {
            // A semantic error: shaped as a tool result so the brain routes
            // it back to the model, which rephrases and retries (§5.4).
            return CapResult::semantic_error(json!({
                "error": "only a single SELECT/WITH statement is allowed",
                "got": sql,
            }));
        }
        match run_select(&self.conn, sql, self.max_rows) {
            Ok((columns, rows, truncated)) => CapResult::done(json!({
                "columns": columns,
                "rows": rows,               // capped at max_rows from the manifest
                "truncated": truncated,     // never silent: the model is told
            })),
            Err(e) => CapResult::semantic_error(json!({ "error": e.to_string() })),
        }
    }
}

impl Capability for SqliteSchema {
    fn name(&self) -> String {
        "sqlite_schema".into()
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new("sqlite_schema")
            .description("List all tables with their columns, types, and indexes.")
            .read_only(true)
    }

    fn invoke(&self, _args: Value, _sink: &mut dyn ChunkSink) -> CapResult {
        // Walks sqlite_master + PRAGMA table_info per table (read-only
        // pragmas are queries, not writes) and returns one JSON document the
        // model can persist to its scratchpad.
        CapResult::done(schema_as_json(&self.conn))
    }
}

// --- mechanics elided: open_read_only(), is_single_select() (sqlite3_stmt
// --- introspection, not regex), run_select(), schema_as_json() ---
