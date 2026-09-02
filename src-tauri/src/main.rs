// NOBS SQL Editor - a cross-platform MySQL/MariaDB client.
// Copyright (C) 2026 Viktor Ljuca <https://monsama.ch>
//
// This program is free software; you can redistribute it and/or modify it
// under the terms of the GNU General Public License as published by the Free
// Software Foundation; either version 2 of the License, or (at your option)
// any later version.
//
// This program is distributed in the hope that it will be useful, but
// WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY
// or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License
// for more details.
//
// You should have received a copy of the GNU General Public License along
// with this program; if not, see <https://www.gnu.org/licenses/>. A copy is
// in the LICENSE file at the root of this repository.

// NOBS SQL Editor - Tauri (Rust) backend
// Cross-platform desktop app. Uses the `mysql` driver for typed results
// (real NULL, proper bit/binary handling) and shells out to mysql/mysqldump
// only for dump-style export/import (which need DELIMITER handling).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use mysql::prelude::*;
use mysql::{Conn, Opts, OptsBuilder, SslOpts, Value as MyValue, Column, Row};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use tauri::Manager;
use tauri::Emitter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
static EXPORT_CANCEL: AtomicBool = AtomicBool::new(false);
// Marks a run() that ended because the user pressed Cancel, so the caller can log it as a
// cancellation rather than a failure. A NUL cannot appear in a mysqldump message.
const RUN_CANCELLED: &str = "\u{0}cancelled";

// ---------- cancellable Export / Import jobs ----------
// One "job" is a single Export or Import run started from the UI, identified by the jobId the
// UI generates and passes in. Such a run shells out to mysqldump/mysql once per database, table
// or file, so cancelling has to do two things: stop the loop before it starts the next child,
// and kill the child already running. Stopping only between children would leave a large single
// dump writing for minutes after the user pressed Cancel, which is what the Cancel button
// appeared to do before this existed. Mirrors the PowerShell backend's Api-CancelJob.
struct Job {
    cancelled: AtomicBool,
    // The child currently running for this job, if any. Held in its own lock so the cancelling
    // thread can reach it while the worker thread is polling it.
    child: Mutex<Option<std::process::Child>>,
}
static JOBS: OnceLock<Mutex<std::collections::HashMap<String, std::sync::Arc<Job>>>> = OnceLock::new();
fn jobs() -> &'static Mutex<std::collections::HashMap<String, std::sync::Arc<Job>>> {
    JOBS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}
fn job_start(id: &str) -> Option<std::sync::Arc<Job>> {
    if id.is_empty() { return None; }
    let j = std::sync::Arc::new(Job { cancelled: AtomicBool::new(false), child: Mutex::new(None) });
    jobs().lock().ok()?.insert(id.to_string(), j.clone());
    Some(j)
}
fn job_is_cancelled(job: &Option<std::sync::Arc<Job>>) -> bool {
    job.as_ref().map(|j| j.cancelled.load(Ordering::SeqCst)).unwrap_or(false)
}
// Deregisters on every exit path, including the `?` early returns inside the worker closure -
// a job left in the map would make a later cancel look like it succeeded against a dead run.
struct JobGuard(String);
impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.0.is_empty() { if let Ok(mut m) = jobs().lock() { m.remove(&self.0); } }
    }
}

// Runs one child process as part of `job`, returning what Command::output() would have.
// Two differences from output(), both required here:
//   - the Child is parked in the job so cancel_job can kill it mid-run;
//   - the wait is a poll rather than a blocking wait(), because holding the child's lock across
//     a blocking wait would make the cancelling thread wait for the process it wants to kill.
// stderr is drained on its own thread for the reason output() does the same internally: a child
// that fills the stderr pipe buffer blocks forever if nobody is reading it.
// Stops a child export/import process.
//
// On Windows this delegates to taskkill instead of Child::kill(). Calling TerminateProcess on
// our own child handle, while mysqldump was actively writing a large dump, took the entire
// application down - no window, no process, and no Rust panic message, so an abort rather than
// a panic. Confirmed by A/B: a build with the kill removed survives the same cancel, one with
// it does not. Cancelling between tables was always fine, because no kill happens there; only
// interrupting a dump in flight reaches this.
//
// taskkill runs the terminate in its own process, and /T takes any grandchildren with it.
// CREATE_NO_WINDOW keeps a console from flashing over the app on every cancel.
#[cfg(windows)]
fn kill_child(c: &mut std::process::Child) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let pid = c.id();
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
#[cfg(not(windows))]
fn kill_child(c: &mut std::process::Child) { let _ = c.kill(); }

fn run_job_child(job: Option<&std::sync::Arc<Job>>, cmd: &mut Command) -> std::io::Result<std::process::Output> {
    let mut child = cmd.stderr(Stdio::piped()).spawn()?;
    let mut pipe = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = pipe.as_mut() { use std::io::Read; let _ = p.read_to_end(&mut buf); }
        buf
    });
    let status = match job {
        None => child.wait()?,
        Some(j) => {
            if let Ok(mut slot) = j.child.lock() { *slot = Some(child); }
            loop {
                let mut finished = None;
                if let Ok(mut slot) = j.child.lock() {
                    if let Some(c) = slot.as_mut() {
                        if j.cancelled.load(Ordering::SeqCst) { kill_child(c); }
                        finished = c.try_wait()?;
                    } else {
                        // Nothing parked: treat as finished rather than spinning forever.
                        break std::process::ExitStatus::default();
                    }
                }
                if let Some(st) = finished {
                    if let Ok(mut slot) = j.child.lock() { *slot = None; }
                    break st;
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
        }
    };
    // After a kill the child's stderr is of no interest, and anything it spawned that inherited
    // the pipe can hold it open long after the kill - joining then would block for exactly as
    // long as cancelling was meant to save. Leave that reader to finish on its own.
    let stderr = if job.map(|j| j.cancelled.load(Ordering::SeqCst)).unwrap_or(false) {
        Vec::new()
    } else {
        reader.join().unwrap_or_default()
    };
    Ok(std::process::Output { status, stdout: Vec::new(), stderr })
}
// Tracks in-flight SELECT queries so Cancel can stop them server-side, the same way MySQL
// Workbench does it: keep the query's own MySQL CONNECTION_ID(), and to cancel, open a brand
// new connection and run KILL QUERY <id> on it (you can't cancel over the same connection
// that's busy running the query - it has to come from elsewhere).
fn running_queries() -> &'static Mutex<std::collections::HashMap<String, (u64, Value)>> {
    static MAP: OnceLock<Mutex<std::collections::HashMap<String, (u64, Value)>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

// A simple thread-safe set of requestIds the user has asked to cancel. Compare operations run
// MANY sequential queries (one per table/chunk) rather than one big one, so instead of trying to
// kill whichever single sub-query happens to be in flight, each loop just checks this set
// between iterations and stops cleanly if its requestId shows up here.
fn cancelled_compares() -> &'static Mutex<std::collections::HashSet<String>> {
    static SET: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
// Request ids whose query was actually killed by a Cancel click. The PowerShell build keeps
// this as a Cancelled flag on its RunningQueries entry; this port dropped it and inferred
// cancellation from "a requestId was supplied", which is true of EVERY query the editor runs -
// so every failure was reported as a cancel and the real error was thrown away.
fn cancelled_queries() -> &'static Mutex<std::collections::HashSet<String>> {
    static C: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
fn mark_query_cancelled(rid: &str) { if let Ok(mut s) = cancelled_queries().lock() { s.insert(rid.to_string()); } }
// Consumes the marker: a later query reusing the id (ids are per-run uuids, so this is really
// just hygiene) must not inherit it.
fn take_query_cancelled(rid: &str) -> bool {
    cancelled_queries().lock().map(|mut s| s.remove(rid)).unwrap_or(false)
}
fn is_compare_cancelled(rid: &str) -> bool { cancelled_compares().lock().unwrap().contains(rid) }
fn clear_compare_cancel(rid: &str) { cancelled_compares().lock().unwrap().remove(rid); }

// Mirrors running_queries() above, but a compare step can have TWO connections (source + target)
// live under the same requestId at once, so this holds a connection_id + conn-info pair PER
// connection rather than one. Without this, Cancel only ever set the cooperative flag above,
// which a single un-chunked SELECT (get_rows_by_pk's fetch, or either side's plain PK scan) can't
// see until it finishes on its own - so Stop looked like it did nothing on any table big enough
// for that one query to take a while, and closing the Compare Databases dialog mid-scan left it
// running in the background for the same reason.
fn running_compare_conns() -> &'static Mutex<std::collections::HashMap<String, Vec<(u64, Value)>>> {
    static MAP: OnceLock<Mutex<std::collections::HashMap<String, Vec<(u64, Value)>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}
fn register_compare_conn(rid: &Option<String>, conn: &mut Conn, connj: &Value) {
    let Some(r) = rid else { return };
    if let Ok((_c, rows)) = run_select(conn, "SELECT CONNECTION_ID()") {
        if let Some(cid) = rows.get(0).and_then(|row| row.get(0)).cloned().flatten().and_then(|s| s.parse::<u64>().ok()) {
            running_compare_conns().lock().unwrap().entry(r.clone()).or_default().push((cid, connj.clone()));
        }
    }
}
fn unregister_compare_conns(rid: &Option<String>) {
    if let Some(r) = rid { running_compare_conns().lock().unwrap().remove(r); }
}
// RAII guard so the registration above is always cleaned up - compare_rows/compare_rows_diff
// have several early `?`/return paths, and leaving a stale entry behind would let a LATER,
// unrelated request that happens to reuse this id inherit connections that no longer exist.
struct CompareConnGuard(Option<String>);
impl Drop for CompareConnGuard {
    fn drop(&mut self) { unregister_compare_conns(&self.0); }
}

#[tauri::command]
async fn compare_cancel(req: Value) -> R {
    if let Some(rid) = req["requestId"].as_str() {
        if !rid.is_empty() {
            cancelled_compares().lock().unwrap().insert(rid.to_string());
            // Set BEFORE the kill lands, same ordering as compare_rows/compare_rows_diff's own
            // checkpoints, so a query that dies from the KILL below is seen as "cancelled" by
            // whichever loop is waiting on it rather than surfacing as a real error.
            let conns = running_compare_conns().lock().unwrap().get(rid).cloned();
            if let Some(conns) = conns {
                tokio::task::spawn_blocking(move || {
                    for (cid, connj) in conns {
                        if let Ok(mut kc) = build_conn(&connj) { let _ = kc.query_drop(format!("KILL QUERY {}", cid)); }
                    }
                }).await.ok();
            }
        }
    }
    Ok(json!({"ok":true}))
}

type R = Result<Value, String>;

// ---------- connection ----------
fn build_conn(connj: &Value) -> Result<Conn, String> {
    let host = connj["host"].as_str().unwrap_or("127.0.0.1").to_string();
    let port: u16 = connj["port"].as_str().and_then(|s| s.parse().ok())
        .or_else(|| connj["port"].as_u64().map(|v| v as u16)).unwrap_or(3306);
    let user = connj["user"].as_str().unwrap_or("root").to_string();
    let pass = connj["password"].as_str().unwrap_or("").to_string();
    let ssl = connj["ssl"].as_str().unwrap_or("default");
    let mut ob = OptsBuilder::new()
        .ip_or_hostname(Some(host)).tcp_port(port)
        .user(Some(user)).pass(Some(pass))
        .tcp_connect_timeout(Some(std::time::Duration::from_secs(10)));
    match ssl {
        "required" => { ob = ob.ssl_opts(Some(SslOpts::default().with_danger_accept_invalid_certs(true))); }
        "verify"   => { ob = ob.ssl_opts(Some(SslOpts::default())); }
        _ => {}
    }
    Conn::new(Opts::from(ob)).map_err(|e| e.to_string())
}

// ---------- value conversion (typed -> Option<String>) ----------
fn is_binaryish(c: &Column) -> bool {
    use mysql::consts::ColumnType::*;
    match c.column_type() {
        // BIT is always rendered as a hex literal
        MYSQL_TYPE_BIT => true,
        // BLOB/TEXT and CHAR/VARCHAR share type codes; only charset 63 (binary) is real binary data.
        // TEXT columns (e.g. the "OK" status from CHECK/ANALYZE TABLE) have a real charset -> keep as text.
        MYSQL_TYPE_BLOB | MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR | MYSQL_TYPE_GEOMETRY
            => c.character_set() == 63,
        _ => false,
    }
}
// is_binaryish() above answers "does this column display/round-trip as a 0x.. hex literal" -
// true for BOTH BIT and real binary (BLOB/VARBINARY/etc) columns, which is all display needs to
// know. But the two have different VALID WRITE syntax in MySQL: a bare integer literal
// (`INSERT ... VALUES (8)`) is a correct, unambiguous way to set a BIT column - MySQL treats it
// as the bit pattern, not as text - while the same bare integer sent to a true binary/BLOB column
// would store the bytes of the digit character itself. The editor's write-validation needs this
// finer distinction (see applyChanges() client-side, and the bitCols field in query()'s
// response) so it can accept a plain "0"/"1" for a BIT flag column - the overwhelmingly common
// case - without also opening the door to that byte-corruption mistake on a real binary column.
fn is_bit_col(c: &Column) -> bool {
    matches!(c.column_type(), mysql::consts::ColumnType::MYSQL_TYPE_BIT)
}
fn val_to_opt(v: &MyValue, binaryish: bool) -> Option<String> {
    match v {
        MyValue::NULL => None,
        MyValue::Bytes(b) => {
            if binaryish { return Some(format!("0x{}", hex::encode(b))); }
            match std::str::from_utf8(b) {
                Ok(s) => Some(s.to_string()),
                Err(_) => Some(format!("0x{}", hex::encode(b))),
            }
        }
        MyValue::Int(i) => Some(i.to_string()),
        MyValue::UInt(u) => Some(u.to_string()),
        MyValue::Float(f) => Some(f.to_string()),
        MyValue::Double(d) => Some(d.to_string()),
        MyValue::Date(y, mo, d, h, mi, s, us) => Some(if *us > 0 {
            format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}", y, mo, d, h, mi, s, us)
        } else { format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, mi, s) }),
        MyValue::Time(neg, d, h, mi, s, us) => {
            let sign = if *neg { "-" } else { "" };
            let hh = (*d) * 24 + (*h as u32);
            Some(if *us > 0 { format!("{}{}:{:02}:{:02}.{:06}", sign, hh, mi, s, us) }
                 else { format!("{}{}:{:02}:{:02}", sign, hh, mi, s) })
        }
    }
}

// The mysql crate's Display for a server error is its Debug-ish wrapper,
// "MySqlError { ERROR 1146 (42S02): Table 'x' doesn't exist }". Users see these now that real
// errors are no longer masked as cancellations, and the wrapper is noise - the PowerShell build
// shows the bare "ERROR 1146 (42S02): ..." line. Strip it, leaving anything unrecognised alone.
fn db_err(e: impl std::string::ToString) -> String {
    let s = e.to_string();
    s.strip_prefix("MySqlError { ").and_then(|r| r.strip_suffix(" }"))
        .map(|r| r.to_string()).unwrap_or(s)
}

// Which columns are binary/BIT is decided here for display encoding; run_select_bin also hands
// it back so the grid can refuse to write a decimal into one. Typing 8 into a BIT(8) cell stored
// 56 - the byte value of the character '8' - with no error at all.
fn run_select_bin(conn: &mut Conn, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>, Vec<bool>), String> {
    let mut result = conn.query_iter(sql).map_err(db_err)?;
    let cols: Vec<String> = result.columns().as_ref().iter().map(|c| c.name_str().to_string()).collect();
    let bin: Vec<bool> = result.columns().as_ref().iter().map(|c| is_binaryish(c)).collect();
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    for r in result.by_ref() {
        let row = r.map_err(db_err)?;
        rows.push(decode_row(&row, &bin));
    }
    Ok((cols, rows, bin))
}

fn run_select(conn: &mut Conn, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), String> {
    let (c, r, _) = run_select_bin(conn, sql)?;
    Ok((c, r))
}

// Decodes one already-fetched row into the grid's Option<String> cell format. Shared by
// run_select_bin (whole-result-set reads used everywhere except ad-hoc queries) and the cursor
// thread below (which reads a bounded batch at a time from a query it keeps open across
// multiple Tauri command calls). `bin` is computed once per query, not per row/batch.
fn decode_row(row: &Row, bin: &[bool]) -> Vec<Option<String>> {
    let mut cells = Vec::with_capacity(bin.len());
    for i in 0..bin.len() {
        let v = row.as_ref(i).cloned().unwrap_or(MyValue::NULL);
        cells.push(val_to_opt(&v, *bin.get(i).unwrap_or(&false)));
    }
    cells
}

// ---------- ad-hoc query result cursors ----------
// Ad-hoc queries from the editor (the "Run" button and "Fetch next N rows") are streamed through
// a cursor rather than materialized in one shot: without this, a query with a high or missing
// LIMIT against a large table gets fully read into memory (once in the driver's row buffers,
// again in a Vec, again in the serde_json::Value tree, again in the serialized IPC payload) - on
// a multi-GB table that exhausts the process's memory and the allocator aborts the whole app
// rather than returning an error. A cursor only ever pulls one page's worth of rows into memory
// at a time, regardless of how many total rows the query matches or what LIMIT (if any) the user
// wrote, and - unlike re-running the query with an increasing OFFSET - it never makes MySQL
// rescan and discard everything before the current page: the same open result set is read
// incrementally across calls.
//
// A cursor is a live mysql::Conn + its still-open mysql::QueryResult, which cannot be split
// across two separate Tauri command invocations without becoming a self-referential struct
// (QueryResult borrows &mut Conn). Instead both live together in the stack frame of one
// dedicated OS thread that outlives any single command call: the thread opens the query once,
// then blocks on a channel between command calls, fetching another page whenever asked and
// exiting (dropping QueryResult then Conn, closing the connection) once the result set is
// exhausted, it's told to close, or nobody has asked for more in a while.
enum CursorCmd {
    Fetch { n: usize, reply: std::sync::mpsc::Sender<Result<CursorBatch, String>> },
    Close,
}
struct CursorBatch {
    rows: Vec<Vec<Option<String>>>,
    has_more: bool,
}
// Registry of open cursors, keyed by a server-generated cursorId: the frontend only ever holds
// the id, never the sender itself, and looks it up here on every subsequent fetch/close. Same
// OnceLock<Mutex<HashMap<...>>> idiom as running_queries()/cancelled_queries() above.
fn cursors() -> &'static Mutex<std::collections::HashMap<String, std::sync::mpsc::Sender<CursorCmd>>> {
    static MAP: OnceLock<Mutex<std::collections::HashMap<String, std::sync::mpsc::Sender<CursorCmd>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}
// No uuid crate in this project's Cargo.toml, and nothing here needs global uniqueness beyond
// "never collides with another cursor this process has open" - a monotonic counter plus the
// wall-clock time it was minted is simpler than pulling in a dependency for it.
fn next_cursor_id() -> String {
    static COUNTER: OnceLock<std::sync::atomic::AtomicU64> = OnceLock::new();
    let n = COUNTER.get_or_init(|| std::sync::atomic::AtomicU64::new(0)).fetch_add(1, Ordering::SeqCst);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("cur{}_{}", now, n)
}

// Spawns the cursor's dedicated thread. `conn` is moved in and never touched again outside it.
// Returns a receiver that fires exactly once with the result of OPENING the query (columns +
// which are binary, or the error `conn.query_iter` failed with), and the sender used for every
// Fetch/Close for the lifetime of the cursor.
fn spawn_cursor_thread(conn: Conn, sql: String, cursor_id: String, request_id: Option<String>) -> (
    std::sync::mpsc::Receiver<Result<(Vec<String>, Vec<bool>, Vec<bool>), String>>,
    std::sync::mpsc::Sender<CursorCmd>,
) {
    let (open_tx, open_rx) = std::sync::mpsc::channel::<Result<(Vec<String>, Vec<bool>, Vec<bool>), String>>();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<CursorCmd>();
    std::thread::spawn(move || {
        let mut conn = conn;
        // Deregisters this cursor from both cursors() and, if the caller supplied one,
        // running_queries() - called from every exit path below. query()'s own registration of
        // requestId -> connection id (made before the cursor ever opens, so Cancel can find it
        // immediately) is deliberately NOT removed by query() itself once a cursor is going to
        // outlive that call: ownership of "when is this requestId no longer cancellable" passes
        // to the cursor thread, so Cancel keeps working against the same connection for as long
        // as the cursor stays open - including while idle between "fetch next" calls, and while
        // a slow fetch is in flight - not just during the very first page.
        let cleanup = |request_id: &Option<String>| {
            if let Ok(mut m) = cursors().lock() { m.remove(&cursor_id); }
            if let Some(rid) = request_id { running_queries().lock().unwrap().remove(rid); }
        };
        // Conn and its still-open QueryResult live together in this one stack frame for the
        // entire life of the cursor - the only way to avoid QueryResult's self-referential
        // borrow of Conn across separate command invocations.
        let mut result = match conn.query_iter(&sql) {
            Ok(r) => r,
            Err(e) => { let _ = open_tx.send(Err(db_err(e))); cleanup(&request_id); return; }
        };
        let cols: Vec<String> = result.columns().as_ref().iter().map(|c| c.name_str().to_string()).collect();
        let bin: Vec<bool> = result.columns().as_ref().iter().map(|c| is_binaryish(c)).collect();
        let bit: Vec<bool> = result.columns().as_ref().iter().map(|c| is_bit_col(c)).collect();
        if open_tx.send(Ok((cols.clone(), bin.clone(), bit.clone()))).is_err() { cleanup(&request_id); return; } // caller went away

        loop {
            // Idle safety net: an abandoned cursor (tab closed without the UI's cleanup call
            // reaching the backend, app crashed, etc.) can't hold its DB connection open forever.
            match cmd_rx.recv_timeout(std::time::Duration::from_secs(600)) {
                Ok(CursorCmd::Fetch { n, reply }) => {
                    // Pull n+1 so "is there more?" is answered from this same fetch, with no
                    // separate round trip - the extra row is dropped before replying.
                    let mut rows = Vec::new();
                    let mut has_more = false;
                    let mut err: Option<String> = None;
                    let mut count = 0usize;
                    while count < n + 1 {
                        match result.next() {
                            Some(Ok(row)) => {
                                count += 1;
                                if count <= n { rows.push(decode_row(&row, &bin)); }
                                else { has_more = true; }
                            }
                            Some(Err(e)) => { err = Some(db_err(e)); break; }
                            None => break,
                        }
                    }
                    let done = err.is_some() || !has_more;
                    let resp = match err { Some(e) => Err(e), None => Ok(CursorBatch { rows, has_more }) };
                    let _ = reply.send(resp);
                    if done { break; }
                }
                Ok(CursorCmd::Close) => break,
                Err(_) => break, // idle timeout, or every Sender (incl. the registry's) was dropped
            }
        }
        // Exhaustion, Close, and timeout all land here: deregister first (a stale entry would
        // make a later fetch/close look like it's still talking to a live cursor), then let
        // `result` and `conn` drop, closing the connection.
        cleanup(&request_id);
    });
    (open_rx, cmd_tx)
}

// Opens a new cursor and fetches its first page in one call - the common path for the "Run"
// button, whether or not the result ends up needing more than one page. `conn` is consumed: it
// belongs to the cursor thread from here on, not to the caller. Returns the cursorId (only
// meaningful if has_more is also true - see below), columns, binary-column flags, the first
// page of rows, and has_more. `request_id`, if supplied, is the same id query() registered in
// running_queries() before calling this - handed to the cursor thread so it (not this function's
// caller) can eventually clear that registration once the cursor genuinely closes.
fn open_cursor(conn: Conn, sql: String, first_n: usize, request_id: Option<String>) -> Result<(String, Vec<String>, Vec<bool>, Vec<bool>, Vec<Vec<Option<String>>>, bool), String> {
    let cursor_id = next_cursor_id();
    let (open_rx, cmd_tx) = spawn_cursor_thread(conn, sql, cursor_id.clone(), request_id);
    let (cols, bin, bit) = match open_rx.recv() {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("Cursor thread failed to start.".to_string()),
    };
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    if cmd_tx.send(CursorCmd::Fetch { n: first_n, reply: reply_tx }).is_err() {
        return Err("Cursor closed unexpectedly.".to_string());
    }
    let batch = match reply_rx.recv() {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("Cursor closed unexpectedly.".to_string()),
    };
    // Whole result fit in one page (or there were no columns at all, e.g. a statement with no
    // result set): the thread has already exhausted it, deregistered itself, and exited - so
    // there is nothing left to register here, and no cursorId the frontend should hold onto.
    if !cols.is_empty() && batch.has_more {
        cursors().lock().unwrap().insert(cursor_id.clone(), cmd_tx);
    }
    Ok((cursor_id, cols, bin, bit, batch.rows, batch.has_more))
}

