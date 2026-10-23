use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::{log_debug, log_info};
use crate::ffi_types::{sqlite3_stmt, PgStmt};

use super::ring_tracker::{
    clear_finalized_entry, clear_prepared_stmt, is_prepared_stmt, is_recently_finalized_stmt,
    log_clear_bindings_anomaly, remember_finalized_stmt, remember_prepared_stmt,
    skip_clear_bindings_on_finalized,
};
use super::*;
use crate::log_debug_lazy;

unsafe fn clear_dynamic_param_values(stmt: &mut PgStmt) {
    for i in 0..stmt.param_values.len() {
        if !stmt.param_values[i].is_null() && !stmt.is_preallocated_buffer(i) {
            libc::free(stmt.param_values[i] as *mut c_void);
            stmt.param_values[i] = ptr::null_mut();
        }
    }
}

/// `sqlite3_clear_bindings`: every parameter becomes NULL, an int or a double
/// in the preallocated buffer as much as a heap value. Reset keeps every
/// binding; a clear that kept them sent the previous
/// row's integers to a statement whose caller cleared and bound only some.
unsafe fn clear_all_param_values(stmt: &mut PgStmt) {
    clear_dynamic_param_values(stmt);
    for v in stmt.param_values.iter_mut() {
        *v = ptr::null_mut();
    }
    stmt.param_lengths.iter_mut().for_each(|l| *l = 0);
    stmt.param_formats.iter_mut().for_each(|f| *f = 0);
}

unsafe fn reset_pg_stmt_locked(p_stmt: *mut sqlite3_stmt, stmt: *mut PgStmt) -> c_int {
    let stmt_ref = &mut *stmt;
    let _guard = PgStmt::lock_mutex(stmt);
    stmt_ref.in_step.store(0, Ordering::Relaxed);

    // sqlite3_reset retains every binding, including heap-backed text/blob
    // values. Only clear_bindings, rebind or finalize may release them.
    pg_stmt_clear_result(stmt);

    // PG-routed non-cached stmts: skip orig_sqlite3_reset entirely.
    // This prevents LD_PRELOAD self-deadlock (SQLite internal re-entry).
    // PG state is already cleared above via pg_stmt_clear_result().
    if stmt_ref.is_pg != 0 && stmt_ref.is_cached == 0 {
        return SQLITE_OK;
    }
    // Legacy: cached PgRead stmts also skip
    if stmt_ref.is_pg == 2 {
        return SQLITE_OK;
    }
    orig_sqlite3_reset
        .map(|f| f(p_stmt))
        .unwrap_or(SQLITE_ERROR)
}

pub(super) fn note_stmt_prepare_impl(p_stmt: *mut sqlite3_stmt, sql: *const c_char) {
    unsafe {
        remember_prepared_stmt(p_stmt, sql);
        clear_finalized_entry(p_stmt);
    }
}

pub(super) fn reset_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    let pg_stmt = pg_find_any_stmt(p_stmt);
    if !pg_stmt.is_null() {
        return unsafe { reset_pg_stmt_locked(p_stmt, pg_stmt) };
    }

    let cached = pg_find_cached_stmt(p_stmt);
    if !cached.is_null() {
        return unsafe { reset_pg_stmt_locked(p_stmt, cached) };
    }

    unsafe {
        orig_sqlite3_reset
            .map(|f| f(p_stmt))
            .unwrap_or(SQLITE_ERROR)
    }
}

/// Whether `p_stmt` is a live statement of the connection the shim is running
/// real SQLite on in this thread (`ConnScope`).
///
/// SQLite prepares statements of its own inside exec, prepare and step, so the
/// shim never notes them and a recently finalized entry for their address
/// survives; SQLite then finalizes them through the shim. Without this check
/// the guard below took such a live statement for a double finalize and left
/// it open, "in progress" -- FTS3's `PRAGMA 'main'.page_size`, when an FTS4
/// table was created or connected -- so the connection's next VACUUM failed
/// (Plex's XMLTV EPG migration, DVR creation 500, 2026-10-07).
/// `sqlite3_next_stmt` walks only live statements, on the thread that holds
/// the connection, so a freed address is never read.
unsafe fn is_live_stmt_of_current_conn(p_stmt: *mut sqlite3_stmt) -> bool {
    let db = super::current_conn();
    if db.is_null() || p_stmt.is_null() {
        return false;
    }
    let Some(next) = crate::db_interpose_common::orig_sqlite3_next_stmt else {
        return false;
    };
    let mut cur = next(db, ptr::null_mut());
    while !cur.is_null() {
        if cur == p_stmt {
            return true;
        }
        cur = next(db, cur);
    }
    false
}

