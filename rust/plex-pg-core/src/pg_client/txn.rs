//! Plex's transactions, forwarded to PostgreSQL.
//!
//! Until 2026-10-16 BEGIN, COMMIT, ROLLBACK, SAVEPOINT and RELEASE were on
//! the skip list and answered SQLITE_DONE without reaching PostgreSQL, which
//! ran every statement in autocommit. A ROLLBACK after a failed write left
//! the writes before it committed -- half a library item, half a play queue.
//!
//! Pool slots are owned by one thread, and Plex runs a transaction on one
//! thread, so BEGIN can be forwarded on the thread's own slot provided every
//! statement until COMMIT uses that slot too. The slot is *pinned* to the
//! SQLite handle for the transaction's duration (`pinned_conn`), the step and
//! exec paths prefer the pin, and a read inside a transaction is fetched
//! eagerly rather than streamed, because a stream in flight and a second
//! statement cannot share a connection.
//!
//! `PLEX_PG_TRANSACTIONS=0` turns the forwarding off (back to no-ops) if it
//! misbehaves in the field.
//!
//! SQLite's dialect is mapped as follows. `BEGIN`, `BEGIN DEFERRED`,
//! `BEGIN IMMEDIATE` and `BEGIN EXCLUSIVE` are all `BEGIN`: PostgreSQL has no
//! reserved write lock to take, and MVCC does what IMMEDIATE was for. A
//! nested `BEGIN` -- an error in SQLite, which Plex never relies on -- is a
//! savepoint, so its COMMIT releases and its ROLLBACK rolls back to it. A
//! `SAVEPOINT` outside a transaction begins one, as SQLite does, and the
//! RELEASE of that outermost savepoint commits it. A bare ROLLBACK or COMMIT
//! ends the whole transaction, savepoints and all, as in SQLite.

use std::collections::HashMap;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::db_interpose_conn_utils::{log_error, log_info, PthreadMutexGuard};
use crate::ffi_types::{sqlite3, PgConnection};
use crate::libpq_helpers::{
    rust_pq_clear, rust_pq_exec, rust_pq_result_error_message, rust_pq_result_status,
};
use crate::log_info_lazy;
use crate::sync_utils::mutex_lock;

const PGRES_COMMAND_OK: c_int = 1;
const SQLITE_ERROR: c_int = 1;

/// A transaction-control statement, as SQLite spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TxnCmd {
    Begin,
    Commit,
    Rollback,
    Savepoint(String),
    Release(String),
    RollbackTo(String),
}

fn strip_leading_noise(sql: &str) -> &str {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        break;
    }
    &sql[i..]
}

/// A savepoint name as SQLite accepts it: an identifier, bare or quoted.
fn savepoint_name(token: &str) -> Option<String> {
    let t = token.trim().trim_end_matches(';').trim();
    let unquoted = if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
        || (t.starts_with('`') && t.ends_with('`') && t.len() >= 2)
    {
        &t[1..t.len() - 1]
    } else if t.starts_with('[') && t.ends_with(']') && t.len() >= 2 {
        &t[1..t.len() - 1]
    } else {
        t
    };
    if unquoted.is_empty()
        || !unquoted
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        return None;
    }
    Some(unquoted.to_string())
}

