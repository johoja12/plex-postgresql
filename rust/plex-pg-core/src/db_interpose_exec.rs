use crate::byte_utils::{contains_bytes, contains_icase_bytes, starts_with_icase_bytes};
mod pg_path;
mod support;

use crate::db_interpose_common::stderr_ptr;
use crate::db_interpose_conn_utils::{
    apply_pg_session_settings, connect_new, cstr_prefix, cstr_to_string_or, log_error, log_info,
    PgConnConfig, PthreadMutexGuard,
};
use crate::ffi_types::sqlite3;
use crate::libpq_helpers::PGresult;
use pg_path::exec_via_postgres;
use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use support::orig_exec;

/// Real `sqlite3_exec` as the shim calls it, for other modules' tests.
#[cfg(test)]
pub(crate) fn orig_exec_for_tests(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    orig_exec(db, sql, callback, arg, errmsg)
}

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

const CONNECTION_OK: c_int = 0;
const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;
const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;

const PG_RETRY_MAX_DELAYS: usize = 10;

type ExecCallback =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>;

thread_local! {
    static EXEC_RETRY_COUNT: Cell<i32> = const { Cell::new(0) };
    static EXEC_PG_CONN_ERROR: Cell<i32> = const { Cell::new(0) };
}

#[repr(C)]
struct SqlTranslation {
    sql: *mut c_char,
    param_names: *mut *mut c_char,
    param_count: c_int,
    success: c_int,
    error: [c_char; 256],
}

extern "C" {
    static mut orig_sqlite3_exec: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_char,
            ExecCallback,
            *mut c_void,
            *mut *mut c_char,
        ) -> c_int,
    >;

    fn rewrite_blobs_schema_migrations(sql: *const c_char, db_path: *const c_char) -> *mut c_char;
    fn pg_config_get() -> *mut PgConnConfig;
    fn sql_translate(sql: *const c_char) -> SqlTranslation;
    fn sql_translation_free(result: *mut SqlTranslation);
}

use crate::env_utils::loadone_trace_enabled;

fn trim_ascii_sql(bytes: &[u8]) -> &[u8] {
    let mut i = 0usize;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    &bytes[i..]
}

unsafe fn trace_exec_skipped_select(
    route: *const c_char,
    db: *mut sqlite3,
    pg_conn: *mut crate::ffi_types::PgConnection,
    sql: *const c_char,
) {
    if !loadone_trace_enabled() || sql.is_null() {
        return;
    }

    let sql_bytes = trim_ascii_sql(CStr::from_ptr(sql).to_bytes());
    if !starts_with_icase_bytes(sql_bytes, b"select") {
        return;
    }

    let file = crate::db_interpose_open::lookup_db_handle_filename(db);
    let file_ptr = file
        .as_ref()
        .map(|s| s.as_ptr())
        .unwrap_or(b"<untracked>\0".as_ptr() as *const c_char);
    let handle_conn = crate::pg_client::rust_pg_find_handle_connection(db);
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[LOADONE_TRACE][exec] route=%s db=%p file=%.900s handle_conn=%p pg_conn=%p sql=%.900s\n\0"
            .as_ptr() as *const c_char,
        route,
        db as *mut c_void,
        file_ptr,
        handle_conn as *mut c_void,
        pg_conn as *mut c_void,
        sql,
    );
    let _ = libc::fflush(stderr_ptr());
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    let rc = rust_my_sqlite3_exec_impl(db, sql, callback, arg, errmsg);

    let mut delays = [0i32; PG_RETRY_MAX_DELAYS];
    let mut max_retries = 0i32;
    crate::pg_config::pg_config_get_retry_delays(delays.as_mut_ptr(), &mut max_retries);

    let retry_count = EXEC_RETRY_COUNT.with(|c| c.get());
    let conn_error = EXEC_PG_CONN_ERROR.with(|c| c.get());

    if rc == SQLITE_ERROR && retry_count < max_retries && conn_error != 0 {
        EXEC_PG_CONN_ERROR.with(|c| c.set(0));
        let delay = delays[retry_count as usize];
        let new_count = retry_count + 1;
        EXEC_RETRY_COUNT.with(|c| c.set(new_count));
        log_error(&format!(
            "exec: PG conn error, retry {}/{} in {}ms (thread {:p})",
            new_count,
            max_retries,
            delay,
            unsafe { libc::pthread_self() } as *mut c_void
        ));

        let delay_ms = if delay < 0 { 0 } else { delay as u32 };
        unsafe {
            libc::usleep(delay_ms.saturating_mul(1000));
        }

        EXEC_PG_CONN_ERROR.with(|c| c.set(0));
        let retry_rc = rust_my_sqlite3_exec(db, sql, callback, arg, errmsg);

        if new_count > 0 && retry_rc != SQLITE_ERROR {
            log_error(&format!(
                "exec: retry succeeded after {} attempt(s)",
                new_count
            ));
        }
        EXEC_RETRY_COUNT.with(|c| c.set(0));
        return retry_rc;
    }

    if retry_count > 0 {
        if rc == SQLITE_ERROR {
            log_error("exec: retries exhausted, returning SQLITE_ERROR");
        }
        EXEC_RETRY_COUNT.with(|c| c.set(0));
    }

    rc
}

