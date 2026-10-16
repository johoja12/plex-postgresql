use super::*;
use crate::log_debug_lazy;
use std::sync::atomic::Ordering;

pub(super) fn changes_impl(db: *mut sqlite3) -> c_int {
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    // Read, never acquired: these accessors run after the statement, on the
    // connection it used, and acquiring here blocked for the retry ladder
    // on a full pool and re-assigned the handle's slot on every call.
    let pg_conn = crate::pg_client::peek_connection(db);
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        if conn.is_pg_active != 0 {
            return conn.last_changes;
        }
    }
    // Not a handle we redirected -- Plex's in-memory statistics database is
    // opened through the interposer and left on SQLite -- so SQLite has the
    // answer and returning 0 would be a lie. See the note on
    // `last_insert_rowid_impl`.
    match crate::db_interpose_common::get_orig_sqlite3_changes() {
        Some(f) => unsafe { f(db) },
        None => 0,
    }
}

pub(super) fn changes64_impl(db: *mut sqlite3) -> i64 {
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = crate::pg_client::peek_connection(db);
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        if conn.is_pg_active != 0 {
            return conn.last_changes as i64;
        }
    }
    match crate::db_interpose_common::get_orig_sqlite3_changes64() {
        Some(f) => unsafe { f(db) },
        None => 0,
    }
}

pub(super) fn last_insert_rowid_impl(db: *mut sqlite3) -> i64 {
    if unsafe { *tls_in_interpose_call_ptr() } != 0 {
        log_debug("last_insert_rowid: RECURSION DETECTED, returning 0");
        return 0;
    }
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = crate::pg_client::peek_connection(db);
    if pg_conn.is_null() {
        // No PostgreSQL connection for this handle means the shim never took
        // it over. Plex opens its in-memory statistics database through the
        // interposer and it is deliberately left on SQLite (`redirect=0`), so
        // the row id it wants is SQLite's.
        //
        // Answering from the global PostgreSQL row id instead hands Plex an id
        // out of an unrelated database. Its insert-then-read-the-id loop never
        // agrees with itself and retries once a second for ever, which is
        // where startup used to stop: past migrations, serving 503, with this
        // line in the log once a second.
        if let Some(f) = crate::db_interpose_common::get_orig_sqlite3_last_insert_rowid() {
            let rowid = unsafe { f(db) };
            log_debug_lazy!(
                "last_insert_rowid: CALLED db={:p} not redirected, SQLite says {}",
                db,
                rowid
            );
            return rowid;
        }
        let global_rowid = crate::pg_client::rust_get_global_last_insert_rowid();
        log_debug_lazy!(
            "last_insert_rowid: CALLED db={:p} pg_conn=NULL, no original to ask (global={})",
            db,
            global_rowid
        );
        return if global_rowid > 0 { global_rowid } else { 0 };
    }

    log_debug_lazy!(
        "last_insert_rowid: CALLED db={:p} pg_conn={:p} (exact match)",
        db,
        pg_conn
    );

    let pg = unsafe { &*pg_conn };
    if pg.last_insert_failed.load(Ordering::Acquire) != 0 {
        // The last INSERT on this connection was refused: 0, not the row
        // before it (see PgConnection::last_insert_failed).
        log_debug("last_insert_rowid: last insert failed, RETURNING 0");
        return 0;
    }
    if pg.last_insert_rowid > 0 {
        let rowid = pg.last_insert_rowid;
        log_debug_lazy!("last_insert_rowid: using cached connection rowid={}", rowid);
        return rowid;
    }

    // Nothing has been inserted on this connection. The answer used to be
    // the process-global row id -- the last INSERT by any thread on any
    // handle -- and then `SELECT lastval()`, which is the session's last
    // sequence value whatever statement advanced it. Both hand a redirected
    // handle an id that is not its own; SQLite's answer for a handle that
    // has inserted nothing is 0.
    log_debug("last_insert_rowid: no insert on this connection, RETURNING 0");
    0
}