pub(super) fn finalize_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    unsafe {
        if skip_clear_bindings_on_finalized()
            && is_recently_finalized_stmt(p_stmt)
            && !is_live_stmt_of_current_conn(p_stmt)
        {
            log_clear_bindings_anomaly("finalize on recently finalized", p_stmt);
            clear_prepared_stmt(p_stmt);
            return SQLITE_OK;
        }
        if !is_prepared_stmt(p_stmt) {
            log_clear_bindings_anomaly("finalize on unknown stmt", p_stmt);
        }

        let mut is_pg_only = 0;
        let mut is_pg_value = 0;
        let mut final_sql: *const c_char = ptr::null();

        let pg_stmt = pg_find_stmt(p_stmt);
        if !pg_stmt.is_null() {
            let pg_stmt_ref = &*pg_stmt;
            is_pg_value = pg_stmt_ref.is_pg;
            is_pg_only = if pg_stmt_ref.is_pg == 2 { 1 } else { 0 };
            final_sql = if !pg_stmt_ref.pg_sql.is_null() {
                pg_stmt_ref.pg_sql
            } else {
                pg_stmt_ref.sql
            };

            remember_finalized_stmt(p_stmt, final_sql, is_pg_value);

            let cached = pg_find_cached_stmt(p_stmt);
            if cached == pg_stmt {
                log_debug("finalize: stmt in both global and TLS, clearing TLS ref");
                pg_clear_cached_stmt(p_stmt);
            } else if !cached.is_null() {
                log_info("finalize: different pg_stmt in global vs TLS for same sqlite_stmt (cross-thread re-prepare)");
                pg_clear_cached_stmt(p_stmt);
            }

            pg_unregister_stmt(p_stmt);
            pg_stmt_unref(pg_stmt);
        } else {
            let cached = pg_find_cached_stmt(p_stmt);
            if !cached.is_null() {
                let cached_ref = &*cached;
                is_pg_value = cached_ref.is_pg;
                is_pg_only = if cached_ref.is_pg == 2 { 1 } else { 0 };
                if final_sql.is_null() {
                    final_sql = if !cached_ref.pg_sql.is_null() {
                        cached_ref.pg_sql
                    } else {
                        cached_ref.sql
                    };
                }

                remember_finalized_stmt(p_stmt, final_sql, is_pg_value);

                log_debug_lazy!(
                    "finalize: stmt only in TLS (ref_count={}), clearing",
                    cached_ref.ref_count.load(Ordering::Relaxed)
                );
                pg_clear_cached_stmt(p_stmt);
                pg_stmt_unref(cached);
            } else {
                if final_sql.is_null() {
                    if let Some(f) = orig_sqlite3_sql {
                        final_sql = f(p_stmt);
                    }
                }
                remember_finalized_stmt(p_stmt, final_sql, is_pg_value);
            }
        }

        let mut rc = SQLITE_OK;
        if is_pg_only == 0 {
            rc = orig_sqlite3_finalize
                .map(|f| f(p_stmt))
                .unwrap_or(SQLITE_ERROR);
        }
        clear_prepared_stmt(p_stmt);
        rc
    }
}

pub(super) fn clear_bindings_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    unsafe {
        if skip_clear_bindings_on_finalized() && is_recently_finalized_stmt(p_stmt) {
            log_clear_bindings_anomaly("recently finalized", p_stmt);
            return SQLITE_OK;
        }

        let pg_stmt = pg_find_stmt(p_stmt);
        if pg_stmt.is_null() {
            log_clear_bindings_anomaly("stmt not registered", p_stmt);
        }

        if !pg_stmt.is_null() {
            let pg_stmt_ref = &mut *pg_stmt;
            let _guard = PgStmt::lock_mutex(pg_stmt);
            clear_all_param_values(pg_stmt_ref);
            if pg_stmt_ref.is_pg == 0 {
                return orig_sqlite3_clear_bindings
                    .map(|f| f(p_stmt))
                    .unwrap_or(SQLITE_ERROR);
            }
            return SQLITE_OK;
        }

        orig_sqlite3_clear_bindings
            .map(|f| f(p_stmt))
            .unwrap_or(SQLITE_ERROR)
    }
}