/// What transaction control `sql` is, or None for any other statement.
pub(crate) fn classify_sql(sql: &str) -> Option<TxnCmd> {
    let body = strip_leading_noise(sql).trim().trim_end_matches(';').trim();
    let words: Vec<&str> = body.split_whitespace().collect();
    let first = words.first()?.to_ascii_lowercase();
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    match first.as_str() {
        "begin" => {
            // BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION]
            let ok = lower[1..].iter().all(|w| {
                matches!(
                    w.as_str(),
                    "deferred" | "immediate" | "exclusive" | "transaction"
                )
            });
            ok.then_some(TxnCmd::Begin)
        }
        "commit" | "end" => {
            // COMMIT [TRANSACTION] / END [TRANSACTION]
            let ok = lower[1..].iter().all(|w| w == "transaction");
            ok.then_some(TxnCmd::Commit)
        }
        "rollback" => {
            // ROLLBACK [TRANSACTION] [TO [SAVEPOINT] name]
            let mut rest = &lower[1..];
            if rest.first().map(|w| w.as_str()) == Some("transaction") {
                rest = &rest[1..];
            }
            if rest.is_empty() {
                return Some(TxnCmd::Rollback);
            }
            if rest[0] != "to" {
                return None;
            }
            let mut idx = 2;
            if rest.get(1).map(|w| w.as_str()) == Some("savepoint") {
                idx = 3;
            }
            // Names keep their case: use the original tokens.
            let name_idx = words.len() - rest.len() + idx - 1;
            let name = savepoint_name(words.get(name_idx)?)?;
            (words.len() == name_idx + 1).then_some(TxnCmd::RollbackTo(name))
        }
        "savepoint" => {
            // SAVEPOINT name
            if words.len() != 2 {
                return None;
            }
            Some(TxnCmd::Savepoint(savepoint_name(words[1])?))
        }
        "release" => {
            // RELEASE [SAVEPOINT] name
            let name_idx = if lower.get(1).map(|w| w.as_str()) == Some("savepoint") {
                2
            } else {
                1
            };
            if words.len() != name_idx + 1 {
                return None;
            }
            Some(TxnCmd::Release(savepoint_name(words[name_idx])?))
        }
        _ => None,
    }
}

/// One level of the transaction Plex believes it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    /// The transaction itself; `explicit` is false when a SAVEPOINT began it.
    Outer { explicit: bool },
    /// A SAVEPOINT Plex named.
    Savepoint(String),
    /// A nested BEGIN, mapped to a savepoint of the shim's naming.
    NestedBegin(String),
}

/// The transaction state of one connection: a stack of frames, empty when
/// no transaction is open, and the SQLite handle it is pinned to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TxnState {
    pub(crate) frames: Vec<Frame>,
    pub(crate) db: usize,
}

impl TxnState {
    pub(crate) fn is_open(&self) -> bool {
        !self.frames.is_empty()
    }
}

/// What to send PostgreSQL for `cmd` in state `state`, and the state after.
///
/// Pure, so the mapping can be tested without a server. An empty list is a
/// no-op: COMMIT or ROLLBACK with nothing open (Plex's "END" after a
/// migration the shim never began), RELEASE of a savepoint it never saw.
pub(crate) fn plan(state: &TxnState, cmd: &TxnCmd) -> (Vec<String>, TxnState) {
    let mut next = state.clone();
    let mut sql = Vec::new();
    match cmd {
        TxnCmd::Begin => {
            if next.frames.is_empty() {
                sql.push("BEGIN".to_string());
                next.frames.push(Frame::Outer { explicit: true });
            } else {
                let name = format!("plexpg_nested_{}", next.frames.len());
                sql.push(format!("SAVEPOINT {name}"));
                next.frames.push(Frame::NestedBegin(name));
            }
        }
        TxnCmd::Commit => match next.frames.last() {
            None => {}
            Some(Frame::NestedBegin(name)) => {
                sql.push(format!("RELEASE SAVEPOINT {name}"));
                next.frames.pop();
            }
            Some(_) => {
                sql.push("COMMIT".to_string());
                next.frames.clear();
            }
        },
        TxnCmd::Rollback => match next.frames.last() {
            None => {}
            Some(Frame::NestedBegin(name)) => {
                sql.push(format!("ROLLBACK TO SAVEPOINT {name}"));
                sql.push(format!("RELEASE SAVEPOINT {name}"));
                next.frames.pop();
            }
            Some(_) => {
                sql.push("ROLLBACK".to_string());
                next.frames.clear();
            }
        },
        TxnCmd::Savepoint(name) => {
            if next.frames.is_empty() {
                sql.push("BEGIN".to_string());
                next.frames.push(Frame::Outer { explicit: false });
            }
            sql.push(format!("SAVEPOINT {name}"));
            next.frames.push(Frame::Savepoint(name.clone()));
        }
        TxnCmd::Release(name) => {
            if let Some(pos) = next
                .frames
                .iter()
                .rposition(|f| matches!(f, Frame::Savepoint(n) if n == name))
            {
                sql.push(format!("RELEASE SAVEPOINT {name}"));
                next.frames.truncate(pos);
                if next.frames == [Frame::Outer { explicit: false }] {
                    // The outermost savepoint of a transaction SAVEPOINT
                    // began: releasing it commits, as in SQLite.
                    sql.push("COMMIT".to_string());
                    next.frames.clear();
                }
            }
        }
        TxnCmd::RollbackTo(name) => {
            if let Some(pos) = next
                .frames
                .iter()
                .rposition(|f| matches!(f, Frame::Savepoint(n) if n == name))
            {
                sql.push(format!("ROLLBACK TO SAVEPOINT {name}"));
                // The savepoint itself survives a ROLLBACK TO, in both.
                next.frames.truncate(pos + 1);
            }
        }
    }
    (sql, next)
}

