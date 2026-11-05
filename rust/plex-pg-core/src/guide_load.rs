//! A Live TV guide load in PostgreSQL: the shim half of cluster-plex's
//! `docs/superpowers/specs/2026-11-04-guide-loads-in-postgresql-design.md`
//! (owner, 2026-11-04).
//!
//! PMS builds every guide load in `<guide>-loading.db`: it copies its
//! bundled template there, opens it, migrates and fills it, closes it and
//! renames it onto the guide. Until now that handle was genuine SQLite, and
//! the `rename()` hook (`guide_swap`) had the manager copy the finished file
//! into the guide's schema (35 s for a cloud guide). With
//! `PLEX_PG_GUIDE_LOADS=1` the load is PostgreSQL's from its first
//! statement:
//!
//! - at the open of a load file the shim asks the manager over the guide
//!   socket to `stage` it. The manager makes the guide's staging schema
//!   (`epg_<uuid>_next`, where `epg_schema_for_path` sends a load's
//!   statements) and overwrites PMS's template copy with a shadow, and the
//!   handle connects as any guide does. Any other answer, or none, leaves
//!   the handle SQLite, which is the copy path as before;
//! - a load handle's reads are never answered from the thread's result
//!   cache: the loader looks a title up, inserts it, and looks it up again
//!   within the cache's second;
//! - a write on a load handle inside PMS's transaction runs in a savepoint
//!   of its own, so a statement PostgreSQL refuses leaves the transaction
//!   usable, as SQLite leaves its own (`PLEX_PG_GUIDE_LOAD_SAVEPOINTS=0`
//!   turns that off once the rig shows the loader never continues past an
//!   error).
//!
//! With `PLEX_PG_GUIDE_LOADS` unset or `0` none of this runs and a load file
//! never redirects, as before.

use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use crate::db_interpose_conn_utils::{log_error, log_info};
use crate::db_interpose_helpers::{cstr_to_str_or_empty, is_guide_load_path};
use crate::env_utils;
use crate::ffi_types::PgConnection;
use crate::libpq_helpers::{
    rust_pq_clear, rust_pq_exec, rust_pq_result_error_message, rust_pq_result_status, PGconn,
    PGRES_TUPLES_OK,
};

/// Routes a guide load into PostgreSQL when set to anything but `0`.
pub const GUIDE_LOADS_ENV: &str = "PLEX_PG_GUIDE_LOADS";

/// The environment variable naming the manager's guide socket, set by
/// cluster-plex on the PMS it starts; `stage` and `swap` requests both go
/// there.
pub const GUIDE_SOCKET_ENV: &str = "PLEX_PG_GUIDE_SOCKET";

/// Wraps each write of a load handle's transaction in a savepoint when set
/// to anything but `0`. Off by default since `.51`: PMS's loader never
/// continues past a refused statement (it ends the load with "Exception
/// inside transaction"), so the savepoint keeps nothing usable, and its
/// two statements per write were 168,000 of a 100-channel load's (the rig,
/// 2026-11-04).
pub const SAVEPOINTS_ENV: &str = "PLEX_PG_GUIDE_LOAD_SAVEPOINTS";

/// How long the open waits for the manager to stage a load: longer than the
/// manager's own deadline on the request (45 s), so a slow clone ends in the
/// manager's reason rather than here, and the manager never touches the file
/// after the shim stopped waiting. A pre-cloned staging schema answers in
/// milliseconds; a clone takes about 3 s.
pub const STAGE_DEADLINE: Duration = Duration::from_secs(60);

/// The savepoint a load handle's write runs in. Distinct from the names
/// Plex's own SAVEPOINTs carry through `pg_client::txn`.
pub const STATEMENT_SAVEPOINT: &str = "plex_pg_stmt";

pub(crate) const SAVEPOINT_SQL: &str = "SAVEPOINT plex_pg_stmt";
pub(crate) const RELEASE_SQL: &str = "RELEASE SAVEPOINT plex_pg_stmt";
pub(crate) const ROLLBACK_TO_SQL: &str = "ROLLBACK TO SAVEPOINT plex_pg_stmt";

const PGRES_COMMAND_OK: i32 = 1;

static ENABLED: AtomicI32 = AtomicI32::new(-1);
static SAVEPOINTS: AtomicI32 = AtomicI32::new(-1);

