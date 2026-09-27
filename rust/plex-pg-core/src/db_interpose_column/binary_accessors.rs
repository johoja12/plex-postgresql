use super::*;
use crate::log_debug_lazy;

struct LiveBinaryState {
    row: c_int,
    oid_u: c_uint,
    col_name: *const c_char,
}

unsafe fn load_cached_blob_ptr(pg_stmt: &mut PgStmt, idx: c_int) -> *const c_void {
    let cached = &*pg_stmt.cached_result;
    let row = pg_stmt.current_row;
    if idx >= 0 && idx < cached.num_cols && row >= 0 && row < cached.num_rows {
        let crow = &*cached.rows.add(row as usize);
        let is_null = *crow.is_null.add(idx as usize) != 0;
        if !is_null {
            let val_ptr = *crow.values.add(idx as usize);
            if !val_ptr.is_null() {
                return val_ptr as *const c_void;
            }
        }
    }
    ptr::null()
}

unsafe fn load_cached_bytes_len(pg_stmt: &mut PgStmt, idx: c_int) -> c_int {
    let cached = &*pg_stmt.cached_result;
    let row = pg_stmt.current_row;
    if idx >= 0 && idx < cached.num_cols && row >= 0 && row < cached.num_rows {
        let crow = &*cached.rows.add(row as usize);
        let is_null = *crow.is_null.add(idx as usize) != 0;
        if !is_null && !crow.lengths.is_null() {
            return *crow.lengths.add(idx as usize);
        }
    }
    0
}

unsafe fn load_live_binary_state(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    require_param_slot: bool,
) -> Option<LiveBinaryState> {
    if pg_stmt.result.is_null() {
        return None;
    }
    if idx < 0 || idx >= pg_stmt.num_cols {
        return None;
    }
    if require_param_slot && (idx as usize) >= pg_stmt.decoded_blobs.len() {
        return None;
    }

    let row = pg_stmt.current_row;
    if row < 0 || row >= pg_stmt.num_rows {
        return None;
    }

    let mut is_null = 0;
    let mut oid_u: c_uint = 0;
    let mut sqlite_type = SQLITE_NULL;
    crate::db_interpose_helpers::rust_pg_result_type_info(
        helpers_result_ptr(pg_stmt.result),
        row,
        idx,
        &mut oid_u as *mut c_uint,
        &mut is_null as *mut c_int,
        &mut sqlite_type as *mut c_int,
    );
    if is_null != 0 {
        return None;
    }

    let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
        helpers_result_ptr(pg_stmt.result),
        idx,
    );
    Some(LiveBinaryState {
        row,
        oid_u,
        col_name,
    })
}

unsafe fn refresh_cached_blob_row(pg_stmt: &mut PgStmt, row: c_int) {
    if pg_stmt.cached_row == row {
        return;
    }

    crate::db_interpose_helpers::rust_step_clear_row_caches(
        pg_stmt.cached_text.as_mut_ptr(),
        pg_stmt.cached_blob.as_mut_ptr(),
        pg_stmt.cached_blob_len.as_mut_ptr(),
        ptr::null_mut(),
        ptr::null_mut(),
        pg_stmt.cached_text.len() as c_int,
        &mut pg_stmt.cached_row as *mut c_int,
        ptr::null_mut(),
    );
    pg_stmt.cached_row = row;
}

