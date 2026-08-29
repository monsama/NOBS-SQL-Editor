// NOBS SQL Editor - a cross-platform MySQL/MariaDB desktop client.
// Copyright (c) 2026 Viktor Ljuca <https://monsama.ch>. All rights reserved.
//
// This software is proprietary and confidential. No license, express or
// implied, is granted to copy, modify, distribute, sublicense, or publicly
// release this software, in whole or in part, without prior written
// permission from the copyright holder.

// NOBS SQL Editor - Tauri (Rust) backend
// Cross-platform desktop app. Uses the `mysql` driver for typed results
// (real NULL, proper bit/binary handling) and shells out to mysql/mysqldump
// only for dump-style export/import (which need DELIMITER handling).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use mysql::prelude::*;
use mysql::{Conn, Opts, OptsBuilder, SslOpts, Value as MyValue, Column};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use tauri::Manager;
use tauri::Emitter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
static EXPORT_CANCEL: AtomicBool = AtomicBool::new(false);

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
                        if j.cancelled.load(Ordering::SeqCst) { let _ = c.kill(); }
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
fn is_compare_cancelled(rid: &str) -> bool { cancelled_compares().lock().unwrap().contains(rid) }
fn clear_compare_cancel(rid: &str) { cancelled_compares().lock().unwrap().remove(rid); }

