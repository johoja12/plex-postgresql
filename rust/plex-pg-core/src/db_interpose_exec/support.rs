use super::*;

pub(crate) fn malloc_cstring(value: &str) -> *mut c_char {
    let bytes = value.as_bytes();
    unsafe {
        let ptr = libc::malloc(bytes.len() + 1) as *mut c_char;
        if ptr.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        *ptr.add(bytes.len()) = 0;
        ptr
    }
}

pub(crate) fn is_duplicate_prepared_stmt(res: *mut PGresult) -> bool {
    if res.is_null() {
        return false;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    crate::pg_client::rust_is_duplicate_sqlstate(sqlstate) != 0
}

pub(crate) fn is_stale_prepared_stmt(res: *mut PGresult) -> bool {
    if res.is_null() {
        return false;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    crate::pg_client::rust_is_stale_sqlstate(sqlstate) != 0
}

pub(crate) fn parse_positive_returning_rowid(id_str: *const c_char) -> Option<i64> {
    if id_str.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(id_str).to_bytes() };
    if bytes.is_empty() {
        return None;
    }
    let rowid = crate::db_interpose_helpers::rust_pg_text_to_int64(id_str);
    if rowid > 0 {
        Some(rowid)
    } else {
        None
    }
}

pub(crate) fn orig_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    // The finalize guard needs exec's connection: see ConnScope.
    let _scope = crate::db_interpose_stmt_lifecycle::ConnScope::enter(db);
    unsafe {
        match orig_sqlite3_exec {
            Some(f) => f(db, sql, callback, arg, errmsg),
            None => SQLITE_ERROR,
        }
    }
}
