use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

use crate::db_interpose_conn_utils::log_error;
use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::ffi_types::PgConnection;
use crate::libpq_helpers::{
    rust_pq_clear, rust_pq_exec, rust_pq_result_error_message, rust_pq_result_status,
    rust_pq_socket, rust_pq_transaction_status, PGconn,
};
use crate::log_debug_lazy;

const PG_SOCKET_TIMEOUT_SEC: i64 = 60;
pub(super) const CONNECTION_OK: i32 = 0;
const PGRES_COMMAND_OK: i32 = 1;
const PGRES_TUPLES_OK: i32 = 2;
pub(super) const PQTRANS_INTRANS: i32 = 2;
pub(super) const PQTRANS_INERROR: i32 = 3;

pub(super) fn pg_set_socket_timeout(pg_conn: *mut PGconn) {
    if pg_conn.is_null() {
        return;
    }
    let sock = rust_pq_socket(pg_conn);
    if sock < 0 {
        log_error("pg_set_socket_timeout: invalid socket");
        return;
    }

    let tv = libc::timeval {
        tv_sec: PG_SOCKET_TIMEOUT_SEC,
        tv_usec: 0,
    };
    unsafe {
        if libc::setsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        ) < 0
        {
            log_error("pg_set_socket_timeout: failed to set SO_RCVTIMEO");
        }
        if libc::setsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        ) < 0
        {
            log_error("pg_set_socket_timeout: failed to set SO_SNDTIMEO");
        }
    }

    log_debug_lazy!(
        "Socket timeout set to {} seconds for socket {}",
        PG_SOCKET_TIMEOUT_SEC,
        sock
    );
}

pub(super) fn exec_command(pg_conn: *mut PGconn, sql: &str) -> bool {
    if pg_conn.is_null() {
        return false;
    }
    let cs = match CString::new(sql) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let res = rust_pq_exec(pg_conn, cs.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_COMMAND_OK;
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}

pub(super) fn exec_tuples(pg_conn: *mut PGconn, sql: &str) -> bool {
    if pg_conn.is_null() {
        return false;
    }
    let cs = match CString::new(sql) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let res = rust_pq_exec(pg_conn, cs.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_TUPLES_OK;
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}

/// The statement timeout a library connection's session starts with. A
/// pooled library connection is reset to the library's five minutes before
/// each read (`prepared_send.rs`).
pub(super) const SESSION_STATEMENT_TIMEOUT_SQL: &str = "SET statement_timeout = '60s'";

/// The statement timeout a connection's session starts with: none for a Live
/// TV guide's own connection (or a load's), [`SESSION_STATEMENT_TIMEOUT_SQL`]
/// for every other.
///
/// A guide's reads skip the per-read reset since `.51`, so what is set here
/// is what every statement on a guide's connection runs under. Until `.56`
/// this set sixty seconds on a guide's connection too, and `.51` had read
/// `.46`'s per-read "no timeout" as set at connect: PMS's guide refresh
/// streams the whole guide it holds through one SELECT for about six minutes
/// on talos (248 channels), PostgreSQL cancelled it at sixty seconds
/// ("canceling statement due to statement timeout", 2026-11-09 22:51Z and
/// 2026-11-10 07:58Z), PMS's loader died with an uncaught exception, its
/// "Refreshing guide data" activity sat at 0% until the next restart, and no
/// refresh completed after `.53` reached talos.
pub(super) fn session_statement_timeout_sql(guide: bool) -> &'static str {
    if guide {
        crate::db_interpose_conn_utils::GUIDE_STATEMENT_TIMEOUT_SQL
    } else {
        SESSION_STATEMENT_TIMEOUT_SQL
    }
}

/// Sets a fresh or reset connection's session up: `schema` first on the
/// search path, and the statement timeout for a guide's connection (`guide`)
/// or a library's ([`session_statement_timeout_sql`]).
pub(super) fn apply_session_settings(
    pg_conn: *mut PGconn,
    schema: &str,
    deallocate_all: bool,
    guide: bool,
) {
    if pg_conn.is_null() {
        return;
    }

    let schema_cmd = format!("SET search_path TO {}, public", schema);
    let res = match CString::new(schema_cmd) {
        Ok(s) => rust_pq_exec(pg_conn, s.as_ptr()),
        Err(_) => std::ptr::null_mut(),
    };
    if res.is_null() || rust_pq_result_status(res) != PGRES_COMMAND_OK {
        let err = if res.is_null() {
            "<null result>".to_string()
        } else {
            unsafe {
                let msg = rust_pq_result_error_message(res);
                if msg.is_null() {
                    "<null>".to_string()
                } else {
                    CStr::from_ptr(msg).to_string_lossy().into_owned()
                }
            }
        };
        log_error(&format!("Failed to set search_path: {}", err));
    }
    if !res.is_null() {
        rust_pq_clear(res);
    }

    if deallocate_all {
        let _ = exec_command(pg_conn, "DEALLOCATE ALL");
    }

    if !exec_command(pg_conn, session_statement_timeout_sql(guide)) {
        log_error("Failed to set statement_timeout");
    }
}

pub(super) fn exec_simple(raw_conn: *mut c_void, sql: *const c_char) -> bool {
    let conn_ptr = raw_conn as *mut PgConnection;
    if conn_ptr.is_null() || sql.is_null() {
        return false;
    }
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &*conn_ptr };
    let s = unsafe { cstr_to_str_or_empty(sql) };
    let trimmed = s.trim_start();
    let lower = trimmed.to_ascii_lowercase();

    if lower.starts_with("commit") || lower.starts_with("rollback") || lower.starts_with("end") {
        let txn = if conn.conn.is_null() {
            0
        } else {
            rust_pq_transaction_status(conn.conn)
        };
        if txn != PQTRANS_INTRANS && txn != PQTRANS_INERROR {
            log_debug_lazy!(
                "exec_simple: skipped {} in non-transaction state={}",
                trimmed,
                txn
            );
            return true;
        }
    }

    let cmd = match CString::new(trimmed) {
        Ok(s) => s,
        Err(_) => return false,
    };
    if conn.conn.is_null() {
        return false;
    }
    let res = rust_pq_exec(conn.conn, cmd.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_COMMAND_OK;
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}