unsafe fn materialize_live_blob(pg_stmt: &mut PgStmt, idx: c_int, row: c_int) -> *const c_void {
    if pg_stmt.cached_row == row && !pg_stmt.cached_blob[idx as usize].is_null() {
        return pg_stmt.cached_blob[idx as usize] as *const c_void;
    }

    refresh_cached_blob_row(pg_stmt, row);

    let blob_len = crate::db_interpose_helpers::rust_pg_result_length(
        helpers_result_ptr(pg_stmt.result),
        row,
        idx,
    );
    if blob_len > 0 {
        let buf = libc::malloc(blob_len as usize) as *mut u8;
        if buf.is_null() {
            // NOTE: do NOT log here — this may be called under pg_stmt.mutex,
            // and log_error acquires the LOGGER mutex (ABBA deadlock risk).
            return ptr::null();
        }

        let copied = crate::db_interpose_helpers::rust_pg_result_blob_copy(
            helpers_result_ptr(pg_stmt.result),
            row,
            idx,
            buf,
            blob_len as usize,
        );
        if copied <= 0 {
            libc::free(buf as *mut c_void);
            pg_stmt.cached_blob[idx as usize] = ptr::null_mut();
            pg_stmt.cached_blob_len[idx as usize] = 0;
            return ptr::null();
        }

        pg_stmt.cached_blob[idx as usize] = buf as *mut c_void;
        pg_stmt.cached_blob_len[idx as usize] = copied;
        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                PMT_COLUMN_CACHED_BLOB_ALLOC,
                copied as u64,
                1,
            );
        }
    }

    pg_stmt.cached_blob[idx as usize] as *const c_void
}

pub(super) fn column_blob_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_void {
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);

    if raw_pg_stmt.is_null() || unsafe { (&*raw_pg_stmt).is_pg == 0 } {
        return get_orig_sqlite3_column_blob()
            .map(|f| unsafe { f(p_stmt, idx) })
            .unwrap_or(ptr::null());
    }

    let pg_stmt = unsafe { &mut *raw_pg_stmt };

    let result = {
        let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };

        if !pg_stmt.cached_result.is_null() {
            return unsafe { load_cached_blob_ptr(pg_stmt, idx) };
        }

        let Some(state) = (unsafe { load_live_binary_state(pg_stmt, idx, true) }) else {
            return ptr::null();
        };

        let blob_result = if state.oid_u == 17 {
            let mut blob_len = 0;
            pg_decode_bytea_cached_impl(raw_pg_stmt, state.row, idx, &mut blob_len as *mut c_int)
        } else {
            unsafe { materialize_live_blob(pg_stmt, idx, state.row) }
        };

        (blob_result, state)
    };
    // Log AFTER releasing pg_stmt.mutex to avoid ABBA deadlock with LOGGER mutex.
    let (blob_result, state) = result;
    log_debug_lazy!(
        "column_blob called: col={} name={} type={} row={}",
        idx,
        cstr_to_string_or(state.col_name, "?"),
        state.oid_u,
        state.row
    );

    blob_result
}

pub(super) fn column_bytes_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    log_debug_lazy!("COLUMN_BYTES: stmt={:p} idx={}", p_stmt, idx);
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);

    if raw_pg_stmt.is_null() || unsafe { (&*raw_pg_stmt).is_pg == 0 } {
        return get_orig_sqlite3_column_bytes()
            .map(|f| unsafe { f(p_stmt, idx) })
            .unwrap_or(0);
    }

    let pg_stmt = unsafe { &mut *raw_pg_stmt };

    let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };

    if !pg_stmt.cached_result.is_null() {
        return unsafe { load_cached_bytes_len(pg_stmt, idx) };
    }

    let Some(state) = (unsafe { load_live_binary_state(pg_stmt, idx, false) }) else {
        return 0;
    };

    if let Some(Some(owned)) = pg_stmt.owned_column_text.get(idx as usize) {
        return owned.len().saturating_sub(1) as c_int;
    }

    if state.oid_u == 17 {
        let mut blob_len = 0;
        pg_decode_bytea_cached_impl(raw_pg_stmt, state.row, idx, &mut blob_len as *mut c_int);
        return blob_len;
    }

    let len = crate::db_interpose_helpers::rust_pg_result_length(
        helpers_result_ptr(pg_stmt.result),
        state.row,
        idx,
    );
    if len < 0 {
        0
    } else {
        len
    }
}
