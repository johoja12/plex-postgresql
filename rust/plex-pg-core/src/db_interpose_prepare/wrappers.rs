use super::*;
use crate::db_interpose_common::stderr_ptr;
use crate::log_info_lazy;

use crate::env_utils::loadone_trace_enabled;

unsafe fn trace_recursive_internal_flow_sql(sql: *const c_char) {
    if !loadone_trace_enabled() {
        return;
    }

    let sql = if sql.is_null() {
        b"<null>\0".as_ptr() as *const c_char
    } else {
        sql
    };
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[LOADONE_TRACE][prepare] recursive_internal_flow sql=%.900s\n\0".as_ptr()
            as *const c_char,
        sql,
    );
    let _ = libc::fflush(stderr_ptr());
}

pub(super) fn prepare_v2_impl(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    unsafe { ensure_real_sqlite_loaded() };

    unsafe {
        if *tls_in_interpose_call_ptr() != 0 {
            // Recursive prepare — always go through internal_flow so PgStmt
            // is registered. Without this, step falls back to the dummy shadow
            // → SQLITE_DONE → SOCI "loadOne: not an error".
            // The 50-call depth guard in internal_flow prevents infinite recursion.
            trace_recursive_internal_flow_sql(z_sql);
            return super::prepare_v2_internal_impl(db, z_sql, n_byte, pp_stmt, pz_tail, 0);
        }

        *tls_in_interpose_call_ptr() = 1;
        let result = super::prepare_v2_internal_impl(db, z_sql, n_byte, pp_stmt, pz_tail, 0);
        *tls_in_interpose_call_ptr() = 0;
        result
    }
}

fn utf16_input_len(z_sql: *const c_void, n_byte: c_int) -> usize {
    if z_sql.is_null() {
        return 0;
    }
    if n_byte >= 0 {
        return n_byte as usize;
    }

    let mut utf16_len = 0usize;
    let mut p = z_sql as *const u16;
    unsafe {
        while *p != 0 {
            utf16_len += 2;
            p = p.add(1);
        }
    }
    utf16_len
}

pub(super) fn prepare_impl(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_v2_impl(db, z_sql, n_byte, pp_stmt, pz_tail)
}

pub(super) fn prepare16_v2_impl(
    db: *mut sqlite3,
    z_sql: *const c_void,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_void,
) -> c_int {
    // The finalize guard needs the connection: see ConnScope.
    let _scope = crate::db_interpose_stmt_lifecycle::ConnScope::enter(db);
    if z_sql.is_null() {
        return unsafe {
            orig_sqlite3_prepare16_v2
                .map(|f| f(db, z_sql, n_byte, pp_stmt, pz_tail))
                .unwrap_or(SQLITE_ERROR)
        };
    }
    let byte_len = utf16_input_len(z_sql, n_byte);
    let utf16_slice = unsafe { std::slice::from_raw_parts(z_sql as *const u8, byte_len) };
    let utf8 = String::from_utf8_lossy(utf16_slice);
    log_info_lazy!("PREPARE16_V2: {}", &utf8[..utf8.len().min(200)]);
    unsafe {
        orig_sqlite3_prepare16_v2
            .map(|f| f(db, z_sql, n_byte, pp_stmt, pz_tail))
            .unwrap_or(SQLITE_ERROR)
    }
}

pub(super) fn prepare_v3_impl(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    flags: u32,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    let _ = flags;
    prepare_v2_impl(db, z_sql, n_byte, pp_stmt, pz_tail)
}
