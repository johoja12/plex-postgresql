use super::*;

struct CachedTextState {
    col_name: *const c_char,
    oid: u32,
    source_value: *const c_char,
}

struct LiveTextState {
    row: c_int,
    col_name: *const c_char,
    _oid: u32,
    oid_u: c_uint,
}

fn empty_text_buffer() -> *const c_uchar {
    let buf_idx = next_text_buffer_index();
    let mut out_ptr: *const c_uchar = ptr::null();
    COLUMN_TEXT_BUFFERS.with(|bufs| {
        let mut bufs = bufs.borrow_mut();
        let buf = &mut bufs[buf_idx];
        buf[0] = 0;
        out_ptr = buf.as_ptr();
    });
    out_ptr
}

unsafe fn load_cached_text_state(pg_stmt: &mut PgStmt, idx: c_int) -> Option<CachedTextState> {
    let cached = &*pg_stmt.cached_result;
    let row = pg_stmt.current_row;
    if idx < 0 || idx >= cached.num_cols || row < 0 || row >= cached.num_rows {
        return None;
    }

    let crow = &*cached.rows.add(row as usize);
    if *crow.is_null.add(idx as usize) != 0 {
        return None;
    }

    let source_value = *crow.values.add(idx as usize);
    if source_value.is_null() {
        return None;
    }

    let col_name = if !cached.col_names.is_null() {
        *cached.col_names.add(idx as usize)
    } else {
        ptr::null()
    };
    let oid = if !cached.col_types.is_null() {
        *cached.col_types.add(idx as usize)
    } else {
        0
    };

    Some(CachedTextState {
        col_name,
        oid,
        source_value,
    })
}

/// Write cached text output into a thread-local buffer. Returns the text
/// and whether it is in the ring (and so must be pinned to the statement).
/// SAFETY: Must be called while stmt mutex is held. Does NOT call log_debug/log_error
/// to avoid deadlock with the LOGGER mutex.
unsafe fn write_cached_text_output(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    state: &CachedTextState,
) -> (*const c_uchar, bool) {
    if let Some(Some(owned)) = pg_stmt.owned_column_text.get(idx as usize) {
        return (owned.as_ptr(), false);
    }
    let str_len = libc::strlen(state.source_value) as usize;
    // Cached transformed values need the same unbounded statement ownership
    // as live values; a fixed ring would truncate a long rewritten URI.
    let bytes = std::slice::from_raw_parts(state.source_value as *const u8, str_len);
    if let Some(owned) = crate::db_interpose_helpers::column_text_transform_owned(
        state.col_name, state.oid, pg_stmt.pg_sql, state.source_value, bytes,
    ) {
        pg_stmt.owned_column_text.resize_with(
            pg_stmt.owned_column_text.len().max(idx as usize + 1), || None);
        pg_stmt.owned_column_text[idx as usize] = Some(owned);
        return (pg_stmt.owned_column_text[idx as usize].as_ref().unwrap().as_ptr(), false);
    }
    let buf_idx = next_text_buffer_index();
    let mut out_ptr: *const c_uchar = ptr::null();
    let mut from_ring = true;
    COLUMN_TEXT_BUFFERS.with(|bufs| {
        let mut bufs = bufs.borrow_mut();
        let buf = &mut bufs[buf_idx];
        let transform_rc = crate::db_interpose_helpers::rust_column_text_transform(
            state.col_name,
            state.oid as c_uint,
            pg_stmt.pg_sql,
            state.source_value,
            str_len,
            buf.as_mut_ptr() as *mut c_char,
            TEXT_BUFFER_SIZE,
        );
        if transform_rc == -1 || transform_rc == 1 {
            out_ptr = buf.as_ptr();
            return;
        }
        if str_len >= TEXT_BUFFER_SIZE {
            // Longer than the buffer: the cached row's own copy, which lives
            // as long as SQLite keeps a column_text pointer (the row).
            out_ptr = state.source_value as *const c_uchar;
            from_ring = false;
            return;
        }

        let copy_len = str_len.min(TEXT_BUFFER_SIZE - 1);
        if copy_len > 0 {
            ptr::copy_nonoverlapping(state.source_value as *const u8, buf.as_mut_ptr(), copy_len);
        }
        buf[copy_len] = 0;
        out_ptr = buf.as_ptr();
    });
    (out_ptr, from_ring)
}