fn rust_my_sqlite3_exec_impl(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    if sql.is_null() {
        log_error("exec called with NULL SQL");
        return orig_exec(db, sql, callback, arg, errmsg);
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);

    if !pg_conn.is_null() && unsafe { (&*pg_conn).is_pg_active } != 0 {
        // SQLite engine config (fts3_tokenizer, icu_load_collation, load_extension)
        // must execute on real SQLite, not PG.
        let sql_str = unsafe { CStr::from_ptr(sql).to_str().unwrap_or("") };
        if crate::pg_config::is_sqlite_passthrough_str(sql_str) {
            unsafe {
                trace_exec_skipped_select(
                    b"sqlite_passthrough\0".as_ptr() as *const c_char,
                    db,
                    pg_conn,
                    sql,
                );
            }
            return orig_exec(db, sql, callback, arg, errmsg);
        }

        unsafe {
            trace_exec_skipped_select(b"pg_route\0".as_ptr() as *const c_char, db, pg_conn, sql);
        }
        // Transaction control (BEGIN/COMMIT/ROLLBACK/SAVEPOINT) is in skip-SQL,
        // so exec_via_postgres will no-op them. PG runs in autocommit mode —
        // each statement commits immediately. This is intentional: the pool
        // architecture cannot guarantee connection affinity across statements,
        // so forwarding transactions would send BEGIN/COMMIT to different
        // connections, causing data visibility bugs.
        return exec_via_postgres(pg_conn, sql);
    }

    // For non-PG databases (e.g. :memory:), icu_load_collation may fail because
    // the ICU extension isn't available in the Docker runtime environment.
    // Rewrite to "SELECT NULL" so SOCI's loadOne gets a valid row instead of crashing.
    let sql_str = unsafe { CStr::from_ptr(sql).to_str().unwrap_or("") };
    let trimmed_lower = sql_str.trim().to_ascii_lowercase();
    if trimmed_lower.starts_with("select icu_load_collation")
        || trimmed_lower.starts_with("icu_load_collation")
    {
        return orig_exec(db, c"SELECT NULL".as_ptr(), callback, arg, errmsg);
    }

    let mut cleaned_sql: *mut c_char = std::ptr::null_mut();
    let mut exec_sql = sql;
    let sql_bytes = unsafe { CStr::from_ptr(sql).to_bytes() };
    if contains_icase_bytes(sql_bytes, b"collate icu_root") {
        cleaned_sql = crate::db_interpose_helpers::rust_strip_collate_icu_root(sql);
        if !cleaned_sql.is_null() {
            exec_sql = cleaned_sql;
        }
    }

    unsafe {
        trace_exec_skipped_select(
            b"sqlite_orig\0".as_ptr() as *const c_char,
            db,
            pg_conn,
            exec_sql,
        );
    }
    let rc = orig_exec(db, exec_sql, callback, arg, errmsg);
    if !cleaned_sql.is_null() {
        crate::db_interpose_helpers::rust_free_cstring(cleaned_sql);
    }
    rc
}

#[cfg(test)]
mod tests {
    use super::support::parse_positive_returning_rowid;
    use std::ffi::CString;

    #[test]
    fn parse_positive_returning_rowid_accepts_positive_values() {
        let value = CString::new("12345").unwrap();
        assert_eq!(parse_positive_returning_rowid(value.as_ptr()), Some(12345));
    }

    #[test]
    fn parse_positive_returning_rowid_rejects_null_and_empty_values() {
        let empty = CString::new("").unwrap();
        assert_eq!(parse_positive_returning_rowid(std::ptr::null()), None);
        assert_eq!(parse_positive_returning_rowid(empty.as_ptr()), None);
    }

    #[test]
    fn parse_positive_returning_rowid_rejects_zero_and_negative_values() {
        let zero = CString::new("0").unwrap();
        let negative = CString::new("-9").unwrap();
        assert_eq!(parse_positive_returning_rowid(zero.as_ptr()), None);
        assert_eq!(parse_positive_returning_rowid(negative.as_ptr()), None);
    }
}