/// A flag read once: unset or empty is `default`, `0` is off, anything
/// else is on.
fn flag(cell: &AtomicI32, name: &str, default: bool) -> bool {
    let cached = cell.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let on = match env_utils::env_string(name) {
        Some(v) if !v.trim().is_empty() => v.trim() != "0",
        _ => default,
    };
    cell.store(if on { 1 } else { 0 }, Ordering::Relaxed);
    on
}

/// Whether guide loads are routed into PostgreSQL (`PLEX_PG_GUIDE_LOADS`).
pub(crate) fn enabled() -> bool {
    flag(&ENABLED, GUIDE_LOADS_ENV, false)
}

/// Whether a load handle's writes run in savepoints
/// (`PLEX_PG_GUIDE_LOAD_SAVEPOINTS`, on unless `0`).
pub(crate) fn savepoints_enabled() -> bool {
    flag(&SAVEPOINTS, SAVEPOINTS_ENV, false)
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    ENABLED.store(-1, Ordering::Relaxed);
    SAVEPOINTS.store(-1, Ordering::Relaxed);
}

/// The path of the guide load file `filename` names, if it is one.
///
/// # Safety
/// `filename` is NUL-terminated or null, as `sqlite3_open_v2` takes it.
pub(crate) unsafe fn load_file_path(filename: *const c_char) -> Option<String> {
    if filename.is_null() {
        return None;
    }
    let path = cstr_to_str_or_empty(filename);
    if is_guide_load_path(path) {
        Some(path.to_owned())
    } else {
        None
    }
}

/// Whether the manager's answer to a `stage` request says the load is
/// staged: `ok`, with or without a note. Anything else is the reason the
/// load stays SQLite.
pub(crate) fn staged(answer: Result<String, String>) -> Result<String, String> {
    match answer {
        Ok(line) if line == "ok" || line.starts_with("ok ") => Ok(line),
        Ok(line) => Err(format!("the manager answered {line:?}")),
        Err(err) => Err(err),
    }
}