// ─── Process state ───────────────────────────────────────────────────────────

struct Registry {
    /// Connection (pointer) -> its transaction.
    by_conn: HashMap<usize, TxnState>,
    /// SQLite handle (pointer) -> the connection its transaction is on.
    pins: HashMap<usize, usize>,
}

static TXN: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        by_conn: HashMap::new(),
        pins: HashMap::new(),
    })
});

static ENABLED: AtomicI32 = AtomicI32::new(-1);

/// Whether transactions are forwarded: `PLEX_PG_TRANSACTIONS`, on unless
/// set to 0, false, no or off.
pub(crate) fn enabled() -> bool {
    let cached = ENABLED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let on = crate::env_utils::env_string("PLEX_PG_TRANSACTIONS")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !(v == "0" || v == "false" || v == "no" || v == "off")
        })
        .unwrap_or(true);
    ENABLED.store(if on { 1 } else { 0 }, Ordering::Relaxed);
    on
}

#[cfg(test)]
pub(crate) fn reset_enabled_for_tests() {
    ENABLED.store(-1, Ordering::Relaxed);
}

/// The connection `db`'s open transaction is on, or null. A pin whose
/// connection has since been closed is dropped.
pub(crate) fn pinned_conn(db: *const sqlite3) -> *mut PgConnection {
    if db.is_null() {
        return std::ptr::null_mut();
    }
    let mut reg = mutex_lock(&TXN);
    let Some(&conn) = reg.pins.get(&(db as usize)) else {
        return std::ptr::null_mut();
    };
    let open = reg
        .by_conn
        .get(&conn)
        .map(|s| s.is_open() && s.db == db as usize)
        .unwrap_or(false);
    let conn_ptr = conn as *mut PgConnection;
    // SAFETY: a pinned connection is one the pool still holds: pins are
    // dropped in `forget_connection` before a connection is closed.
    let live = open && !conn_ptr.is_null() && unsafe { &*conn_ptr }.is_pg_active != 0;
    if !live {
        reg.pins.remove(&(db as usize));
        return std::ptr::null_mut();
    }
    conn_ptr
}

/// Whether a transaction is open on `conn`.
pub(crate) fn in_transaction(conn: *const PgConnection) -> bool {
    if conn.is_null() {
        return false;
    }
    mutex_lock(&TXN)
        .by_conn
        .get(&(conn as usize))
        .map(|s| s.is_open())
        .unwrap_or(false)
}

/// Drop whatever transaction state `conn` had: it has been reset,
/// reconnected, rolled back by the pool or closed. Logged when a transaction
/// was open, because Plex's COMMIT of it will now be a no-op.
pub(crate) fn forget_connection(conn: *const PgConnection) {
    if conn.is_null() {
        return;
    }
    let mut reg = mutex_lock(&TXN);
    if let Some(state) = reg.by_conn.remove(&(conn as usize)) {
        if state.is_open() {
            log_info_lazy!(
                "TXN: transaction on conn {:p} (db {:#x}, {} level(s)) rolled back by the shim; the handle's COMMIT will be a no-op",
                conn,
                state.db,
                state.frames.len()
            );
        }
    }
    reg.pins.retain(|_, c| *c != conn as usize);
}

/// Drop the pin `db` holds, when the handle closes.
pub(crate) fn forget_db(db: usize) {
    let mut reg = mutex_lock(&TXN);
    if let Some(conn) = reg.pins.remove(&db) {
        if let Some(state) = reg.by_conn.get(&conn) {
            if state.is_open() && state.db == db {
                log_info_lazy!(
                    "TXN: handle {:#x} closed with a transaction open on conn {:#x}; the pool rolls it back on release",
                    db,
                    conn
                );
            }
        }
    }
}

