use super::*;

unsafe fn column_value_debug_sql(dbg_stmt: *mut PgStmt) -> *const c_char {
    if dbg_stmt.is_null() {
        return ptr::null();
    }
    let s = &*dbg_stmt;
    if !s.pg_sql.is_null() {
        s.pg_sql
    } else {
        s.sql
    }
}

fn passthrough_column_value(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *mut sqlite3_value {
    get_orig_sqlite3_column_value()
        .map(|f| unsafe { f(p_stmt, idx) })
        .unwrap_or(ptr::null_mut())
}

pub(super) fn column_value_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *mut sqlite3_value {
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);
    let dbg_sql = unsafe { column_value_debug_sql(raw_pg_stmt) };
    let dbg_db = get_orig_sqlite3_db_handle()
        .map(|f| unsafe { f(p_stmt) })
        .unwrap_or(ptr::null_mut());
    unsafe {
        crate::db_interpose_common::note_column_phase(
            b"column_value\0",
            dbg_sql,
            p_stmt as *const c_void,
            dbg_db as *const c_void,
            idx,
        );
    }

    if raw_pg_stmt.is_null() || unsafe { (&*raw_pg_stmt).is_pg == 0 } {
        return passthrough_column_value(p_stmt, idx);
    }

    let pg_stmt = unsafe { &mut *raw_pg_stmt };

    if env_utils::env_truthy_str("PLEX_PG_DISABLE_COLUMN_VALUE") {
        return ptr::null_mut();
    }

    // Call ensure_pg_result_for_metadata BEFORE acquiring stmt mutex to avoid
    // ABBA deadlock (stmt mutex -> conn mutex).
    let needs_metadata =
        pg_stmt.result.is_null() && pg_stmt.cached_result.is_null() && !pg_stmt.pg_sql.is_null();
    if needs_metadata {
        if !ensure_pg_result_for_metadata(raw_pg_stmt) {
            return passthrough_column_value(p_stmt, idx);
        }
    }

    // Hold mutex only for data reads — no logging inside this block
    // to avoid ABBA deadlock between stmt mutex and LOGGER mutex.
    // Extract row value, then release mutex before calling allocate_fake_sqlite_value
    // (which acquires fake_value_mutex).
    let row = {
        let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };

        if pg_stmt.result.is_null() {
            return passthrough_column_value(p_stmt, idx);
        }

        if idx < 0 || idx >= pg_stmt.num_cols {
            return ptr::null_mut();
        }

        pg_stmt.current_row
    };
    // Mutex released here.

    // allocate_fake_sqlite_value acquires fake_value_mutex — called outside stmt mutex.
    unsafe { allocate_fake_sqlite_value(raw_pg_stmt, idx, row) }
}