/// Asks the manager to stage the load at `path` before PMS opens it, and
/// reports whether it did: true means the staging schema exists and the
/// file is the manager's shadow, so the handle is to connect as a guide.
/// False leaves the open to SQLite, as before guide loads were routed, with
/// the reason logged at ERROR.
pub(crate) fn stage_before_open(path: &str) -> bool {
    if !enabled() {
        return false;
    }
    let Some(socket) = env_utils::env_string(GUIDE_SOCKET_ENV) else {
        log_error(&format!(
            "GUIDE_LOAD: no {} to stage {}; loading into SQLite",
            GUIDE_SOCKET_ENV, path
        ));
        return false;
    };
    let start = std::time::Instant::now();
    match staged(stage_request(&socket, path, STAGE_DEADLINE)) {
        Ok(answer) => {
            log_info(&format!(
                "GUIDE_LOAD: the manager staged {} ({:?}) in {:.1?}; loading into PostgreSQL",
                path,
                answer,
                start.elapsed()
            ));
            true
        }
        Err(err) => {
            log_error(&format!(
                "GUIDE_LOAD: {} for {} after {:.1?}; loading into SQLite, copied at the rename",
                err,
                path,
                start.elapsed()
            ));
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn stage_request(socket: &str, path: &str, deadline: Duration) -> Result<String, String> {
    crate::guide_swap::stage_request(socket, path, deadline)
}

#[cfg(not(target_os = "linux"))]
fn stage_request(_socket: &str, _path: &str, _deadline: Duration) -> Result<String, String> {
    Err("guide loads are staged on Linux only".to_string())
}

/// The thread's result-cache key for a statement on the database at `path`:
/// the statement's own key for the library, qualified by the schema for a
/// Live TV guide (the same statement has two answers), and no key at all --
/// nothing looked up, nothing stored -- for a guide load. The loader looks a
/// title up, inserts it and looks it up again within the cache's second; an
/// answer from the cache would make a duplicate show.
pub(crate) fn result_cache_key(key: u64, path: &str) -> u64 {
    if key == 0 || is_guide_load_path(path) {
        return 0;
    }
    match crate::db_interpose_helpers::epg_schema_for_path(path) {
        Some(schema) => crate::pg_query_cache::rust_query_cache_key_in_schema(key, &schema),
        None => key,
    }
}

/// Whether a write on the database at `path` runs in a savepoint of its
/// own: a guide load's, inside a transaction, with savepoints on.
pub(crate) fn wraps_in_savepoint(path: &str, in_transaction: bool, savepoints: bool) -> bool {
    savepoints && in_transaction && is_guide_load_path(path)
}

/// Whether the write about to run on `conn` runs in a savepoint.
///
/// # Safety
/// `conn` is null or a live `PgConnection` the caller holds.
pub(crate) unsafe fn savepoint_wraps(conn: *mut PgConnection) -> bool {
    if conn.is_null() {
        return false;
    }
    let path = cstr_to_str_or_empty((*conn).db_path.as_ptr());
    if !is_guide_load_path(path) {
        return false;
    }
    wraps_in_savepoint(
        path,
        crate::pg_client::txn::in_transaction(conn),
        savepoints_enabled(),
    )
}

fn exec_ok(pg: *mut PGconn, sql: &str) -> bool {
    let Ok(cs) = CString::new(sql) else {
        return false;
    };
    let res = rust_pq_exec(pg, cs.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_COMMAND_OK;
    if !ok {
        let msg = if res.is_null() {
            "PostgreSQL connection lost".to_string()
        } else {
            let p = rust_pq_result_error_message(res);
            if p.is_null() {
                "PostgreSQL error".to_string()
            } else {
                unsafe { std::ffi::CStr::from_ptr(p) }
                    .to_string_lossy()
                    .trim_end()
                    .to_string()
            }
        };
        log_error(&format!("GUIDE_LOAD: {sql} failed: {msg}"));
    }
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}

/// Begins the statement's savepoint on `pg`, under the connection's mutex.
/// False when PostgreSQL refused it (logged): the statement then runs
/// unwrapped, as it would have.
pub(crate) fn begin_statement_savepoint(pg: *mut PGconn) -> bool {
    exec_ok(pg, SAVEPOINT_SQL)
}

/// Ends the statement's savepoint: released after a statement that ran,
/// rolled back to and then released after one PostgreSQL refused, so the
/// transaction stays usable and the statement's own error is what PMS
/// reads.
pub(crate) fn end_statement_savepoint(pg: *mut PGconn, succeeded: bool) {
    if !succeeded {
        exec_ok(pg, ROLLBACK_TO_SQL);
    }
    exec_ok(pg, RELEASE_SQL);
}

/// Whether `conn` is a guide load's handle (its path is `<guide>-loading.db`).
///
/// # Safety
/// `conn` is null or a live `PgConnection` the caller holds.
pub(crate) unsafe fn is_load_handle(conn: *mut PgConnection) -> bool {
    if conn.is_null() {
        return false;
    }
    is_guide_load_path(cstr_to_str_or_empty((*conn).db_path.as_ptr()))
}

/// The table whose `id` the statement assigns itself -- an UPDATE whose SET
/// list names `id`, an INSERT or REPLACE whose column list names it -- or
/// None. SQLite gives the next INSERT max(rowid)+1 whatever a row's id was
/// set to; a PostgreSQL sequence knows nothing of it, so the shim raises the
/// table's sequence after such a statement on a load (`resync_sequence`).
/// PMS's loader renumbers each EPG section it makes -- `update
/// library_sections set id=2 where id=1` -- and the next section's insert
/// took 2 from the sequence again (the rig, 2026-11-04).
pub(crate) fn id_assignment(sql: &str) -> Option<String> {
    let lower = sql.trim_start().to_ascii_lowercase();
    let first = lower.split_whitespace().next()?;
    match first {
        "update" => {
            let rest = lower["update".len()..].trim_start();
            let (table, after) = identifier(rest)?;
            let after = after.trim_start().strip_prefix("set")?;
            if !after.starts_with(char::is_whitespace) {
                return None;
            }
            let set_list = match after.find(" where ") {
                Some(i) => &after[..i],
                None => after,
            };
            split_top_level(set_list)
                .iter()
                .any(|a| unquote(a.split('=').next().unwrap_or("").trim()) == "id")
                .then_some(table)
        }
        "insert" | "replace" => {
            let mut rest = lower[first.len()..].trim_start();
            if let Some(r) = rest.strip_prefix("or ") {
                // insert or replace|ignore|abort|fail|rollback into
                rest = r.trim_start();
                rest = rest[rest.find(char::is_whitespace)?..].trim_start();
            }
            if let Some(r) = rest.strip_prefix("into ") {
                rest = r.trim_start();
            }
            let (table, after) = identifier(rest)?;
            let cols = after.trim_start().strip_prefix('(')?;
            let end = cols.find(')')?;
            cols[..end]
                .split(',')
                .any(|c| unquote(c.trim()) == "id")
                .then_some(table)
        }
        _ => None,
    }
}

/// Reads one table identifier (bare, backtick- or double-quoted) off the
/// start of `s`: its name, lowercased and unquoted, and what follows. A name
/// that would not make a safe identifier is None.
fn identifier(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let (raw, rest) = match s.chars().next()? {
        q @ ('`' | '"') => {
            let inner = &s[1..];
            let end = inner.find(q)?;
            (&inner[..end], &inner[end + 1..])
        }
        _ => {
            let end = s
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
                .unwrap_or(s.len());
            (&s[..end], &s[end..])
        }
    };
    let name = raw.rsplit('.').next()?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((name.to_string(), rest))
}

fn unquote(s: &str) -> &str {
    s.trim_matches(|c| c == '`' || c == '"' || c == '[' || c == ']')
}

/// Splits on the commas outside quotes and parentheses.
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut start) = (0usize, None::<char>, 0usize);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// The statement that raises `table`'s id sequence to its ids: setval to
/// the greater of max(id) and the sequence's own last value (never lowered
/// past another session's take), is_called unless both are nothing, so the
/// next nextval is max(id)+1 as SQLite's next rowid would be. The sequence
/// is the one the column's default names, which the manager's clone makes
/// without OWNED BY. `table` is a safe identifier (`identifier`).
pub(crate) fn resync_sequence_sql(table: &str) -> String {
    format!(
        concat!(
            "SELECT setval(s.seq, GREATEST(m.mx, COALESCE(pg_sequence_last_value(s.seq), 0), 1), ",
            "m.mx > 0 OR pg_sequence_last_value(s.seq) IS NOT NULL) ",
            "FROM (SELECT (regexp_match(pg_get_expr(d.adbin, d.adrelid), ",
            "'^nextval\\(''(.*)''::regclass\\)$'))[1]::regclass AS seq ",
            "FROM pg_attrdef d JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum ",
            "WHERE d.adrelid = to_regclass('\"{t}\"') AND a.attname = 'id') s, ",
            "LATERAL (SELECT COALESCE(max(id), 0) AS mx FROM \"{t}\") m"
        ),
        t = table
    )
}

/// The resync's own savepoint: inside PMS's transaction a refused resync
/// would otherwise abort the transaction (and PMS the load), which the
/// statement savepoints (off by default since `.51`) no longer prevent.
const RESYNC_SAVEPOINT: &str = "plex_pg_resync";
const RESYNC_SAVEPOINT_SQL: &str = "SAVEPOINT plex_pg_resync";
const RESYNC_RELEASE_SQL: &str = "RELEASE SAVEPOINT plex_pg_resync";
const RESYNC_ROLLBACK_SQL: &str = "ROLLBACK TO SAVEPOINT plex_pg_resync";

/// Raises `table`'s id sequence on `pg` after a load's statement assigned
/// an id itself (`id_assignment`), under the connection's mutex. Inside a
/// transaction it runs in a savepoint of its own, so a refusal (a table
/// without an `id` column, say) leaves PMS's transaction usable; the
/// refusal is logged and the load goes on, the next insert taking what the
/// sequence has, as before. A missing table is refused by the `max(id)`
/// read and absorbed the same way (`to_regclass` only keeps the sequence
/// lookup from refusing first); outside a transaction a refused statement
/// is its own, and aborts nothing. Proven on the rig's PostgreSQL,
/// 2026-11-05: schema_migrations and a missing table, each refused inside
/// the savepoint, left the transaction usable.
pub(crate) fn resync_sequence(pg: *mut PGconn, table: &str, in_transaction: bool) {
    let guarded = in_transaction && exec_ok(pg, RESYNC_SAVEPOINT_SQL);
    let ok = resync_sequence_once(pg, table);
    if guarded {
        if !ok {
            exec_ok(pg, RESYNC_ROLLBACK_SQL);
        }
        exec_ok(pg, RESYNC_RELEASE_SQL);
    }
}

/// One attempt at the resync statement; true when PostgreSQL answered it.
fn resync_sequence_once(pg: *mut PGconn, table: &str) -> bool {
    let sql = resync_sequence_sql(table);
    let Ok(cs) = CString::new(sql.as_str()) else {
        return false;
    };
    let res = rust_pq_exec(pg, cs.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_TUPLES_OK;
    if ok {
        log_info(&format!(
            "GUIDE_LOAD: {table}'s id sequence raised to its ids after an explicit id"
        ));
    } else {
        let msg = if res.is_null() {
            "PostgreSQL connection lost".to_string()
        } else {
            let p = rust_pq_result_error_message(res);
            if p.is_null() {
                "PostgreSQL error".to_string()
            } else {
                unsafe { std::ffi::CStr::from_ptr(p) }
                    .to_string_lossy()
                    .trim_end()
                    .to_string()
            }
        };
        log_error(&format!(
            "GUIDE_LOAD: raising {table}'s id sequence failed: {msg}"
        ));
    }
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str =
        "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases";
    const LOAD: &str =
        "tv.plex.providers.epg.cloud-0ab001b3-182f-4835-897c-43a9ed6797c5-loading.db";
    const GUIDE: &str = "tv.plex.providers.epg.cloud-0ab001b3-182f-4835-897c-43a9ed6797c5.db";

    fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _guard = crate::test_utils::env_lock().lock().unwrap();
        let prev: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        reset_for_tests();
        let out = f();
        for (k, v) in prev {
            match v {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
        reset_for_tests();
        out
    }

    #[test]
    fn guide_loads_are_off_unless_the_flag_turns_them_on() {
        with_env(&[(GUIDE_LOADS_ENV, None)], || assert!(!enabled()));
        with_env(&[(GUIDE_LOADS_ENV, Some(""))], || assert!(!enabled()));
        with_env(&[(GUIDE_LOADS_ENV, Some("0"))], || assert!(!enabled()));
        with_env(&[(GUIDE_LOADS_ENV, Some("1"))], || assert!(enabled()));
        with_env(&[(GUIDE_LOADS_ENV, Some("true"))], || assert!(enabled()));
    }

    #[test]
    fn savepoints_are_off_unless_the_flag_turns_them_on() {
        with_env(&[(SAVEPOINTS_ENV, None)], || assert!(!savepoints_enabled()));
        with_env(&[(SAVEPOINTS_ENV, Some("0"))], || {
            assert!(!savepoints_enabled())
        });
        with_env(&[(SAVEPOINTS_ENV, Some("1"))], || {
            assert!(savepoints_enabled())
        });
    }

    #[test]
    fn only_a_load_file_is_a_load() {
        let load = CString::new(format!("{DIR}/{LOAD}")).unwrap();
        let guide = CString::new(format!("{DIR}/{GUIDE}")).unwrap();
        let library = CString::new(format!("{DIR}/com.plexapp.plugins.library.db")).unwrap();
        unsafe {
            assert_eq!(
                load_file_path(load.as_ptr()).as_deref(),
                Some(format!("{DIR}/{LOAD}").as_str())
            );
            assert_eq!(load_file_path(guide.as_ptr()), None);
            assert_eq!(load_file_path(library.as_ptr()), None);
            assert_eq!(load_file_path(std::ptr::null()), None);
        }
    }

    #[test]
    fn only_ok_stages_a_load() {
        assert_eq!(staged(Ok("ok".to_string())).unwrap(), "ok");
        assert_eq!(
            staged(Ok("ok a second handle".to_string())).unwrap(),
            "ok a second handle"
        );
        assert_eq!(
            staged(Ok("skip a stale schema".to_string())).unwrap_err(),
            "the manager answered \"skip a stale schema\""
        );
        assert_eq!(
            staged(Ok("fail not a guide load".to_string())).unwrap_err(),
            "the manager answered \"fail not a guide load\""
        );
        assert_eq!(
            staged(Ok("okay".to_string())).unwrap_err(),
            "the manager answered \"okay\""
        );
        assert_eq!(
            staged(Err("connect /x: refused".to_string())).unwrap_err(),
            "connect /x: refused"
        );
    }

    #[test]
    fn a_load_without_the_flag_or_the_socket_stays_sqlite() {
        let path = format!("{DIR}/{LOAD}");
        with_env(
            &[
                (GUIDE_LOADS_ENV, None),
                (GUIDE_SOCKET_ENV, Some("/nonexistent.sock")),
            ],
            || assert!(!stage_before_open(&path)),
        );
        with_env(
            &[(GUIDE_LOADS_ENV, Some("1")), (GUIDE_SOCKET_ENV, None)],
            || assert!(!stage_before_open(&path)),
        );
        with_env(
            &[
                (GUIDE_LOADS_ENV, Some("1")),
                (GUIDE_SOCKET_ENV, Some("/nonexistent/g.sock")),
            ],
            || assert!(!stage_before_open(&path)),
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_managers_ok_stages_the_load_and_anything_else_does_not() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        fn manager(name: &str, reply: &'static str) -> String {
            let dir = std::env::temp_dir()
                .join(format!("plex-pg-guide-load-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("g.sock").to_string_lossy().into_owned();
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).unwrap();
            std::thread::spawn(move || {
                let (mut c, _) = listener.accept().unwrap();
                let mut buf = [0u8; 512];
                let n = c.read(&mut buf).unwrap();
                assert!(String::from_utf8_lossy(&buf[..n]).starts_with("stage "));
                c.write_all(reply.as_bytes()).unwrap();
            });
            path
        }

        let path = format!("{DIR}/{LOAD}");
        let sock = manager("ok", "ok\n");
        with_env(
            &[
                (GUIDE_LOADS_ENV, Some("1")),
                (GUIDE_SOCKET_ENV, Some(&sock)),
            ],
            || assert!(stage_before_open(&path)),
        );
        let sock = manager("fail", "fail not a guide load\n");
        with_env(
            &[
                (GUIDE_LOADS_ENV, Some("1")),
                (GUIDE_SOCKET_ENV, Some(&sock)),
            ],
            || assert!(!stage_before_open(&path)),
        );
        let sock = manager("malformed", "copied\n");
        with_env(
            &[
                (GUIDE_LOADS_ENV, Some("1")),
                (GUIDE_SOCKET_ENV, Some(&sock)),
            ],
            || assert!(!stage_before_open(&path)),
        );
    }

    #[test]
    fn a_loads_reads_have_no_cache_key() {
        let key = 0x1234_5678_9abc_def0u64;
        assert_eq!(result_cache_key(key, &format!("{DIR}/{LOAD}")), 0);
        assert_eq!(result_cache_key(key, &format!("{DIR}/{LOAD}-wal")), 0);
        assert_eq!(
            result_cache_key(key, &format!("{DIR}/com.plexapp.plugins.library.db")),
            key
        );
        let guide_key = result_cache_key(key, &format!("{DIR}/{GUIDE}"));
        assert_ne!(guide_key, 0);
        assert_ne!(guide_key, key, "a guide's key is qualified by its schema");
        assert_eq!(result_cache_key(0, &format!("{DIR}/{GUIDE}")), 0);
    }

    #[test]
    fn only_a_loads_write_inside_a_transaction_runs_in_a_savepoint() {
        let load = format!("{DIR}/{LOAD}");
        assert!(wraps_in_savepoint(&load, true, true));
        assert!(
            !wraps_in_savepoint(&load, false, true),
            "autocommit: its own transaction"
        );
        assert!(!wraps_in_savepoint(&load, true, false), "turned off");
        assert!(!wraps_in_savepoint(&format!("{DIR}/{GUIDE}"), true, true));
        assert!(!wraps_in_savepoint(
            &format!("{DIR}/com.plexapp.plugins.library.db"),
            true,
            true
        ));
    }

    #[test]
    fn savepoint_wraps_reads_the_connections_path_and_transaction() {
        let conn =
            unsafe { libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection };
        let path = format!("{DIR}/{LOAD}");
        unsafe {
            for (i, b) in path.as_bytes().iter().enumerate() {
                (*conn).db_path[i] = *b as c_char;
            }
            (*conn).is_pg_active = 1;
        }
        with_env(&[(SAVEPOINTS_ENV, None)], || unsafe {
            assert!(!savepoint_wraps(conn), "no transaction open");
            assert!(!savepoint_wraps(std::ptr::null_mut()));
        });
        unsafe { libc::free(conn as *mut libc::c_void) };
    }

    #[test]
    fn the_savepoint_statements_name_the_shims_own_savepoint() {
        assert_eq!(SAVEPOINT_SQL, format!("SAVEPOINT {STATEMENT_SAVEPOINT}"));
        assert_eq!(
            RELEASE_SQL,
            format!("RELEASE SAVEPOINT {STATEMENT_SAVEPOINT}")
        );
        assert_eq!(
            ROLLBACK_TO_SQL,
            format!("ROLLBACK TO SAVEPOINT {STATEMENT_SAVEPOINT}")
        );
        // A refused connection is a refused savepoint, never a crash.
        assert!(!begin_statement_savepoint(std::ptr::null_mut()));
        end_statement_savepoint(std::ptr::null_mut(), false);
    }

    #[test]
    fn a_statement_that_assigns_an_id_names_its_table() {
        assert_eq!(
            id_assignment("update library_sections set id=? where id=?").as_deref(),
            Some("library_sections")
        );
        assert_eq!(
            id_assignment(
                "UPDATE `library_sections` SET content_changed_at=?, `id` = ? WHERE id=?"
            )
            .as_deref(),
            Some("library_sections")
        );
        assert_eq!(
            id_assignment("update library_sections set content_changed_at=? where id=?"),
            None,
            "the WHERE's id is no assignment"
        );
        assert_eq!(
            id_assignment("update t set id_x=?, title='a,id=' where id=?"),
            None
        );
        assert_eq!(
            id_assignment("INSERT INTO metadata_items (`id`,`title`) VALUES (?,?)").as_deref(),
            Some("metadata_items")
        );
        assert_eq!(
            id_assignment("insert or replace into tags (tag_type, id, tag) values (?,?,?)")
                .as_deref(),
            Some("tags")
        );
        assert_eq!(
            id_assignment("REPLACE INTO \"media_items\" (id) VALUES (?)").as_deref(),
            Some("media_items")
        );
        assert_eq!(
            id_assignment("INSERT INTO library_sections (`library_id`,`name`) VALUES (?,?)"),
            None
        );
        assert_eq!(
            id_assignment("INSERT INTO t VALUES (?, ?)"),
            None,
            "no column list"
        );
        assert_eq!(id_assignment("select id from t"), None);
        assert_eq!(
            id_assignment("update \"bad name\" set id=?"),
            None,
            "no safe identifier"
        );
        assert_eq!(id_assignment(""), None);
    }

    #[test]
    fn the_resync_statement_raises_the_tables_own_sequence() {
        let sql = resync_sequence_sql("library_sections");
        assert!(sql.starts_with("SELECT setval(s.seq, GREATEST(m.mx, "));
        assert!(
            sql.contains("d.adrelid = to_regclass('\"library_sections\"')"),
            "a missing table is an empty answer, never an error"
        );
        assert!(sql.contains("FROM \"library_sections\") m"));
        assert!(sql.contains("'^nextval\\(''(.*)''::regclass\\)$'"));
        assert!(sql.contains("m.mx > 0 OR pg_sequence_last_value(s.seq) IS NOT NULL"));
        // A refused connection is logged, never a crash, in and out of a
        // transaction.
        resync_sequence(std::ptr::null_mut(), "library_sections", false);
        resync_sequence(std::ptr::null_mut(), "library_sections", true);
        assert!(!resync_sequence_once(
            std::ptr::null_mut(),
            "library_sections"
        ));
    }

    #[test]
    fn the_resync_runs_in_a_savepoint_of_its_own() {
        assert_eq!(
            RESYNC_SAVEPOINT_SQL,
            format!("SAVEPOINT {RESYNC_SAVEPOINT}")
        );
        assert_eq!(
            RESYNC_RELEASE_SQL,
            format!("RELEASE SAVEPOINT {RESYNC_SAVEPOINT}")
        );
        assert_eq!(
            RESYNC_ROLLBACK_SQL,
            format!("ROLLBACK TO SAVEPOINT {RESYNC_SAVEPOINT}")
        );
        assert_ne!(
            RESYNC_SAVEPOINT, STATEMENT_SAVEPOINT,
            "never the statement savepoint's name"
        );
    }

    #[test]
    fn a_load_handle_is_told_by_its_path() {
        let conn =
            unsafe { libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection };
        for (path, want) in [
            (format!("{DIR}/{LOAD}"), true),
            (format!("{DIR}/{GUIDE}"), false),
            (format!("{DIR}/com.plexapp.plugins.library.db"), false),
        ] {
            unsafe {
                for b in (*conn).db_path.iter_mut() {
                    *b = 0;
                }
                for (i, b) in path.as_bytes().iter().enumerate() {
                    (*conn).db_path[i] = *b as c_char;
                }
                assert_eq!(is_load_handle(conn), want, "{path}");
            }
        }
        assert!(!unsafe { is_load_handle(std::ptr::null_mut()) });
        unsafe { libc::free(conn as *mut libc::c_void) };
    }
}