// ---------- SQL text helpers (identifier + literal, Workbench-style + hex rule) ----------
fn sql_id(name: &str) -> String { format!("`{}`", name.replace('`', "``")) }
// Mirrors the PowerShell version's Test-SqlReadOnly: strips /* */, --, and # comments, then
// every statement's leading keyword must be on the allow-list for the SQL to be read-only.
// This is the server-side enforcement backing a connection's "read-only / safe mode" flag.
// Removes every balanced (...) group, tracking nesting depth so this is correct for parentheses
// nested inside parentheses (unlike a regex, which can't do that). Used by sql_is_readonly to see
// past a CTE's own body (or a subquery's) to the keyword actually driving the statement. Each
// removed group leaves a single space behind so words on either side don't get glued together.
fn strip_parens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0i32;
    // A ')' (or, for that matter, a keyword) inside a quoted string/identifier isn't a real
    // paren/keyword at all - "WHERE a=')SELECT('" is one string literal, not three tokens. Without
    // tracking quote state, that stray ')' closed the depth early and let the literal SELECT it
    // contains leak out as an exposed depth-0 token, which sql_is_readonly's "first verb wins"
    // check then picked over the CTE's real (dangerous) trailing statement.
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if escaped { escaped = false; }
            else if c == '\\' { escaped = true; }
            else if c == q {
                // A doubled quote ('' or "" or ``) is an escaped literal quote, not the closer -
                // consume the pair and stay inside the string.
                if chars.peek() == Some(&q) { chars.next(); }
                else { quote = None; }
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => { quote = Some(c); }
            '(' => { if depth == 0 { out.push(' '); } depth += 1; }
            ')' => { if depth > 0 { depth -= 1; } if depth == 0 { out.push(' '); } }
            _ => { if depth == 0 { out.push(c); } }
        }
    }
    out
}
fn sql_is_readonly(sql: &str) -> bool {
    if sql.trim().is_empty() { return true; }
    // /*! ... */ and /*!50000 ... */ are NOT comments: MySQL executes their contents. Stripping
    // them like a comment hid the statement inside from the keyword check below, so
    // "/*!50000 DELETE FROM t */" passed as read-only and then deleted rows. Unwrap them first
    // so the SQL they carry is checked like any other, and only then strip real comments.
    let re_exec  = regex::Regex::new(r"(?s)/\*!\d*(.*?)\*/").unwrap();
    let re_block = regex::Regex::new(r"(?s)/\*.*?\*/").unwrap();
    let re_dash  = regex::Regex::new(r"(?m)--.*$").unwrap();
    let re_hash  = regex::Regex::new(r"(?m)#.*$").unwrap();
    // MariaDB's ANALYZE [FORMAT=JSON] <statement> form (distinct from ANALYZE TABLE) actually
    // EXECUTES the wrapped statement while profiling it - bare "ANALYZE" was allow-listed for the
    // genuinely read-only ANALYZE TABLE form, which also let "ANALYZE DELETE FROM t" straight
    // through untouched. This strips a leading FORMAT=JSON clause so the wrapped statement's own
    // keyword is what's left to check.
    let re_analyze_fmt = regex::Regex::new(r"(?i)^FORMAT\s*=\s*JSON\s+").unwrap();
    let unwrapped = re_exec.replace_all(sql, " $1 ");
    let step1 = re_block.replace_all(&unwrapped, " ");
    let step2 = re_dash.replace_all(&step1, " ");
    let s = re_hash.replace_all(&step2, " ");
    const ALLOW: &[&str] = &["SELECT","SHOW","DESCRIBE","DESC","EXPLAIN","USE","WITH","SET","HELP","VALUES","TABLE","ANALYZE","CHECK","CHECKSUM"];
    for stmt in s.split(';') {
        let t = stmt.trim();
        if t.is_empty() { continue; }
        let w = t.split_whitespace().next().unwrap_or("").to_uppercase();
        if !ALLOW.contains(&w.as_str()) { return false; }
        // SET is allowed because a session variable is harmless, but SET GLOBAL / SET PERSIST -
        // and their @@GLOBAL. / @@PERSIST. spellings - reconfigure the server for every
        // connection, which is not something a read-only connection should be able to do.
        if w == "SET" {
            let up = t.to_uppercase();
            let second = up.split_whitespace().nth(1).unwrap_or("");
            if second.starts_with("GLOBAL") || second.starts_with("PERSIST")
                || up.contains("@@GLOBAL") || up.contains("@@PERSIST") { return false; }
        }
        // A CTE only stays read-only if it's actually prefixing a SELECT/TABLE/VALUES - MySQL
        // 8.0.19+/MariaDB also allow "WITH x AS (...) DELETE/UPDATE FROM t ...", which the leading
        // "WITH" alone can't reveal. Strip every CTE's own (possibly nested) body via
        // strip_parens, leaving roughly "WITH cte1 AS , cte2 AS  DELETE FROM t ..." - the first
        // remaining recognizable verb after that is the statement actually being run.
        if w == "WITH" {
            let stripped = strip_parens(t);
            const VERBS: &[&str] = &["SELECT","INSERT","UPDATE","DELETE","REPLACE","TABLE","VALUES"];
            let verb = stripped.split_whitespace().map(|w| w.to_uppercase()).find(|w| VERBS.contains(&w.as_str()));
            match verb.as_deref() {
                Some("SELECT") | Some("TABLE") | Some("VALUES") => {}
                _ => return false,
            }
        }
        if w == "ANALYZE" {
            let rest = t.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim_start();
            let is_analyze_table = rest.split_whitespace().next().map(|f| f.eq_ignore_ascii_case("TABLE")).unwrap_or(false);
            if !is_analyze_table {
                let inner = re_analyze_fmt.replace(rest, "");
                let inner_w = inner.trim().split_whitespace().next().unwrap_or("").to_uppercase();
                if inner_w != "SELECT" { return false; }
            }
        }
    }
    true
}
// The actual escaping: backslash first (so a literal backslash never combines with the quote
// doubling below to re-open the string), then the quote itself. Used directly wherever a plain
// string literal is needed (schema/table names from information_schema, usernames, ...) - unlike
// sql_lit() below, this has no hex-literal special case, so it's the right one for a NAME, which
// should never be reinterpreted as a raw hex value just because it happens to look like one.
fn sql_str_lit(s: &str) -> String { format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")) }
fn sql_lit(s: &str) -> String {
    if !s.is_empty() && s.starts_with("0x") && s[2..].chars().all(|c| c.is_ascii_hexdigit()) && s.len() > 2 {
        return s.to_string(); // hex literal (bit/binary)
    }
    sql_str_lit(s)
}
// Mirrors the PowerShell version's SqlValLit: a "0x.." hex-encoded string produced by
// val_to_opt() for binary/bit values is our OWN display encoding, not a real string value.
// Quoting it normally would insert the literal text "0x00" (4+ chars) instead of the 1-byte
// value it represents, which fails for bit(1)/binary columns ("Data too long for column").
// Emitting it as a raw (unquoted) hex literal lets MySQL correctly interpret the real value.
fn sql_val_lit(s: &str) -> String {
    if regex::Regex::new(r"^0x[0-9A-Fa-f]+$").unwrap().is_match(s) { s.to_string() } else { sql_lit(s) }
}


// ---------- mysql / mysqldump CLI resolution ----------
fn config_file() -> std::path::PathBuf {
    let mut p = dirs::config_dir().unwrap_or(std::env::temp_dir());
    p.push("NOBSSQL-Desktop"); std::fs::create_dir_all(&p).ok(); p.push("config.json"); p
}
fn log_line(msg: &str) {
    let mut p = dirs::config_dir().unwrap_or(std::env::temp_dir());
    p.push("NOBSSQL-Desktop"); let _ = std::fs::create_dir_all(&p); p.push("log.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        use std::io::Write as _;
        let _ = writeln!(f, "{}  {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
    }
}
fn load_cfg() -> Value {
    std::fs::read_to_string(config_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}))
}
fn tools_dir() -> std::path::PathBuf {
    let mut p = dirs::config_dir().unwrap_or(std::env::temp_dir());
    p.push("NOBSSQL-Desktop"); p.push("bin"); p
}
fn ver_key(s: &str) -> Vec<u64> {
    s.split('.').map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)).collect()
}

fn resolve_tool(app: &tauri::AppHandle, base: &str, names: &[&str], env_key: &str) -> Result<String, String> {
    // 0) user-configured path (Settings / downloaded tools) wins
    let cfg = load_cfg();
    if let Some(pth) = cfg.get(&format!("{}_bin", base)).and_then(|v| v.as_str()) {
        if !pth.is_empty() && std::path::Path::new(pth).exists() { return Ok(pth.to_string()); }
    }
    // 1) prefer a binary bundled with the app (src-tauri/binaries)
    if let Ok(dir) = app.path().resource_dir() {
        let mut p = dir.clone();
        #[cfg(windows)]
        p.push(format!("binaries/{}.exe", base));
        #[cfg(not(windows))]
        p.push(format!("binaries/{}", base));
        if p.exists() { return Ok(p.to_string_lossy().to_string()); }
    }
    // 2) fall back to env var / common install dirs / PATH
    resolve_bin(names, env_key)
}
// Directories to search for a client-tool executable, in order, under each of `bases`: any child
// named MariaDB*/MySQL* contributes its own bin, then each of ITS children's bin. Both layouts
// are needed and the old code only handled the second: MariaDB installs to
// "Program Files\MariaDB 11.4\bin" (version in the folder name, bin directly inside) while
// MySQL installs to "Program Files\MySQL\MySQL Server 8.0\bin" (one level deeper). Scanning
// only <root>\<child>\bin from a "Program Files\MariaDB" root missed every real MariaDB
// install, and the same shape missed XAMPP, whose bin sits directly at "xampp\mysql\bin" -
// both of which the Settings dialog claimed were checked. `direct` covers that last case.
// Kept separate from resolve_bin so it can be exercised against a temporary tree in a test
// rather than only on a machine that happens to have these products installed.
fn tool_search_dirs(bases: &[std::path::PathBuf], direct: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for base in bases {
        let rd = match std::fs::read_dir(base) { Ok(r) => r, Err(_) => continue };
        // Sorted so the order is deterministic rather than whatever the filesystem returns.
        let mut kids: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        kids.sort();
        for kid in kids {
            let name = kid.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !(name.starts_with("mariadb") || name.starts_with("mysql")) { continue; }
            out.push(kid.join("bin"));
            if let Ok(sub) = std::fs::read_dir(&kid) {
                let mut subs: Vec<std::path::PathBuf> = sub.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
                subs.sort();
                for s in subs { out.push(s.join("bin")); }
            }
        }
    }
    out.extend(direct.iter().cloned());
    out
}

fn resolve_bin(names: &[&str], env_key: &str) -> Result<String, String> {
    if let Ok(p) = std::env::var(env_key) {
        if !p.is_empty() && std::path::Path::new(&p).exists() { return Ok(p); }
    }
    {
        #[cfg(windows)]
        let (bases, direct) = (
            vec![std::path::PathBuf::from("C:\\Program Files"),
                 std::path::PathBuf::from("C:\\Program Files (x86)"),
                 std::path::PathBuf::from("C:\\wamp64\\bin")],
            vec![std::path::PathBuf::from("C:\\xampp\\mysql\\bin")]);
        #[cfg(not(windows))]
        let (bases, direct): (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) = (vec![], vec![]);
        for d in tool_search_dirs(&bases, &direct) {
            for n in names {
                let f = d.join(format!("{}.exe", n));
                if f.exists() { return Ok(f.to_string_lossy().to_string()); }
            }
        }
    }
    // Last resort: search PATH directories for the executable directly, rather than spawning it
    // with --version just to confirm it runs. All that's actually needed here is "does this file
    // exist and is it presumably runnable" - a plain existence check answers that without paying
    // for process creation (and, in practice, whatever a fresh/unscanned .exe costs to launch
    // under Windows Defender's real-time scanning) on every uncached tools_status call. Returns
    // the bare name on a match, same as the old --version-spawn check did (not the resolved full
    // path) - describe()'s "found on PATH" vs "found on system" label depends on that; a bare
    // name still spawns fine later since Command::new() resolves it via PATH itself.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for n in names {
                #[cfg(windows)]
                let candidate = dir.join(format!("{}.exe", n));
                #[cfg(not(windows))]
                let candidate = dir.join(n);
                if candidate.exists() { return Ok(n.to_string()); }
            }
        }
    }
    let name = names[0];
    // Names the Settings dialog first: it is the fix inside the app (pick the path, or download
    // the MariaDB client tools), whereas PATH and the environment variable both need a restart
    // to take effect. The "Could not find '<tool>'" prefix is matched by showToolError() in the
    // UI to decide whether to offer an Open Settings button - keep it if this text changes.
    Err(format!("Could not find '{}'. Open Settings in the app to select {}.exe or download the MariaDB client tools. Alternatively add its bin folder to PATH, or set the {} environment variable to its full path.", name, name, env_key))
}

fn first_err(s: &str) -> String {
    // prefer the real "ERROR NNNN ..." line if present (mysql may echo the statement first)
    if let Some(l) = s.lines().map(|l| l.trim()).find(|l| l.starts_with("ERROR") || l.contains("ERROR ")) {
        return l.to_string();
    }
    s.lines().map(|l| l.trim()).find(|l| !l.is_empty() && !l.chars().all(|c| c == '-'))
        .unwrap_or("").to_string()
}

// mysqldump/mysql print exactly this wording (no "ERROR NNNN" prefix, so first_err() returns it
// verbatim) when a flag the binary doesn't recognise is passed - which happens whenever an
// export/import option only supported by one dump-tool flavor (MySQL vs MariaDB, or an older
// version of either) is used against the other. Name the likely cause instead of leaving a bare
// "unknown variable" for the user to puzzle over.
fn friendly_dump_err(raw: &str) -> String {
    if let Some(opt) = raw.split("unknown variable '").nth(1).and_then(|s| s.split('\'').next()) {
        format!("{} - '{}' isn't supported by this build of the tool (MySQL and MariaDB's client tools, and different versions of each, support different flag sets). Uncheck the matching export/import option, or point Settings at the other flavor's .exe.", raw, opt)
    } else {
        raw.to_string()
    }
}

