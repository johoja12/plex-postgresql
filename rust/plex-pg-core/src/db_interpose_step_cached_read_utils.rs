use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

use crate::db_interpose_conn_utils::{log_error, PthreadMutexGuard};
use crate::ffi_types::{sqlite3_stmt, PgConnection, PgStmt};
use crate::libpq_helpers::PGresult;
use crate::log_debug_lazy;

const STEP_RESULT_FALLBACK: c_int = -1;
const SQLITE_DONE: c_int = 101;
const SQLITE_ROW: c_int = 100;
const SQLITE_ERROR: c_int = 1;
const STEP_RESULT_DONE: c_int = SQLITE_DONE;
const STEP_RESULT_ROW: c_int = SQLITE_ROW;
const STEP_RESULT_ERROR: c_int = SQLITE_ERROR;

const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;
const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;

use crate::c_abi::resolve_column_tables;

extern "C" {
    fn sqlite3_free(ptr: *mut c_void);
    fn log_sql_fallback(
        original_sql: *const c_char,
        translated_sql: *const c_char,
        error_msg: *const c_char,
        context: *const c_char,
    );
}

fn cstr_to_str(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "?";
    }
    unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("?") }
}

fn contains_icase(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn is_duplicate_prepared_stmt(res: *mut PGresult) -> bool {
    if res.is_null() {
        return false;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    crate::pg_client::rust_is_duplicate_sqlstate(sqlstate) != 0
}

#[no_mangle]
pub extern "C" fn rust_step_cached_read_finalize_advance(
    cached: *mut PgStmt,
    expanded_sql: *mut c_char,
    step_rc_out: *mut c_int,
) -> c_int {
    unsafe {
        if !step_rc_out.is_null() {
            *step_rc_out = STEP_RESULT_DONE;
        }
        if cached.is_null() {
            return 0;
        }
        let c = &mut *cached;
        if c.result.is_null() {
            return 0;
        }

        c.current_row += 1;
        if c.current_row >= c.num_rows {
            crate::libpq_helpers::rust_pq_clear(c.result);
            c.result = std::ptr::null_mut();
            if !expanded_sql.is_null() {
                sqlite3_free(expanded_sql as *mut c_void);
            }
            if !step_rc_out.is_null() {
                *step_rc_out = STEP_RESULT_DONE;
            }
            return 1;
        }

        if !expanded_sql.is_null() {
            sqlite3_free(expanded_sql as *mut c_void);
        }
        if !step_rc_out.is_null() {
            *step_rc_out = STEP_RESULT_ROW;
        }
        1
    }
}

#[no_mangle]
pub extern "C" fn rust_step_cached_read_prepare_stmt(
    cached: *mut PgStmt,
    conn: *mut PgConnection,
    sql: *const c_char,
    p_stmt: *mut sqlite3_stmt,
    translated_sql: *const c_char,
) -> *mut PgStmt {
    if !cached.is_null() {
        return cached;
    }
    if conn.is_null() || sql.is_null() || p_stmt.is_null() || translated_sql.is_null() {
        return std::ptr::null_mut();
    }

    let new_stmt = crate::pg_statement::rust_stmt_create(conn, sql, p_stmt);
    if new_stmt.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: new_stmt is non-null (checked above).
    let ns = unsafe { &mut *new_stmt };
    ns.pg_sql = unsafe { libc::strdup(translated_sql) };
    ns.is_pg = 2;
    ns.is_cached = 1;
    crate::pg_statement::rust_cached_stmt_register(p_stmt as usize, new_stmt as usize);
    new_stmt
}

#[no_mangle]
pub extern "C" fn rust_step_cached_read_execute(
    stmt: *mut PgStmt,
    conn: *mut PgConnection,
    orig_sql: *const c_char,
    translated_sql: *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    unsafe {
        if !pg_conn_error_out.is_null() {
            *pg_conn_error_out = 0;
        }
        if stmt.is_null() || conn.is_null() || (&*conn).conn.is_null() || translated_sql.is_null() {
            return STEP_RESULT_FALLBACK;
        }
        let s = &mut *stmt;
        let c = &mut *conn;

        crate::pg_client::rust_pool_touch_connection(conn as *const c_void);
        let mut conn_guard = PthreadMutexGuard::lock(&mut c.mutex as *mut _);

        if c.conn.is_null() {
            log_error("CACHED READ: conn became NULL after lock (TOCTOU race)");
            conn_guard.unlock();
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        let scope_tag = CString::new("CACHED READ").unwrap();
        crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(conn, scope_tag.as_ptr());

        let read_sql_hash = crate::pg_client::rust_hash_sql(translated_sql);
        let read_stmt_name = format!("cr_{:x}", read_sql_hash);

        let translated_str = cstr_to_str(translated_sql);
        if contains_icase(translated_str, "DISTINCT") {
            log_error(&format!(
                "TRACE_STEP_PGSQL hash=0x{:x} sql={:.1200}",
                read_sql_hash, translated_str
            ));
        }

        let mut cached_read_stmt_name: *const c_char = std::ptr::null();
        if crate::pg_client::rust_stmt_cache_lookup(
            conn as *mut c_void,
            read_sql_hash,
            &mut cached_read_stmt_name,
        ) != 0
        {
            log_debug_lazy!(
                "CACHED READ (prepared): stmt={} sql={:.60}",
                cstr_to_str(cached_read_stmt_name),
                translated_str
            );
            s.result = crate::libpq_helpers::rust_pq_exec_prepared(
                c.conn,
                cached_read_stmt_name,
                0,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
        } else {
            let c_read_stmt_name =
                CString::new(read_stmt_name).unwrap_or_else(|_| CString::new("cr").unwrap());
            let prep_res = crate::libpq_helpers::rust_pq_prepare(
                c.conn,
                c_read_stmt_name.as_ptr(),
                translated_sql,
                0,
                std::ptr::null(),
            );
            if crate::libpq_helpers::rust_pq_result_status(prep_res) == PGRES_COMMAND_OK {
                crate::pg_client::rust_stmt_cache_add(
                    conn as *mut c_void,
                    read_sql_hash,
                    c_read_stmt_name.as_ptr(),
                    0,
                );
                crate::libpq_helpers::rust_pq_clear(prep_res);
                log_debug_lazy!(
                    "CACHED READ (new prepared): stmt={} sql={:.60}",
                    cstr_to_str(c_read_stmt_name.as_ptr()),
                    translated_str
                );
                s.result = crate::libpq_helpers::rust_pq_exec_prepared(
                    c.conn,
                    c_read_stmt_name.as_ptr(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                );
            } else if is_duplicate_prepared_stmt(prep_res) {
                crate::pg_client::rust_stmt_cache_add(
                    conn as *mut c_void,
                    read_sql_hash,
                    c_read_stmt_name.as_ptr(),
                    0,
                );
                crate::libpq_helpers::rust_pq_clear(prep_res);
                s.result = crate::libpq_helpers::rust_pq_exec_prepared(
                    c.conn,
                    c_read_stmt_name.as_ptr(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                );
            } else {
                let err = cstr_to_str(crate::libpq_helpers::rust_pq_error_message(c.conn));
                log_debug_lazy!("CACHED READ prepare failed, using PQexec: {}", err);
                crate::libpq_helpers::rust_pq_clear(prep_res);
                s.result = crate::libpq_helpers::rust_pq_exec(c.conn, translated_sql);
            }
        }
        if crate::libpq_helpers::rust_pq_result_status(s.result) == PGRES_TUPLES_OK {
            s.num_rows = crate::libpq_helpers::rust_pq_ntuples(s.result);
            s.num_cols = crate::libpq_helpers::rust_pq_nfields(s.result);
            s.ensure_column_capacity(s.num_cols as usize);
            s.current_row = 0;
            s.set_result_conn(conn);

            if resolve_column_tables(stmt, conn) < 0 {
                log_error("Failed to resolve column tables, cleaning up");
            }
            return if s.num_rows > 0 {
                STEP_RESULT_ROW
            } else {
                STEP_RESULT_DONE
            };
        }

        // A refused read used to fall back to whatever the shadow SQLite had
        // answered, which for the library is nothing. One classifier with the
        // step: retry what a retry can mend, report the rest.
        let (class, message) = crate::pg_client::classify_result(c.conn, s.result);
        let ctx = CString::new("CACHED READ").unwrap();
        let message_c = CString::new(message.clone()).unwrap_or_default();
        log_sql_fallback(orig_sql, translated_sql, message_c.as_ptr(), ctx.as_ptr());
        crate::libpq_helpers::rust_pq_clear(s.result);
        s.result = std::ptr::null_mut();
        match class {
            crate::pg_client::PgErrorClass::StalePlan => {
                crate::pg_client::rust_stmt_cache_clear_local(conn as *mut c_void);
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
            }
            crate::pg_client::PgErrorClass::Retry => {
                crate::pg_client::rust_pool_check_health(conn as *mut c_void);
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
            }
            crate::pg_client::PgErrorClass::Fail => {
                c.set_last_error(SQLITE_ERROR, &message);
            }
        }
        STEP_RESULT_ERROR
    }
}