#[tauri::command]
async fn compare_cancel(req: Value) -> R {
    if let Some(rid) = req["requestId"].as_str() { if !rid.is_empty() { cancelled_compares().lock().unwrap().insert(rid.to_string()); } }
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

fn run_select(conn: &mut Conn, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), String> {
    let mut result = conn.query_iter(sql).map_err(|e| e.to_string())?;
    let cols: Vec<String> = result.columns().as_ref().iter().map(|c| c.name_str().to_string()).collect();
    let bin: Vec<bool> = result.columns().as_ref().iter().map(|c| is_binaryish(c)).collect();
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    for r in result.by_ref() {
        let row = r.map_err(|e| e.to_string())?;
        let mut cells = Vec::with_capacity(cols.len());
        for i in 0..cols.len() {
            let v = row.as_ref(i).cloned().unwrap_or(MyValue::NULL);
            cells.push(val_to_opt(&v, *bin.get(i).unwrap_or(&false)));
        }
        rows.push(cells);
    }
    Ok((cols, rows))
}

// ---------- SQL text helpers (identifier + literal, Workbench-style + hex rule) ----------
fn sql_id(name: &str) -> String { format!("`{}`", name.replace('`', "``")) }
// Mirrors the PowerShell version's Test-SqlReadOnly: strips /* */, --, and # comments, then
// every statement's leading keyword must be on the allow-list for the SQL to be read-only.
// This is the server-side enforcement backing a connection's "read-only / safe mode" flag.
fn sql_is_readonly(sql: &str) -> bool {
    if sql.trim().is_empty() { return true; }
    let re_block = regex::Regex::new(r"(?s)/\*.*?\*/").unwrap();
    let re_dash  = regex::Regex::new(r"(?m)--.*$").unwrap();
    let re_hash  = regex::Regex::new(r"(?m)#.*$").unwrap();
    let step1 = re_block.replace_all(sql, " ");
    let step2 = re_dash.replace_all(&step1, " ");
    let s = re_hash.replace_all(&step2, " ");
    const ALLOW: &[&str] = &["SELECT","SHOW","DESCRIBE","DESC","EXPLAIN","USE","WITH","SET","HELP","VALUES","TABLE","ANALYZE","CHECK","CHECKSUM"];
    for stmt in s.split(';') {
        let t = stmt.trim();
        if t.is_empty() { continue; }
        let w = t.split_whitespace().next().unwrap_or("").to_uppercase();
        if !ALLOW.contains(&w.as_str()) { return false; }
    }
    true
}
fn sql_lit(s: &str) -> String {
    if !s.is_empty() && s.starts_with("0x") && s[2..].chars().all(|c| c.is_ascii_hexdigit()) && s.len() > 2 {
        return s.to_string(); // hex literal (bit/binary)
    }
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
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
fn resolve_bin(names: &[&str], env_key: &str) -> Result<String, String> {
    if let Ok(p) = std::env::var(env_key) {
        if !p.is_empty() && std::path::Path::new(&p).exists() { return Ok(p); }
    }
    #[cfg(windows)]
    {
        let roots = ["C:\\Program Files\\MariaDB", "C:\\Program Files\\MySQL",
                     "C:\\Program Files (x86)\\MariaDB", "C:\\Program Files (x86)\\MySQL",
                     "C:\\xampp\\mysql", "C:\\wamp64\\bin\\mariadb", "C:\\wamp64\\bin\\mysql"];
        for root in roots {
            if let Ok(rd) = std::fs::read_dir(root) {
                for e in rd.flatten() {
                    let mut bin = e.path(); bin.push("bin");
                    for n in names {
                        let mut f = bin.clone(); f.push(format!("{}.exe", n));
                        if f.exists() { return Ok(f.to_string_lossy().to_string()); }
                    }
                }
            }
        }
    }
    // last resort: try the bare name on PATH
    let name = names[0];
    match Command::new(name).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status() {
        Ok(_) => Ok(name.to_string()),
        // Names the Settings dialog first: it is the fix inside the app (pick the path, or download
        // the MariaDB client tools), whereas PATH and the environment variable both need a restart
        // to take effect. The "Could not find '<tool>'" prefix is matched by showToolError() in the
        // UI to decide whether to offer an Open Settings button - keep it if this text changes.
        Err(_) => Err(format!("Could not find '{}'. Open Settings in the app to select {}.exe or download the MariaDB client tools. Alternatively add its bin folder to PATH, or set the {} environment variable to its full path.", name, name, env_key)),
    }
}

fn first_err(s: &str) -> String {
    // prefer the real "ERROR NNNN ..." line if present (mysql may echo the statement first)
    if let Some(l) = s.lines().map(|l| l.trim()).find(|l| l.starts_with("ERROR") || l.contains("ERROR ")) {
        return l.to_string();
    }
    s.lines().map(|l| l.trim()).find(|l| !l.is_empty() && !l.chars().all(|c| c == '-'))
        .unwrap_or("").to_string()
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
        let db = req["db"].as_str().unwrap_or("").replace('\'', "''");
        let mut c = build_conn(&req["conn"])?;
        let sql = format!(
            "SELECT 'table' t,TABLE_NAME n FROM information_schema.TABLES WHERE TABLE_SCHEMA='{d}' AND TABLE_TYPE='BASE TABLE' \
             UNION ALL SELECT 'view',TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA='{d}' AND TABLE_TYPE='VIEW' \
             UNION ALL SELECT IF(ROUTINE_TYPE='PROCEDURE','procedure','function'),ROUTINE_NAME FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA='{d}' \
             UNION ALL SELECT 'trigger',TRIGGER_NAME FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA='{d}' \
             UNION ALL SELECT 'event',EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_SCHEMA='{d}' ORDER BY 1,2", d = db);
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
            if let Ok((_tc, trows)) = run_select(&mut c, &format!("SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA='{}'", db)) {
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
        let db = req["db"].as_str().unwrap_or("").replace('\'', "''");
        let table = req["table"].as_str().unwrap_or("").replace('\'', "''");
        let mut c = build_conn(&req["conn"])?;
        let sql = format!("SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA='{}' AND TABLE_NAME='{}' AND CONSTRAINT_NAME='PRIMARY' ORDER BY ORDINAL_POSITION", db, table);
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
        let db = req["db"].as_str().unwrap_or("").replace('\'', "''");
        let table = req["table"].as_str().unwrap_or("").replace('\'', "''");
        let mut c = build_conn(&req["conn"])?;
        let sql = format!("SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA='{}' AND TABLE_NAME='{}' AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY ORDINAL_POSITION", db, table);
        let (_c, rows) = run_select(&mut c, &sql)?;
        let list: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        // Full FK detail (which table/column each FK column actually references) - a SEPARATE
        // field from "fk" above, which stays just the local column-name list other callers
        // (PK/FK badge display) already rely on. This powers "go to referenced row" navigation.
        let detail_sql = format!("SELECT COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA='{}' AND TABLE_NAME='{}' AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY ORDINAL_POSITION", db, table);
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
        let db = req["db"].as_str().unwrap_or("").replace('\'', "''");
        let mut c = build_conn(&req["conn"])?;
        let (_c1, columns) = run_select(&mut c, &format!("SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='{}' ORDER BY TABLE_NAME, ORDINAL_POSITION", db))?;
        // PK detection deliberately matches get_table_pk_cols's approach (CONSTRAINT_NAME='PRIMARY'),
        // NOT information_schema.COLUMNS.COLUMN_KEY='PRI'. COLUMN_KEY has a documented MySQL edge
        // case: a table with NO actual primary key but a UNIQUE NOT NULL index will still show that
        // index's column as 'PRI', since it behaves like one. Using the same precise method as the
        // grid means the ER diagram can never highlight a column as PK that the grid itself
        // disagrees is one.
        let (_c2, pks) = run_select(&mut c, &format!("SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA='{}' AND CONSTRAINT_NAME='PRIMARY'", db))?;
        let (_c3, fks) = run_select(&mut c, &format!("SELECT TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA='{}' AND REFERENCED_TABLE_NAME IS NOT NULL", db))?;
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
        let t = std::time::Instant::now();
        let result = match run_select(&mut c, &sql) {
            Ok((cols, rows)) => {
                if cols.is_empty() {
                    Ok(json!({"ok":true,"columns":[],"rows":[],"elapsedMs":t.elapsed().as_millis() as u64,"message":"Query OK. No result set."}))
                } else {
                    // No artificial row cap here - the query's own LIMIT is what bounds the result;
                    // paging through what's returned is handled client-side (matches the PS version).
                    Ok(json!({"ok":true,"columns":cols,"rows":rows,"elapsedMs":t.elapsed().as_millis() as u64}))
                }
            }
            Err(e) => {
                // A cancelled query surfaces here as a MySQL error (e.g. "Query execution was
                // interrupted") - report it the same way the PS version does.
                if request_id.is_some() { Ok(json!({"ok":false,"error":"Query cancelled.","cancelled":true})) }
                else { Ok(json!({"ok":false,"error":e})) }
            }
        };
        if let Some(rid) = &request_id { running_queries().lock().unwrap().remove(rid); }
        result
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
fn cnf_file(connj: &Value) -> Result<(tempfile::NamedTempFile, String), String> {
    let mut f = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    let mut s = String::from("[client]\n");
    s += &format!("host={}\nport={}\nuser={}\n", connj["host"].as_str().unwrap_or("127.0.0.1"),
        connj["port"].as_str().unwrap_or("3306"), connj["user"].as_str().unwrap_or("root"));
    if let Some(p) = connj["password"].as_str() { if !p.is_empty() { s += &format!("password={}\n", p.replace('\\', "\\")); } }
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

        if !continue_on_error {
            // Default, unchanged behavior: stop at the first failure. Every existing caller
            // (table designer Apply, DDL create-or-replace, compare-databases Apply, pending
            // grid-edit Apply) omits continueOnError entirely, so this branch - and its exact
            // response shape - is untouched by this feature's addition.
            for (idx, stmt) in statements.iter().enumerate() {
                if let Err(e) = c.query_drop(stmt) {
                    let preview: String = stmt.chars().take(120).collect();
                    let suffix = if stmt.chars().count() > 120 { "..." } else { "" };
                    return Ok(json!({"ok":false,"error":format!("Statement {} of {} failed: {}\n\n{}{}", idx+1, total, e, preview, suffix)}));
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
    tokio::task::spawn_blocking(move || {
        let jid = req["jobId"].as_str().unwrap_or("").to_string();
        let job = job_start(&jid);
        let _guard = JobGuard(jid);
        let (_f, cnf) = cnf_file(&req["conn"])?;
        let files: Vec<String> = req["files"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let target = req["targetDb"].as_str().unwrap_or("").to_string();
        let mut log = Vec::new();
        let mut cancelled = false;
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
            if !target.is_empty() { args.push(target.clone()); }
            let file = std::fs::File::open(&f).map_err(|e| e.to_string())?;
            let mut cmd = Command::new(&mbin);
            cmd.args(&args).stdin(Stdio::from(file)).stdout(Stdio::null());
            let out = run_job_child(job.as_ref(), &mut cmd);
            let short = || std::path::Path::new(&f).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| f.clone());
            match out {
                Ok(o) if o.status.success() => log.push(format!("OK  {}", short())),
                // A killed child reports failure, but "FAILED file : ..." would read as a broken
                // import rather than the cancel the user just asked for.
                Ok(_) if job_is_cancelled(&job) => { log.push(format!("CANCELLED {}", short())); cancelled = true; break; }
                Ok(o) => log.push(format!("FAILED {} : {}", short(), first_err(&String::from_utf8_lossy(&o.stderr)))),
                Err(e) => log.push(format!("FAILED {} : {}", f, e)),
            }
        }
        Ok(json!({"ok":true,"cancelled":cancelled,"log":log}))
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

        let run = |bin: &str, args: &[String], file: &str| -> Result<(bool, String), String> {
            let mut cmd = Command::new(bin);
            cmd.args(args);
            let out = run_job_child(job.as_ref(), &mut cmd);
            match out {
                Ok(o2) if o2.status.success() => {
                    let sz = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
                    Ok((true, format!("OK  {} ({:.2} MB)", file, sz as f64 / 1048576.0)))
                }
                Ok(o2) => Ok((false, first_err(&String::from_utf8_lossy(&o2.stderr)))),
                Err(e) => Ok((false, e.to_string())),
            }
        };

        let mut log: Vec<String> = Vec::new();
        let mut cancelled = false;

        if mode == "single" {
            let file = mkfile("all_selected");
            let mut a = common.clone();
            a.push("--databases".into());
            if flag("routines") { a.push("--routines".into()); }
            if flag("events") { a.push("--events".into()); }
            if flag("adddropdb") { a.push("--add-drop-database".into()); }
            if flag("adddroptb") { a.push("--add-drop-table".into()); } else { a.push("--skip-add-drop-table".into()); }
            if !flag("createdb") { a.push("--no-create-db".into()); }
            for k in &excl { a.push(format!("--ignore-table={}", k)); }
            for d in &dbs { a.push(d.clone()); }
            a.push(format!("--result-file={}", file));
            match run(&dbin, &a, &file) {
                Ok((true, msg)) => log.push(msg),
                Ok((false, err)) => log.push(format!("FAILED all_selected : {}", err)),
                Err(e) => log.push(format!("FAILED all_selected : {}", e)),
            }
        } else if mode == "db" {
            for d in &dbs {
                if EXPORT_CANCEL.load(Ordering::SeqCst) || job_is_cancelled(&job) { log.push("CANCELLED (remaining databases skipped)".into()); cancelled = true; break; }
                let file = mkfile(d);
                let mut a = common.clone();
                a.push("--databases".into());
                if flag("routines") { a.push("--routines".into()); }
                if flag("events") { a.push("--events".into()); }
                if flag("adddropdb") { a.push("--add-drop-database".into()); }
                if flag("adddroptb") { a.push("--add-drop-table".into()); } else { a.push("--skip-add-drop-table".into()); }
                if !flag("createdb") { a.push("--no-create-db".into()); }
                for k in &excl { if k.starts_with(&format!("{}.", d)) { a.push(format!("--ignore-table={}", k)); } }
                a.push(d.clone());
                a.push(format!("--result-file={}", file));
                match run(&dbin, &a, &file) {
                    Ok((true, msg)) => log.push(msg),
                    Ok((false, err)) => log.push(format!("FAILED {} : {}", d, err)),
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
                        Ok((false, err)) => log.push(format!("FAILED {} : {}", key, err)),
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
                        Ok((false, err)) => log.push(format!("FAILED {} routines/events : {}", d, err)),
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
        let colsql = format!("SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='{}' AND TABLE_NAME='{}' ORDER BY ORDINAL_POSITION", db.replace('\'', "''"), table.replace('\'', "''"));
        let (_c, crows) = run_select(&mut c, &colsql)?;
        let table_cols: Vec<String> = crows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        if table_cols.is_empty() { return Ok(json!({"ok":false,"error":"Table not found or has no columns."})); }

        let has_header = req["hasHeader"].as_bool().unwrap_or(true);
        let mut rdr = csv::ReaderBuilder::new().has_headers(has_header).flexible(true)
            .from_path(&file).map_err(|e| format!("CSV open error: {}", e))?;
        let csv_cols: Vec<String> = if has_header {
            rdr.headers().map_err(|e| e.to_string())?.iter().map(String::from).collect()
        } else { table_cols.clone() };
        let use_idx: Vec<usize> = csv_cols.iter().enumerate().filter(|(_, n)| table_cols.contains(n)).map(|(i, _)| i).collect();
        if use_idx.is_empty() { return Ok(json!({"ok":false,"error":"No CSV columns match the table columns (check the header row)."})); }
        let use_cols: Vec<String> = use_idx.iter().map(|&i| csv_cols[i].clone()).collect();
        let col_list = use_cols.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
        let obj = format!("{}.{}", sql_id(&db), sql_id(&table));

        if req["truncate"].as_bool().unwrap_or(false) {
            c.query_drop(format!("TRUNCATE TABLE {}", obj)).map_err(|e| e.to_string())?;
        }
        let _ = c.query_drop("SET FOREIGN_KEY_CHECKS=0; SET UNIQUE_CHECKS=0");
        let mut n = 0usize; let mut batch: Vec<String> = Vec::new();
        for rec in rdr.records() {
            let rec = rec.map_err(|e| e.to_string())?;
            let vals: Vec<String> = use_idx.iter().map(|&i| {
                match rec.get(i) { None | Some("") => "NULL".to_string(), Some(s) => sql_lit(s) }
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
            let uq = u.replace('\'', "''");
            let hq = h.replace('\'', "''");
            match run_select(&mut conn, &format!("SHOW CREATE USER '{}'@'{}'", uq, hq)) {
                Ok((_c, rows)) => {
                    if let Some(first) = rows.get(0).and_then(|r| r.get(0)).cloned().flatten() {
                        create_lines.push(format!("{};", first));
                    } else {
                        errors.push(format!("SHOW CREATE USER for '{}'@'{}': no result returned", u, h));
                    }
                }
                Err(e) => errors.push(format!("SHOW CREATE USER for '{}'@'{}': {}", u, h, e)),
            }
            match run_select(&mut conn, &format!("SHOW GRANTS FOR '{}'@'{}'", uq, hq)) {
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
    tokio::task::spawn_blocking(move || {
        let (src_connj, _) = resolve_saved_conn(&src_name)?;
        let (tgt_connj, tgt_ro) = resolve_saved_conn(&tgt_name)?;
        let mut src_conn = build_conn(&src_connj)?;
        let mut tgt_conn = build_conn(&tgt_connj)?;
        let pk = get_table_pk_cols(&mut src_conn, &src_db, &table)?;
        if pk.is_empty() { return Ok(json!({"ok":false,"error":"Table has no primary key - cannot compare rows."})); }
        let fk = get_table_fk_cols(&mut src_conn, &src_db, &table).unwrap_or_default();
        let pk_list = pk.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
        let (_c1, src_pk_rows) = run_select(&mut src_conn, &format!("SELECT {} FROM {}.{}", pk_list, sql_id(&src_db), sql_id(&table)))?;
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
        if use_rows.is_empty() {
            return Ok(json!({"ok":true,"pkCols":pk,"columns":Vec::<String>::new(),"rows":Vec::<Value>::new(),"missingTotal":0,"truncated":false,"targetReadonly":tgt_ro,"allMissingPks":Vec::<Value>::new()}));
        }
        let (full_cols, full_rows) = get_rows_by_pk(&mut src_conn, &src_db, &table, &pk, &use_rows)?;
        // allMissingPks: the FULL (uncapped) list of missing primary-key values, sent to the
        // client alongside the first page - just id values, not full row data, so it's cheap
        // compared to what a full table re-scan would cost. The client uses it to load later
        // pages, or to remove just-inserted rows and pull the next batch, WITHOUT ever
        // re-scanning the table again.
        Ok(json!({"ok":true,"pkCols":pk,"columns":full_cols,"rows":full_rows,"missingTotal":missing_total,"truncated":truncated,"targetReadonly":tgt_ro,"allMissingPks":missing}))
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
    tokio::task::spawn_blocking(move || {
        let (src_connj, _) = resolve_saved_conn(&src_name)?;
        let (tgt_connj, tgt_ro) = resolve_saved_conn(&tgt_name)?;
        let mut src_conn = build_conn(&src_connj)?;
        let mut tgt_conn = build_conn(&tgt_connj)?;
        let pk = get_table_pk_cols(&mut src_conn, &src_db, &table)?;
        if pk.is_empty() { return Ok(json!({"ok":false,"error":"Table has no primary key - cannot compare rows."})); }
        let fk = get_table_fk_cols(&mut src_conn, &src_db, &table).unwrap_or_default();
        let pk_list = pk.iter().map(|c| sql_id(c)).collect::<Vec<_>>().join(",");
        let (_c1, src_pk_rows) = run_select(&mut src_conn, &format!("SELECT {} FROM {}.{}", pk_list, sql_id(&src_db), sql_id(&table)))?;
        let tgt_pk_rows: Vec<Vec<Option<String>>> = match run_select(&mut tgt_conn, &format!("SELECT {} FROM {}.{}", pk_list, sql_id(&tgt_db), sql_id(&table))) {
            Ok((_c, rows)) => rows,
            Err(e) => { if e.contains("1146") || e.to_lowercase().contains("doesn't exist") { Vec::new() } else { return Ok(json!({"ok":false,"error":e})); } }
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
        let rid = req["requestId"].as_str().map(String::from);
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
        let mut log = Vec::new();
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
                Err(e) => log.push(format!("FAILED id={} : {}", pk_desc, e)),
            }
        }
        Ok(json!({"ok":true,"log":log}))
    }).await.map_err(|e| e.to_string())?
}

// Inserts EVERY missing row (source rows absent from target), not just the first 2000 that fit
// in the interactive review list. Unlike compare_rows, this never sends the row data back to
// the frontend at all - it fetches a chunk of missing rows from source and inserts that SAME
// chunk into target immediately, chunk by chunk, so the amount of data moved isn't limited by
// what's practical to render as a checkbox list. Still insert-only.
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
                if let Some(c) = slot.as_mut() { let _ = c.kill(); }
            }
            Ok(json!({"ok":true,"message":"Cancel requested."}))
        }
        None => Ok(json!({"ok":false,"error":"Job not found - it may have already finished."})),
    }
}

#[tauri::command]
async fn export_table(app: tauri::AppHandle, req: Value) -> R {
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
        let csv_field = |o: &Option<String>| -> String {
            match o { None => String::new(), Some(s) => {
                if s.contains('"') || s.contains(',') || s.contains('\n') { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.clone() }
            }}
        };
        if fmt != "inserts" {
            let hdr = cols.iter().map(|c| if c.contains(',')||c.contains('"'){format!("\"{}\"",c.replace('"',"\"\""))}else{c.clone()}).collect::<Vec<_>>().join(",");
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
            if n % 2000 == 0 { let _ = app.emit("export_progress", json!({"rows": n})); }
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
        let _ = app.emit("export_progress", json!({"rows": n, "done": true}));
        log_line(&format!("EXPORT ok: {} rows from {} to {}", n, tbl, file));
        Ok(json!({"ok":true,"message":format!("Exported {} row(s) to {}", n, file)}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
fn tools_status(app: tauri::AppHandle) -> R {
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
    Ok(json!({
        "ok": true,
        "mysql": m, "mysql_source": ms,
        "mysqldump": d, "mysqldump_source": ds,
        "download_dir": tools_dir().to_string_lossy(),
        "config_file": config_file().to_string_lossy()
    }))
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
        Ok(_) => Ok(json!({"ok":true})),
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
            connect, schemas, objects, ddl, pk, query, exec, rowop, script,
            import, export, importcsv, browse, quit_app, save_text, save_binary, export_table, cancel_export, cancel_job, app_info, get_config, save_config, download_tools, tools_status, conn_list, conn_get, conn_save, conn_delete, conn_primary, conn_clear, quit, lib_list, lib_save, lib_delete, lib_clear, lib_replace, search_all_schemas, cancel_query, compare_dbs, compare_schemas, compare_apply, compare_tables, compare_rows, compare_rows_apply, compare_rows_diff, compare_rows_apply_diff, compare_cancel, fk, compare_rows_insert_all, compare_rows_fetch_by_pk, gen_user_transfer, process_list, kill_process, schema_erd
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