// ---------- commands ----------
#[tauri::command]
async fn connect(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let mut c = match build_conn(&req["conn"]) { Ok(c) => c, Err(e) => return Ok(json!({"ok":false,"error":format!("Connection failed: {}", e)})) };
        match c.query_first::<String, _>("SELECT VERSION()") {
            Ok(Some(v)) => Ok(json!({"ok":true,"version":v,"mariadb":v.contains("MariaDB")})),
            Ok(None) => Ok(json!({"ok":true,"version":"","mariadb":false})),
            Err(e) => Ok(json!({"ok":false,"error":e.to_string()})),
        }
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
// Mirrors the PowerShell version's Api-Schemas: each entry is {name, size} (not a bare
// string) - the sidebar and export dialog both read .name, and the sidebar also shows the
// per-database size badge computed from information_schema.TABLES.
async fn schemas(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let mut c = build_conn(&req["conn"])?;
        let (_c, rows) = run_select(&mut c, "SHOW DATABASES")?;
        let mut names: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        names.sort();
        let schemas: Vec<Value> = names.into_iter().map(|db| {
            let sql = format!(
                "SELECT SUM(DATA_LENGTH + INDEX_LENGTH) FROM information_schema.TABLES WHERE TABLE_SCHEMA={} AND TABLE_TYPE='BASE TABLE'",
                sql_lit(&db)
            );
            let size = run_select(&mut c, &sql).ok()
                .and_then(|(_cols, rows)| rows.into_iter().next())
                .and_then(|r| r.into_iter().next().flatten())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            json!({"name": db, "size": size})
        }).collect();
        Ok(json!({"ok":true,"schemas":schemas}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn objects(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let db = sql_str_lit(req["db"].as_str().unwrap_or(""));
        let mut c = build_conn(&req["conn"])?;
        let sql = format!(
            "SELECT 'table' t,TABLE_NAME n FROM information_schema.TABLES WHERE TABLE_SCHEMA={d} AND TABLE_TYPE='BASE TABLE' \
             UNION ALL SELECT 'view',TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA={d} AND TABLE_TYPE='VIEW' \
             UNION ALL SELECT IF(ROUTINE_TYPE='PROCEDURE','procedure','function'),ROUTINE_NAME FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA={d} \
             UNION ALL SELECT 'trigger',TRIGGER_NAME FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA={d} \
             UNION ALL SELECT 'event',EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_SCHEMA={d} ORDER BY 1,2", d = db);
        let (_c, rows) = run_select(&mut c, &sql)?;
        let (mut tables, mut views, mut procedures, mut functions, mut triggers, mut events) =
            (vec![], vec![], vec![], vec![], vec![], vec![]);
        for r in rows {
            let t = r.get(0).cloned().flatten().unwrap_or_default();
            let n = r.get(1).cloned().flatten().unwrap_or_default();
            match t.as_str() {
                "table" => tables.push(n), "view" => views.push(n),
                "procedure" => procedures.push(n), "function" => functions.push(n),
                "trigger" => triggers.push(n), "event" => events.push(n), _ => {}
            }
        }
        // Which table each trigger belongs to, so a table's own right-click menu can offer its
        // EXISTING triggers directly, not just the flat "Triggers" list elsewhere in the tree.
        // A separate lookup (rather than adding a column to the UNION above) so the shape of
        // the existing flat trigger-name array - which other code already relies on - never changes.
        let mut trigger_tables: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if !triggers.is_empty() {
            if let Ok((_tc, trows)) = run_select(&mut c, &format!("SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA={}", db)) {
                for tr in trows {
                    let tname = tr.get(0).cloned().flatten().unwrap_or_default();
                    let ttable = tr.get(1).cloned().flatten().unwrap_or_default();
                    if !tname.is_empty() { trigger_tables.insert(tname, ttable); }
                }
            }
        }
        Ok(json!({"ok":true,"tables":tables,"views":views,"procedures":procedures,"functions":functions,"triggers":triggers,"events":events,"triggerTables":trigger_tables}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn ddl(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let (db, name, ty) = (req["db"].as_str().unwrap_or(""), req["name"].as_str().unwrap_or(""), req["type"].as_str().unwrap_or(""));
        let obj = format!("{}.{}", sql_id(db), sql_id(name));
        let sql = match ty {
            "table" => format!("SHOW CREATE TABLE {}", obj),
            "view" => format!("SHOW CREATE VIEW {}", obj),
            "procedure" => format!("SHOW CREATE PROCEDURE {}", obj),
            "function" => format!("SHOW CREATE FUNCTION {}", obj),
            "trigger" => format!("SHOW CREATE TRIGGER {}", obj),
            "event" => format!("SHOW CREATE EVENT {}", obj),
            _ => return Ok(json!({"ok":false,"error":"unknown type"})),
        };
        let mut c = build_conn(&req["conn"])?;
        let (cols, rows) = run_select(&mut c, &sql)?;
        if rows.is_empty() { return Ok(json!({"ok":false,"error":"no DDL returned"})); }
        let idx = cols.iter().position(|h| { let l = h.to_lowercase(); l.contains("create") || l.contains("statement") })
            .unwrap_or(cols.len().saturating_sub(1));
        Ok(json!({"ok":true,"ddl":rows[0].get(idx).cloned().flatten().unwrap_or_default()}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn pk(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let db = sql_str_lit(req["db"].as_str().unwrap_or(""));
        let table = sql_str_lit(req["table"].as_str().unwrap_or(""));
        let mut c = build_conn(&req["conn"])?;
        let sql = format!("SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA={} AND TABLE_NAME={} AND CONSTRAINT_NAME='PRIMARY' ORDER BY ORDINAL_POSITION", db, table);
        let (_c, rows) = run_select(&mut c, &sql)?;
        let list: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        Ok(json!({"ok":true,"pk":list}))
    }).await.map_err(|e| e.to_string())?
}

// Same idea as pk(), but for foreign-key columns - used so the grid can highlight FK columns
// the same way it already highlights the primary key.
#[tauri::command]
async fn fk(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let db = sql_str_lit(req["db"].as_str().unwrap_or(""));
        let table = sql_str_lit(req["table"].as_str().unwrap_or(""));
        let mut c = build_conn(&req["conn"])?;
        let sql = format!("SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA={} AND TABLE_NAME={} AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY ORDINAL_POSITION", db, table);
        let (_c, rows) = run_select(&mut c, &sql)?;
        let list: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        // Full FK detail (which table/column each FK column actually references) - a SEPARATE
        // field from "fk" above, which stays just the local column-name list other callers
        // (PK/FK badge display) already rely on. This powers "go to referenced row" navigation.
        let detail_sql = format!("SELECT COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA={} AND TABLE_NAME={} AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY ORDINAL_POSITION", db, table);
        let fk_details: Vec<Vec<Option<String>>> = run_select(&mut c, &detail_sql).map(|(_c, r)| r).unwrap_or_default();
        Ok(json!({"ok":true,"fk":list,"fkDetails":fk_details}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn process_list(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let mut c = build_conn(&req["conn"])?;
        let (cols, rows) = run_select(&mut c, "SHOW FULL PROCESSLIST")?;
        Ok(json!({"ok":true,"columns":cols,"rows":rows}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn kill_process(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        if req["ro"].as_bool().unwrap_or(false) {
            return Ok(json!({"ok":false,"error":"Read-only mode: statement blocked."}));
        }
        let pid = req["pid"].as_str().unwrap_or("");
        if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
            return Ok(json!({"ok":false,"error":"Invalid process id."}));
        }
        let mut c = build_conn(&req["conn"])?;
        match c.query_drop(format!("KILL {}", pid)) {
            Ok(_) => Ok(json!({"ok":true})),
            Err(e) => Ok(json!({"ok":false,"error":e.to_string()})),
        }
    }).await.map_err(|e| e.to_string())?
}

// Column list (with PK flags) plus every FK relationship for a schema - the raw data an ER
// diagram is drawn from. Layout/rendering happens entirely client-side (this just supplies the
// facts: which tables, which columns, which are primary keys, and which columns reference which).
#[tauri::command]
async fn schema_erd(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let db = sql_str_lit(req["db"].as_str().unwrap_or(""));
        let mut c = build_conn(&req["conn"])?;
        let (_c1, columns) = run_select(&mut c, &format!("SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA={} ORDER BY TABLE_NAME, ORDINAL_POSITION", db))?;
        // PK detection deliberately matches get_table_pk_cols's approach (CONSTRAINT_NAME='PRIMARY'),
        // NOT information_schema.COLUMNS.COLUMN_KEY='PRI'. COLUMN_KEY has a documented MySQL edge
        // case: a table with NO actual primary key but a UNIQUE NOT NULL index will still show that
        // index's column as 'PRI', since it behaves like one. Using the same precise method as the
        // grid means the ER diagram can never highlight a column as PK that the grid itself
        // disagrees is one.
        let (_c2, pks) = run_select(&mut c, &format!("SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA={} AND CONSTRAINT_NAME='PRIMARY'", db))?;
        let (_c3, fks) = run_select(&mut c, &format!("SELECT TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA={} AND REFERENCED_TABLE_NAME IS NOT NULL", db))?;
        Ok(json!({"ok":true,"columns":columns,"pks":pks,"fks":fks}))
    }).await.map_err(|e| e.to_string())?
}

// If the incoming SQL is one or more leading "USE <schema>;" statements followed by a final
// statement (this is exactly what the frontend sends for "USE db;\nSELECT ...;" - both when a
// user types it directly, and previously when "+ New Query" auto-inserted it before this
// session's earlier fix), extract the LAST USE's schema name and reduce sql down to just the
// final statement. This sidesteps needing real multi-statement protocol support (CLIENT_MULTI_
// STATEMENTS) entirely: the USE effect is applied via a plain, separate query_drop on this SAME
// connection - exactly the same mechanism already used for the ordinary db parameter below -
// and run_select only ever sees a genuinely single statement, unchanged from every other caller.
// Only matches a well-formed "USE <plain-identifier-or-`backtick-quoted`>;" at the very start,
// repeated as many times as it matches - it never touches anything after the last such match, so
// a semicolon inside the ACTUAL query's own string literals is never at risk of being split on.
fn strip_leading_use_statements(sql: &str) -> (Option<String>, String) {
    let use_re = regex::Regex::new(r"(?is)^\s*use\s+(`[^`]+`|[A-Za-z0-9_$]+)\s*;\s*").unwrap();
    let mut remaining = sql.to_string();
    let mut last_db: Option<String> = None;
    loop {
        match use_re.captures(&remaining) {
            Some(caps) => {
                let raw = caps.get(1).unwrap().as_str();
                last_db = Some(raw.trim_matches('`').to_string());
                let matched_len = caps.get(0).unwrap().end();
                remaining = remaining[matched_len..].to_string();
            }
            None => break,
        }
    }
    (last_db, remaining)
}

#[tauri::command]
async fn query(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let sql = req["sql"].as_str().unwrap_or("").to_string();
        if sql.trim().is_empty() { return Ok(json!({"ok":false,"error":"Empty query."})); }
        if req["ro"].as_bool().unwrap_or(false) && !sql_is_readonly(&sql) {
            return Ok(json!({"ok":false,"error":"Read-only mode: statement blocked."}));
        }
        let mut c = build_conn(&req["conn"])?;
        if let Some(db) = req["db"].as_str() { if !db.is_empty() { let _ = c.query_drop(format!("USE {}", sql_id(db))); } }
        // A USE explicitly written in the query text itself takes priority over the ambient db
        // parameter above - the user may be deliberately switching schemas mid-query - and only
        // the statement AFTER it actually needs to run through run_select.
        let (explicit_db, sql) = strip_leading_use_statements(&sql);
        if let Some(db) = explicit_db { let _ = c.query_drop(format!("USE {}", sql_id(&db))); }
        // If the frontend gave us a requestId, register this connection's own MySQL
        // CONNECTION_ID() so a Cancel click can look it up and KILL QUERY it from elsewhere.
        let request_id = req["requestId"].as_str().filter(|s| !s.is_empty()).map(String::from);
        if let Some(rid) = &request_id {
            if let Ok((_cols, rows)) = run_select(&mut c, "SELECT CONNECTION_ID()") {
                if let Some(cid) = rows.get(0).and_then(|r| r.get(0)).cloned().flatten().and_then(|s| s.parse::<u64>().ok()) {
                    running_queries().lock().unwrap().insert(rid.clone(), (cid, req["conn"].clone()));
                }
            }
        }
        // Cursor-based streaming (Workbench-style "fetch next batch"): every ad-hoc query - with
        // or without its own LIMIT - opens a cursor and pulls just its first page (pageSize rows,
        // default 1000) through it. This replaces the earlier OFFSET/LIMIT rewrite, which (a)
        // only ever kicked in for a query with no LIMIT of its own, leaving one written by the
        // user still hitting the old hard row cap with no way to see the rest, and (b) cost a
        // full rescan-and-discard of everything before OFFSET on every "fetch next" against a
        // large table. A cursor sidesteps both: it applies uniformly regardless of the query's
        // own LIMIT, and paging forward just keeps reading the same already-open result set.
        let page_size = req.get("pageSize").and_then(|v| v.as_u64()).unwrap_or(1000).max(1) as usize;

        let t = std::time::Instant::now();
        // Whether the requestId's running_queries() registration should outlive this call - true
        // only when a cursor is left open afterward, so Cancel keeps being able to KILL QUERY the
        // same connection while the user is idle between "fetch next" calls or waiting on a slow
        // one. False in every other case (no result set, single-page result, or an error), where
        // the cursor thread (if one ever ran) has already deregistered itself by the time this
        // returns, and the removal below is what clears it otherwise.
        let mut cursor_persists = false;
        let result = match open_cursor(c, sql, page_size, request_id.clone()) {
            Ok((cursor_id, cols, bin, bit, rows, has_more)) => {
                cursor_persists = has_more;
                if cols.is_empty() {
                    Ok(json!({"ok":true,"columns":[],"rows":[],"elapsedMs":t.elapsed().as_millis() as u64,"message":"Query OK. No result set."}))
                } else {
                    let cursor_id_for_response = if has_more { Some(cursor_id) } else { None };
                    Ok(json!({"ok":true,"columns":cols,"rows":rows,"binaryCols":bin,"bitCols":bit,"hasMore":has_more,"cursorId":cursor_id_for_response,"elapsedMs":t.elapsed().as_millis() as u64}))
                }
            }
            Err(e) => {
                // A cancelled query surfaces here as a MySQL error ("Query execution was
                // interrupted"), so it has to be told apart from an ordinary failure. Ask
                // whether THIS request was actually killed, rather than whether it merely had
                // a requestId - the editor sends one with every run, so the old check turned
                // every syntax error, missing table and permission failure into
                // "Query cancelled." and discarded what really went wrong.
                let was_cancelled = request_id.as_deref().map(take_query_cancelled).unwrap_or(false);
                if was_cancelled { Ok(json!({"ok":false,"error":"Query cancelled.","cancelled":true})) }
                else { Ok(json!({"ok":false,"error":e})) }
            }
        };
        if !cursor_persists {
            if let Some(rid) = &request_id { running_queries().lock().unwrap().remove(rid); let _ = take_query_cancelled(rid); }
        }
        result
    }).await.map_err(|e| e.to_string())?
}

// Fetches the next page (default 1000 rows) from a cursor previously opened by `query`. Returns
// an error if the cursorId is unknown - already exhausted, explicitly closed, or timed out from
// 10 minutes of inactivity - which the frontend surfaces and treats as "nothing more to fetch"
// rather than a hard failure, since all three are ordinary end states, not corruption.
#[tauri::command]
async fn fetch_cursor_batch(req: Value) -> R {
    let cursor_id = req["cursorId"].as_str().unwrap_or("").to_string();
    let page_size = req.get("pageSize").and_then(|v| v.as_u64()).unwrap_or(1000).max(1) as usize;
    // The same requestId query() registered under running_queries() when this cursor was opened
    // - still valid for the cursor's whole life (see query()'s tail / the cursor thread's own
    // cleanup), so a Cancel click reaches this fetch exactly like it reaches the first page.
    let request_id = req["requestId"].as_str().filter(|s| !s.is_empty()).map(String::from);
    tokio::task::spawn_blocking(move || {
        let tx = cursors().lock().unwrap().get(&cursor_id).cloned();
        let Some(tx) = tx else {
            return Ok(json!({"ok":false,"error":"Cursor not found or already closed."}));
        };
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if tx.send(CursorCmd::Fetch { n: page_size, reply: reply_tx }).is_err() {
            cursors().lock().unwrap().remove(&cursor_id);
            return Ok(json!({"ok":false,"error":"Cursor not found or already closed."}));
        }
        match reply_rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(Ok(batch)) => {
                // has_more false means the cursor thread already exhausted, deregistered, and
                // exited on its own - nothing left here to clean up in that case.
                if !batch.has_more { cursors().lock().unwrap().remove(&cursor_id); }
                Ok(json!({"ok":true,"rows":batch.rows,"hasMore":batch.has_more}))
            }
            Ok(Err(e)) => {
                cursors().lock().unwrap().remove(&cursor_id);
                // A Cancel click KILLs the connection this fetch is blocked on, which surfaces
                // here as an ordinary MySQL error ("Query execution was interrupted") - tell it
                // apart from a real failure the same way query()'s own first-page fetch does.
                let was_cancelled = request_id.as_deref().map(take_query_cancelled).unwrap_or(false);
                if was_cancelled { Ok(json!({"ok":false,"error":"Query cancelled.","cancelled":true})) }
                else { Ok(json!({"ok":false,"error":e})) }
            }
            Err(_) => { cursors().lock().unwrap().remove(&cursor_id); Ok(json!({"ok":false,"error":"Cursor not found or already closed."})) }
        }
    }).await.map_err(|e| e.to_string())?
}

// Tears down an idle-but-open cursor: sent whenever the frontend is done with one before it ran
// out on its own (a tab closed, a new query replacing the previous one in the same tab, the app
// quitting with results still on screen). Always succeeds, including when the cursorId is
// already gone (already exhausted, already closed, idle-timed-out) - callers fire this
// defensively without checking whether there is anything left to close.
#[tauri::command]
async fn close_cursor(req: Value) -> R {
    let cursor_id = req["cursorId"].as_str().unwrap_or("").to_string();
    tokio::task::spawn_blocking(move || {
        if let Some(tx) = cursors().lock().unwrap().remove(&cursor_id) {
            let _ = tx.send(CursorCmd::Close);
        }
        Ok(json!({"ok":true}))
    }).await.map_err(|e| e.to_string())?
}

// Cancels a running query by requestId: looks up the MySQL connection id it was registered
// under, opens a FRESH connection with the same credentials, and runs KILL QUERY <id> on it.
// If the query already finished (nothing registered under that id), this is a harmless no-op.
#[tauri::command]
async fn cancel_query(req: Value) -> R {
    let rid = req["requestId"].as_str().unwrap_or("").to_string();
    let entry = running_queries().lock().unwrap().get(&rid).cloned();
    if let Some((cid, connj)) = entry {
        // Marked before the KILL lands so the query thread, which may fail immediately after,
        // can already see that its failure was a cancel rather than a real error.
        mark_query_cancelled(&rid);
        tokio::task::spawn_blocking(move || {
            if let Ok(mut kc) = build_conn(&connj) {
                let _ = kc.query_drop(format!("KILL QUERY {}", cid));
            }
        }).await.ok();
    }
    Ok(json!({"ok":true}))
}

#[tauri::command]
async fn exec(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let sql = req["sql"].as_str().unwrap_or("").to_string();
        if req["ro"].as_bool().unwrap_or(false) && !sql_is_readonly(&sql) {
            return Ok(json!({"ok":false,"error":"Read-only mode: statement blocked."}));
        }
        let mut c = build_conn(&req["conn"])?;
        match c.query_drop(&sql) { Ok(_) => Ok(json!({"ok":true})), Err(e) => Ok(json!({"ok":false,"error":e.to_string()})) }
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
// NOTE: not currently called by the frontend (row edits are built and sent as plain SQL via
// applyChanges()/`lit()` -> the query/script command instead), but the command is still
// registered, so litv uses the same hex-aware sql_val_lit as everywhere else that touches real
// row data - a value that LOOKS like a plain sql_lit-quoted string here could otherwise silently
// corrupt a bit/binary column exactly like the bug already fixed in Compare's row apply.
async fn rowop(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        if req["ro"].as_bool().unwrap_or(false) {
            return Ok(json!({"ok":false,"error":"Read-only mode: statement blocked."}));
        }
        let db = req["db"].as_str().unwrap_or(""); let table = req["table"].as_str().unwrap_or("");
        let obj = format!("{}.{}", sql_id(db), sql_id(table));
        let op = req["op"].as_str().unwrap_or("");
        let pairs = |o: &Value| -> Vec<(String, Value)> {
            o.as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default()
        };
        let litv = |v: &Value| -> String { if v.is_null() { "NULL".into() } else { sql_val_lit(v.as_str().unwrap_or(&v.to_string())) } };
        let sql = match op {
            "update" => {
                let sets: Vec<String> = pairs(&req["set"]).iter().map(|(k, v)| format!("{}={}", sql_id(k), litv(v))).collect();
                let wh: Vec<String> = pairs(&req["where"]).iter().map(|(k, v)| format!("{}={}", sql_id(k), litv(v))).collect();
                if wh.is_empty() { return Ok(json!({"ok":false,"error":"no key columns"})); }
                format!("UPDATE {} SET {} WHERE {} LIMIT 1", obj, sets.join(","), wh.join(" AND "))
            }
            "delete" => {
                let wh: Vec<String> = pairs(&req["where"]).iter().map(|(k, v)| format!("{}={}", sql_id(k), litv(v))).collect();
                if wh.is_empty() { return Ok(json!({"ok":false,"error":"no key columns"})); }
                format!("DELETE FROM {} WHERE {} LIMIT 1", obj, wh.join(" AND "))
            }
            "insert" => {
                let vals = pairs(&req["values"]);
                if vals.is_empty() { return Ok(json!({"ok":false,"error":"no values"})); }
                let cols: Vec<String> = vals.iter().map(|(k, _)| sql_id(k)).collect();
                let vs: Vec<String> = vals.iter().map(|(_, v)| litv(v)).collect();
                format!("INSERT INTO {} ({}) VALUES ({})", obj, cols.join(","), vs.join(","))
            }
            _ => return Ok(json!({"ok":false,"error":"bad op"})),
        };
        let mut c = build_conn(&req["conn"])?;
        match c.query_drop(&sql) { Ok(_) => Ok(json!({"ok":true})), Err(e) => Ok(json!({"ok":false,"error":e.to_string()})) }
    }).await.map_err(|e| e.to_string())?
}

// script / import / export shell out to the mysql/mysqldump CLI (DELIMITER-safe)
// A raw newline in a value would otherwise start a brand new line in the .cnf file, letting a
// saved connection's host/user/password inject an arbitrary extra option-file directive (e.g.
// "pager=<command>", which the mysql CLI executes) rather than staying part of THIS value.
// Backslash-doubling (below) only protects against \n being misread as an escape sequence -
// it does nothing for an actual embedded newline BYTE, which this strips outright since none of
// these fields have any legitimate use for one.
fn cnf_safe(s: &str) -> String { s.replace(['\r', '\n'], "") }
fn cnf_file(connj: &Value) -> Result<(tempfile::NamedTempFile, String), String> {
    let mut f = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    let mut s = String::from("[client]\n");
    s += &format!("host={}\nport={}\nuser={}\n", cnf_safe(connj["host"].as_str().unwrap_or("127.0.0.1")),
        cnf_safe(connj["port"].as_str().unwrap_or("3306")), cnf_safe(connj["user"].as_str().unwrap_or("root")));
    // MySQL option files treat backslash as an escape character in values (\t, \n, \\, ...), so a
    // password containing a literal backslash has to be doubled here or the .cnf parser would
    // silently consume it as (the start of) an escape sequence instead of a literal character -
    // corrupting the password and breaking auth for anyone whose password happens to contain one.
    if let Some(p) = connj["password"].as_str() { if !p.is_empty() { s += &format!("password={}\n", cnf_safe(p).replace('\\', "\\\\")); } }
    f.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
    let path = f.path().to_string_lossy().to_string();
    Ok((f, path))
}

// Splits a SQL script into individual statements, honoring quoted strings/identifiers,
// comments, and DELIMITER directives - so DDL bodies (procedures, triggers, functions) with
// semicolons inside BEGIN...END blocks split correctly without needing to shell out to the
// mysql CLI, which was previously the only reason this path needed it. Everything else in this
// app already talks to the server directly through the native driver.
//
// Operates on Vec<char> rather than raw byte indices: SQL string literals/comments can contain
// multi-byte UTF-8 text (accented characters, CJK, emoji), and Rust panics if you slice a &str
// at a non-character-boundary byte offset - char-level indexing sidesteps that entirely.
//
// Validated with 45 unit tests (quoting/escaping rules for ', ", `; --, #, /* */ comments;
// DELIMITER changes including nested procedures, CRLF, case-insensitivity; UTF-8 safety at
// split boundaries) plus a 2000-trial fuzz test asserting no panics on adversarial random
// input - see the project notes for the full suite this was checked against before integration.
fn split_sql_statements(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut statements: Vec<String> = Vec::new();
    let mut delimiter: Vec<char> = vec![';'];
    let mut buf: Vec<char> = Vec::new();
    let mut i: usize = 0;
    // true when buf (everything accumulated since the last statement boundary) is empty or
    // whitespace-only - used to recognize DELIMITER only when it's the start of a new statement
    let mut line_start = true;

    fn matches_at(chars: &[char], pos: usize, needle: &[char]) -> bool {
        let n = chars.len();
        if pos + needle.len() > n { return false; }
        chars[pos..pos + needle.len()] == *needle
    }
    fn matches_at_ci(chars: &[char], pos: usize, needle: &[char]) -> bool {
        let n = chars.len();
        if pos + needle.len() > n { return false; }
        chars[pos..pos + needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a.to_ascii_uppercase() == b.to_ascii_uppercase())
    }
    fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
        let mut k = from;
        while k < chars.len() {
            if chars[k] == target { return Some(k); }
            k += 1;
        }
        None
    }
    fn find_two_char(chars: &[char], from: usize, a: char, b: char) -> Option<usize> {
        if chars.is_empty() { return None; }
        let mut k = from;
        while k + 1 < chars.len() {
            if chars[k] == a && chars[k + 1] == b { return Some(k); }
            k += 1;
        }
        None
    }

    let delimiter_kw: Vec<char> = "DELIMITER".chars().collect();

    while i < n {
        let c = chars[i];

        // --- DELIMITER directive: only recognized at the start of a new statement ---
        if line_start
            && matches_at_ci(&chars, i, &delimiter_kw)
            && (i + 9 >= n || chars[i + 9] == ' ' || chars[i + 9] == '\t')
        {
            let mut j = i + 9;
            while j < n && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            let mut k = j;
            while k < n && chars[k] != '\r' && chars[k] != '\n' {
                k += 1;
            }
            let new_delim: String = chars[j..k].iter().collect::<String>().trim().to_string();
            if !new_delim.is_empty() {
                delimiter = new_delim.chars().collect();
            }
            i = k;
            buf.clear();
            line_start = true;
            continue;
        }

        // --- block comment /* ... */ ---
        if matches_at(&chars, i, &['/', '*']) {
            match find_two_char(&chars, i + 2, '*', '/') {
                Some(end) => { buf.extend_from_slice(&chars[i..end + 2]); i = end + 2; }
                None => { buf.extend_from_slice(&chars[i..]); i = n; }
            }
            continue;
        }

        // --- '--' line comment (MySQL requires whitespace/EOL/EOF right after '--') ---
        if matches_at(&chars, i, &['-', '-'])
            && (i + 2 >= n || chars[i + 2] == ' ' || chars[i + 2] == '\t' || chars[i + 2] == '\r' || chars[i + 2] == '\n')
        {
            match find_char(&chars, i, '\n') {
                Some(end) => { buf.extend_from_slice(&chars[i..end + 1]); i = end + 1; }
                None => { buf.extend_from_slice(&chars[i..]); i = n; }
            }
            continue;
        }

        // --- '#' line comment ---
        if c == '#' {
            match find_char(&chars, i, '\n') {
                Some(end) => { buf.extend_from_slice(&chars[i..end + 1]); i = end + 1; }
                None => { buf.extend_from_slice(&chars[i..]); i = n; }
            }
            continue;
        }

        // --- quoted strings / identifiers: ' " ` ---
        if c == '\'' || c == '"' || c == '`' {
            let quote = c;
            let mut j = i + 1;
            while j < n {
                if chars[j] == '\\' && quote != '`' && j + 1 < n {
                    // backslash escapes the next char (MySQL default sql_mode) - not inside backticks
                    j += 2;
                    continue;
                }
                if chars[j] == quote {
                    if j + 1 < n && chars[j + 1] == quote {
                        // doubled-quote escape ('' or "" or ``)
                        j += 2;
                        continue;
                    }
                    j += 1;
                    break;
                }
                j += 1;
            }
            buf.extend_from_slice(&chars[i..j]);
            i = j;
            line_start = false;
            continue;
        }

        // --- delimiter match ---
        if i + delimiter.len() <= n && chars[i..i + delimiter.len()] == delimiter[..] {
            let stmt: String = buf.iter().collect::<String>().trim().to_string();
            if !stmt.is_empty() {
                statements.push(stmt);
            }
            buf.clear();
            i += delimiter.len();
            line_start = true;
            continue;
        }

        buf.push(c);
        if !c.is_whitespace() {
            line_start = false;
        }
        i += 1;
    }

    let stmt: String = buf.iter().collect::<String>().trim().to_string();
    if !stmt.is_empty() {
        statements.push(stmt);
    }

    statements
}

// Replaces the previous mysql.exe shell-out for DDL/Apply operations (table designer, stored
// procedure/function/trigger create-or-replace, compare-databases apply, pending grid-edit
// apply) with the native driver, using split_sql_statements above for the one thing that
// actually required the CLI: DELIMITER handling. Runs statements sequentially and stops at the
// first failure (matching the CLI's default, non---force behavior), reporting which statement
// number failed and a preview of it - clearer than the CLI's own error output, which had no
// per-statement context since the whole script was piped to it as one stdin stream.
#[tauri::command]
async fn script(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        let raw = req["sql"].as_str().unwrap_or("").to_string();
        if req["ro"].as_bool().unwrap_or(false) && !sql_is_readonly(&raw) {
            return Ok(json!({"ok":false,"error":"Read-only mode: statement blocked."}));
        }
        let mut c = build_conn(&req["conn"])?;
        let db = req["db"].as_str().unwrap_or("");
        if !db.is_empty() {
            c.query_drop(format!("USE {}", sql_id(db))).map_err(|e| e.to_string())?;
        }
        let statements = split_sql_statements(&raw);
        let total = statements.len();
        let continue_on_error = req["continueOnError"].as_bool().unwrap_or(false);

        // Applying staged grid edits sends several statements that only make sense together -
        // if the third fails, the first two must not stay. Without this the batch ran on
        // autocommit and a failure left the table half-updated, which is the one outcome a
        // "pending changes" model exists to prevent.
        let transactional = req["transaction"].as_bool().unwrap_or(false) && !continue_on_error;

        if !continue_on_error {
            // Default: stop at the first failure. Callers that pass transaction:true also get
            // everything rolled back; the others keep the original autocommit behaviour.
            if transactional { c.query_drop("START TRANSACTION").map_err(db_err)?; }
            for (idx, stmt) in statements.iter().enumerate() {
                if let Err(e) = c.query_drop(stmt) {
                    let preview: String = stmt.chars().take(120).collect();
                    let suffix = if stmt.chars().count() > 120 { "..." } else { "" };
                    if transactional { let _ = c.query_drop("ROLLBACK"); }
                    let e = db_err(e);
                    let note = if transactional { "\n\nNo changes were applied - the batch was rolled back." } else { "" };
                    return Ok(json!({"ok":false,"error":format!("Statement {} of {} failed: {}\n\n{}{}{}", idx+1, total, e, preview, suffix, note)}));
                }
            }
            if transactional {
                if let Err(e) = c.query_drop("COMMIT") {
                    let _ = c.query_drop("ROLLBACK");
                    return Ok(json!({"ok":false,"error":format!("Could not commit: {}\n\nNo changes were applied.", db_err(e))}));
                }
            }
            return Ok(json!({"ok":true}));
        }

        // Continue-on-error mode: run every statement regardless of earlier failures, and
        // report a full breakdown - the whole point of turning this on is seeing everything
        // that needs fixing in one pass, not stopping at (and hiding everything past) the first.
        let mut succeeded = 0usize;
        let mut failures: Vec<Value> = Vec::new();
        for (idx, stmt) in statements.iter().enumerate() {
            match c.query_drop(stmt) {
                Ok(_) => { succeeded += 1; }
                Err(e) => {
                    let preview: String = stmt.chars().take(120).collect();
                    let suffix = if stmt.chars().count() > 120 { "..." } else { "" };
                    failures.push(json!({"index":idx+1,"preview":format!("{}{}", preview, suffix),"error":e.to_string()}));
                }
            }
        }
        Ok(json!({"ok":failures.is_empty(),"total":total,"succeeded":succeeded,"failures":failures}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn import(app: tauri::AppHandle, req: Value) -> R {
    if req["ro"].as_bool().unwrap_or(false) {
        return Ok(json!({"ok":false,"error":"Read-only mode: statement blocked."}));
    }
    let mbin = match resolve_tool(&app, "mysql", &["mysql", "mariadb"], "MYSQL_BIN") { Ok(b) => b, Err(e) => return Ok(json!({"ok":false,"error":e})) };
    import_run(req, mbin).await
}

// The body, split out so a test can drive it without a tauri::AppHandle - resolving the mysql
// path is the only thing the handle provided.
async fn import_run(req: Value, mbin: String) -> R {
    tokio::task::spawn_blocking(move || {
        let jid = req["jobId"].as_str().unwrap_or("").to_string();
        let job = job_start(&jid);
        let _guard = JobGuard(jid);
        let (_f, cnf) = cnf_file(&req["conn"])?;
        let files: Vec<String> = req["files"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let target = req["targetDb"].as_str().unwrap_or("").to_string();
        let mut log = Vec::new();
        let mut cancelled = false;
        let mut errors_skipped = 0usize;
        if !target.is_empty() && req["createDb"].as_bool().unwrap_or(false) {
            let _ = Command::new(&mbin).arg(format!("--defaults-extra-file={}", cnf))
                .arg("-e").arg(format!("CREATE DATABASE IF NOT EXISTS {}", sql_id(&target))).output();
            log.push(format!("Ensured database {}", target));
        }
        for f in files {
            if job_is_cancelled(&job) { log.push("CANCELLED (remaining files skipped)".into()); cancelled = true; break; }
            if !std::path::Path::new(&f).exists() { log.push(format!("SKIP (missing): {}", f)); continue; }
            let mut args = vec![format!("--defaults-extra-file={}", cnf)];
            if req["force"].as_bool().unwrap_or(false) { args.push("--force".into()); }
            if req["fkOff"].as_bool().unwrap_or(false) { args.push("--init-command=SET FOREIGN_KEY_CHECKS=0; SET UNIQUE_CHECKS=0".into()); }
            // The "binary-mode" checkbox sends this, but nothing ever read it - checking it in
            // the UI silently did nothing, so the exact NUL-byte error its tooltip promises to
            // fix ("ASCII '\0' appeared in the statement") kept recurring with the box checked.
            if req["binaryMode"].as_bool().unwrap_or(false) { args.push("--binary-mode".into()); }
            // Export lets you raise mysqldump's --max-allowed-packet (needed for extended-insert
            // with large rows/BLOBs), but the mysql client re-importing that exact file has its
            // own, separate default (16M) - without a matching bump here, re-importing a dump
            // exported with a larger packet size fails with "MySQL server has gone away".
            if let Some(mp) = req["maxpacket"].as_str() { if !mp.is_empty() { args.push(format!("--max-allowed-packet={}", mp)); } }
            if !target.is_empty() { args.push(target.clone()); }
            let file = std::fs::File::open(&f).map_err(|e| e.to_string())?;
            let mut cmd = Command::new(&mbin);
            cmd.args(&args).stdin(Stdio::from(file)).stdout(Stdio::null());
            let out = run_job_child(job.as_ref(), &mut cmd);
            let short = || std::path::Path::new(&f).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| f.clone());
            match out {
                Ok(o) if o.status.success() => {
                    // "Continue on error" passes --force, and mysql then exits 0 even when every
                    // statement failed, reporting what went wrong on stderr instead. Taking the
                    // exit code at face value turned a completely failed import into a clean list
                    // of OK lines - the worst possible outcome for a restore, since it looks like
                    // it worked. Report what the tool actually said.
                    let err = String::from_utf8_lossy(&o.stderr);
                    let errs: Vec<&str> = err.lines().map(|l| l.trim()).filter(|l| l.contains("ERROR")).collect();
                    if errs.is_empty() {
                        log.push(format!("OK  {}", short()));
                    } else {
                        log.push(format!("OK with {} error(s) SKIPPED  {} : {}{}",
                            errs.len(), short(), errs[0],
                            if errs.len() > 1 { format!(" (+{} more)", errs.len() - 1) } else { String::new() }));
                        errors_skipped += errs.len();
                    }
                }
                // A killed child reports failure, but "FAILED file : ..." would read as a broken
                // import rather than the cancel the user just asked for.
                Ok(_) if job_is_cancelled(&job) => { log.push(format!("CANCELLED {}", short())); cancelled = true; break; }
                Ok(o) => {
                    let e = friendly_dump_err(&first_err(&String::from_utf8_lossy(&o.stderr)));
                    // A per-table dump carries no CREATE DATABASE or USE, so it has nowhere to go
                    // unless a target is chosen. "No database selected" is accurate and useless.
                    let hint = if e.contains("1046") || e.contains("No database selected") {
                        "\n  This file has no CREATE DATABASE/USE of its own - set \"Target database\" in the Import dialog."
                    } else { "" };
                    log.push(format!("FAILED {} : {}{}", short(), e, hint));
                }
                Err(e) => log.push(format!("FAILED {} : {}", f, e)),
            }
        }
        Ok(json!({"ok":true,"cancelled":cancelled,"errorsSkipped":errors_skipped,"log":log}))
    }).await.map_err(|e| e.to_string())?
}

// Mirrors the PowerShell version's Api-Export exactly: three modes (table = one file per
// table, the default; db = one file per database; single = one combined file), a set of
// excluded "db.table" entries turned into --ignore-table flags (or skipped entirely in table
// mode), an optional timestamp suffix, and the same mysqldump flag set (including tz-utc and
// max-allowed-packet). Routines/events are database-level, so table mode writes them to one
// extra "<db>.routines_events.sql" file per database, same as the PowerShell backend.
#[tauri::command]
async fn export(app: tauri::AppHandle, req: Value) -> R {
    let dbin = match resolve_tool(&app, "mysqldump", &["mysqldump", "mariadb-dump"], "MYSQLDUMP_BIN") { Ok(b) => b, Err(e) => return Ok(json!({"ok":false,"error":e})) };
    export_run(req, dbin).await
}

// mysqldump's --databases mode can only exclude tables via a repeated --ignore-table flag per
// table - no ignore-list file, no wildcard. A schema with hundreds of tables where the user
// only wants (or only excludes) a handful can blow straight through Windows' ~32K-character
// CreateProcess command-line limit, failing with the unhelpful "os error 206: The filename or
// extension is too long" despite a perfectly ordinary filename. Picks whichever side of the
// include/exclude split is smaller for database `d`: a short exclude list stays --ignore-table
// (as before); a short include list switches to naming those tables positionally instead
// (mysqldump db table1 table2 ... dumps only the named tables - no --databases needed, but this
// only works for a single database at a time). Returns (ignore_table_args, positional_tables) -
// exactly one of the two is non-empty whenever `d` has any exclusions at all.
fn table_filter_args(d: &str, excl: &std::collections::HashSet<String>, conn_req: &Value) -> Result<(Vec<String>, Vec<String>), String> {
    let prefix = format!("{}.", d);
    let this_excl: Vec<&str> = excl.iter().filter(|k| k.starts_with(&prefix)).map(|k| k.as_str()).collect();
    if this_excl.is_empty() { return Ok((vec![], vec![])); }
    // A normal handful of exclusions is cheap as --ignore-table either way - only worth the
    // extra information_schema round-trip once the exclude list itself is already sizeable.
    if this_excl.len() < 40 {
        return Ok((this_excl.iter().map(|k| format!("--ignore-table={}", k)).collect(), vec![]));
    }
    let mut conn = build_conn(conn_req)?;
    let sql = format!("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA={} ORDER BY TABLE_NAME", sql_lit(d));
    let (_cols, rows) = run_select(&mut conn, &sql)?;
    let excl_names: std::collections::HashSet<&str> = this_excl.iter().map(|k| k.splitn(2, '.').nth(1).unwrap_or("")).collect();
    let included: Vec<String> = rows.iter().filter_map(|r| r.get(0).cloned().flatten())
        .filter(|t| !excl_names.contains(t.as_str())).collect();
    if this_excl.len() <= included.len() {
        Ok((this_excl.iter().map(|k| format!("--ignore-table={}", k)).collect(), vec![]))
    } else {
        Ok((vec![], included))
    }
}

// The body, split out so it can be driven from a test without a tauri::AppHandle - resolving
// the mysqldump path is the only thing the handle was needed for.
async fn export_run(req: Value, dbin: String) -> R {
    EXPORT_CANCEL.store(false, Ordering::SeqCst);
    tokio::task::spawn_blocking(move || {
        let jid = req["jobId"].as_str().unwrap_or("").to_string();
        let job = job_start(&jid);
        let _guard = JobGuard(jid);
        let (_f, cnf) = cnf_file(&req["conn"])?;
        let dbs: Vec<String> = req["dbs"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        if dbs.is_empty() { return Ok(json!({"ok":false,"error":"No databases selected."})); }
        let folder = req["folder"].as_str().unwrap_or(".").to_string();
        std::fs::create_dir_all(&folder).ok();
        let o = &req["options"];
        let flag = |k: &str| o[k].as_bool().unwrap_or(false);
        let stamp = if req["stamp"].as_bool().unwrap_or(false) {
            format!("_{}", chrono::Local::now().format("%Y%m%d_%H%M%S")) } else { String::new() };
        // mode: "table" (default), "db", or "single"; the older {single:true} flag still works.
        let mode = req["mode"].as_str().map(String::from).unwrap_or_else(|| {
            if req["single"].as_bool().unwrap_or(false) { "single".into() } else { "table".into() }
        });
        let excl: std::collections::HashSet<String> = req["excludes"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let safe_name = |n: &str| -> String { n.chars().map(|c| if c.is_alphanumeric() || c=='_' || c=='.' || c=='-' { c } else { '_' }).collect() };
        let mkfile = |base: &str| format!("{}/{}{}.sql", folder.trim_end_matches(['/', '\\']), safe_name(base), stamp);
        // Only meaningful in "single" mode - db/table mode each produce one file per object, so
        // a single manual name has nowhere to go. A trailing .sql the user typed themselves is
        // stripped so it doesn't end up doubled ("backup.sql" + mkfile's own ".sql" suffix).
        let custom_name = req["filename"].as_str().unwrap_or("").trim().to_string();
        let single_base = if custom_name.is_empty() { "all_selected".to_string() } else {
            custom_name.strip_suffix(".sql").or_else(|| custom_name.strip_suffix(".SQL")).unwrap_or(&custom_name).to_string()
        };

        // Flags shared by every mysqldump call in this run (no database-level flags here -
        // those differ between "table" mode, which dumps table-by-table, and db/single modes).
        let mut common = vec![format!("--defaults-extra-file={}", cnf), format!("--default-character-set={}", o["charset"].as_str().unwrap_or("utf8mb4"))];
        if flag("singletx") { common.push("--single-transaction".into()); }
        if flag("quick") { common.push("--quick".into()); }
        if flag("hexblob") { common.push("--hex-blob".into()); }
        if flag("triggers") { common.push("--triggers".into()); } else { common.push("--skip-triggers".into()); }
        if flag("diskeys") { common.push("--disable-keys".into()); }
        if flag("notablespaces") { common.push("--no-tablespaces".into()); }
        if flag("colstats") { common.push("--column-statistics=0".into()); }
        if flag("compress") { common.push("--compress".into()); }
        if flag("gtid") { common.push("--set-gtid-purged=OFF".into()); }
        if flag("complete") { common.push("--complete-insert".into()); }
        if flag("extinsert") { common.push("--extended-insert".into()); } else { common.push("--skip-extended-insert".into()); }
        if flag("tzutc") { common.push("--tz-utc".into()); } else { common.push("--skip-tz-utc".into()); }
        if let Some(mp) = o["maxpacket"].as_str() { if !mp.is_empty() { common.push(format!("--max-allowed-packet={}", mp)); } }

        // mysqldump has no flag for this (unlike HeidiSQL's exporter) - DEFINER=`user`@`host`
        // hardcodes whichever MySQL account happened to create each view/trigger/procedure/event
        // into the dump. Restoring on a server where that exact account doesn't exist (a
        // different host, a managed DB service, a teammate's machine, CI) then fails or warns on
        // every one of those objects. Stripping it leaves `SQL SECURITY DEFINER/INVOKER` intact
        // and just falls back to CURRENT_USER at creation time - safe on the same server too.
        let definer_re = if flag("nodefiner") { Some(regex::Regex::new(r"DEFINER=`(?:[^`]|``)*`@`(?:[^`]|``)*`\s*").unwrap()) } else { None };

        let run = |bin: &str, args: &[String], file: &str| -> Result<(bool, String), String> {
            let mut cmd = Command::new(bin);
            cmd.args(args);
            let out = run_job_child(job.as_ref(), &mut cmd);
            match out {
                Ok(o2) if o2.status.success() => {
                    if let Some(re) = &definer_re {
                        if let Ok(content) = std::fs::read_to_string(file) {
                            let stripped = re.replace_all(&content, "");
                            let _ = std::fs::write(file, stripped.as_ref());
                        }
                    }
                    let sz = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
                    Ok((true, format!("OK  {} ({:.2} MB)", file, sz as f64 / 1048576.0)))
                }
                Ok(o2) => {
                    // A child killed by Cancel has no stderr to report - run_job_child skips
                    // reading it, because anything holding the pipe open would stall exactly
                    // the cancel it was asked to perform. That produced "FAILED <table> : "
                    // with nothing after the colon. Name the real reason instead, and never
                    // report an empty one.
                    let e = first_err(&String::from_utf8_lossy(&o2.stderr));
                    if !e.trim().is_empty() { Ok((false, friendly_dump_err(&e))) }
                    else if EXPORT_CANCEL.load(Ordering::SeqCst) || job_is_cancelled(&job) { Ok((false, RUN_CANCELLED.into())) }
                    else { Ok((false, format!("mysqldump exited with {} and no error output", o2.status))) }
                }
                Err(e) => Ok((false, e.to_string())),
            }
        };

        let mut log: Vec<String> = Vec::new();
        let mut cancelled = false;

        if mode == "single" {
            let file = mkfile(&single_base);
            // Positional per-table filtering only works against a single database at a time, so
            // the command-line-length fix (see table_filter_args) only kicks in when exactly one
            // db is selected - a multi-database single-file export needs --databases to combine
            // them anyway, and heavy exclusions on TOP of that is a rarer combination left as-is.
            let mut positional: Option<Vec<String>> = None;
            let mut listing_failed = false;
            if dbs.len() == 1 {
                match table_filter_args(&dbs[0], &excl, &req["conn"]) {
                    Ok((_, included)) if !included.is_empty() => positional = Some(included),
                    Ok(_) => {}
                    Err(e) => { log.push(format!("FAILED (list tables) {} : {}", dbs[0], e)); listing_failed = true; }
                }
            }
            if !listing_failed {
                let mut a = common.clone();
                if let Some(included) = positional {
                    if flag("routines") { a.push("--routines".into()); }
                    if flag("events") { a.push("--events".into()); }
                    if flag("adddroptb") { a.push("--add-drop-table".into()); } else { a.push("--skip-add-drop-table".into()); }
                    a.push(dbs[0].clone());
                    a.extend(included);
                } else {
                    a.push("--databases".into());
                    if flag("routines") { a.push("--routines".into()); }
                    if flag("events") { a.push("--events".into()); }
                    if flag("adddropdb") { a.push("--add-drop-database".into()); }
                    if flag("adddroptb") { a.push("--add-drop-table".into()); } else { a.push("--skip-add-drop-table".into()); }
                    if !flag("createdb") { a.push("--no-create-db".into()); }
                    for k in &excl { a.push(format!("--ignore-table={}", k)); }
                    for d in &dbs { a.push(d.clone()); }
                }
                a.push(format!("--result-file={}", file));
                match run(&dbin, &a, &file) {
                    Ok((true, msg)) => log.push(msg),
                    Ok((false, err)) => { if err == RUN_CANCELLED { log.push(format!("CANCELLED {}", single_base)); cancelled = true; } else { log.push(format!("FAILED {} : {}", single_base, err)); } }
                    Err(e) => log.push(format!("FAILED {} : {}", single_base, e)),
                }
            }
        } else if mode == "db" {
            for d in &dbs {
                if EXPORT_CANCEL.load(Ordering::SeqCst) || job_is_cancelled(&job) { log.push("CANCELLED (remaining databases skipped)".into()); cancelled = true; break; }
                let file = mkfile(d);
                let positional = match table_filter_args(d, &excl, &req["conn"]) {
                    Ok((_, included)) if !included.is_empty() => Some(included),
                    Ok(_) => None,
                    Err(e) => { log.push(format!("FAILED (list tables) {} : {}", d, e)); continue; }
                };
                let mut a = common.clone();
                if let Some(included) = positional {
                    if flag("routines") { a.push("--routines".into()); }
                    if flag("events") { a.push("--events".into()); }
                    if flag("adddroptb") { a.push("--add-drop-table".into()); } else { a.push("--skip-add-drop-table".into()); }
                    a.push(d.clone());
                    a.extend(included);
                } else {
                    a.push("--databases".into());
                    if flag("routines") { a.push("--routines".into()); }
                    if flag("events") { a.push("--events".into()); }
                    if flag("adddropdb") { a.push("--add-drop-database".into()); }
                    if flag("adddroptb") { a.push("--add-drop-table".into()); } else { a.push("--skip-add-drop-table".into()); }
                    if !flag("createdb") { a.push("--no-create-db".into()); }
                    for k in &excl { if k.starts_with(&format!("{}.", d)) { a.push(format!("--ignore-table={}", k)); } }
                    a.push(d.clone());
                }
                a.push(format!("--result-file={}", file));
                match run(&dbin, &a, &file) {
                    Ok((true, msg)) => log.push(msg),
                    Ok((false, err)) => { if err == RUN_CANCELLED { log.push(format!("CANCELLED {}", d)); cancelled = true; } else { log.push(format!("FAILED {} : {}", d, err)); } }
                    Err(e) => log.push(format!("FAILED {} : {}", d, e)),
                }
            }
        } else {
            // PER TABLE (default): dump every table to its own file, like Workbench's Dump Project Folder.
            'dbloop: for d in &dbs {
                if EXPORT_CANCEL.load(Ordering::SeqCst) || job_is_cancelled(&job) { log.push("CANCELLED (remaining databases skipped)".into()); cancelled = true; break; }
                let mut conn = match build_conn(&req["conn"]) { Ok(c) => c, Err(e) => { log.push(format!("FAILED (connect) {} : {}", d, e)); continue; } };
                let sql = format!("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA={} ORDER BY TABLE_NAME", sql_lit(d));
                let tabs: Vec<String> = match run_select(&mut conn, &sql) {
                    Ok((_cols, rows)) => rows.iter().filter_map(|r| r.get(0).cloned().flatten()).collect(),
                    Err(e) => { log.push(format!("FAILED (list tables) {} : {}", d, e)); continue; }
                };
                if tabs.is_empty() { log.push(format!("(no tables) {}", d)); }
                for t in &tabs {
                    if EXPORT_CANCEL.load(Ordering::SeqCst) || job_is_cancelled(&job) { log.push("CANCELLED (remaining tables skipped)".into()); cancelled = true; break 'dbloop; }
                    let key = format!("{}.{}", d, t);
                    if excl.contains(&key) { log.push(format!("(excluded) {}", key)); continue; }
                    let file = mkfile(&key);
                    let mut a = common.clone();
                    if flag("adddroptb") { a.push("--add-drop-table".into()); } else { a.push("--skip-add-drop-table".into()); }
                    a.push(d.clone()); a.push(t.clone());
                    a.push(format!("--result-file={}", file));
                    match run(&dbin, &a, &file) {
                        Ok((true, msg)) => log.push(msg),
                        Ok((false, err)) => { if err == RUN_CANCELLED { log.push(format!("CANCELLED {}", key)); cancelled = true; } else { log.push(format!("FAILED {} : {}", key, err)); } }
                        Err(e) => log.push(format!("FAILED {} : {}", key, e)),
                    }
                }
                if cancelled { break; }
                if flag("routines") || flag("events") {
                    let file = mkfile(&format!("{}.routines_events", d));
                    let mut a = common.clone();
                    a.push("--no-create-info".into()); a.push("--no-data".into()); a.push("--no-create-db".into()); a.push("--skip-triggers".into());
                    if flag("routines") { a.push("--routines".into()); }
                    if flag("events") { a.push("--events".into()); }
                    a.push(d.clone());
                    a.push(format!("--result-file={}", file));
                    match run(&dbin, &a, &file) {
                        Ok((true, msg)) => log.push(format!("{} (routines/events)", msg)),
                        Ok((false, err)) => { if err == RUN_CANCELLED { log.push(format!("CANCELLED {} routines/events", d)); cancelled = true; } else { log.push(format!("FAILED {} routines/events : {}", d, err)); } }
                        Err(e) => log.push(format!("FAILED {} routines/events : {}", d, e)),
                    }
                }
            }
        }
        Ok(json!({"ok":true,"cancelled":cancelled,"log":log}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn importcsv(req: Value) -> R {
    tokio::task::spawn_blocking(move || {
        if req["ro"].as_bool().unwrap_or(false) {
            return Ok(json!({"ok":false,"error":"Read-only mode: statement blocked."}));
        }
        let file = req["file"].as_str().unwrap_or("").to_string();
        if !std::path::Path::new(&file).exists() { return Ok(json!({"ok":false,"error":"CSV file not found."})); }
        let db = req["db"].as_str().unwrap_or("").to_string();
        let table = req["table"].as_str().unwrap_or("").to_string();
        let mut c = build_conn(&req["conn"])?;
        let colsql = format!("SELECT COLUMN_NAME,DATA_TYPE FROM information_schema.COLUMNS WHERE TABLE_SCHEMA={} AND TABLE_NAME={} ORDER BY ORDINAL_POSITION", sql_str_lit(&db), sql_str_lit(&table));
        let (_c, crows) = run_select(&mut c, &colsql)?;
        // sql_lit()'s "0xDEADBEEF passes through unquoted as a hex literal" rule exists so a
        // genuinely binary/BIT column can be filled from its own hex display - it was never meant
        // to apply to an ordinary text column that merely happens to contain a value that LOOKS
        // like hex ("0xFF", a hash, an ID). Used indiscriminately here, that silently reinterpreted
        // such a CSV cell as raw bytes instead of the literal text, with no error. Only the columns
        // information_schema actually reports as binary/BIT get that treatment; everything else
        // goes through sql_str_lit(), which always quotes.
        const BIN_TYPES: &[&str] = &["binary", "varbinary", "blob", "tinyblob", "mediumblob", "longblob", "bit"];
        let bin_cols: std::collections::HashSet<String> = crows.iter()
            .filter(|r| {
                let ty = r.get(1).and_then(|v| v.as_deref()).unwrap_or("").to_lowercase();
                BIN_TYPES.contains(&ty.as_str())
            })
            .filter_map(|r| r.first().cloned().flatten()).collect();
        let table_cols: Vec<String> = crows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        if table_cols.is_empty() { return Ok(json!({"ok":false,"error":"Table not found or has no columns."})); }

        let has_header = req["hasHeader"].as_bool().unwrap_or(true);
        let mut rdr = csv::ReaderBuilder::new().has_headers(has_header).flexible(true)
            .from_path(&file).map_err(|e| format!("CSV open error: {}", e))?;
        let csv_cols: Vec<String> = if has_header {
            rdr.headers().map_err(|e| e.to_string())?.iter().map(String::from).collect()
        } else { table_cols.clone() };
        let null_marker = req["nullValue"].as_str().unwrap_or("\\N").to_string();
        let use_idx: Vec<usize> = csv_cols.iter().enumerate().filter(|(_, n)| table_cols.contains(n)).map(|(i, _)| i).collect();
        if use_idx.is_empty() { return Ok(json!({"ok":false,"error":"No CSV columns match the table columns (check the header row)."})); }
        let use_cols: Vec<String> = use_idx.iter().map(|&i| csv_cols[i].clone()).collect();
        let col_list = use_cols.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
        let obj = format!("{}.{}", sql_id(&db), sql_id(&table));

        // "Truncate table" + a mid-file failure (a bad value, an FK violation, disk full) used to
        // leave the table permanently truncated and only partially reloaded - every batch ran on
        // autocommit with nothing to undo the ones that had already landed. Wrapped in the same
        // START TRANSACTION/COMMIT/ROLLBACK pattern the grid's own edit-apply path uses for
        // exactly this failure mode (see the comment above it, near req["transaction"]).
        //
        // TRUNCATE TABLE itself is DDL - MySQL/MariaDB implicitly commit it the moment it runs,
        // transaction or not, so wrapping THAT in START TRANSACTION would do nothing to protect
        // it. DELETE FROM (no WHERE) does the same job here and, unlike TRUNCATE, is ordinary
        // transactional DML that a ROLLBACK genuinely undoes - the one real cost is that it
        // doesn't reset an AUTO_INCREMENT counter the way TRUNCATE does.
        c.query_drop("START TRANSACTION").map_err(|e| e.to_string())?;
        let import_result: Result<usize, String> = (|| {
            if req["truncate"].as_bool().unwrap_or(false) {
                c.query_drop(format!("DELETE FROM {}", obj)).map_err(|e| e.to_string())?;
            }
            let _ = c.query_drop("SET FOREIGN_KEY_CHECKS=0; SET UNIQUE_CHECKS=0");
            let mut n = 0usize; let mut batch: Vec<String> = Vec::new();
            for rec in rdr.records() {
                let rec = rec.map_err(|e| e.to_string())?;
                let vals: Vec<String> = use_idx.iter().enumerate().map(|(ci, &i)| {
                    // The marker decides what an empty cell means. With one set (the default, \N)
                    // the file states NULL explicitly, so an empty cell is an empty string and a
                    // round trip keeps both. Clearing the marker restores the older reading, where a
                    // blank means NULL - which is what a spreadsheet usually intends.
                    match rec.get(i) {
                        None => "NULL".to_string(),
                        Some(s) if !null_marker.is_empty() && s == null_marker => "NULL".to_string(),
                        Some("") if null_marker.is_empty() => "NULL".to_string(),
                        // sql_lit()'s hex-literal passthrough only applies to a column information_schema
                        // actually reports as binary/BIT - anything else always gets a real quoted string,
                        // even if the cell's text happens to look like hex (see bin_cols above).
                        Some(s) if bin_cols.contains(&use_cols[ci]) => sql_lit(s),
                        Some(s) => sql_str_lit(s),
                    }
                }).collect();
                batch.push(format!("({})", vals.join(","))); n += 1;
                if batch.len() >= 500 {
                    c.query_drop(format!("INSERT INTO {} ({}) VALUES {}", obj, col_list, batch.join(","))).map_err(|e| e.to_string())?;
                    batch.clear();
                }
            }
            if !batch.is_empty() {
                c.query_drop(format!("INSERT INTO {} ({}) VALUES {}", obj, col_list, batch.join(","))).map_err(|e| e.to_string())?;
            }
            Ok(n)
        })();
        let n = match import_result {
            Ok(n) => n,
            Err(e) => { let _ = c.query_drop("ROLLBACK"); return Ok(json!({"ok":false,"error":format!("{}\n\nNo rows were imported - the batch was rolled back.", e)})); }
        };
        if let Err(e) = c.query_drop("COMMIT") {
            let _ = c.query_drop("ROLLBACK");
            return Ok(json!({"ok":false,"error":format!("Could not commit: {}\n\nNo rows were imported.", e.to_string())}));
        }
        Ok(json!({"ok":true,"message":format!("Imported {} row(s) into {}.{} (columns: {})", n, db, table, use_cols.join(", "))}))
    }).await.map_err(|e| e.to_string())?
}

// ---------- connection profiles (config file + OS keychain) ----------
fn conn_path() -> std::path::PathBuf {
    let mut p = dirs::config_dir().unwrap_or(std::env::temp_dir());
    p.push("NOBSSQL-Desktop"); std::fs::create_dir_all(&p).ok(); p.push("connections.json"); p
}
fn load_profiles() -> Vec<Value> {
    std::fs::read_to_string(conn_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_profiles(v: &Vec<Value>) { let _ = std::fs::write(conn_path(), serde_json::to_string_pretty(v).unwrap_or_default()); }

// A saved connection's non-secret fields. The password itself never lives in this JSON file -
// it goes through the OS keychain via `keyring`, same idea as DPAPI in the PowerShell version.
#[tauri::command]
async fn conn_list(_req: Value) -> R {
    // hasPassword: an actual keyring lookup per connection (not a field-presence check like the
    // PowerShell version, since here the password itself isn't stored alongside the connection's
    // other metadata at all - it lives in the OS keychain, keyed by connection name, matching
    // exactly how conn_get already retrieves it). Only checks whether an entry exists; never
    // logs or uses the returned secret itself for anything beyond that.
    let items: Vec<Value> = load_profiles().into_iter().map(|c| {
        let name = c["name"].as_str().unwrap_or("");
        let has_password = keyring::Entry::new("NOBSSQL-Desktop", name).ok().and_then(|e| e.get_password().ok()).is_some();
        json!({
            "name": c["name"], "host": c["host"], "port": c["port"], "user": c["user"], "ssl": c["ssl"],
            "accent": c["accent"], "env": c["env"], "readonly": c["readonly"].as_bool().unwrap_or(false),
            "primary": c["primary"].as_bool().unwrap_or(false), "hasPassword": has_password
        })
    }).collect();
    Ok(json!({"ok":true,"items":items}))
}
#[tauri::command]
async fn conn_get(req: Value) -> R {
    let name = req["name"].as_str().unwrap_or("").to_string();
    let c = load_profiles().into_iter().find(|c| c["name"].as_str() == Some(&name));
    match c {
        Some(c) => {
            let pass = keyring::Entry::new("NOBSSQL-Desktop", &name).ok().and_then(|e| e.get_password().ok()).unwrap_or_default();
            Ok(json!({"ok":true,"conn":{"host":c["host"],"port":c["port"],"user":c["user"],"ssl":c["ssl"],"password":pass},
                "accent":c["accent"],"env":c["env"],"readonly":c["readonly"].as_bool().unwrap_or(false)}))
        }
        None => Ok(json!({"ok":false})),
    }
}
#[tauri::command]
async fn conn_save(req: Value) -> R {
    let name = req["name"].as_str().unwrap_or("").to_string();
    if name.is_empty() { return Ok(json!({"ok":false,"error":"name required"})); }
    let conn = &req["conn"];
    // savepw defaults to true (matches the PS backend's "quick save preserves the existing password" behavior);
    // savepw:false explicitly removes any saved password for this connection.
    let save_pw = req.get("savepw").and_then(|v| v.as_bool()).unwrap_or(true);
    if !save_pw {
        if let Ok(e) = keyring::Entry::new("NOBSSQL-Desktop", &name) { let _ = e.delete_credential(); }
    } else if let Some(pw) = conn["password"].as_str() {
        if !pw.is_empty() {
            if let Ok(e) = keyring::Entry::new("NOBSSQL-Desktop", &name) { let _ = e.set_password(pw); }
        }
        // empty password on a quick-save with savepw:true -> leave any existing keychain entry untouched
    }
    let before = load_profiles();
    let was_primary = before.iter().find(|c| c["name"].as_str() == Some(name.as_str()))
        .map(|c| c["primary"].as_bool().unwrap_or(false)).unwrap_or(false);
    let mut list: Vec<Value> = before.into_iter().filter(|c| c["name"].as_str() != Some(name.as_str())).collect();
    list.push(json!({
        "name": name, "host": conn["host"], "port": conn["port"], "user": conn["user"], "ssl": conn["ssl"],
        "accent": req.get("accent").cloned().unwrap_or(Value::Null),
        "env": req.get("env").cloned().unwrap_or(Value::Null),
        "readonly": req.get("readonly").and_then(|v| v.as_bool()).unwrap_or(false),
        "primary": was_primary
    }));
    save_profiles(&list);
    Ok(json!({"ok":true}))
}
#[tauri::command]
async fn conn_delete(req: Value) -> R {
    let name = req["name"].as_str().unwrap_or("").to_string();
    if let Ok(e) = keyring::Entry::new("NOBSSQL-Desktop", &name) { let _ = e.delete_credential(); }
    let list: Vec<Value> = load_profiles().into_iter().filter(|c| c["name"].as_str() != Some(&name)).collect();
    save_profiles(&list);
    Ok(json!({"ok":true}))
}
// Mirrors the PowerShell version's Api-ConnSetPrimary: "primary" is a plain field on each saved
// connection object (not a separate config key) - setting one clears it from all the others.
#[tauri::command]
async fn conn_primary(req: Value) -> R {
    let name = req["name"].as_str().unwrap_or("").to_string();
    let list: Vec<Value> = load_profiles().into_iter().map(|mut c| {
        let is_this_one = !name.is_empty() && c["name"].as_str() == Some(name.as_str());
        if let Some(obj) = c.as_object_mut() {
            obj.insert("primary".into(), json!(is_this_one));
        }
        c
    }).collect();
    save_profiles(&list);
    Ok(json!({"ok":true}))
}
// Wipes ALL saved connections (used by "Clear all app data"), including each one's keychain
// password entry so nothing orphaned is left behind in the OS credential store.
#[tauri::command]
async fn conn_clear(_req: Value) -> R {
    for c in load_profiles() {
        if let Some(name) = c["name"].as_str() {
            if let Ok(e) = keyring::Entry::new("NOBSSQL-Desktop", name) { let _ = e.delete_credential(); }
        }
    }
    save_profiles(&Vec::new());
    Ok(json!({"ok":true}))
}

// ---- Compare Databases: structure diff + one-way sync (source -> target) ----
// Mirrors the PowerShell version's Resolve-SavedConn/Get-SchemaColumns/Compare-TableSets exactly,
// so both apps generate the same diffs and the same SQL for the same two databases.

#[derive(Clone)]
struct ColumnDef { name: String, ctype: String, nullable: String, default: Option<String>, extra: String }

// Looks up a saved connection by name and returns (connection JSON usable with build_conn, readonly).
fn resolve_saved_conn(name: &str) -> Result<(Value, bool), String> {
    let c = load_profiles().into_iter().find(|c| c["name"].as_str() == Some(name))
        .ok_or_else(|| "Connection not found.".to_string())?;
    let pass = keyring::Entry::new("NOBSSQL-Desktop", name).ok().and_then(|e| e.get_password().ok()).unwrap_or_default();
    let connj = json!({"host":c["host"],"port":c["port"],"user":c["user"],"ssl":c["ssl"],"password":pass});
    Ok((connj, c["readonly"].as_bool().unwrap_or(false)))
}

fn get_schema_columns(conn: &mut Conn, db: &str) -> Result<std::collections::BTreeMap<String, Vec<ColumnDef>>, String> {
    let sql = format!(
        "SELECT TABLE_NAME,COLUMN_NAME,COLUMN_TYPE,IS_NULLABLE,COLUMN_DEFAULT,EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA={} ORDER BY TABLE_NAME,ORDINAL_POSITION",
        sql_lit(db)
    );
    let (_cols, rows) = run_select(conn, &sql)?;
    let mut map: std::collections::BTreeMap<String, Vec<ColumnDef>> = std::collections::BTreeMap::new();
    for r in rows {
        let t = r.get(0).cloned().flatten().unwrap_or_default();
        let cd = ColumnDef {
            name: r.get(1).cloned().flatten().unwrap_or_default(),
            ctype: r.get(2).cloned().flatten().unwrap_or_default(),
            nullable: r.get(3).cloned().flatten().unwrap_or_default(),
            default: r.get(4).cloned().flatten(),
            extra: r.get(5).cloned().flatten().unwrap_or_default(),
        };
        map.entry(t).or_insert_with(Vec::new).push(cd);
    }
    Ok(map)
}

fn get_create_table_sql(conn: &mut Conn, db: &str, table: &str) -> Option<String> {
    let sql = format!("SHOW CREATE TABLE {}.{}", sql_id(db), sql_id(table));
    run_select(conn, &sql).ok().and_then(|(_c, rows)| rows.into_iter().next()).and_then(|r| r.get(1).cloned().flatten())
}

// Best-effort DEFAULT clause: numeric and keyword defaults (CURRENT_TIMESTAMP, NULL) are emitted
// bare; everything else is quoted as a string literal. Always double-check via Preview SQL.
fn col_default_clause(default: &Option<String>) -> String {
    match default {
        None => String::new(),
        Some(d) => {
            let is_numeric = regex::Regex::new(r"^-?[0-9]+(\.[0-9]+)?$").unwrap().is_match(d);
            let is_keyword = regex::Regex::new(r"^(CURRENT_TIMESTAMP(\(\d*\))?|NULL)$").unwrap().is_match(d);
            if is_numeric || is_keyword { format!(" DEFAULT {}", d) } else { format!(" DEFAULT {}", sql_lit(d)) }
        }
    }
}
fn col_def_line(col: &ColumnDef) -> String {
    let null_part = if col.nullable == "YES" { "NULL" } else { "NOT NULL" };
    let extra_part = if col.extra.is_empty() { String::new() } else { format!(" {}", col.extra) };
    format!("{} {} {}{}{}", sql_id(&col.name), col.ctype, null_part, col_default_clause(&col.default), extra_part)
}

struct SqlStmt { stmt: String, checked: bool, kind: &'static str }
struct TableDiff { name: String, status: &'static str, sql: Vec<SqlStmt> }

fn compare_table_sets(
    src_conn: &mut Conn, src_db: &str,
    src_cols: &std::collections::BTreeMap<String, Vec<ColumnDef>>,
    tgt_cols: &std::collections::BTreeMap<String, Vec<ColumnDef>>,
    request_id: Option<&str>,
) -> (Vec<TableDiff>, bool) {
    let mut names: Vec<&String> = src_cols.keys().chain(tgt_cols.keys()).collect();
    names.sort(); names.dedup();
    let mut out = Vec::new();
    let mut cancelled = false;
    for t in names {
        if let Some(rid) = request_id { if is_compare_cancelled(rid) { cancelled = true; break; } }
        let in_src = src_cols.contains_key(t);
        let in_tgt = tgt_cols.contains_key(t);
        if in_src && !in_tgt {
            let ddl = get_create_table_sql(&mut *src_conn, src_db, t).unwrap_or_default();
            out.push(TableDiff { name: t.clone(), status: "missing_target",
                sql: vec![SqlStmt { stmt: ddl, checked: true, kind: "create_table" }] });
            continue;
        }
        if in_tgt && !in_src {
            out.push(TableDiff { name: t.clone(), status: "missing_source",
                sql: vec![SqlStmt { stmt: format!("DROP TABLE {};", sql_id(t)), checked: false, kind: "drop_table" }] });
            continue;
        }
        let s_cols = &src_cols[t]; let t_cols = &tgt_cols[t];
        let t_by_name: std::collections::HashMap<&str, &ColumnDef> = t_cols.iter().map(|c| (c.name.as_str(), c)).collect();
        let s_by_name: std::collections::HashMap<&str, &ColumnDef> = s_cols.iter().map(|c| (c.name.as_str(), c)).collect();
        let mut diffs = Vec::new();
        for c in s_cols {
            match t_by_name.get(c.name.as_str()) {
                None => diffs.push(SqlStmt { stmt: format!("ALTER TABLE {} ADD COLUMN {};", sql_id(t), col_def_line(c)), checked: true, kind: "add_column" }),
                Some(tc) => {
                    if c.ctype != tc.ctype || c.nullable != tc.nullable || c.default != tc.default {
                        diffs.push(SqlStmt { stmt: format!("ALTER TABLE {} MODIFY COLUMN {};", sql_id(t), col_def_line(c)), checked: true, kind: "modify_column" });
                    }
                }
            }
        }
        for c in t_cols {
            if !s_by_name.contains_key(c.name.as_str()) {
                diffs.push(SqlStmt { stmt: format!("ALTER TABLE {} DROP COLUMN {};", sql_id(t), sql_id(&c.name)), checked: false, kind: "drop_column" });
            }
        }
        if diffs.is_empty() { out.push(TableDiff { name: t.clone(), status: "same", sql: Vec::new() }); }
        else { out.push(TableDiff { name: t.clone(), status: "diff", sql: diffs }); }
    }
    (out, cancelled)
}

// Reusable primary-key column lookup (plain Vec, not a JSON response) - used by the row-level
// compare below. Returns an empty Vec if the table has no primary key.
fn get_table_pk_cols(conn: &mut Conn, db: &str, table: &str) -> Result<Vec<String>, String> {
    let sql = format!(
        "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA={} AND TABLE_NAME={} AND CONSTRAINT_NAME='PRIMARY' ORDER BY ORDINAL_POSITION",
        sql_lit(db), sql_lit(table)
    );
    let (_cols, rows) = run_select(conn, &sql)?;
    Ok(rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
}
fn get_table_fk_cols(conn: &mut Conn, db: &str, table: &str) -> Result<Vec<String>, String> {
    let sql = format!(
        "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA={} AND TABLE_NAME={} AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY ORDINAL_POSITION",
        sql_lit(db), sql_lit(table)
    );
    let (_cols, rows) = run_select(conn, &sql)?;
    Ok(rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
}
// Joins a row's cell values with a control character (0x01) that can't appear in normal data,
// to build a single comparable key for both single-column and composite primary keys.
fn row_key(row: &[Option<String>]) -> String {
    row.iter().map(|v| v.clone().unwrap_or_default()).collect::<Vec<_>>().join("\u{1}")
}

// Fetches full row data for a SPECIFIC list of primary-key value combinations, chunked (same
// reasoning as elsewhere: MySQL's max_allowed_packet and general sanity for very large IN-lists).
// Shared by compare_rows (its first page) and compare_rows_fetch_by_pk (loading a later page the
// client already knows about, without re-scanning the whole table again).
fn get_rows_by_pk(conn: &mut Conn, db: &str, table: &str, pk_cols: &[String], pk_values: &[Vec<Option<String>>]) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), String> {
    if pk_values.is_empty() { return Ok((Vec::new(), Vec::new())); }
    let pk_list = pk_cols.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
    let fetch_chunk = 200;
    let mut full_cols: Vec<String> = Vec::new();
    let mut full_rows: Vec<Vec<Option<String>>> = Vec::new();
    for chunk in pk_values.chunks(fetch_chunk) {
        let where_clause = if pk_cols.len() == 1 {
            let vals = chunk.iter().map(|r| sql_val_lit(&r[0].clone().unwrap_or_default())).collect::<Vec<_>>().join(",");
            format!("{} IN ({})", sql_id(&pk_cols[0]), vals)
        } else {
            let tuples = chunk.iter().map(|r| {
                let vs = r.iter().map(|v| sql_val_lit(&v.clone().unwrap_or_default())).collect::<Vec<_>>().join(",");
                format!("({})", vs)
            }).collect::<Vec<_>>().join(",");
            format!("({}) IN ({})", pk_list, tuples)
        };
        let (cols, rows) = run_select(conn, &format!("SELECT * FROM {}.{} WHERE {}", sql_id(db), sql_id(table), where_clause))?;
        if full_cols.is_empty() { full_cols = cols; }
        full_rows.extend(rows);
    }
    Ok((full_cols, full_rows))
}

// Standalone command for loading a LATER page of missing rows the client already knows about
// (from compare_rows' allMissingPks) - a lightweight, bounded fetch that never re-scans the
// whole table, unlike re-running the full comparison.
#[tauri::command]
async fn compare_rows_fetch_by_pk(req: Value) -> R {
    let src_name = req["sourceConnName"].as_str().unwrap_or("").to_string();
    let src_db = req["sourceDb"].as_str().unwrap_or("").to_string();
    let table = req["table"].as_str().unwrap_or("").to_string();
    let pk_cols: Vec<String> = req["pkCols"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    let pks: Vec<Vec<Option<String>>> = req["pks"].as_array().map(|a| a.iter().map(|row| {
        row.as_array().map(|r| r.iter().map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
    }).collect()).unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        if pk_cols.is_empty() { return Ok(json!({"ok":false,"error":"Missing primary key columns."})); }
        let (src_connj, _) = resolve_saved_conn(&src_name)?;
        let mut src_conn = build_conn(&src_connj)?;
        let (cols, rows) = get_rows_by_pk(&mut src_conn, &src_db, &table, &pk_cols, &pks)?;
        Ok(json!({"ok":true,"columns":cols,"rows":rows}))
    }).await.map_err(|e| e.to_string())?
}

// Generates a user + grants transfer script for the CURRENT connection, using SHOW CREATE USER
// and SHOW GRANTS FOR rather than hand-building CREATE USER/GRANT text from the grant tables
// directly. This matters: SHOW CREATE USER encodes whatever auth plugin and password hash the
// account actually uses (native password, ed25519, unix_socket, etc.) instead of assuming
// mysql_native_password, and SHOW GRANTS FOR already includes column/routine grants, WITH GRANT
// OPTION, and (on MariaDB) role grants - all of which a plain SELECT against the grant tables
// would silently miss. Works unchanged on MySQL 5.7.6+ and MariaDB 10.2+.
// CREATE USER statements are emitted before any GRANT statements (genuinely grouped that way,
// not just alphabetically) so replaying the result on a target server never grants to a user
// that doesn't exist yet.
#[tauri::command]
async fn gen_user_transfer(req: Value) -> R {
    let exclude_raw = req["exclude"].as_str().unwrap_or("").to_string();
    let conn_json = req["conn"].clone();
    tokio::task::spawn_blocking(move || {
        let mut excl: Vec<String> = exclude_raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if excl.is_empty() {
            excl = ["mysql.sys","root","debian-sys-maint","mariadb.sys","healthcheck","mariabackup","galera","replica","PUBLIC"].iter().map(|s| s.to_string()).collect();
        }
        let in_list = excl.iter().map(|s| sql_val_lit(s)).collect::<Vec<_>>().join(",");
        let mut conn = build_conn(&conn_json)?;
        let (_cols, user_rows) = run_select(&mut conn, &format!("SELECT user, host FROM mysql.user WHERE user NOT IN ({}) AND user <> ''", in_list))?;
        if user_rows.is_empty() {
            return Ok(json!({"ok":true,"sql":"-- No accounts matched (everything was excluded, or mysql.user is empty).","userCount":0,"errorCount":0}));
        }
        let mut create_lines: Vec<String> = Vec::new();
        let mut grant_lines: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for row in &user_rows {
            let u = row.get(0).cloned().flatten().unwrap_or_default();
            let h = row.get(1).cloned().flatten().unwrap_or_default();
            let uq = sql_str_lit(&u);
            let hq = sql_str_lit(&h);
            match run_select(&mut conn, &format!("SHOW CREATE USER {}@{}", uq, hq)) {
                Ok((_c, rows)) => {
                    if let Some(first) = rows.get(0).and_then(|r| r.get(0)).cloned().flatten() {
                        create_lines.push(format!("{};", first));
                    } else {
                        errors.push(format!("SHOW CREATE USER for '{}'@'{}': no result returned", u, h));
                    }
                }
                Err(e) => errors.push(format!("SHOW CREATE USER for '{}'@'{}': {}", u, h, e)),
            }
            match run_select(&mut conn, &format!("SHOW GRANTS FOR {}@{}", uq, hq)) {
                Ok((_c, rows)) => {
                    for r in rows {
                        if let Some(v) = r.get(0).cloned().flatten() { grant_lines.push(format!("{};", v)); }
                    }
                }
                Err(e) => errors.push(format!("SHOW GRANTS for '{}'@'{}': {}", u, h, e)),
            }
        }
        let mut out = String::new();
        out.push_str(&format!("-- Generated user transfer script - {} account(s) matched (after exclusions)\n", user_rows.len()));
        out.push_str("-- Run this on the TARGET server. CREATE USER statements are listed first so the GRANT\n");
        out.push_str("-- statements below can reference them.\n\n");
        out.push_str("-- ===== CREATE USER =====\n");
        for l in &create_lines { out.push_str(l); out.push('\n'); }
        out.push_str("\n-- ===== GRANTS =====\n");
        for l in &grant_lines { out.push_str(l); out.push('\n'); }
        if !errors.is_empty() {
            out.push_str(&format!("\n-- ===== {} account(s) could not be read (the script above is complete for everyone else) =====\n", errors.len()));
            for e in &errors { out.push_str("-- "); out.push_str(&e.replace('\n', " ").replace('\r', " ")); out.push('\n'); }
        }
        Ok(json!({"ok":true,"sql":out,"userCount":user_rows.len(),"errorCount":errors.len()}))
    }).await.map_err(|e| e.to_string())?
}

// Finds rows present in the source table but missing (by primary key) on the target - INSERT
// only, never UPDATE/DELETE. Row data is fetched from the source and re-inserted with the exact
// same primary key value(s), so ids stay identical between the two databases. Capped at 2000
// rows per comparison to stay interactive; a larger gap should go through Export/Import instead.
#[tauri::command]
async fn compare_rows(req: Value) -> R {
    let src_name = req["sourceConnName"].as_str().unwrap_or("").to_string();
    let tgt_name = req["targetConnName"].as_str().unwrap_or("").to_string();
    let src_db = req["sourceDb"].as_str().unwrap_or("").to_string();
    let tgt_db = req["targetDb"].as_str().unwrap_or("").to_string();
    let table = req["table"].as_str().unwrap_or("").to_string();
    let rid = req["requestId"].as_str().map(String::from);
    tokio::task::spawn_blocking(move || {
        // A single SELECT can't be interrupted mid-flight once the driver call has started (no
        // chunking here, unlike compare_rows_diff below), so this can only bail out BETWEEN the
        // blocking steps - still meaningful for cmpScanRowDiffs' bulk scan, which calls this once
        // per table: it skips the (often equally expensive) target-side query and the display-row
        // fetch entirely once Stop has been clicked, rather than doing that work for nothing.
        let cancelled_now = |rid: &Option<String>| rid.as_deref().map(is_compare_cancelled).unwrap_or(false);
        if cancelled_now(&rid) { return Ok(json!({"ok":true,"pkCols":Vec::<String>::new(),"columns":Vec::<String>::new(),"rows":Vec::<Value>::new(),"missingTotal":0,"truncated":false,"targetReadonly":false,"allMissingPks":Vec::<Value>::new(),"cancelled":true})); }
        let (src_connj, _) = resolve_saved_conn(&src_name)?;
        let (tgt_connj, tgt_ro) = resolve_saved_conn(&tgt_name)?;
        let mut src_conn = build_conn(&src_connj)?;
        let mut tgt_conn = build_conn(&tgt_connj)?;
        // From here on, a Cancel click can KILL QUERY whichever of these two connections is
        // actually blocked on a SELECT right now, instead of only being noticed once that query
        // finishes on its own. _guard drops (and unregisters) on every return path below.
        register_compare_conn(&rid, &mut src_conn, &src_connj);
        register_compare_conn(&rid, &mut tgt_conn, &tgt_connj);
        let _guard = CompareConnGuard(rid.clone());
        let pk = get_table_pk_cols(&mut src_conn, &src_db, &table)?;
        if pk.is_empty() { return Ok(json!({"ok":false,"error":"Table has no primary key - cannot compare rows."})); }
        let fk = get_table_fk_cols(&mut src_conn, &src_db, &table).unwrap_or_default();
        let pk_list = pk.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
        let (_c1, src_pk_rows) = match run_select(&mut src_conn, &format!("SELECT {} FROM {}.{}", pk_list, sql_id(&src_db), sql_id(&table))) {
            Ok(v) => v,
            // A KILL QUERY from Cancel surfaces here as an ordinary MySQL error, so check whether
            // this request was actually cancelled before reporting it as a real failure.
            Err(e) => {
                if cancelled_now(&rid) {
                    if let Some(r) = &rid { clear_compare_cancel(r); }
                    return Ok(json!({"ok":true,"pkCols":pk,"columns":Vec::<String>::new(),"rows":Vec::<Value>::new(),"missingTotal":0,"truncated":false,"targetReadonly":tgt_ro,"allMissingPks":Vec::<Value>::new(),"cancelled":true}));
                }
                return Ok(json!({"ok":false,"error":e}));
            }
        };
        if cancelled_now(&rid) {
            if let Some(r) = &rid { clear_compare_cancel(r); }
            return Ok(json!({"ok":true,"pkCols":pk,"columns":Vec::<String>::new(),"rows":Vec::<Value>::new(),"missingTotal":0,"truncated":false,"targetReadonly":tgt_ro,"allMissingPks":Vec::<Value>::new(),"cancelled":true}));
        }
        let tgt_pk_rows: Vec<Vec<Option<String>>> = match run_select(&mut tgt_conn, &format!("SELECT {} FROM {}.{}", pk_list, sql_id(&tgt_db), sql_id(&table))) {
            Ok((_c, rows)) => rows,
            // Target table hasn't been created yet - treat it as new/empty rather than failing,
            // so every source row correctly comes back as "missing".
            Err(e) => { if e.contains("1146") || e.to_lowercase().contains("doesn't exist") { Vec::new() } else { return Ok(json!({"ok":false,"error":e})); } }
        };
        let tgt_set: std::collections::HashSet<String> = tgt_pk_rows.iter().map(|r| row_key(r)).collect();
        let missing: Vec<Vec<Option<String>>> = src_pk_rows.into_iter().filter(|r| !tgt_set.contains(&row_key(r))).collect();
        let missing_total = missing.len();
        const CAP: usize = 2000;
        let truncated = missing_total > CAP;
        let use_rows: Vec<Vec<Option<String>>> = missing.iter().take(CAP).cloned().collect();
        if cancelled_now(&rid) {
            return Ok(json!({"ok":true,"pkCols":pk,"columns":Vec::<String>::new(),"rows":Vec::<Value>::new(),"missingTotal":missing_total,"truncated":truncated,"targetReadonly":tgt_ro,"allMissingPks":missing,"cancelled":true}));
        }
        if let Some(r) = &rid { clear_compare_cancel(r); }
        if use_rows.is_empty() {
            return Ok(json!({"ok":true,"pkCols":pk,"columns":Vec::<String>::new(),"rows":Vec::<Value>::new(),"missingTotal":0,"truncated":false,"targetReadonly":tgt_ro,"allMissingPks":Vec::<Value>::new(),"cancelled":false}));
        }
        let (full_cols, full_rows) = match get_rows_by_pk(&mut src_conn, &src_db, &table, &pk, &use_rows) {
            Ok(v) => v,
            Err(e) => {
                if cancelled_now(&rid) {
                    return Ok(json!({"ok":true,"pkCols":pk,"columns":Vec::<String>::new(),"rows":Vec::<Value>::new(),"missingTotal":missing_total,"truncated":truncated,"targetReadonly":tgt_ro,"allMissingPks":missing,"cancelled":true}));
                }
                return Ok(json!({"ok":false,"error":e}));
            }
        };
        // allMissingPks: the FULL (uncapped) list of missing primary-key values, sent to the
        // client alongside the first page - just id values, not full row data, so it's cheap
        // compared to what a full table re-scan would cost. The client uses it to load later
        // pages, or to remove just-inserted rows and pull the next batch, WITHOUT ever
        // re-scanning the table again.
        Ok(json!({"ok":true,"pkCols":pk,"columns":full_cols,"rows":full_rows,"missingTotal":missing_total,"truncated":truncated,"targetReadonly":tgt_ro,"allMissingPks":missing,"cancelled":false}))
    }).await.map_err(|e| e.to_string())?
}

// Inserts the (client-selected) missing rows into the target, batched, using the exact column
// list and values fetched from the source - so ids/keys match the source exactly. Always
// INSERT-only; never touches an existing target row.
// Finds rows present on BOTH sides (same primary key) whose CONTENT differs - detection only,
// never writes anything. Capped tighter (500) than the missing-rows check since this fetches
// full row data from BOTH source and target for every candidate, which is heavier. Mirrors the
// PowerShell version's Api-CompareRowsDiff exactly.
#[tauri::command]
async fn compare_rows_diff(req: Value) -> R {
    let src_name = req["sourceConnName"].as_str().unwrap_or("").to_string();
    let tgt_name = req["targetConnName"].as_str().unwrap_or("").to_string();
    let src_db = req["sourceDb"].as_str().unwrap_or("").to_string();
    let tgt_db = req["targetDb"].as_str().unwrap_or("").to_string();
    let table = req["table"].as_str().unwrap_or("").to_string();
    let rid = req["requestId"].as_str().map(String::from);
    tokio::task::spawn_blocking(move || {
        let cancelled_now = |rid: &Option<String>| rid.as_deref().map(is_compare_cancelled).unwrap_or(false);
        let (src_connj, _) = resolve_saved_conn(&src_name)?;
        let (tgt_connj, tgt_ro) = resolve_saved_conn(&tgt_name)?;
        let mut src_conn = build_conn(&src_connj)?;
        let mut tgt_conn = build_conn(&tgt_connj)?;
        // See compare_rows above: lets Cancel actually KILL QUERY whichever of these is blocked,
        // rather than only being noticed once the current SELECT finishes on its own.
        register_compare_conn(&rid, &mut src_conn, &src_connj);
        register_compare_conn(&rid, &mut tgt_conn, &tgt_connj);
        let _guard = CompareConnGuard(rid.clone());
        let pk = get_table_pk_cols(&mut src_conn, &src_db, &table)?;
        if pk.is_empty() { return Ok(json!({"ok":false,"error":"Table has no primary key - cannot compare rows."})); }
        let fk = get_table_fk_cols(&mut src_conn, &src_db, &table).unwrap_or_default();
        let pk_list = pk.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
        let (_c1, src_pk_rows) = match run_select(&mut src_conn, &format!("SELECT {} FROM {}.{}", pk_list, sql_id(&src_db), sql_id(&table))) {
            Ok(v) => v,
            Err(e) => {
                if cancelled_now(&rid) { return Ok(json!({"ok":true,"pkCols":pk,"fkCols":fk,"diffs":Vec::<Value>::new(),"commonTotal":0,"comparedCount":0,"truncated":false,"targetReadonly":tgt_ro,"cancelled":true})); }
                return Ok(json!({"ok":false,"error":e}));
            }
        };
        let tgt_pk_rows: Vec<Vec<Option<String>>> = match run_select(&mut tgt_conn, &format!("SELECT {} FROM {}.{}", pk_list, sql_id(&tgt_db), sql_id(&table))) {
            Ok((_c, rows)) => rows,
            Err(e) => {
                if e.contains("1146") || e.to_lowercase().contains("doesn't exist") { Vec::new() }
                else if cancelled_now(&rid) { return Ok(json!({"ok":true,"pkCols":pk,"fkCols":fk,"diffs":Vec::<Value>::new(),"commonTotal":0,"comparedCount":0,"truncated":false,"targetReadonly":tgt_ro,"cancelled":true})); }
                else { return Ok(json!({"ok":false,"error":e})); }
            }
        };
        let tgt_pk_set: std::collections::HashSet<String> = tgt_pk_rows.iter().map(|r| row_key(r)).collect();
        let common: Vec<&Vec<Option<String>>> = src_pk_rows.iter().filter(|r| tgt_pk_set.contains(&row_key(*r))).collect();
        let common_total = common.len();
        const CAP: usize = 500;
        let truncated = common_total > CAP;
        let use_common: Vec<&Vec<Option<String>>> = common.into_iter().take(CAP).collect();
        if use_common.is_empty() {
            return Ok(json!({"ok":true,"pkCols":pk,"fkCols":fk,"diffs":Vec::<Value>::new(),"commonTotal":common_total,"comparedCount":0,"truncated":false,"targetReadonly":tgt_ro}));
        }
        let fetch_chunk = 200;
        let mut full_cols: Option<Vec<String>> = None;
        let mut src_full: std::collections::HashMap<String, Vec<Option<String>>> = std::collections::HashMap::new();
        let mut tgt_full: std::collections::HashMap<String, Vec<Option<String>>> = std::collections::HashMap::new();
        let mut cancelled = false;
        for chunk in use_common.chunks(fetch_chunk) {
            if let Some(r) = &rid { if is_compare_cancelled(r) { cancelled = true; break; } }
            let where_clause = if pk.len() == 1 {
                let vals = chunk.iter().map(|r| sql_lit(&r[0].clone().unwrap_or_default())).collect::<Vec<_>>().join(",");
                format!("{} IN ({})", sql_id(&pk[0]), vals)
            } else {
                let tuples = chunk.iter().map(|r| {
                    let vs = r.iter().map(|v| sql_lit(&v.clone().unwrap_or_default())).collect::<Vec<_>>().join(",");
                    format!("({})", vs)
                }).collect::<Vec<_>>().join(",");
                format!("({}) IN ({})", pk_list, tuples)
            };
            let (sc, sr) = run_select(&mut src_conn, &format!("SELECT * FROM {}.{} WHERE {}", sql_id(&src_db), sql_id(&table), where_clause))?;
            if full_cols.is_none() { full_cols = Some(sc); }
            let cols_ref = full_cols.as_ref().unwrap();
            let pk_idx: Vec<usize> = pk.iter().map(|c| cols_ref.iter().position(|x| x == c).unwrap_or(0)).collect();
            for row in sr { let k = pk_idx.iter().map(|&i| row[i].clone().unwrap_or_default()).collect::<Vec<_>>().join("\u{1}"); src_full.insert(k, row); }
            let (_tc, tr) = run_select(&mut tgt_conn, &format!("SELECT * FROM {}.{} WHERE {}", sql_id(&tgt_db), sql_id(&table), where_clause))?;
            for row in tr { let k = pk_idx.iter().map(|&i| row[i].clone().unwrap_or_default()).collect::<Vec<_>>().join("\u{1}"); tgt_full.insert(k, row); }
        }
        let cols_final = full_cols.unwrap_or_default();
        let pk_idx_final: Vec<usize> = pk.iter().map(|c| cols_final.iter().position(|x| x == c).unwrap_or(0)).collect();
        let mut diffs: Vec<Value> = Vec::new();
        for (k, s_row) in src_full.iter() {
            let t_row = match tgt_full.get(k) { Some(r) => r, None => continue };
            let mut col_diffs: Vec<Value> = Vec::new();
            for ci in 0..cols_final.len() {
                if s_row[ci] != t_row[ci] {
                    col_diffs.push(json!({"col": cols_final[ci], "src": s_row[ci], "tgt": t_row[ci]}));
                }
            }
            if !col_diffs.is_empty() {
                let pk_vals: Vec<Option<String>> = pk_idx_final.iter().map(|&i| s_row[i].clone()).collect();
                diffs.push(json!({"pk": pk_vals, "colDiffs": col_diffs}));
            }
        }
        if let Some(r) = &rid { clear_compare_cancel(r); }
        Ok(json!({"ok":true,"pkCols":pk,"fkCols":fk,"diffs":diffs,"commonTotal":common_total,"comparedCount":use_common.len(),"truncated":truncated,"targetReadonly":tgt_ro,"cancelled":cancelled}))
    }).await.map_err(|e| e.to_string())?
}

// Applies the (client-selected) content updates: one UPDATE per row, using the SOURCE value for
// each column flagged as different, matched by primary key. This OVERWRITES existing target
// data for those rows - the only write path in Compare that does so - and is always
// client-confirmed with an explicit warning before this is ever called.
#[tauri::command]
async fn compare_rows_apply_diff(req: Value) -> R {
    let tgt_name = req["targetConnName"].as_str().unwrap_or("").to_string();
    let tgt_db = req["targetDb"].as_str().unwrap_or("").to_string();
    let table = req["table"].as_str().unwrap_or("").to_string();
    let pk_cols: Vec<String> = req["pkCols"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    let updates: Vec<Value> = req["updates"].as_array().cloned().unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let (connj, readonly) = resolve_saved_conn(&tgt_name)?;
        if readonly { return Ok(json!({"ok":false,"error":"Target connection is read-only / safe mode - blocked."})); }
        if pk_cols.is_empty() || updates.is_empty() { return Ok(json!({"ok":false,"error":"No rows to update."})); }
        let mut c = build_conn(&connj)?;
        let obj = format!("{}.{}", sql_id(&tgt_db), sql_id(&table));
        // Unlike compare_rows_apply/compare_rows_insert_all (INSERT-only, each batch already
        // atomic as a single multi-row statement, and a chunk failing partway through a large
        // bulk insert shouldn't block the rest), this updates EXISTING target rows one at a time -
        // the exact "apply this reviewed set of corrections" shape the grid's own staged-edits
        // apply already wraps in a transaction for. A batch failing partway through here left
        // some rows changed and others not, with no way back - same failure mode, same fix.
        c.query_drop("START TRANSACTION").map_err(|e| e.to_string())?;
        let mut log = Vec::new();
        let mut failed = false;
        for u in &updates {
            let col_diffs = u["colDiffs"].as_array().cloned().unwrap_or_default();
            let pk_vals = u["pk"].as_array().cloned().unwrap_or_default();
            if col_diffs.is_empty() || pk_vals.len() != pk_cols.len() {
                log.push("SKIPPED (no columns/key)".to_string());
                continue;
            }
            let sets = col_diffs.iter().map(|cd| {
                let col = cd["col"].as_str().unwrap_or("");
                let val = match &cd["src"] {
                    Value::Null => "NULL".to_string(),
                    Value::String(s) => sql_val_lit(s),
                    other => sql_lit(&other.to_string().trim_matches('"').to_string()),
                };
                format!("{}={}", sql_id(col), val)
            }).collect::<Vec<_>>().join(",");
            let wheres = pk_cols.iter().zip(pk_vals.iter()).map(|(col, v)| {
                let val = match v {
                    Value::Null => "NULL".to_string(),
                    Value::String(s) => sql_val_lit(s),
                    other => sql_lit(&other.to_string().trim_matches('"').to_string()),
                };
                format!("{}={}", sql_id(col), val)
            }).collect::<Vec<_>>().join(" AND ");
            let pk_desc = pk_vals.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",");
            let sql = format!("UPDATE {} SET {} WHERE {} LIMIT 1", obj, sets, wheres);
            match c.query_drop(&sql) {
                Ok(_) => log.push(format!("OK  updated id={}", pk_desc)),
                Err(e) => { log.push(format!("FAILED id={} : {}", pk_desc, e)); failed = true; break; }
            }
        }
        if failed {
            let _ = c.query_drop("ROLLBACK");
            log.push("No rows were updated - the batch was rolled back.".to_string());
            return Ok(json!({"ok":false,"log":log}));
        }
        if let Err(e) = c.query_drop("COMMIT") {
            let _ = c.query_drop("ROLLBACK");
            log.push(format!("Could not commit: {} - no rows were updated.", e));
            return Ok(json!({"ok":false,"log":log}));
        }
        Ok(json!({"ok":true,"log":log}))
    }).await.map_err(|e| e.to_string())?
}

// Inserts EVERY missing row (source rows absent from target), not just the first 2000 that fit
// in the interactive review list. Unlike compare_rows, this never sends the row data back to
// the frontend at all - it fetches a chunk of missing rows from source and inserts that SAME
// chunk into target immediately, chunk by chunk, so the amount of data moved isn't limited by
// what's practical to render as a checkbox list. Still insert-only.
//
// Deliberately not wrapped in one big transaction across every chunk: each chunk's INSERT is
// already one SQL statement, which MySQL/InnoDB itself only ever applies all-or-nothing - a
// chunk failing partway through can't leave that chunk half-inserted. What continue-on-error
// buys here is that ONE bad chunk (say, a duplicate key from a row someone else inserted since
// the scan started) doesn't roll back or abort inserting the rest of what could be tens of
// thousands of otherwise-good rows - unlike compare_rows_apply_diff below, this never touches an
// existing row, so a chunk that fails simply leaves those rows still missing, not corrupted.
#[tauri::command]
async fn compare_rows_insert_all(req: Value) -> R {
    let src_name = req["sourceConnName"].as_str().unwrap_or("").to_string();
    let tgt_name = req["targetConnName"].as_str().unwrap_or("").to_string();
    let src_db = req["sourceDb"].as_str().unwrap_or("").to_string();
    let tgt_db = req["targetDb"].as_str().unwrap_or("").to_string();
    let table = req["table"].as_str().unwrap_or("").to_string();
    let rid = req["requestId"].as_str().map(String::from);
    tokio::task::spawn_blocking(move || {
        let (src_connj, _) = resolve_saved_conn(&src_name)?;
        let (tgt_connj, tgt_ro) = resolve_saved_conn(&tgt_name)?;
        if tgt_ro { return Ok(json!({"ok":false,"error":"Target connection is read-only / safe mode - blocked."})); }
        let mut src_conn = build_conn(&src_connj)?;
        let mut tgt_conn = build_conn(&tgt_connj)?;
        let pk = get_table_pk_cols(&mut src_conn, &src_db, &table)?;
        if pk.is_empty() { return Ok(json!({"ok":false,"error":"Table has no primary key - cannot compare rows."})); }
        let pk_list = pk.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
        let (_c1, src_pk_rows) = run_select(&mut src_conn, &format!("SELECT {} FROM {}.{}", pk_list, sql_id(&src_db), sql_id(&table)))?;
        let tgt_pk_rows: Vec<Vec<Option<String>>> = match run_select(&mut tgt_conn, &format!("SELECT {} FROM {}.{}", pk_list, sql_id(&tgt_db), sql_id(&table))) {
            Ok((_c, rows)) => rows,
            Err(e) => { if e.contains("1146") || e.to_lowercase().contains("doesn't exist") { Vec::new() } else { return Ok(json!({"ok":false,"error":e})); } }
        };
        let tgt_set: std::collections::HashSet<String> = tgt_pk_rows.iter().map(|r| row_key(r)).collect();
        let missing: Vec<Vec<Option<String>>> = src_pk_rows.into_iter().filter(|r| !tgt_set.contains(&row_key(r))).collect();
        let missing_total = missing.len();
        if missing_total == 0 {
            return Ok(json!({"ok":true,"missingTotal":0,"inserted":0,"cancelled":false,"log":Vec::<String>::new()}));
        }
        let chunk_size = 200;
        let mut log: Vec<String> = Vec::new();
        let mut inserted: usize = 0;
        let mut cancelled = false;
        for (ci, chunk) in missing.chunks(chunk_size).enumerate() {
            if let Some(r) = &rid { if is_compare_cancelled(r) { cancelled = true; break; } }
            let where_clause = if pk.len() == 1 {
                let vals = chunk.iter().map(|r| sql_val_lit(&r[0].clone().unwrap_or_default())).collect::<Vec<_>>().join(",");
                format!("{} IN ({})", sql_id(&pk[0]), vals)
            } else {
                let tuples = chunk.iter().map(|r| {
                    let vs = r.iter().map(|v| sql_val_lit(&v.clone().unwrap_or_default())).collect::<Vec<_>>().join(",");
                    format!("({})", vs)
                }).collect::<Vec<_>>().join(",");
                format!("({}) IN ({})", pk_list, tuples)
            };
            let (full_cols, full_rows) = match run_select(&mut src_conn, &format!("SELECT * FROM {}.{} WHERE {}", sql_id(&src_db), sql_id(&table), where_clause)) {
                Ok(v) => v,
                Err(e) => { log.push(format!("FAILED (fetch) chunk {} : {}", ci + 1, e)); continue; }
            };
            if full_rows.is_empty() { continue; }
            let col_list = full_cols.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
            let obj = format!("{}.{}", sql_id(&tgt_db), sql_id(&table));
            let values_sql = full_rows.iter().map(|row| {
                let vs = row.iter().map(|v| match v { None => "NULL".to_string(), Some(s) => sql_val_lit(s) }).collect::<Vec<_>>().join(",");
                format!("({})", vs)
            }).collect::<Vec<_>>().join(",");
            let sql = format!("INSERT INTO {} ({}) VALUES {}", obj, col_list, values_sql);
            match tgt_conn.query_drop(&sql) {
                Ok(_) => { inserted += full_rows.len(); log.push(format!("OK  inserted {} row(s) ({} of {} so far)", full_rows.len(), inserted, missing_total)); }
                Err(e) => log.push(format!("FAILED (insert) chunk {} : {}", ci + 1, e)),
            }
        }
        if let Some(r) = &rid { clear_compare_cancel(r); }
        Ok(json!({"ok":true,"missingTotal":missing_total,"inserted":inserted,"cancelled":cancelled,"log":log}))
    }).await.map_err(|e| e.to_string())?
}

// Same reasoning as compare_rows_insert_all above: insert-only, each batch already atomic as one
// SQL statement, continue-on-error across batches so one bad batch doesn't block the rest of a
// large reviewed set from landing.
#[tauri::command]
async fn compare_rows_apply(req: Value) -> R {
    let tgt_name = req["targetConnName"].as_str().unwrap_or("").to_string();
    let tgt_db = req["targetDb"].as_str().unwrap_or("").to_string();
    let table = req["table"].as_str().unwrap_or("").to_string();
    let columns: Vec<String> = req["columns"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    let rows: Vec<Vec<Value>> = req["rows"].as_array().map(|a| a.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect()).unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let (connj, readonly) = resolve_saved_conn(&tgt_name)?;
        if readonly { return Ok(json!({"ok":false,"error":"Target connection is read-only / safe mode - blocked."})); }
        if columns.is_empty() || rows.is_empty() { return Ok(json!({"ok":false,"error":"No rows to insert."})); }
        let mut c = build_conn(&connj)?;
        let col_list = columns.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
        let obj = format!("{}.{}", sql_id(&tgt_db), sql_id(&table));
        let mut log = Vec::new();
        const BATCH: usize = 500;
        for (bi, chunk) in rows.chunks(BATCH).enumerate() {
            let values_sql = chunk.iter().map(|row| {
                let vs = row.iter().map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::String(s) => sql_val_lit(s),
                    other => sql_lit(&other.to_string().trim_matches('"').to_string()),
                }).collect::<Vec<_>>().join(",");
                format!("({})", vs)
            }).collect::<Vec<_>>().join(",");
            let sql = format!("INSERT INTO {} ({}) VALUES {}", obj, col_list, values_sql);
            match c.query_drop(&sql) {
                Ok(_) => log.push(format!("OK  inserted {} row(s) (batch {})", chunk.len(), bi + 1)),
                Err(e) => log.push(format!("FAILED batch {} : {}", bi + 1, e)),
            }
        }
        Ok(json!({"ok":true,"log":log}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn compare_dbs(req: Value) -> R {
    let name = req["connName"].as_str().unwrap_or("").to_string();
    tokio::task::spawn_blocking(move || {
        let (connj, readonly) = resolve_saved_conn(&name)?;
        let mut c = build_conn(&connj)?;
        let (_cols, rows) = run_select(&mut c, "SHOW DATABASES")?;
        let dbs: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        Ok(json!({"ok":true,"databases":dbs,"readonly":readonly}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn compare_tables(req: Value) -> R {
    let src_name = req["sourceConnName"].as_str().unwrap_or("").to_string();
    let tgt_name = req["targetConnName"].as_str().unwrap_or("").to_string();
    let src_db = req["sourceDb"].as_str().unwrap_or("").to_string();
    let tgt_db = req["targetDb"].as_str().unwrap_or("").to_string();
    tokio::task::spawn_blocking(move || {
        let (src_connj, _) = resolve_saved_conn(&src_name)?;
        let (tgt_connj, _) = resolve_saved_conn(&tgt_name)?;
        let mut src_conn = build_conn(&src_connj)?;
        let mut tgt_conn = build_conn(&tgt_connj)?;
        let sql1 = format!("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA={} ORDER BY TABLE_NAME", sql_lit(&src_db));
        let sql2 = format!("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA={} ORDER BY TABLE_NAME", sql_lit(&tgt_db));
        let (_c1, r1) = run_select(&mut src_conn, &sql1)?;
        let (_c2, r2) = run_select(&mut tgt_conn, &sql2)?;
        let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for r in r1 { if let Some(Some(n)) = r.into_iter().next() { set.insert(n); } }
        for r in r2 { if let Some(Some(n)) = r.into_iter().next() { set.insert(n); } }
        let names: Vec<String> = set.into_iter().collect();
        Ok(json!({"ok":true,"tables":names}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn compare_schemas(req: Value) -> R {
    let src_name = req["sourceConnName"].as_str().unwrap_or("").to_string();
    let tgt_name = req["targetConnName"].as_str().unwrap_or("").to_string();
    let src_db = req["sourceDb"].as_str().unwrap_or("").to_string();
    let tgt_db = req["targetDb"].as_str().unwrap_or("").to_string();
    if src_db.is_empty() || tgt_db.is_empty() { return Ok(json!({"ok":false,"error":"Pick a database on both sides."})); }
    tokio::task::spawn_blocking(move || {
        let (src_connj, _sro) = resolve_saved_conn(&src_name)?;
        let (tgt_connj, tgt_ro) = resolve_saved_conn(&tgt_name)?;
        let mut src_conn = build_conn(&src_connj)?;
        let mut tgt_conn = build_conn(&tgt_connj)?;
        let mut src_cols = get_schema_columns(&mut src_conn, &src_db)?;
        let mut tgt_cols = get_schema_columns(&mut tgt_conn, &tgt_db)?;
        if let Some(arr) = req.get("tables").and_then(|v| v.as_array()) {
            let keep: std::collections::HashSet<String> = arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
            src_cols.retain(|k, _| keep.contains(k));
            tgt_cols.retain(|k, _| keep.contains(k));
        }
        let rid = req["requestId"].as_str().map(String::from);
        let (diffs, cancelled) = compare_table_sets(&mut src_conn, &src_db, &src_cols, &tgt_cols, rid.as_deref());
        if let Some(r) = &rid { clear_compare_cancel(r); }
        let tables: Vec<Value> = diffs.into_iter().map(|t| json!({
            "name": t.name, "status": t.status,
            "sql": t.sql.into_iter().map(|s| json!({"stmt": s.stmt, "checked": s.checked, "kind": s.kind})).collect::<Vec<_>>()
        })).collect();
        Ok(json!({"ok":true,"tables":tables,"targetReadonly":tgt_ro,"cancelled":cancelled}))
    }).await.map_err(|e| e.to_string())?
}

// Deliberately NOT wrapped in a transaction: these are schema-diff statements (ALTER/CREATE/DROP
// TABLE), and every one of them is an implicit-commit statement in MySQL/MariaDB - a
// START TRANSACTION here would be silently ignored the moment the first DDL statement ran, giving
// false confidence that a failure partway through could be rolled back when it can't be. Running
// each independently and reporting OK/FAILED per line (as already done below) is the honest
// behavior given that constraint - a half-migrated schema is visible in the log, not hidden by a
// rollback that was never actually possible.
#[tauri::command]
async fn compare_apply(req: Value) -> R {
    let tgt_name = req["targetConnName"].as_str().unwrap_or("").to_string();
    let tgt_db = req["targetDb"].as_str().unwrap_or("").to_string();
    let statements: Vec<String> = req["statements"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let (connj, readonly) = resolve_saved_conn(&tgt_name)?;
        if readonly { return Ok(json!({"ok":false,"error":"Target connection is read-only / safe mode - blocked."})); }
        let mut c = build_conn(&connj)?;
        if !tgt_db.is_empty() { let _ = c.query_drop(format!("USE {}", sql_id(&tgt_db))); }
        let mut log = Vec::new();
        for stmt in &statements {
            match c.query_drop(stmt) {
                Ok(_) => log.push(format!("OK  {}", stmt)),
                Err(e) => log.push(format!("FAILED  {}  :  {}", stmt, e)),
            }
        }
        Ok(json!({"ok":true,"log":log}))
    }).await.map_err(|e| e.to_string())?
}

// Server-side folder/file browser backing the in-app mBrowse modal (matches the PowerShell
// version's Api-Browse: "path":"ROOT" lists available drives on Windows / "/" on Unix;
// otherwise it lists the subdirectories ("dirs") and, unless dirsOnly, matching files ("files")
// of the given directory, plus "parent" so the modal's Up button can navigate back out.
// Saved-query library (favorites), mirrors the PowerShell version's Load-Lib/Save-Lib -
// a flat JSON array of {name, sql, schema, ts} stored alongside connections.json.
fn lib_path() -> std::path::PathBuf {
    let mut p = dirs::config_dir().unwrap_or(std::env::temp_dir());
    p.push("NOBSSQL-Desktop"); std::fs::create_dir_all(&p).ok(); p.push("library.json"); p
}
fn load_lib() -> Vec<Value> {
    std::fs::read_to_string(lib_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_lib(v: &Vec<Value>) { let _ = std::fs::write(lib_path(), serde_json::to_string_pretty(v).unwrap_or_default()); }
fn now_ms() -> i64 { chrono::Utc::now().timestamp_millis() }

#[tauri::command]
async fn lib_list(_req: Value) -> R {
    Ok(json!({"ok":true,"items":load_lib()}))
}
#[tauri::command]
async fn lib_save(req: Value) -> R {
    let name = req["name"].as_str().unwrap_or("").to_string();
    if name.is_empty() { return Ok(json!({"ok":false,"error":"name required"})); }
    let ts = req["ts"].as_i64().unwrap_or_else(now_ms);
    let mut list: Vec<Value> = load_lib().into_iter().filter(|x| x["name"].as_str() != Some(name.as_str())).collect();
    list.insert(0, json!({"name":name,"sql":req["sql"].as_str().unwrap_or(""),"schema":req["schema"].as_str().unwrap_or(""),"ts":ts}));
    save_lib(&list);
    Ok(json!({"ok":true}))
}
#[tauri::command]
async fn lib_delete(req: Value) -> R {
    let name = req["name"].as_str().unwrap_or("").to_string();
    let list: Vec<Value> = load_lib().into_iter().filter(|x| x["name"].as_str() != Some(name.as_str())).collect();
    save_lib(&list);
    Ok(json!({"ok":true}))
}
#[tauri::command]
async fn lib_clear(_req: Value) -> R {
    save_lib(&Vec::new());
    Ok(json!({"ok":true}))
}
#[tauri::command]
async fn lib_replace(req: Value) -> R {
    let mut list = Vec::new();
    if let Some(items) = req["items"].as_array() {
        for x in items {
            if x["name"].as_str().map(|s| !s.is_empty()).unwrap_or(false) {
                let ts = x["ts"].as_i64().unwrap_or_else(now_ms);
                list.push(json!({"name":x["name"],"sql":x["sql"].as_str().unwrap_or(""),"schema":x["schema"].as_str().unwrap_or(""),"ts":ts}));
            }
        }
    }
    save_lib(&list);
    Ok(json!({"ok":true}))
}

#[tauri::command]
async fn search_all_schemas(req: Value) -> R {
    let term = req["term"].as_str().unwrap_or("").trim().to_string();
    if term.is_empty() { return Ok(json!({"ok":false,"error":"Empty search term."})); }
    tokio::task::spawn_blocking(move || {
        let mut c = build_conn(&req["conn"])?;
        let like = sql_lit(&format!("%{}%", term));
        let sql = format!(
            "SELECT TABLE_SCHEMA,'table',TABLE_NAME FROM information_schema.TABLES WHERE TABLE_TYPE='BASE TABLE' AND TABLE_NAME LIKE {t} \
             UNION ALL SELECT TABLE_SCHEMA,'view',TABLE_NAME FROM information_schema.TABLES WHERE TABLE_TYPE='VIEW' AND TABLE_NAME LIKE {t} \
             UNION ALL SELECT ROUTINE_SCHEMA,IF(ROUTINE_TYPE='PROCEDURE','procedure','function'),ROUTINE_NAME FROM information_schema.ROUTINES WHERE ROUTINE_NAME LIKE {t} \
             UNION ALL SELECT TRIGGER_SCHEMA,'trigger',TRIGGER_NAME FROM information_schema.TRIGGERS WHERE TRIGGER_NAME LIKE {t} \
             UNION ALL SELECT EVENT_SCHEMA,'event',EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_NAME LIKE {t} \
             ORDER BY 1,2,3", t = like);
        match run_select(&mut c, &sql) {
            Ok((_cols, rows)) => {
                let items: Vec<Value> = rows.iter().map(|r| json!({
                    "schema": r.get(0).cloned().flatten(),
                    "type": r.get(1).cloned().flatten(),
                    "name": r.get(2).cloned().flatten()
                })).collect();
                Ok(json!({"ok":true,"items":items}))
            }
            Err(e) => Ok(json!({"ok":false,"error":e})),
        }
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn browse(req: Value) -> R {
    let raw_path = req["path"].as_str().unwrap_or("ROOT").to_string();
    let filter = req["filter"].as_str().unwrap_or("*").to_string(); // e.g. "*.sql"
    let dirs_only = req["dirsOnly"].as_bool().unwrap_or(false);
    let ext = filter.trim_start_matches("*.").to_lowercase();

    if raw_path.is_empty() || raw_path == "ROOT" {
        // List drives on Windows (C:\, D:\, ...); a single root on Unix.
        let mut dirs = Vec::new();
        #[cfg(target_os = "windows")]
        {
            for letter in b'A'..=b'Z' {
                let drive = format!("{}:\\", letter as char);
                if std::path::Path::new(&drive).exists() {
                    dirs.push(json!({"name": drive.clone(), "path": drive}));
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            dirs.push(json!({"name": "/", "path": "/"}));
        }
        return Ok(json!({"ok":true,"path":"","parent":Value::Null,"dirs":dirs,"files":Vec::<Value>::new()}));
    }

    let dir = std::path::Path::new(&raw_path);
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            let mut dirs = Vec::new();
            let mut files = Vec::new();
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if p.is_dir() {
                    dirs.push(json!({"name": name, "path": p.to_string_lossy()}));
                } else if !dirs_only {
                    if filter == "*" || filter.is_empty() || name.to_lowercase().ends_with(&format!(".{}", ext)) {
                        files.push(json!({"path": p.to_string_lossy(), "name": name, "dir": false}));
                    }
                }
            }
            dirs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            // "parent": the containing directory, or "ROOT" if we're already at a drive/filesystem root.
            let parent = match dir.parent() {
                Some(pp) if pp.as_os_str().len() > 0 && pp != dir => json!(pp.to_string_lossy()),
                _ => json!("ROOT"),
            };
            Ok(json!({"ok":true,"path":dir.to_string_lossy(),"parent":parent,"dirs":dirs,"files":files}))
        }
        Err(e) => Ok(json!({"ok":false,"error":format!("Cannot read folder: {}", e)})),
    }
}

#[tauri::command]
fn cancel_export() { EXPORT_CANCEL.store(true, Ordering::SeqCst); }

// Cancels one Export or Import run by the jobId the UI generated for it. Sets the flag the run's
// loops check before starting the next child, and kills the child running right now so a large
// single dump stops immediately instead of finishing minutes later. Messages match the
// PowerShell backend's Api-CancelJob, including the case where the job already completed.
#[tauri::command]
fn cancel_job(req: Value) -> R {
    let id = req["jobId"].as_str().unwrap_or("").to_string();
    if id.is_empty() { return Ok(json!({"ok":false,"error":"no jobId"})); }
    let job = jobs().lock().ok().and_then(|m| m.get(&id).cloned());
    match job {
        Some(j) => {
            j.cancelled.store(true, Ordering::SeqCst);
            if let Ok(mut slot) = j.child.lock() {
                if let Some(c) = slot.as_mut() { kill_child(c); }
            }
            Ok(json!({"ok":true,"message":"Cancel requested."}))
        }
        None => Ok(json!({"ok":false,"error":"Job not found - it may have already finished."})),
    }
}

#[tauri::command]
async fn export_table(app: tauri::AppHandle, req: Value) -> R {
    export_table_run(Some(app), req).await
}

// The body, split out so a test can drive it without a tauri::AppHandle. The handle is only used
// to emit progress events, which simply do not fire when there is nobody to receive them.
async fn export_table_run(app: Option<tauri::AppHandle>, req: Value) -> R {
    tokio::task::spawn_blocking(move || -> R {
        use std::io::Write as _;
        let db = req["db"].as_str().unwrap_or("").to_string();
        let table = req["table"].as_str().unwrap_or("").to_string();
        let file = req["file"].as_str().unwrap_or("").to_string();
        let fmt = req["format"].as_str().unwrap_or("csv").to_string();
        if db.is_empty() || table.is_empty() { return Ok(json!({"ok":false,"error":"no table"})); }
        if file.is_empty() { return Ok(json!({"ok":false,"error":"no output file"})); }
        EXPORT_CANCEL.store(false, Ordering::SeqCst);
        let mut c = build_conn(&req["conn"])?;
        let f = std::fs::File::create(&file).map_err(|e| e.to_string())?;
        let mut w = std::io::BufWriter::new(f);
        let tbl = format!("{}.{}", sql_id(&db), sql_id(&table));
        let mut result = c.query_iter(format!("SELECT * FROM {}", tbl)).map_err(|e| e.to_string())?;
        let (cols, bin): (Vec<String>, Vec<bool>) = {
            let cs = result.columns();
            let sl: &[Column] = cs.as_ref();
            (sl.iter().map(|c| c.name_str().to_string()).collect(),
             sl.iter().map(|c| is_binaryish(c)).collect())
        };
        let collist = cols.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
        // A NULL and an empty string both used to come out as an empty field, so the two were
        // indistinguishable in the file - and the CSV importer turns an empty cell into NULL, so
        // an empty string did not survive a round trip. Write NULL as an explicit marker
        // instead, defaulting to \N, which is what LOAD DATA reads and what HeidiSQL defaults
        // to. The marker is never quoted: the server only recognises \N unenclosed.
        let null_marker = req["nullValue"].as_str().unwrap_or("\\N").to_string();
        // A bare \r (no following \n) has to be quoted too, not just \n - both this app's own
        // CSV importer and a spreadsheet's CSV rules treat a lone \r as ending the row, so an
        // unquoted one silently splits one logical row into two and shifts every column after it.
        let csv_field = |o: &Option<String>| -> String {
            match o { None => null_marker.clone(), Some(s) => {
                if s.contains('"') || s.contains(',') || s.contains('\n') || s.contains('\r') { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.clone() }
            }}
        };
        if fmt != "inserts" {
            let hdr = cols.iter().map(|c| if c.contains(',')||c.contains('"')||c.contains('\n')||c.contains('\r'){format!("\"{}\"",c.replace('"',"\"\""))}else{c.clone()}).collect::<Vec<_>>().join(",");
            w.write_all(hdr.as_bytes()).map_err(|e| e.to_string())?; w.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        let mut n: usize = 0;
        let mut batch: Vec<String> = Vec::new();
        let mut cancelled = false;
        for rr in result.by_ref() {
            if EXPORT_CANCEL.load(Ordering::SeqCst) { cancelled = true; break; }
            let row = rr.map_err(|e| e.to_string())?;
            let cells: Vec<Option<String>> = (0..cols.len()).map(|i| val_to_opt(row.as_ref(i).unwrap_or(&MyValue::NULL), bin[i])).collect();
            if fmt == "inserts" {
                let vals = cells.iter().map(|o| match o { None => "NULL".to_string(), Some(s) => sql_lit(s) }).collect::<Vec<_>>().join(",");
                batch.push(format!("({})", vals));
                if batch.len() >= 1000 {
                    w.write_all(format!("INSERT IGNORE INTO {} ({}) VALUES {};\n", tbl, collist, batch.join(",")).as_bytes()).map_err(|e| e.to_string())?;
                    batch.clear();
                }
            } else {
                let line = cells.iter().map(csv_field).collect::<Vec<_>>().join(",");
                w.write_all(line.as_bytes()).map_err(|e| e.to_string())?; w.write_all(b"\n").map_err(|e| e.to_string())?;
            }
            n += 1;
            if n % 2000 == 0 { if let Some(a) = &app { let _ = a.emit("export_progress", json!({"rows": n})); } }
        }
        if fmt == "inserts" && !batch.is_empty() && !cancelled {
            w.write_all(format!("INSERT IGNORE INTO {} ({}) VALUES {};\n", tbl, collist, batch.join(",")).as_bytes()).map_err(|e| e.to_string())?;
        }
        w.flush().map_err(|e| e.to_string())?;
        drop(w);
        if cancelled {
            let _ = std::fs::remove_file(&file);
            log_line(&format!("EXPORT cancelled: {} ({} rows written before cancel)", tbl, n));
            return Ok(json!({"ok":false,"error":"Export cancelled.","cancelled":true}));
        }
        if let Some(a) = &app { let _ = a.emit("export_progress", json!({"rows": n, "done": true})); }
        log_line(&format!("EXPORT ok: {} rows from {} to {}", n, tbl, file));
        Ok(json!({"ok":true,"message":format!("Exported {} row(s) to {}", n, file)}))
    }).await.map_err(|e| e.to_string())?
}

// tools_status's checks are expensive enough to notice (a directory tree walk under Program
// Files, and - see below - a real process spawn for mysqldump's --version) that recomputing them
// on every single Settings-dialog open was the actual source of the multi-second delay, not any
// one check in isolation. The paths/flavor can't change out from under the app on their own, so
// this caches the whole result for the life of the process and only recomputes when something
// that could actually invalidate it happens: save_config (the user picked a new path) or a
// successful download_tools (new binaries appeared on disk) both clear it.
fn tools_status_cache() -> &'static Mutex<Option<Value>> {
    static CACHE: OnceLock<Mutex<Option<Value>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

#[tauri::command]
fn tools_status(app: tauri::AppHandle) -> R {
    if let Some(cached) = tools_status_cache().lock().unwrap().clone() { return Ok(cached); }
    fn describe(app: &tauri::AppHandle, base: &str, names: &[&str], env_key: &str) -> (String, String) {
        let cfg = load_cfg();
        if let Some(p) = cfg.get(&format!("{}_bin", base)).and_then(|v| v.as_str()) {
            if !p.is_empty() && std::path::Path::new(p).exists() { return (p.to_string(), "configured / downloaded".into()); }
        }
        if let Ok(p) = std::env::var(env_key) { if !p.is_empty() && std::path::Path::new(&p).exists() { return (p, format!("env {}", env_key)); } }
        match resolve_tool(app, base, names, env_key) {
            Ok(p) => {
                let src = if p.contains('/') || p.contains('\\') { "found on system".to_string() } else { "found on PATH".to_string() };
                (p, src)
            }
            Err(_) => ("(not found)".into(), "missing".into()),
        }
    }
    let (m, ms) = describe(&app, "mysql", &["mysql", "mariadb"], "MYSQL_BIN");
    let (d, ds) = describe(&app, "mysqldump", &["mysqldump", "mariadb-dump"], "MYSQLDUMP_BIN");
    // A handful of export options only exist on one dump-tool flavor: --set-gtid-purged is
    // MySQL 5.6+ only, --column-statistics is MySQL 8+ only - MariaDB's mysqldump has neither,
    // and checking either against it aborts the whole export with "unknown variable". Running
    // `--version` once here lets the export dialog grey those options out up front instead of
    // letting the user discover it mid-export. This is the one check here that genuinely needs to
    // launch the binary - the version STRING is the only way to tell the two flavors apart -
    // unlike resolve_bin's PATH fallback below, which only needs to know the binary exists.
    let dump_is_mariadb = if d != "(not found)" {
        Command::new(&d).arg("--version").output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("mariadb"))
    } else { None };
    let result = json!({
        "ok": true,
        "mysql": m, "mysql_source": ms,
        "mysqldump": d, "mysqldump_source": ds,
        "mysqldump_is_mariadb": dump_is_mariadb,
        "download_dir": tools_dir().to_string_lossy(),
        "config_file": config_file().to_string_lossy()
    });
    *tools_status_cache().lock().unwrap() = Some(result.clone());
    Ok(result)
}

#[tauri::command]
fn get_config(_app: tauri::AppHandle) -> R {
    Ok(json!({"ok":true, "config": load_cfg(), "mariadbDownloadUrlDefault": DEFAULT_MARIADB_DOWNLOAD_TEMPLATE}))
}

#[tauri::command]
fn save_config(req: Value) -> R {
    let mut cfg = load_cfg();
    if let Some(m) = req["config"].as_object() {
        for (k, v) in m { cfg[k] = v.clone(); }
    }
    match std::fs::write(config_file(), serde_json::to_string_pretty(&cfg).unwrap_or_default()) {
        Ok(_) => {
            // The saved config may have changed mysql_bin/mysqldump_bin - drop the cached
            // tools_status result so the next check reflects the new path instead of a stale one.
            *tools_status_cache().lock().unwrap() = None;
            Ok(json!({"ok":true}))
        }
        Err(e) => Ok(json!({"ok":false,"error":e.to_string()})),
    }
}

// Default MariaDB client-tools download URL template - user-editable in Settings (stored under
// "mariadb_download_url_template" in the same config file as the mysql_bin/mysqldump_bin
// paths). {version} and {file_name} are substituted from the latest LTS release the MariaDB
// API itself reports. A direct mirror is used by default rather than the API's own
// file_download_url field, which has been observed returning an error page (403) instead of
// the actual archive.
const DEFAULT_MARIADB_DOWNLOAD_TEMPLATE: &str = "https://mirror.mariadb.org/mariadb-{version}/winx64-packages/{file_name}";

#[tauri::command]
async fn download_tools(_app: tauri::AppHandle) -> R {
    tokio::task::spawn_blocking(|| -> R {
        let client = reqwest::blocking::Client::builder()
            .user_agent("NOBSSQL-Desktop")
            .timeout(std::time::Duration::from_secs(600))
            .build().map_err(|e| e.to_string())?;
        // 1) latest LTS stable branch
        let root: Value = client.get("https://downloads.mariadb.org/rest-api/mariadb/")
            .send().map_err(|e| e.to_string())?.json().map_err(|e| e.to_string())?;
        let mut branches: Vec<String> = root["major_releases"].as_array().cloned().unwrap_or_default().iter()
            .filter(|r| r["release_status"].as_str() == Some("Stable")
                     && r["release_support_type"].as_str() == Some("Long Term Support"))
            .filter_map(|r| r["release_id"].as_str().map(String::from)).collect();
        branches.sort_by(|a, b| ver_key(b).cmp(&ver_key(a)));
        let branch = branches.first().cloned().ok_or("No stable LTS branch found.")?;
        // 2) latest patch version in that branch
        let binfo: Value = client.get(format!("https://downloads.mariadb.org/rest-api/mariadb/{}/", branch))
            .send().map_err(|e| e.to_string())?.json().map_err(|e| e.to_string())?;
        let rel = binfo["releases"].as_object().ok_or("No releases in branch.")?;
        let mut patches: Vec<String> = rel.keys().cloned().collect();
        patches.sort_by(|a, b| ver_key(b).cmp(&ver_key(a)));
        let patch = patches.first().cloned().ok_or("No patch version found.")?;
        // 3) winx64 zip (non-debug)
        let files = binfo["releases"][&patch]["files"].as_array().cloned().unwrap_or_default();
        let zip_entry = files.iter().find(|f| {
            let n = f["file_name"].as_str().unwrap_or("");
            n.contains("winx64") && n.ends_with(".zip") && !n.contains("debug")
        }).ok_or("No winx64 zip found in the MariaDB release.")?;
        let file_name = zip_entry["file_name"].as_str().unwrap_or("").to_string();
        let api_url = zip_entry["file_download_url"].as_str().ok_or("No download URL.")?.to_string();
        // 4) download - the API's own file_download_url has been observed returning an error
        // page (403) instead of the actual archive, which is exactly what produces "Could not
        // find EOCD": the downloaded bytes simply aren't a valid zip at all. A direct mirror URL
        // is more reliable, so it's tried first here, falling back to the API's own URL only if
        // that fails too. The mirror URL is a user-editable template (Settings), not hardcoded,
        // so this can be corrected without a code update if mariadb.org's layout changes again.
        let cfg = load_cfg();
        let template = cfg.get("mariadb_download_url_template").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty()).unwrap_or(DEFAULT_MARIADB_DOWNLOAD_TEMPLATE).to_string();
        let primary_url = template.replace("{version}", &patch).replace("{file_name}", &file_name);
        let mut zipf = None;
        let mut errs: Vec<String> = Vec::new();
        for (label, u) in [("configured download URL", primary_url.as_str()), ("MariaDB API URL", api_url.as_str())] {
            let attempt = client.get(u).send().map_err(|e| e.to_string())
                .and_then(|r| r.bytes().map_err(|e| e.to_string()))
                .and_then(|b| zip::ZipArchive::new(std::io::Cursor::new(b)).map_err(|e| e.to_string()));
            match attempt {
                Ok(z) => { zipf = Some(z); break; }
                Err(e) => errs.push(format!("{}: {}", label, e)),
            }
        }
        let mut zipf = zipf.ok_or_else(|| format!("Could not download a valid archive from either source.\n{}", errs.join("\n")))?;
        // 5) extract wanted client binaries
        let dest = tools_dir();
        std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        let want = ["mysqldump.exe", "mysql.exe", "mysqlimport.exe", "mysqlcheck.exe", "mariadb.exe", "mariadb-dump.exe"];
        let mut got: Vec<String> = Vec::new();
        for i in 0..zipf.len() {
            let mut f = zipf.by_index(i).map_err(|e| e.to_string())?;
            let full = f.name().to_string();
            let base = full.rsplit(['/', '\\']).next().unwrap_or("").to_string();
            if want.contains(&base.as_str()) {
                let out = dest.join(&base);
                let mut o = std::fs::File::create(&out).map_err(|e| e.to_string())?;
                std::io::copy(&mut f, &mut o).map_err(|e| e.to_string())?;
                got.push(base);
            }
        }
        if got.is_empty() { return Ok(json!({"ok":false,"error":"Downloaded the archive but found no client binaries inside."})); }
        // 6) record paths in config
        let pick = |a: &str, b: &str| -> Option<String> {
            for n in [a, b] { let p = dest.join(n); if p.exists() { return Some(p.to_string_lossy().to_string()); } }
            None
        };
        let mut cfg = load_cfg();
        if let Some(p) = pick("mysql.exe", "mariadb.exe") { cfg["mysql_bin"] = json!(p); }
        if let Some(p) = pick("mysqldump.exe", "mariadb-dump.exe") { cfg["mysqldump_bin"] = json!(p); }
        std::fs::write(config_file(), serde_json::to_string_pretty(&cfg).unwrap_or_default()).map_err(|e| e.to_string())?;
        // New binaries just landed on disk and the config above may point at them now - the
        // cached tools_status result (if any) is stale.
        *tools_status_cache().lock().unwrap() = None;
        Ok(json!({"ok":true, "message": format!("Downloaded MariaDB {} client tools to {}", patch, dest.to_string_lossy()), "config": cfg}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
fn app_info(app: tauri::AppHandle) -> R {
    let pi = app.package_info();
    Ok(json!({"ok":true, "name": pi.name, "version": pi.version.to_string()}))
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) { app.exit(0); }
#[tauri::command]
fn quit(app: tauri::AppHandle) { app.exit(0); }

// Opens the Buy Me a Coffee link in the OS's default browser, not this app's own webview -
// deliberately takes no argument and hardcodes the exact URL rather than accepting one from the
// frontend, so this can never become a way to launch an arbitrary command/URL. No existing
// shell/opener plugin is set up in this project, and adding one is a bigger dependency change
// than one static link needs - a plain OS-specific spawn is simpler and has no new attack surface
// beyond "open this one fixed https URL", the same thing every platform's own browser already does.
#[tauri::command]
fn open_support_link(_req: Value) -> Result<(), String> {
    const URL: &str = "https://buymeacoffee.com/monsama";
    #[cfg(target_os = "windows")]
    { std::process::Command::new("cmd").args(["/C", "start", "", URL]).spawn().map_err(|e| e.to_string())?; }
    #[cfg(target_os = "macos")]
    { std::process::Command::new("open").arg(URL).spawn().map_err(|e| e.to_string())?; }
    #[cfg(all(unix, not(target_os = "macos")))]
    { std::process::Command::new("xdg-open").arg(URL).spawn().map_err(|e| e.to_string())?; }
    Ok(())
}

#[tauri::command]
// Companion to save_text for binary content (currently just PNG diagram exports). Sent as a
// plain JSON array of byte values rather than base64 - decoding base64 correctly would need a
// new crate dependency that can't be verified compiles without a working cargo toolchain, while
// a numeric array only needs the array/number extraction serde_json already provides elsewhere
// in this file. The size cost of skipping base64 is irrelevant for a one-off, at-most-a-few-MB
// diagram export.
fn save_binary(req: Value) -> R {
    let path = req["path"].as_str().unwrap_or("");
    if path.is_empty() { return Ok(json!({"ok":false,"error":"no path"})); }
    let bytes: Vec<u8> = req["bytes"].as_array().map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect()).unwrap_or_default();
    match std::fs::write(path, bytes) {
        Ok(_) => Ok(json!({"ok":true})),
        Err(e) => Ok(json!({"ok":false,"error":e.to_string()})),
    }
}

#[tauri::command]
fn save_text(req: Value) -> R {
    let path = req["path"].as_str().unwrap_or("");
    let content = req["content"].as_str().unwrap_or("");
    if path.is_empty() { return Ok(json!({"ok":false,"error":"no path"})); }
    match std::fs::write(path, content) {
        Ok(_) => Ok(json!({"ok":true})),
        Err(e) => Ok(json!({"ok":false,"error":e.to_string()})),
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.maximize();
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connect, schemas, objects, ddl, pk, query, exec, rowop, script, fetch_cursor_batch, close_cursor,
            import, export, importcsv, browse, quit_app, save_text, save_binary, export_table, cancel_export, cancel_job, app_info, get_config, save_config, download_tools, tools_status, conn_list, conn_get, conn_save, conn_delete, conn_primary, conn_clear, quit, lib_list, lib_save, lib_delete, lib_clear, lib_replace, search_all_schemas, cancel_query, compare_dbs, compare_schemas, compare_apply, compare_tables, compare_rows, compare_rows_apply, compare_rows_diff, compare_rows_apply_diff, compare_cancel, fk, compare_rows_insert_all, compare_rows_fetch_by_pk, gen_user_transfer, process_list, kill_process, schema_erd, open_support_link
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ---------------------------------------------------------------------------
// Unit tests for the pure helpers that sit on destructive paths: the read-only
// gate, statement splitting, identifier and literal quoting, and the USE-prefix
// handling that decides WHICH database a script runs against. These are where a
// bug silently loses or corrupts someone's data, and they are all pure
// functions, so there is no reason not to pin their behaviour.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_id_wraps_and_escapes_backticks() {
        assert_eq!(sql_id("users"), "`users`");
        assert_eq!(sql_id("my table"), "`my table`");
        assert_eq!(sql_id("we`ird"), "`we``ird`");
        assert_eq!(sql_id("t` ; DROP TABLE x; --"), "`t`` ; DROP TABLE x; --`");
    }

    #[test]
    fn sql_lit_escapes_quotes_and_backslashes() {
        assert_eq!(sql_lit("plain"), "'plain'");
        assert_eq!(sql_lit("O'Brien"), "'O''Brien'");
        assert_eq!(sql_lit("back\\slash"), "'back\\\\slash'");
        assert_eq!(sql_lit("'; DROP TABLE x; --"), "'''; DROP TABLE x; --'");
    }

    // A trailing backslash right before the string's closing quote is the classic bypass for a
    // quote-only escaper (```'{}'``, s.replace("'","''")``` alone): the backslash would combine
    // with the literal `'` MySQL emits right after it to produce an escaped quote, closing the
    // string one character early and leaving whatever follows to execute as SQL. sql_str_lit
    // (and sql_lit, which delegates to it) escape the backslash FIRST so this can't happen -
    // objects()/pk()/fk()/schema_erd() and friends now go through this instead of an ad-hoc
    // single-quote-only replace() that didn't have this protection.
    #[test]
    fn sql_str_lit_neutralizes_trailing_backslash_quote_bypass() {
        assert_eq!(sql_str_lit("x\\"), "'x\\\\'");
        assert_eq!(sql_lit("x\\"), "'x\\\\'");
    }

    #[test]
    fn cnf_safe_strips_embedded_newlines() {
        assert_eq!(cnf_safe("normal-host"), "normal-host");
        assert_eq!(cnf_safe("evil\npager=touch /tmp/pwned"), "evilpager=touch /tmp/pwned");
        assert_eq!(cnf_safe("evil\r\nmore"), "evilmore");
    }

    #[test]
    fn sql_val_lit_passes_hex_through_unquoted() {
        assert_eq!(sql_val_lit("0xDEADBEEF"), "0xDEADBEEF");
        assert_eq!(sql_val_lit("0x00"), "0x00");
        assert_eq!(sql_val_lit("0xZZ"), "'0xZZ'");
        assert_eq!(sql_val_lit("hello"), "'hello'");
    }

    #[test]
    fn read_only_allows_reads() {
        for sql in ["SELECT 1", "select * from t", "SHOW TABLES", "EXPLAIN SELECT 1",
                    "DESCRIBE t", "WITH x AS (SELECT 1) SELECT * FROM x", "", "   "] {
            assert!(sql_is_readonly(sql), "should be allowed: {:?}", sql);
        }
    }

    #[test]
    fn read_only_blocks_writes() {
        for sql in ["DELETE FROM t", "delete from t", "UPDATE t SET a=1", "INSERT INTO t VALUES (1)",
                    "DROP TABLE t", "TRUNCATE t", "ALTER TABLE t ADD c INT", "CREATE TABLE t (a INT)",
                    "GRANT ALL ON *.* TO x", "SELECT 1; DELETE FROM t"] {
            assert!(!sql_is_readonly(sql), "should be blocked: {:?}", sql);
        }
    }

    #[test]
    fn read_only_sees_through_comments() {
        assert!(!sql_is_readonly("/* harmless */ DELETE FROM t"));
        assert!(!sql_is_readonly("-- comment\nDELETE FROM t"));
        assert!(!sql_is_readonly("# comment\nDELETE FROM t"));
        assert!(sql_is_readonly("SELECT 1 -- DELETE FROM t"));
    }

    #[test]
    fn read_only_blocks_mysql_executable_comments() {
        // MySQL EXECUTES the body of /*! ... */ - it is a version-gated directive, not a
        // comment - so stripping it before the keyword check lets a write through.
        assert!(!sql_is_readonly("/*!50000 DELETE FROM t */"));
        assert!(!sql_is_readonly("SELECT 1; /*!DROP TABLE t */"));
    }

    #[test]
    fn read_only_blocks_cte_prefixed_writes() {
        assert!(sql_is_readonly("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(sql_is_readonly("WITH x AS (SELECT 1), y AS (SELECT 2) SELECT * FROM x, y"));
        assert!(!sql_is_readonly("WITH x AS (SELECT 1) DELETE FROM t WHERE id IN (SELECT id FROM x)"));
        assert!(!sql_is_readonly("WITH x AS (SELECT 1) UPDATE t SET a=1"));
        assert!(!sql_is_readonly("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"));
    }

    // A ')' - or a keyword - inside a quoted string isn't a real paren/token: without tracking
    // quote state, this exact statement's embedded ")SELECT(" leaked out as an exposed depth-0
    // "SELECT", which "first verb wins" then picked over the real (and dangerous) trailing DELETE.
    #[test]
    fn read_only_blocks_cte_with_paren_in_string_literal() {
        assert!(!sql_is_readonly("WITH x AS (SELECT 1 FROM t WHERE a=')SELECT(') DELETE FROM t"));
        assert!(!sql_is_readonly("WITH x AS (SELECT 1 FROM t WHERE a=\")SELECT(\") DELETE FROM t"));
        assert!(sql_is_readonly("WITH x AS (SELECT 1 FROM t WHERE a=')DELETE(') SELECT * FROM x"));
    }

    #[test]
    fn read_only_blocks_analyze_wrapped_writes() {
        // ANALYZE TABLE is a genuinely read-only maintenance statement.
        assert!(sql_is_readonly("ANALYZE TABLE t"));
        assert!(sql_is_readonly("analyze table t, t2"));
        // MariaDB's ANALYZE [FORMAT=JSON] <statement> form actually EXECUTES the statement it
        // wraps - a SELECT is fine, anything else must be blocked exactly like it would be
        // unwrapped.
        assert!(sql_is_readonly("ANALYZE SELECT 1"));
        assert!(sql_is_readonly("ANALYZE FORMAT=JSON SELECT * FROM t"));
        assert!(!sql_is_readonly("ANALYZE DELETE FROM t"));
        assert!(!sql_is_readonly("ANALYZE INSERT INTO t VALUES (1)"));
        assert!(!sql_is_readonly("ANALYZE UPDATE t SET a=1"));
        assert!(!sql_is_readonly("ANALYZE FORMAT=JSON DELETE FROM t"));
    }

    #[test]
    fn read_only_blocks_server_state_changes() {
        // SET on a session variable is harmless; GLOBAL and PERSIST change the server for
        // everyone, which "read-only / safe mode" should not permit.
        assert!(sql_is_readonly("SET autocommit=0"));
        assert!(!sql_is_readonly("SET GLOBAL max_connections=1"));
        assert!(!sql_is_readonly("SET PERSIST max_connections=1"));
    }

    #[test]
    fn split_handles_plain_statements() {
        assert_eq!(split_sql_statements("SELECT 1; SELECT 2"), vec!["SELECT 1", "SELECT 2"]);
        assert_eq!(split_sql_statements("SELECT 1;"), vec!["SELECT 1"]);
        assert_eq!(split_sql_statements("   "), Vec::<String>::new());
    }

    #[test]
    fn split_does_not_break_on_semicolons_inside_strings() {
        assert_eq!(split_sql_statements("SELECT 'a;b'"), vec!["SELECT 'a;b'"]);
        assert_eq!(split_sql_statements("SELECT \"a;b\""), vec!["SELECT \"a;b\""]);
        assert_eq!(split_sql_statements("SELECT 'it''s;fine'"), vec!["SELECT 'it''s;fine'"]);
    }

    #[test]
    fn split_honours_delimiter_directive() {
        let sql = "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END$$\nDELIMITER ;";
        let out = split_sql_statements(sql);
        assert_eq!(out.len(), 1, "procedure body must stay one statement, got {:?}", out);
        assert!(out[0].contains("SELECT 1; SELECT 2"), "body was split: {:?}", out);
    }

    #[test]
    fn strip_use_reports_last_database_and_remainder() {
        let (db, rest) = strip_leading_use_statements("USE shop; SELECT 1");
        assert_eq!(db.as_deref(), Some("shop"));
        assert_eq!(rest.trim(), "SELECT 1");

        let (db, _) = strip_leading_use_statements("USE `a b`; USE second; SELECT 1");
        assert_eq!(db.as_deref(), Some("second"), "the LAST USE wins");

        let (db, rest) = strip_leading_use_statements("SELECT 1");
        assert_eq!(db, None);
        assert_eq!(rest.trim(), "SELECT 1");
    }

    #[test]
    fn row_key_distinguishes_rows_that_differ_only_by_field_boundary() {
        let a = vec![Some("a".to_string()), Some("b".to_string())];
        let b = vec![Some("ab".to_string()), None];
        assert_ne!(row_key(&a), row_key(&b));
        assert_eq!(row_key(&[None]), row_key(&[Some(String::new())]));
    }

    #[test]
    fn ver_key_orders_numerically_not_lexically() {
        assert!(ver_key("11.4.2") > ver_key("9.9.9"), "11.x must beat 9.x");
        assert!(ver_key("10.11.0") > ver_key("10.9.0"), "10.11 must beat 10.9");
    }

    // The whole point of table_filter_args is to never build a command line that scales with a
    // huge table count - below its 40-exclusion threshold it must stay on the cheap --ignore-table
    // path without ever touching the database (a bogus connection here would panic/error out if
    // it did), and it must ignore exclusions that belong to a different database entirely.
    #[test]
    fn table_filter_args_stays_on_ignore_table_path_under_threshold() {
        let bogus_conn = json!({"host":"unreachable.invalid","port":3306,"user":"x","password":"x"});
        let mut excl = std::collections::HashSet::new();
        excl.insert("mydb.orders".to_string());
        excl.insert("mydb.customers".to_string());
        excl.insert("otherdb.orders".to_string()); // different database - must be filtered out
        let (ignore_args, positional) = table_filter_args("mydb", &excl, &bogus_conn).unwrap();
        assert!(positional.is_empty());
        assert_eq!(ignore_args.len(), 2);
        assert!(ignore_args.contains(&"--ignore-table=mydb.orders".to_string()));
        assert!(ignore_args.contains(&"--ignore-table=mydb.customers".to_string()));
        assert!(!ignore_args.iter().any(|a| a.contains("otherdb")));
    }

    #[test]
    fn table_filter_args_is_a_noop_with_no_exclusions() {
        let bogus_conn = json!({"host":"unreachable.invalid","port":3306,"user":"x","password":"x"});
        let excl = std::collections::HashSet::new();
        let (ignore_args, positional) = table_filter_args("mydb", &excl, &bogus_conn).unwrap();
        assert!(ignore_args.is_empty() && positional.is_empty());
    }

    #[test]
    fn tool_search_dirs_covers_every_real_install_layout() {
        let root = std::env::temp_dir().join(format!("nobs-tooltest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let layouts = ["Program Files/MariaDB 11.4/bin",
                       "Program Files/MySQL/MySQL Server 8.0/bin",
                       "wamp64/bin/mariadb/mariadb11.4/bin"];
        for l in layouts { std::fs::create_dir_all(root.join(l)).unwrap(); }
        std::fs::create_dir_all(root.join("Program Files/Unrelated/bin")).unwrap();

        let bases = vec![root.join("Program Files"), root.join("wamp64/bin")];
        let direct = vec![root.join("xampp/mysql/bin")];
        let dirs = tool_search_dirs(&bases, &direct);

        for l in layouts { assert!(dirs.contains(&root.join(l)), "layout not searched: {}", l); }
        assert!(dirs.contains(&root.join("xampp/mysql/bin")), "direct dir not searched");
        assert!(!dirs.iter().any(|d| d.to_string_lossy().contains("Unrelated")),
                "unrelated Program Files entry should be skipped");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn first_err_prefers_the_error_line() {
        assert_eq!(first_err("some echoed statement\nERROR 1064 (42000): You have an error"),
                   "ERROR 1064 (42000): You have an error");
        assert_eq!(first_err("plain failure text"), "plain failure text");
    }
}

// ---------------------------------------------------------------------------
// Integration tests that need a live server. They are ignored by default so an
// ordinary `cargo test` stays offline; run them with a database available as:
//
//   NOBS_TEST_DSN='127.0.0.1:3399:nobs:nobs' cargo test -- --ignored --nocapture
// ---------------------------------------------------------------------------
#[cfg(test)]
mod live_tests {
    use super::*;

    fn conn_json() -> Option<Value> {
        let dsn = std::env::var("NOBS_TEST_DSN").ok()?;
        let p: Vec<&str> = dsn.split(':').collect();
        if p.len() != 4 { return None; }
        Some(json!({"host":p[0],"port":p[1],"user":p[2],"password":p[3],"ssl":"default"}))
    }

    // The bug this guards: `query` decided a failure was a cancellation by asking whether a
    // requestId had been supplied. The editor supplies one on every run, so every genuine
    // error - a missing table, a syntax error, no database selected - was reported to the user
    // as "Query cancelled." with the real cause discarded.
    #[tokio::test]
    #[ignore]
    async fn genuine_errors_are_not_reported_as_cancellations() {
        let Some(conn) = conn_json() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let cases = [
            ("missing table",       "SELECT * FROM nobs_test.definitely_not_a_table"),
            ("syntax error",        "SELEKT 1"),
            ("no database selected","SELECT (SELECT COUNT(*) FROM ro_canary) AS n"),
        ];
        for (label, sql) in cases {
            let req = json!({"sql": sql, "conn": conn, "requestId": format!("test-{}", label)});
            let r = query(req).await.expect("command returned Err");
            let err = r["error"].as_str().unwrap_or("");
            println!("  {:<22} -> {}", label, err);
            assert_eq!(r["ok"], false, "{label} should fail");
            assert_ne!(err, "Query cancelled.",
                       "{label}: the real error was masked as a cancellation");
            assert!(r["cancelled"].is_null(), "{label}: should not be flagged as cancelled");
        }
    }

    // ...while a query that really is cancelled still says so.
    #[tokio::test]
    #[ignore]
    async fn a_cancelled_query_still_reports_as_cancelled() {
        let Some(conn) = conn_json() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let rid = "test-real-cancel".to_string();
        let q = tokio::spawn(query(json!({"sql":"SELECT SLEEP(10)", "conn": conn.clone(), "requestId": rid})));
        // let it register its CONNECTION_ID() before killing it
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let c = cancel_query(json!({"requestId":"test-real-cancel", "conn": conn})).await.expect("cancel failed");
        assert_eq!(c["ok"], true);
        let r = q.await.expect("join failed").expect("command returned Err");
        println!("  cancelled query        -> {}", r["error"].as_str().unwrap_or(""));
        assert_eq!(r["error"], "Query cancelled.");
        assert_eq!(r["cancelled"], true);
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    fn conn_json() -> Option<Value> {
        let dsn = std::env::var("NOBS_TEST_DSN").ok()?;
        let p: Vec<&str> = dsn.split(':').collect();
        if p.len() != 4 { return None; }
        Some(json!({"host":p[0],"port":p[1],"user":p[2],"password":p[3],"ssl":"default"}))
    }
    async fn one(conn: &Value, sql: &str) -> Value {
        query(json!({"sql":sql,"conn":conn,"db":"nobs_test"})).await.unwrap()
    }

    // Staged grid edits are applied as one batch. If a later statement fails, the earlier ones
    // must not remain - a half-applied edit is the outcome the pending-changes model exists to
    // prevent, and the README promises a transaction.
    #[tokio::test]
    #[ignore]
    async fn a_failed_batch_applies_nothing() {
        let Some(conn) = conn_json() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        one(&conn, "UPDATE txn_child SET descr='original' WHERE id=1").await;
        let batch = "UPDATE nobs_test.txn_child SET descr='EDITED FIRST' WHERE id=1 LIMIT 1;\n\
                     UPDATE nobs_test.txn_child SET qty=-1 WHERE id=2 LIMIT 1;\n\
                     UPDATE nobs_test.txn_child SET descr='EDITED THIRD' WHERE id=3 LIMIT 1;";
        let r = script(json!({"sql":batch,"conn":conn,"db":"nobs_test","transaction":true})).await.unwrap();
        assert_eq!(r["ok"], false, "the batch should fail on the CHECK constraint");
        println!("  batch error: {}", r["error"].as_str().unwrap_or("").lines().next().unwrap_or(""));
        let after = one(&conn, "SELECT descr FROM txn_child WHERE id=1").await;
        let descr = after["rows"][0][0].as_str().unwrap_or("");
        println!("  descr of row 1 after the failed batch: {:?}", descr);
        assert_eq!(descr, "original", "an earlier statement survived a failed batch");
    }

    // Grid edits used to run with FOREIGN_KEY_CHECKS=0, so an edit could point a row at a
    // parent that does not exist.
    #[tokio::test]
    #[ignore]
    async fn foreign_keys_are_enforced_on_apply() {
        let Some(conn) = conn_json() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        one(&conn, "UPDATE txn_child SET parent_id=2 WHERE code='CCC'").await;
        let batch = "UPDATE nobs_test.txn_child SET parent_id=99 WHERE code='CCC' LIMIT 1;";
        let r = script(json!({"sql":batch,"conn":conn,"db":"nobs_test","transaction":true})).await.unwrap();
        println!("  FK-violating edit: ok={} err={}", r["ok"],
                 r["error"].as_str().unwrap_or("").lines().next().unwrap_or(""));
        assert_eq!(r["ok"], false, "an edit pointing at a missing parent must be rejected");
        let after = one(&conn, "SELECT parent_id FROM txn_child WHERE code='CCC'").await;
        assert_eq!(after["rows"][0][0].as_str().unwrap_or(""), "2", "the orphaning edit was applied anyway");
    }

    // ...while a valid batch still applies in full.
    #[tokio::test]
    #[ignore]
    async fn a_valid_batch_applies_completely() {
        let Some(conn) = conn_json() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        // Rows 4 and 5, which no other test in this module touches: cargo runs these in
        // parallel against one database, and sharing a row made them race.
        let batch = "UPDATE nobs_test.txn_child SET descr='batch-a' WHERE id=4 LIMIT 1;\n\
                     UPDATE nobs_test.txn_child SET descr='batch-b' WHERE id=5 LIMIT 1;";
        let r = script(json!({"sql":batch,"conn":conn,"db":"nobs_test","transaction":true})).await.unwrap();
        assert_eq!(r["ok"], true, "valid batch should apply");
        let a = one(&conn, "SELECT descr FROM txn_child WHERE id=4").await;
        let b = one(&conn, "SELECT descr FROM txn_child WHERE id=5").await;
        println!("  after a valid batch: {:?} / {:?}", a["rows"][0][0], b["rows"][0][0]);
        assert_eq!(a["rows"][0][0].as_str().unwrap_or(""), "batch-a");
        assert_eq!(b["rows"][0][0].as_str().unwrap_or(""), "batch-b");
    }
}

#[cfg(test)]
mod export_cancel_tests {
    use super::*;

    // Cancelling an export kills the running mysqldump, which then has no stderr to report -
    // run_job_child deliberately does not wait for it. The log line was built from that empty
    // string, so a cancelled dump appeared as "FAILED <db> routines/events : " with nothing
    // after the colon: a failure, with no reason, for something the user asked to stop.
    #[tokio::test]
    #[ignore]
    async fn cancelling_an_export_is_logged_as_cancelled_not_an_empty_failure() {
        let Some(dsn) = std::env::var("NOBS_TEST_DSN").ok() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let p: Vec<&str> = dsn.split(':').collect();
        let conn = json!({"host":p[0],"port":p[1],"user":p[2],"password":p[3],"ssl":"default"});
        let dir = std::env::temp_dir().join("nobs-export-cancel");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let jid = "export-cancel-test";
        let req = json!({
            "dbs": ["nobs_test"], "folder": dir.to_string_lossy(), "mode": "table",
            "conn": conn, "jobId": jid, "excludes": [],
            "options": {"charset":"utf8mb4","routines":true,"events":true,"quick":true,"extinsert":true}
        });
        let handle = tokio::spawn(export_run(req, std::env::var("MYSQLDUMP_BIN").unwrap_or_else(|_| "mysqldump".into())));
        // let it get into the dump of the 100k-row tables, then stop it
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let c = cancel_job(json!({"jobId": jid})).unwrap();
        println!("  cancel_job -> {}", c);
        let r = handle.await.unwrap().unwrap();

        let empty: Vec<Value> = Vec::new();
        let lines: Vec<String> = r["log"].as_array().unwrap_or(&empty).iter()
            .map(|v| v.as_str().unwrap_or("").to_string()).collect();
        for l in &lines { println!("  | {}", l); }

        let empty_failure: Vec<&String> = lines.iter()
            .filter(|l| l.starts_with("FAILED") && l.trim_end().ends_with(':')).collect();
        assert!(empty_failure.is_empty(), "a FAILED line with no reason: {:?}", empty_failure);
        assert!(lines.iter().any(|l| l.contains("CANCELLED")), "nothing reported the cancellation: {:?}", lines);
        assert_eq!(r["cancelled"], true, "the run should report itself cancelled");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;

    // "Continue on error" passes --force to mysql, which then exits 0 even when every statement
    // failed. Trusting the exit code turned a wholly failed import into a list of OK lines - the
    // worst outcome for a restore, because it looks like it worked.
    #[tokio::test]
    #[ignore]
    async fn force_mode_reports_the_errors_it_skipped() {
        let Some(dsn) = std::env::var("NOBS_TEST_DSN").ok() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let p: Vec<&str> = dsn.split(':').collect();
        let conn = json!({"host":p[0],"port":p[1],"user":p[2],"password":p[3],"ssl":"default"});
        let mbin = std::env::var("MYSQL_BIN").unwrap_or_else(|_| "mysql".into());

        let f = std::env::temp_dir().join("nobs-import-bad.sql");
        std::fs::write(&f, "INSERT INTO nobs_test.no_such_table VALUES (1);\nSELECT 1;\n").unwrap();
        let req = json!({"files":[f.to_string_lossy()], "targetDb":"nobs_test", "conn":conn, "force":true});
        let r = import_run(req, mbin).await.unwrap();
        let line = r["log"][0].as_str().unwrap_or("").to_string();
        println!("  log line: {}", line);
        println!("  errorsSkipped: {}", r["errorsSkipped"]);
        assert!(!line.starts_with("OK  "), "a failed import was reported as a clean OK: {line}");
        assert!(line.contains("error(s) SKIPPED"), "the skipped error was not reported: {line}");
        assert_eq!(r["errorsSkipped"], 1);
        let _ = std::fs::remove_file(&f);
    }

    // A per-table dump has no CREATE DATABASE or USE, so without a target it fails with a
    // message that does not say what to do about it.
    #[tokio::test]
    #[ignore]
    async fn a_missing_target_database_explains_itself() {
        let Some(dsn) = std::env::var("NOBS_TEST_DSN").ok() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let p: Vec<&str> = dsn.split(':').collect();
        let conn = json!({"host":p[0],"port":p[1],"user":p[2],"password":p[3],"ssl":"default"});
        let mbin = std::env::var("MYSQL_BIN").unwrap_or_else(|_| "mysql".into());

        let f = std::env::temp_dir().join("nobs-import-nodb.sql");
        std::fs::write(&f, "INSERT INTO ro_canary (label) VALUES ('x');\n").unwrap();
        let req = json!({"files":[f.to_string_lossy()], "targetDb":"", "conn":conn});
        let r = import_run(req, mbin).await.unwrap();
        let line = r["log"][0].as_str().unwrap_or("").to_string();
        println!("  log line: {}", line.replace('\n', " | "));
        assert!(line.contains("1046") || line.contains("No database selected"));
        assert!(line.contains("Target database"), "no hint about choosing a target: {line}");
        let _ = std::fs::remove_file(&f);
    }
}

#[cfg(test)]
mod binary_col_tests {
    use super::*;
    #[tokio::test]
    #[ignore]
    async fn query_reports_which_columns_are_binary() {
        let Some(dsn) = std::env::var("NOBS_TEST_DSN").ok() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let p: Vec<&str> = dsn.split(':').collect();
        let conn = json!({"host":p[0],"port":p[1],"user":p[2],"password":p[3],"ssl":"default"});
        let r = query(json!({"sql":"SELECT id, emoji, bin_col, blob_col, bit_col, bit8 FROM charset_binary",
                             "conn":conn,"db":"nobs_test"})).await.unwrap();
        let cols: Vec<String> = r["columns"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        let bin: Vec<bool> = r["binaryCols"].as_array().expect("binaryCols missing").iter().map(|v| v.as_bool().unwrap()).collect();
        let bit: Vec<bool> = r["bitCols"].as_array().expect("bitCols missing").iter().map(|v| v.as_bool().unwrap()).collect();
        for ((c, b), t) in cols.iter().zip(bin.iter()).zip(bit.iter()) { println!("  {:<10} binary={} bit={}", c, b, t); }
        assert_eq!(bin, vec![false, false, true, true, true, true],
                   "id and emoji are text; bin_col, blob_col and both BIT columns are binary");
        // bitCols narrows binaryCols to just the BIT(n) columns: bin_col/blob_col are real
        // binary data (only a 0x.. hex literal is safe there) but not BIT, so a bare integer
        // literal sent to them would store the bytes of its digit character, not a number -
        // unlike bit_col/bit8, where MySQL accepts a bare integer as the correct bit pattern.
        assert_eq!(bit, vec![false, false, false, false, true, true],
                   "only bit_col and bit8 are BIT columns; bin_col/blob_col are binary but not BIT");
    }
}

#[cfg(test)]
mod csv_null_tests {
    use super::*;
    fn conn_json() -> Option<Value> {
        let dsn = std::env::var("NOBS_TEST_DSN").ok()?;
        let p: Vec<&str> = dsn.split(':').collect();
        if p.len() != 4 { return None; }
        Some(json!({"host":p[0],"port":p[1],"user":p[2],"password":p[3],"ssl":"default"}))
    }

    // A NULL and an empty string both came out as an empty field, so a CSV could not tell them
    // apart - and the importer reads an empty cell as NULL, so an empty string did not survive a
    // round trip at all.
    #[tokio::test]
    #[ignore]
    async fn null_and_empty_string_survive_a_csv_round_trip() {
        let Some(conn) = conn_json() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let q = |sql: &str| { let c = conn.clone(); let s = sql.to_string();
            async move { query(json!({"sql":s,"conn":c,"db":"nobs_test"})).await.unwrap() } };

        q("DROP TABLE IF EXISTS csv_null_rt").await;
        q("CREATE TABLE csv_null_rt (id INT PRIMARY KEY, v VARCHAR(32) NULL)").await;
        q("INSERT INTO csv_null_rt VALUES (1, NULL), (2, ''), (3, 'text')").await;

        let file = std::env::temp_dir().join("csv_null_rt.csv");
        let r = export_table_run(None, json!({"conn":conn,"db":"nobs_test","table":"csv_null_rt",
                                    "file":file.to_string_lossy(),"format":"csv","nullValue":"\\N"})).await.unwrap();
        assert_eq!(r["ok"], true, "export failed: {}", r);
        let text = std::fs::read_to_string(&file).unwrap();
        println!("  exported csv:");
        for l in text.lines() { println!("    {}", l); }
        assert!(text.contains("1,\\N"), "NULL was not written as the marker");
        assert!(text.contains("2,\n") || text.ends_with("2,"), "empty string should be an empty field");

        // read it back into a fresh table
        q("DROP TABLE IF EXISTS csv_null_rt2").await;
        q("CREATE TABLE csv_null_rt2 (id INT PRIMARY KEY, v VARCHAR(32) NULL)").await;
        let ir = importcsv(json!({"conn":conn,"db":"nobs_test","table":"csv_null_rt2",
                                  "file":file.to_string_lossy(),"hasHeader":true,"nullValue":"\\N"})).await.unwrap();
        assert_eq!(ir["ok"], true, "import failed: {}", ir);

        let back = q("SELECT id, v IS NULL AS is_null, v = '' AS is_empty FROM csv_null_rt2 ORDER BY id").await;
        let rows = back["rows"].as_array().unwrap();
        for r in rows { println!("  back: id={} is_null={:?} is_empty={:?}", r[0], r[1], r[2]); }
        assert_eq!(rows[0][1].as_str(), Some("1"), "row 1 must come back as NULL");
        assert_eq!(rows[1][1].as_str(), Some("0"), "row 2 must NOT be NULL - it was an empty string");
        assert_eq!(rows[1][2].as_str(), Some("1"), "row 2 must come back as an empty string");

        q("DROP TABLE IF EXISTS csv_null_rt").await;
        q("DROP TABLE IF EXISTS csv_null_rt2").await;
        let _ = std::fs::remove_file(&file);
    }

    // A CSV import with "Truncate table" checked used to leave the table permanently truncated
    // and only partially reloaded if a later row failed (e.g. a duplicate key) - every batch ran
    // on autocommit with nothing to undo the ones that had already landed. It's now wrapped in a
    // transaction: a mid-file failure must leave the table exactly as it was before the import.
    #[tokio::test]
    #[ignore]
    async fn a_failed_csv_import_with_truncate_leaves_the_table_untouched() {
        let Some(conn) = conn_json() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let q = |sql: &str| { let c = conn.clone(); let s = sql.to_string();
            async move { query(json!({"sql":s,"conn":c,"db":"nobs_test"})).await.unwrap() } };

        q("DROP TABLE IF EXISTS csv_txn_rt").await;
        q("CREATE TABLE csv_txn_rt (id INT PRIMARY KEY, v VARCHAR(32))").await;
        q("INSERT INTO csv_txn_rt VALUES (100, 'original')").await;

        // 501 good rows (id 1..501, spanning the importer's 500-row batch boundary) followed by
        // a row that duplicates id 1 - the SECOND batch's INSERT fails. A single statement is
        // already atomic on its own in MySQL, so this specifically checks that the FIRST batch
        // (which had already landed inside the open transaction) gets rolled back too, not just
        // that the failing batch itself doesn't partially apply.
        let mut csv = String::from("id,v\n");
        for i in 1..=501 { csv.push_str(&format!("{},row{}\n", i, i)); }
        csv.push_str("1,duplicate\n");
        let file = std::env::temp_dir().join("csv_txn_rt.csv");
        std::fs::write(&file, csv).unwrap();

        let ir = importcsv(json!({"conn":conn,"db":"nobs_test","table":"csv_txn_rt",
                                   "file":file.to_string_lossy(),"hasHeader":true,"truncate":true})).await.unwrap();
        assert_eq!(ir["ok"], false, "the import should fail on the duplicate key");
        println!("  import error: {}", ir["error"].as_str().unwrap_or("").lines().next().unwrap_or(""));

        let back = q("SELECT id, v FROM csv_txn_rt").await;
        let rows = back["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "the truncate should have been rolled back along with the failed batch");
        assert_eq!(rows[0][0].as_str(), Some("100"));
        assert_eq!(rows[0][1].as_str(), Some("original"), "the original row must survive a rolled-back import");

        q("DROP TABLE IF EXISTS csv_txn_rt").await;
        let _ = std::fs::remove_file(&file);
    }

    // Clearing the marker restores the older reading, where a blank cell means NULL - which is
    // what a spreadsheet exported from Excel usually intends.
    #[tokio::test]
    #[ignore]
    async fn an_empty_marker_makes_blank_cells_null_again() {
        let Some(conn) = conn_json() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let q = |sql: &str| { let c = conn.clone(); let s = sql.to_string();
            async move { query(json!({"sql":s,"conn":c,"db":"nobs_test"})).await.unwrap() } };
        q("DROP TABLE IF EXISTS csv_blank_rt").await;
        q("CREATE TABLE csv_blank_rt (id INT PRIMARY KEY, v VARCHAR(32) NULL)").await;
        let file = std::env::temp_dir().join("csv_blank_rt.csv");
        std::fs::write(&file, "id,v\n1,\n2,text\n").unwrap();
        let ir = importcsv(json!({"conn":conn,"db":"nobs_test","table":"csv_blank_rt",
                                  "file":file.to_string_lossy(),"hasHeader":true,"nullValue":""})).await.unwrap();
        assert_eq!(ir["ok"], true, "import failed: {}", ir);
        let back = q("SELECT id, v IS NULL AS is_null FROM csv_blank_rt ORDER BY id").await;
        let rows = back["rows"].as_array().unwrap();
        for r in rows { println!("  blank-marker: id={} is_null={:?}", r[0], r[1]); }
        assert_eq!(rows[0][1].as_str(), Some("1"), "with no marker, a blank cell must import as NULL");
        q("DROP TABLE IF EXISTS csv_blank_rt").await;
        let _ = std::fs::remove_file(&file);
    }
}

#[cfg(test)]
mod interop_tests {
    use super::*;
    fn conn_json() -> Option<Value> {
        let dsn = std::env::var("NOBS_TEST_DSN").ok()?;
        let p: Vec<&str> = dsn.split(':').collect();
        if p.len() != 4 { return None; }
        Some(json!({"host":p[0],"port":p[1],"user":p[2],"password":p[3],"ssl":"default"}))
    }

    // Can a CSV written by another client be read back with NULL and empty string intact?
    // HeidiSQL defaults to \N for NULL; MySQL Workbench writes the literal word NULL.
    #[tokio::test]
    #[ignore]
    async fn csv_from_heidisql_and_workbench_round_trips() {
        let Some(conn) = conn_json() else { eprintln!("NOBS_TEST_DSN not set - skipping"); return };
        let q = |sql: &str| { let c = conn.clone(); let s = sql.to_string();
            async move { query(json!({"sql":s,"conn":c,"db":"nobs_test"})).await.unwrap() } };

        // (label, marker the user would set, file contents as that tool writes them)
        let cases = [
            ("HeidiSQL default (\\N)",   "\\N",   "id,v\n1,\\N\n2,\n3,text\n"),
            ("Workbench (literal NULL)", "NULL",  "id,v\n1,NULL\n2,\n3,text\n"),
            ("spreadsheet (blank=NULL)", "",      "id,v\n1,\n2,\n3,text\n"),
        ];
        for (label, marker, body) in cases {
            q("DROP TABLE IF EXISTS csv_interop").await;
            q("CREATE TABLE csv_interop (id INT PRIMARY KEY, v VARCHAR(32) NULL)").await;
            let f = std::env::temp_dir().join("csv_interop.csv");
            std::fs::write(&f, body).unwrap();
            let r = importcsv(json!({"conn":conn,"db":"nobs_test","table":"csv_interop",
                                     "file":f.to_string_lossy(),"hasHeader":true,"nullValue":marker})).await.unwrap();
            assert_eq!(r["ok"], true, "{label}: import failed: {r}");
            let back = q("SELECT id, CASE WHEN v IS NULL THEN 'NULL' WHEN v='' THEN 'empty' ELSE v END AS got FROM csv_interop ORDER BY id").await;
            let got: Vec<String> = back["rows"].as_array().unwrap().iter()
                .map(|r| r[1].as_str().unwrap_or("?").to_string()).collect();
            println!("  {:<26} NULL value={:<6} -> {:?}", label, format!("{:?}", marker), got);
            assert_eq!(got[0], "NULL", "{label}: row 1 should be NULL");
            assert_eq!(got[2], "text", "{label}: row 3 should be text");
            let _ = std::fs::remove_file(&f);
        }
        q("DROP TABLE IF EXISTS csv_interop").await;
    }
}

#[cfg(test)]
mod compare_tests {
    use super::*;

    // Compare DB writes to a target database, so it carries the same risks as the grid's apply
    // path. These drive it end to end against a live server: a saved connection file is written
    // where the app keeps one, using a passwordless user so the keyring is not involved.
    fn setup_conns() -> bool {
        if std::env::var("NOBS_TEST_LIVE").is_err() { return false; }
        let profiles = json!([
            {"name":"cmp",   "host":"127.0.0.1","port":"3399","user":"nobsnp","ssl":"default","readonly":false},
            {"name":"cmpro", "host":"127.0.0.1","port":"3399","user":"nobsnp","ssl":"default","readonly":true}
        ]);
        std::fs::write(conn_path(), serde_json::to_string_pretty(&profiles).unwrap()).unwrap();
        true
    }
    fn raw(sql: &str) {
        let c = json!({"host":"127.0.0.1","port":"3399","user":"nobsnp","password":"","ssl":"default"});
        let mut conn = build_conn(&c).unwrap();
        conn.query_drop(sql).unwrap();
    }
    fn scalar(sql: &str) -> String {
        let c = json!({"host":"127.0.0.1","port":"3399","user":"nobsnp","password":"","ssl":"default"});
        let mut conn = build_conn(&c).unwrap();
        let (_c, r) = run_select(&mut conn, sql).unwrap();
        r.get(0).and_then(|x| x.get(0)).cloned().flatten().unwrap_or_default()
    }

    #[tokio::test]
    #[ignore]
    async fn compare_finds_schema_and_row_differences_and_can_apply_them() {
        if !setup_conns() { eprintln!("NOBS_TEST_LIVE not set - skipping"); return }

        raw("DROP DATABASE IF EXISTS cmp_src"); raw("CREATE DATABASE cmp_src");
        raw("DROP DATABASE IF EXISTS cmp_tgt"); raw("CREATE DATABASE cmp_tgt");
        raw("CREATE TABLE cmp_src.t (id INT PRIMARY KEY, v VARCHAR(32) NULL, extra INT NULL)");
        raw("CREATE TABLE cmp_tgt.t (id INT PRIMARY KEY, v VARCHAR(32) NULL)");   // missing a column
        raw("CREATE TABLE cmp_src.only_here (id INT PRIMARY KEY)");                // missing a table
        raw("INSERT INTO cmp_src.t VALUES (1,'same',NULL),(2,'differs',NULL),(3,NULL,NULL),(4,'',NULL)");
        raw("INSERT INTO cmp_tgt.t VALUES (1,'same'),(2,'OTHER'),(3,''),(5,'extra row')");

        // --- structure
        let r = compare_schemas(json!({"sourceConnName":"cmp","sourceDb":"cmp_src",
                                       "targetConnName":"cmp","targetDb":"cmp_tgt"})).await.unwrap();
        assert_eq!(r["ok"], true, "compare_schemas failed: {r}");
        // the response groups statements per table, each with its own checked/kind flags
        let mut stmts: Vec<String> = Vec::new();
        for t in r["tables"].as_array().cloned().unwrap_or_default() {
            for sq in t["sql"].as_array().cloned().unwrap_or_default() {
                if let Some(st) = sq["stmt"].as_str() { stmts.push(st.to_string()); }
            }
        }
        println!("  schema diff produced {} statement(s):", stmts.len());
        for s in &stmts { println!("    {}", s.chars().take(100).collect::<String>()); }
        assert!(stmts.iter().any(|s| s.contains("only_here")), "missing table not detected");
        assert!(stmts.iter().any(|s| s.to_uppercase().contains("EXTRA")), "missing column not detected");

        // --- a read-only target must refuse to apply
        let ro = compare_apply(json!({"targetConnName":"cmpro","targetDb":"cmp_tgt",
                                      "statements":["CREATE TABLE cmp_tgt.should_not_exist (id INT)"]})).await.unwrap();
        println!("  read-only target -> ok={} error={:?}", ro["ok"], ro["error"].as_str().unwrap_or(""));
        assert_eq!(ro["ok"], false, "a read-only target must refuse to apply");
        assert_eq!(scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='cmp_tgt' AND table_name='should_not_exist'"), "0");

        // --- applying the structure diff for real
        let ap = compare_apply(json!({"targetConnName":"cmp","targetDb":"cmp_tgt","statements":stmts})).await.unwrap();
        assert_eq!(ap["ok"], true, "apply failed: {ap}");
        assert_eq!(scalar("SELECT COUNT(*) FROM information_schema.columns WHERE table_schema='cmp_tgt' AND table_name='t' AND column_name='extra'"), "1",
                   "the missing column was not created");
        assert_eq!(scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='cmp_tgt' AND table_name='only_here'"), "1",
                   "the missing table was not created");
        println!("  structure applied: column and table now present in the target");

        // --- rows missing from the target, found by primary key
        let mr = compare_rows(json!({"sourceConnName":"cmp","sourceDb":"cmp_src",
                                     "targetConnName":"cmp","targetDb":"cmp_tgt","table":"t"})).await.unwrap();
        assert_eq!(mr["ok"], true, "compare_rows failed: {mr}");
        let missing: Vec<String> = mr["rows"].as_array().cloned().unwrap_or_default().iter()
            .map(|r| r[0].as_str().unwrap_or("?").to_string()).collect();
        println!("  rows missing in target: {:?}  (source has 4, target had 1,2,3,5)", missing);
        assert_eq!(missing, vec!["4"], "row 4 exists only in the source and should be reported");

        // --- rows present in both but differing, including NULL vs empty string
        let dr = compare_rows_diff(json!({"sourceConnName":"cmp","sourceDb":"cmp_src",
                                          "targetConnName":"cmp","targetDb":"cmp_tgt","table":"t"})).await.unwrap();
        assert_eq!(dr["ok"], true, "compare_rows_diff failed: {dr}");
        let mut found: Vec<(String,String,String,String)> = Vec::new();
        for d in dr["diffs"].as_array().cloned().unwrap_or_default() {
            let pk = d["pk"][0].as_str().unwrap_or("?").to_string();
            for cd in d["colDiffs"].as_array().cloned().unwrap_or_default() {
                found.push((pk.clone(), cd["col"].as_str().unwrap_or("?").to_string(),
                            format!("{}", cd["src"]), format!("{}", cd["tgt"])));
            }
        }
        for f in &found { println!("  differs: id={} col={} src={} tgt={}", f.0, f.1, f.2, f.3); }
        assert!(found.iter().any(|f| f.0=="2" && f.1=="v"), "a plain value difference was missed");
        assert!(found.iter().any(|f| f.0=="3" && f.1=="v" && f.2=="null" && f.3=="\"\""),
                "NULL in the source vs empty string in the target was NOT reported: {found:?}");
        assert!(!found.iter().any(|f| f.0=="1"), "row 1 is identical and must not be reported");
        println!("  NULL vs empty string is detected as a difference");

        // --- a row that exists only in the TARGET
        println!("  note: id=5 exists only in the target -> reported: {}",
                 found.iter().any(|f| f.0=="5") || missing.contains(&"5".to_string()));
    }

    // compare_rows_apply_diff updates existing target rows one at a time, unlike the insert-only
    // apply paths - a batch failing partway through used to leave some rows corrected and others
    // not, with no way back. It's now wrapped in a transaction: a mid-batch failure must leave
    // every target row exactly as it was before the apply.
    #[tokio::test]
    #[ignore]
    async fn compare_rows_apply_diff_rolls_back_a_failed_batch() {
        if std::env::var("NOBS_TEST_LIVE").is_err() { eprintln!("NOBS_TEST_LIVE not set - skipping"); return }
        let profiles = json!([{"name":"cmp_diff_rt","host":"127.0.0.1","port":"3306","user":"nobsnp2","ssl":"default","readonly":false}]);
        std::fs::write(conn_path(), serde_json::to_string_pretty(&profiles).unwrap()).unwrap();
        let raw = |sql: &str| {
            let c = json!({"host":"127.0.0.1","port":"3306","user":"nobsnp2","password":"","ssl":"default"});
            let mut conn = build_conn(&c).unwrap();
            conn.query_drop(sql).unwrap();
        };

        raw("DROP DATABASE IF EXISTS cmp_diff_rt"); raw("CREATE DATABASE cmp_diff_rt");
        raw("CREATE TABLE cmp_diff_rt.t (id INT PRIMARY KEY, qty INT CHECK (qty >= 0))");
        raw("INSERT INTO cmp_diff_rt.t VALUES (1, 10), (2, 20)");

        // Row 1's update is valid; row 2's violates the CHECK constraint and fails.
        let updates = json!([
            {"pk":[1], "colDiffs":[{"col":"qty","src":99}]},
            {"pk":[2], "colDiffs":[{"col":"qty","src":-1}]},
        ]);
        let r = compare_rows_apply_diff(json!({"targetConnName":"cmp_diff_rt","targetDb":"cmp_diff_rt",
                                                "table":"t","pkCols":["id"],"updates":updates})).await.unwrap();
        assert_eq!(r["ok"], false, "the batch should fail on the CHECK constraint: {r}");
        println!("  log: {:?}", r["log"]);

        let c = json!({"host":"127.0.0.1","port":"3306","user":"nobsnp2","password":"","ssl":"default"});
        let mut conn = build_conn(&c).unwrap();
        let (_c, rows) = run_select(&mut conn, "SELECT id, qty FROM cmp_diff_rt.t ORDER BY id").unwrap();
        let qty1 = rows[0][1].clone().unwrap_or_default();
        assert_eq!(qty1, "10", "row 1's update must have been rolled back along with row 2's failure, got qty={qty1}");

        raw("DROP DATABASE IF EXISTS cmp_diff_rt");
    }
}