pub(super) fn errmsg_impl(db: *mut sqlite3) -> *const c_char {
    log_debug_lazy!("ERRMSG: db={:p}", db);
    unsafe {
        if *tls_in_interpose_call_ptr() != 0 {
            if let Some(f) = get_shim_sqlite3_errmsg() {
                return f(db);
            }
        }
    }

    let pg_conn = crate::pg_client::peek_connection(db);
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        if conn.last_error_code != SQLITE_OK && conn.last_error[0] != 0 {
            log_debug_lazy!(
                "ERRMSG: returning tracked error='{}'",
                cstr_to_string_or(conn.last_error.as_ptr(), "")
            );
            return conn.last_error.as_ptr();
        }
        log_debug("ERRMSG: returning 'not an error'");
        return NOT_AN_ERROR.as_ptr() as *const c_char;
    }

    if let Some(f) = get_shim_sqlite3_errmsg() {
        return unsafe { f(db) };
    }
    if let Some(f) = get_orig_sqlite3_errmsg() {
        return unsafe { f(db) };
    }
    b"unknown error\0".as_ptr() as *const c_char
}

pub(super) fn errcode_impl(db: *mut sqlite3) -> c_int {
    log_debug_lazy!("ERRCODE: db={:p}", db);
    unsafe {
        if *tls_in_interpose_call_ptr() != 0 {
            if let Some(f) = get_shim_sqlite3_errcode() {
                return f(db);
            }
        }
    }

    let pg_conn = crate::pg_client::peek_connection(db);
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        log_debug_lazy!(
            "ERRCODE: pg_conn found, returning code={}",
            conn.last_error_code
        );
        return conn.last_error_code;
    }

    if let Some(f) = get_shim_sqlite3_errcode() {
        return unsafe { f(db) };
    }
    if let Some(f) = get_orig_sqlite3_errcode() {
        return unsafe { f(db) };
    }
    SQLITE_ERROR
}

pub(super) fn extended_errcode_impl(db: *mut sqlite3) -> c_int {
    let pg_conn = crate::pg_client::peek_connection(db);
    if !pg_conn.is_null() {
        let conn = unsafe { &*pg_conn };
        return conn.last_error_code;
    }
    if let Some(f) = get_orig_sqlite3_extended_errcode() {
        return unsafe { f(db) };
    }
    SQLITE_ERROR
}

pub(super) fn get_table_impl(
    db: *mut sqlite3,
    sql: *const c_char,
    paz_result: *mut *mut *mut c_char,
    pn_row: *mut c_int,
    pn_column: *mut c_int,
    pz_err_msg: *mut *mut c_char,
) -> c_int {
    if sql.is_null() {
        return match get_orig_sqlite3_get_table() {
            Some(f) => unsafe { f(db, sql, paz_result, pn_row, pn_column, pz_err_msg) },
            None => SQLITE_ERROR,
        };
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    unsafe {
        if !pg_conn.is_null() {
            let pg = &mut *pg_conn;
            if pg.is_pg_active != 0
                && !pg.conn.is_null()
                && crate::pg_config::pg_config_is_read_operation(sql) != 0
            {
                let mut trans = sql_translate(sql);
                if trans.success != 0 && !trans.sql.is_null() {
                    let mut conn_guard = PthreadMutexGuard::lock(&mut pg.mutex as *mut _);
                    let res = crate::libpq_helpers::rust_pq_exec(pg.conn, trans.sql);
                    if crate::libpq_helpers::rust_pq_result_status(res) == PGRES_TUPLES_OK {
                        let mut result: *mut *mut c_char = std::ptr::null_mut();
                        let mut nrows = 0;
                        let mut ncols = 0;
                        if crate::db_interpose_helpers::rust_get_table_from_pgresult(
                            res as *const crate::db_interpose_helpers::PGresult,
                            &mut result,
                            &mut nrows,
                            &mut ncols,
                        ) != 0
                        {
                            if !paz_result.is_null() {
                                *paz_result = result;
                            }
                            if !pn_row.is_null() {
                                *pn_row = nrows;
                            }
                            if !pn_column.is_null() {
                                *pn_column = ncols;
                            }
                            if !pz_err_msg.is_null() {
                                *pz_err_msg = std::ptr::null_mut();
                            }
                            crate::libpq_helpers::rust_pq_clear(res);
                            conn_guard.unlock();
                            sql_translation_free(&mut trans as *mut SqlTranslation);
                            return SQLITE_OK;
                        }
                    }
                    crate::libpq_helpers::rust_pq_clear(res);
                    conn_guard.unlock();
                }
                sql_translation_free(&mut trans as *mut SqlTranslation);
            }
        } // if !pg_conn.is_null()
    }

    match get_orig_sqlite3_get_table() {
        Some(f) => unsafe { f(db, sql, paz_result, pn_row, pn_column, pz_err_msg) },
        None => SQLITE_ERROR,
    }
}