/// Run a transaction-control statement for handle `db` on `conn`.
///
/// Returns Ok(()) when it is done (or was a no-op), Err(message) when
/// PostgreSQL refused one of the statements the command maps to; the message
/// is also left on the connection as `sqlite3_errmsg`. The caller decides
/// SQLITE_DONE versus SQLITE_ERROR.
pub(crate) fn run(conn: *mut PgConnection, db: *const sqlite3, cmd: &TxnCmd) -> Result<(), String> {
    if conn.is_null() {
        return Err("no PostgreSQL connection for transaction control".to_string());
    }
    // SAFETY: the caller holds a live pool connection.
    let c = unsafe { &mut *conn };
    if c.conn.is_null() {
        return Err("PostgreSQL connection is closed".to_string());
    }
    let current = mutex_lock(&TXN)
        .by_conn
        .get(&(conn as usize))
        .cloned()
        .unwrap_or_default();
    let (statements, mut next) = plan(&current, cmd);
    if !next.is_open() {
        next.db = 0;
    } else if next.db == 0 {
        next.db = db as usize;
    }

    let mut guard = unsafe { PthreadMutexGuard::lock(&mut c.mutex as *mut _) };
    for stmt in &statements {
        let stmt_c = match std::ffi::CString::new(stmt.as_str()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let res = rust_pq_exec(c.conn, stmt_c.as_ptr());
        let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_COMMAND_OK;
        if !ok {
            let msg = if res.is_null() {
                "PostgreSQL connection lost".to_string()
            } else {
                let p = rust_pq_result_error_message(res);
                if p.is_null() {
                    "PostgreSQL error".to_string()
                } else {
                    unsafe { std::ffi::CStr::from_ptr(p as *const c_char) }
                        .to_string_lossy()
                        .trim_end()
                        .to_string()
                }
            };
            rust_pq_clear(res);
            unsafe { guard.unlock() };
            log_error(&format!("TXN: {stmt} failed: {msg}"));
            c.set_last_error(SQLITE_ERROR, &msg);
            // What PostgreSQL thinks is what counts from here: a failed
            // BEGIN left nothing open, a failed COMMIT rolled it back.
            let mut reg = mutex_lock(&TXN);
            let txn_open = crate::libpq_helpers::rust_pq_transaction_status(c.conn);
            if txn_open != super::session::PQTRANS_INTRANS
                && txn_open != super::session::PQTRANS_INERROR
            {
                reg.by_conn.remove(&(conn as usize));
                reg.pins.retain(|_, pc| *pc != conn as usize);
            }
            return Err(msg);
        }
        rust_pq_clear(res);
    }
    unsafe { guard.unlock() };

    let mut reg = mutex_lock(&TXN);
    if next.is_open() {
        reg.pins.insert(db as usize, conn as usize);
        reg.by_conn.insert(conn as usize, next);
    } else {
        reg.by_conn.remove(&(conn as usize));
        reg.pins.retain(|_, pc| *pc != conn as usize);
    }
    if !statements.is_empty() {
        log_info(&format!(
            "TXN: {:?} on conn {:p} -> {}",
            cmd,
            conn,
            statements.join("; ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(frames: Vec<Frame>) -> TxnState {
        TxnState { frames, db: 1 }
    }

    #[test]
    fn sqlite_spellings_are_recognised() {
        assert_eq!(classify_sql("BEGIN"), Some(TxnCmd::Begin));
        assert_eq!(classify_sql("begin immediate;"), Some(TxnCmd::Begin));
        assert_eq!(
            classify_sql("BEGIN EXCLUSIVE TRANSACTION"),
            Some(TxnCmd::Begin)
        );
        assert_eq!(classify_sql("BEGIN DEFERRED"), Some(TxnCmd::Begin));
        assert_eq!(classify_sql("/* tx */ COMMIT"), Some(TxnCmd::Commit));
        assert_eq!(classify_sql("END TRANSACTION;"), Some(TxnCmd::Commit));
        assert_eq!(classify_sql("ROLLBACK"), Some(TxnCmd::Rollback));
        assert_eq!(classify_sql("rollback transaction"), Some(TxnCmd::Rollback));
        assert_eq!(
            classify_sql("SAVEPOINT sp1"),
            Some(TxnCmd::Savepoint("sp1".into()))
        );
        assert_eq!(
            classify_sql("RELEASE SAVEPOINT sp1"),
            Some(TxnCmd::Release("sp1".into()))
        );
        assert_eq!(
            classify_sql("RELEASE sp1;"),
            Some(TxnCmd::Release("sp1".into()))
        );
        assert_eq!(
            classify_sql("ROLLBACK TO SAVEPOINT sp1"),
            Some(TxnCmd::RollbackTo("sp1".into()))
        );
        assert_eq!(
            classify_sql("ROLLBACK TRANSACTION TO \"sp1\""),
            Some(TxnCmd::RollbackTo("sp1".into()))
        );
    }

    #[test]
    fn other_statements_are_not_transaction_control() {
        assert_eq!(classify_sql("SELECT 1"), None);
        assert_eq!(classify_sql("PRAGMA foreign_keys"), None);
        assert_eq!(classify_sql("VACUUM"), None);
        assert_eq!(classify_sql("BEGIN WORK NOW"), None);
        assert_eq!(classify_sql("SAVEPOINT a; DROP TABLE x"), None);
        assert_eq!(classify_sql("RELEASE"), None);
        assert_eq!(classify_sql(""), None);
    }

    #[test]
    fn begin_commit_and_rollback_map_to_themselves() {
        let (sql, st) = plan(&TxnState::default(), &TxnCmd::Begin);
        assert_eq!(sql, ["BEGIN"]);
        assert!(st.is_open());
        let (sql, st2) = plan(&st, &TxnCmd::Commit);
        assert_eq!(sql, ["COMMIT"]);
        assert!(!st2.is_open());
        let (sql, st3) = plan(&st, &TxnCmd::Rollback);
        assert_eq!(sql, ["ROLLBACK"]);
        assert!(!st3.is_open());
    }

    #[test]
    fn commit_or_rollback_with_nothing_open_is_a_no_op() {
        let (sql, st) = plan(&TxnState::default(), &TxnCmd::Commit);
        assert!(sql.is_empty());
        assert!(!st.is_open());
        let (sql, _) = plan(&TxnState::default(), &TxnCmd::Rollback);
        assert!(sql.is_empty());
    }

    #[test]
    fn a_nested_begin_is_a_savepoint_its_commit_releases_and_its_rollback_unwinds() {
        let st = s(vec![Frame::Outer { explicit: true }]);
        let (sql, nested) = plan(&st, &TxnCmd::Begin);
        assert_eq!(sql, ["SAVEPOINT plexpg_nested_1"]);
        assert_eq!(nested.frames.len(), 2);
        let (sql, after) = plan(&nested, &TxnCmd::Commit);
        assert_eq!(sql, ["RELEASE SAVEPOINT plexpg_nested_1"]);
        assert_eq!(after, st);
        let (sql, after) = plan(&nested, &TxnCmd::Rollback);
        assert_eq!(
            sql,
            [
                "ROLLBACK TO SAVEPOINT plexpg_nested_1",
                "RELEASE SAVEPOINT plexpg_nested_1"
            ]
        );
        assert_eq!(after, st);
        // The outer transaction is still open and commits as one.
        let (sql, done) = plan(&after, &TxnCmd::Commit);
        assert_eq!(sql, ["COMMIT"]);
        assert!(!done.is_open());
    }

    #[test]
    fn a_savepoint_outside_a_transaction_begins_one_and_releasing_it_commits() {
        let (sql, st) = plan(&TxnState::default(), &TxnCmd::Savepoint("a".into()));
        assert_eq!(sql, ["BEGIN", "SAVEPOINT a"]);
        let (sql, st2) = plan(&st, &TxnCmd::Savepoint("b".into()));
        assert_eq!(sql, ["SAVEPOINT b"]);
        let (sql, st3) = plan(&st2, &TxnCmd::Release("a".into()));
        assert_eq!(sql, ["RELEASE SAVEPOINT a", "COMMIT"]);
        assert!(!st3.is_open(), "releasing the outermost savepoint commits");
    }

    #[test]
    fn a_savepoint_inside_an_explicit_transaction_does_not_commit_on_release() {
        let st = s(vec![Frame::Outer { explicit: true }]);
        let (_, st) = plan(&st, &TxnCmd::Savepoint("a".into()));
        let (sql, st) = plan(&st, &TxnCmd::Release("a".into()));
        assert_eq!(sql, ["RELEASE SAVEPOINT a"]);
        assert!(st.is_open());
        assert_eq!(st.frames, [Frame::Outer { explicit: true }]);
    }

    #[test]
    fn rollback_to_keeps_the_savepoint_and_drops_what_came_after() {
        let (_, st) = plan(&TxnState::default(), &TxnCmd::Savepoint("a".into()));
        let (_, st) = plan(&st, &TxnCmd::Savepoint("b".into()));
        let (sql, st) = plan(&st, &TxnCmd::RollbackTo("a".into()));
        assert_eq!(sql, ["ROLLBACK TO SAVEPOINT a"]);
        assert_eq!(
            st.frames,
            [
                Frame::Outer { explicit: false },
                Frame::Savepoint("a".into())
            ]
        );
    }

    #[test]
    fn an_unknown_savepoint_is_a_no_op_rather_than_a_postgres_error() {
        let st = s(vec![Frame::Outer { explicit: true }]);
        let (sql, after) = plan(&st, &TxnCmd::Release("nope".into()));
        assert!(sql.is_empty());
        assert_eq!(after, st);
        let (sql, _) = plan(&st, &TxnCmd::RollbackTo("nope".into()));
        assert!(sql.is_empty());
    }

    #[test]
    fn a_bare_rollback_ends_everything_savepoints_included() {
        let (_, st) = plan(&TxnState::default(), &TxnCmd::Begin);
        let (_, st) = plan(&st, &TxnCmd::Savepoint("a".into()));
        let (sql, st) = plan(&st, &TxnCmd::Rollback);
        assert_eq!(sql, ["ROLLBACK"]);
        assert!(!st.is_open());
    }

    #[test]
    fn forwarding_is_on_unless_the_flag_turns_it_off() {
        let _guard = crate::test_utils::env_lock().lock().unwrap();
        let prev = std::env::var("PLEX_PG_TRANSACTIONS").ok();
        std::env::remove_var("PLEX_PG_TRANSACTIONS");
        reset_enabled_for_tests();
        assert!(enabled());
        std::env::set_var("PLEX_PG_TRANSACTIONS", "0");
        reset_enabled_for_tests();
        assert!(!enabled());
        std::env::set_var("PLEX_PG_TRANSACTIONS", "off");
        reset_enabled_for_tests();
        assert!(!enabled());
        std::env::set_var("PLEX_PG_TRANSACTIONS", "1");
        reset_enabled_for_tests();
        assert!(enabled());
        match prev {
            Some(v) => std::env::set_var("PLEX_PG_TRANSACTIONS", v),
            None => std::env::remove_var("PLEX_PG_TRANSACTIONS"),
        }
        reset_enabled_for_tests();
    }

    #[test]
    fn a_pin_is_dropped_once_its_connection_is_forgotten() {
        let conn =
            unsafe { libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection };
        unsafe { (*conn).is_pg_active = 1 };
        let db = 0x5150usize as *const sqlite3;
        {
            let mut reg = mutex_lock(&TXN);
            reg.pins.insert(db as usize, conn as usize);
            reg.by_conn.insert(
                conn as usize,
                TxnState {
                    frames: vec![Frame::Outer { explicit: true }],
                    db: db as usize,
                },
            );
        }
        assert_eq!(pinned_conn(db), conn);
        assert!(in_transaction(conn));
        forget_connection(conn);
        assert!(pinned_conn(db).is_null());
        assert!(!in_transaction(conn));
        unsafe { libc::free(conn as *mut libc::c_void) };
    }

    #[test]
    fn running_on_a_closed_connection_is_an_error_not_a_crash() {
        let conn =
            unsafe { libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection };
        let db = 0x5151usize as *const sqlite3;
        assert!(run(conn, db, &TxnCmd::Begin).is_err());
        assert!(run(std::ptr::null_mut(), db, &TxnCmd::Begin).is_err());
        unsafe { libc::free(conn as *mut libc::c_void) };
    }
}
