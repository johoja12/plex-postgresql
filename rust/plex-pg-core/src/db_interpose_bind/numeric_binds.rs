use super::*;
use crate::db_interpose_bind::support::{
    begin_bind, is_pg_routed_noncached, mapped_param_index, retry_on_misuse,
};

pub(super) fn bind_int_impl(p_stmt: *mut sqlite3_stmt, idx: c_int, val: c_int) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_INT, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        SQLITE_OK
    } else {
        // Plex's SQLite implements bind_int by calling bind_int64 through
        // its PLT. Calling bind_int while holding StmtGuard would re-enter
        // our bind_int64 interposer and lock this same non-recursive mutex.
        // Bind the sign-extended value directly with the original leaf API.
        let mut rc = get_orig_sqlite3_bind_int64()
            .map(|f| unsafe { f(p_stmt, idx, i64::from(val)) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_int64()
                    .map(|f| f(p_stmt, idx, i64::from(val)))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
        unsafe {
            let stmt = &mut *pg_stmt;
            libc::snprintf(
                stmt.param_buffers[pg_idx].as_mut_ptr(),
                PARAM_BUF_LEN,
                b"%d\0".as_ptr() as *const c_char,
                val,
            );
            stmt.param_values[pg_idx] = stmt.param_buffers[pg_idx].as_mut_ptr();
        }
    }

    drop(guard);
    rc
}

pub(super) fn bind_int64_impl(p_stmt: *mut sqlite3_stmt, idx: c_int, val: i64) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_INT64, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_int64()
            .map(|f| unsafe { f(p_stmt, idx, val) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_int64()
                    .map(|f| f(p_stmt, idx, val))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
        unsafe {
            let stmt = &mut *pg_stmt;
            libc::snprintf(
                stmt.param_buffers[pg_idx].as_mut_ptr(),
                PARAM_BUF_LEN,
                b"%lld\0".as_ptr() as *const c_char,
                val as libc::c_longlong,
            );
            stmt.param_values[pg_idx] = stmt.param_buffers[pg_idx].as_mut_ptr();
        }
    }

    drop(guard);
    rc
}

pub(super) fn bind_double_impl(p_stmt: *mut sqlite3_stmt, idx: c_int, val: f64) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_DOUBLE, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_double()
            .map(|f| unsafe { f(p_stmt, idx, val) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_double()
                    .map(|f| f(p_stmt, idx, val))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
        unsafe {
            let stmt = &mut *pg_stmt;
            libc::snprintf(
                stmt.param_buffers[pg_idx].as_mut_ptr(),
                PARAM_BUF_LEN,
                b"%.17g\0".as_ptr() as *const c_char,
                val,
            );
            stmt.param_values[pg_idx] = stmt.param_buffers[pg_idx].as_mut_ptr();
        }
    }

    drop(guard);
    rc
}