unsafe fn load_live_text_state(pg_stmt: &mut PgStmt, idx: c_int) -> Option<LiveTextState> {
    if pg_stmt.result.is_null() || idx < 0 || idx >= pg_stmt.num_cols {
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

    Some(LiveTextState {
        row,
        col_name,
        _oid: oid_u as u32,
        oid_u,
    })
}

/// Write live (non-cached) text output into a thread-local buffer. Returns
/// the text and whether it is in the ring (and so must be pinned to the
/// statement).
/// SAFETY: Must be called while stmt mutex is held. Does NOT call log_debug/log_error
/// to avoid deadlock with the LOGGER mutex.
unsafe fn write_live_text_output(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    state: &LiveTextState,
) -> (*const c_uchar, bool) {
    if let Some(Some(owned)) = pg_stmt.owned_column_text.get(idx as usize) {
        return (owned.as_ptr(), false);
    }
    let mut source = ptr::null();
    let mut len = 0;
    let mut is_null = 0;
    let ok = crate::db_interpose_helpers::rust_pg_result_value_ptr_len(
        helpers_result_ptr(pg_stmt.result), state.row, idx,
        &mut source, &mut len, &mut is_null,
    );
    if ok == 0 || is_null != 0 || source.is_null() || len < 0 {
        return (b"\0".as_ptr(), false);
    }
    let bytes = std::slice::from_raw_parts(source as *const u8, len as usize);
    if let Some(owned) = crate::db_interpose_helpers::column_text_transform_owned(
        state.col_name, state.oid_u, pg_stmt.pg_sql, source, bytes,
    ) {
        pg_stmt.owned_column_text.resize_with(pg_stmt.num_cols as usize, || None);
        pg_stmt.owned_column_text[idx as usize] = Some(owned);
        return (pg_stmt.owned_column_text[idx as usize].as_ref().unwrap().as_ptr(), false);
    }
    let buf_idx = next_text_buffer_index();
    let mut out_ptr: *const c_uchar = ptr::null();
    let mut preview = [0u8; 128];
    let mut source_len: usize = 0;
    let mut transform_rc: c_int = 0;
    COLUMN_TEXT_BUFFERS.with(|bufs| {
        let mut bufs = bufs.borrow_mut();
        let buf = &mut bufs[buf_idx];
        transform_rc = crate::db_interpose_helpers::rust_pg_result_text_transform_copy(
            helpers_result_ptr(pg_stmt.result),
            state.row,
            idx,
            state.col_name,
            state.oid_u,
            pg_stmt.pg_sql,
            0,
            buf.as_mut_ptr() as *mut c_char,
            TEXT_BUFFER_SIZE,
            preview.as_mut_ptr() as *mut c_char,
            preview.len(),
            &mut source_len as *mut usize,
        );
        out_ptr = buf.as_ptr();
    });

    if transform_rc == -2 {
        // PQgetvalue returned NULL for a non-NULL cell — likely a stale PGresult
        // race condition. Match the C shim behavior: return empty string instead
        // of NULL to prevent std::string(nullptr) → basic_string exception.
        return (b"\0".as_ptr(), false);
    }
    if transform_rc == 0 && source_len >= TEXT_BUFFER_SIZE {
        // Longer than the buffer, which holds the first 8,191 bytes: libpq's
        // own copy, valid until the result is cleared -- as long as SQLite
        // keeps a column_text pointer. A DVR device's channel map is 16 KB.
        let mut value: *const c_char = ptr::null();
        let mut len: c_int = 0;
        let mut is_null: c_int = 0;
        crate::db_interpose_helpers::rust_pg_result_value_ptr_len(
            helpers_result_ptr(pg_stmt.result),
            state.row,
            idx,
            &mut value as *mut *const c_char,
            &mut len as *mut c_int,
            &mut is_null as *mut c_int,
        );
        if !value.is_null() && is_null == 0 {
            return (value as *const c_uchar, false);
        }
    }
    (out_ptr, true)
}

/// Keep the text the statement is handing out for `idx` on the statement
/// itself, so the pointer lives until its next step, reset or finalize --
/// SQLite's promise for `sqlite3_column_text`.
///
/// The thread-local ring the value was rendered into has 64 slots, and a
/// statement with more columns than that, or Plex reading a column, then
/// sixty-odd others, then the first again, found the first overwritten.
/// `cached_text` is cleared on the next row (here and by the blob accessor,
/// which shares it), by the streaming step, and by reset and finalize.
///
/// A pointer that is not the ring's -- libpq's own copy of a long value, a
/// cached row's -- lives as long as the result does and is handed out as is.
///
/// SAFETY: Must be called while stmt mutex is held. Does NOT log.
unsafe fn pin_text_to_statement(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    text: *const c_uchar,
    from_ring: bool,
) -> *const c_uchar {
    if text.is_null() || !from_ring || idx < 0 {
        return text;
    }
    let row = pg_stmt.current_row;
    if pg_stmt.cached_row != row {
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
    let i = idx as usize;
    pg_stmt.ensure_column_capacity(i + 1);
    let copy = libc::strdup(text as *const c_char);
    if copy.is_null() {
        return text;
    }
    if !pg_stmt.cached_text[i].is_null() {
        libc::free(pg_stmt.cached_text[i] as *mut c_void);
    }
    pg_stmt.cached_text[i] = copy;
    copy as *const c_uchar
}

pub(super) fn column_text_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_uchar {
    let dbg_stmt = pg_find_any_stmt(p_stmt);
    let dbg_sql = if !dbg_stmt.is_null() {
        let ds = unsafe { &*dbg_stmt };
        if !ds.pg_sql.is_null() {
            ds.pg_sql
        } else {
            ds.sql
        }
    } else {
        ptr::null()
    };
    // The shim's own resolver: a routed statement answers its shadow or
    // connection handle and never hands the pointer to SQLite, which a
    // test's fake statement pointer crashed (2026-10-16).
    let dbg_db = crate::db_interpose_metadata::rust_my_sqlite3_db_handle(p_stmt);
    unsafe {
        pg_exception_note_phase(
            b"column_text\0".as_ptr() as *const c_char,
            dbg_sql,
            p_stmt,
            dbg_db,
        );
    }

    validate_type_consistency(p_stmt, idx, "column_text");

    if dbg_stmt.is_null() || unsafe { (&*dbg_stmt).is_pg == 0 } {
        return get_orig_sqlite3_column_text()
            .map(|f| unsafe { f(p_stmt, idx) })
            .unwrap_or(ptr::null());
    }

    let pg_stmt = unsafe { &mut *dbg_stmt };

    // Hold mutex only for data extraction — no logging inside this block
    // to avoid ABBA deadlock between stmt mutex and LOGGER mutex.
    {
        let _guard = unsafe { PgStmt::lock_mutex(dbg_stmt) };

        // Already rendered for this row: the same pointer, as SQLite gives.
        // Rendering again would replace, and free, the copy Plex may still
        // be reading.
        if idx >= 0
            && pg_stmt.cached_row == pg_stmt.current_row
            && (idx as usize) < pg_stmt.cached_text.len()
            && !pg_stmt.cached_text[idx as usize].is_null()
        {
            return pg_stmt.cached_text[idx as usize] as *const c_uchar;
        }

        let (text, from_ring) = if !pg_stmt.cached_result.is_null() {
            match unsafe { load_cached_text_state(pg_stmt, idx) } {
                Some(state) => unsafe { write_cached_text_output(pg_stmt, idx, &state) },
                // SQL NULL: return empty string instead of NULL to prevent
                // Plex's std::string(nullptr) → basic_string crash.
                // Real SQLite returns NULL here, but Plex doesn't always check.
                None => (empty_text_buffer(), true),
            }
        } else if pg_stmt.result.is_null() {
            (empty_text_buffer(), true)
        } else if idx < 0 || idx >= pg_stmt.num_cols {
            (empty_text_buffer(), true)
        } else {
            let row = pg_stmt.current_row;
            if row < 0 || row >= pg_stmt.num_rows {
                (empty_text_buffer(), true)
            } else {
                match unsafe { load_live_text_state(pg_stmt, idx) } {
                    Some(state) => unsafe { write_live_text_output(pg_stmt, idx, &state) },
                    None => (empty_text_buffer(), true),
                }
            }
        };
        unsafe { pin_text_to_statement(pg_stmt, idx, text, from_ring) }
    }
    // Mutex released here.
}
