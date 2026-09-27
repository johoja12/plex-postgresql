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

/// Write cached text output into a thread-local buffer.
/// SAFETY: Must be called while stmt mutex is held. Does NOT call log_debug/log_error
/// to avoid deadlock with the LOGGER mutex.
unsafe fn write_cached_text_output(
    pg_stmt: &mut PgStmt,
    _idx: c_int,
    state: &CachedTextState,
) -> *const c_uchar {
    let str_len = libc::strlen(state.source_value) as usize;
    let buf_idx = next_text_buffer_index();
    let mut out_ptr: *const c_uchar = ptr::null();
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

        let copy_len = str_len.min(TEXT_BUFFER_SIZE - 1);
        if copy_len > 0 {
            ptr::copy_nonoverlapping(state.source_value as *const u8, buf.as_mut_ptr(), copy_len);
        }
        buf[copy_len] = 0;
        out_ptr = buf.as_ptr();
    });
    out_ptr
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

/// Borrow ordinary live text; retain transformed text on the statement.
/// SAFETY: Must be called while stmt mutex is held. Does NOT call log_debug/log_error
/// to avoid deadlock with the LOGGER mutex.
unsafe fn write_live_text_output(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    state: &LiveTextState,
) -> *const c_uchar {
    if let Some(Some(owned)) = pg_stmt.owned_column_text.get(idx as usize) {
        return owned.as_ptr();
    }
    let mut source = ptr::null();
    let mut len = 0;
    let mut is_null = 0;
    let ok = crate::db_interpose_helpers::rust_pg_result_value_ptr_len(
        helpers_result_ptr(pg_stmt.result), state.row, idx,
        &mut source, &mut len, &mut is_null,
    );
    if ok == 0 || is_null != 0 || source.is_null() || len < 0 {
        return b"\0".as_ptr();
    }
    let bytes = std::slice::from_raw_parts(source as *const u8, len as usize);
    if let Some(owned) = crate::db_interpose_helpers::column_text_transform_owned(
        state.col_name, state.oid_u, pg_stmt.pg_sql, source, bytes,
    ) {
        pg_stmt.owned_column_text.resize_with(pg_stmt.num_cols as usize, || None);
        pg_stmt.owned_column_text[idx as usize] = Some(owned);
        return pg_stmt.owned_column_text[idx as usize].as_ref().unwrap().as_ptr();
    }
    // libpq owns this immutable, NUL-terminated value until PQclear. The
    // statement keeps that result alive through subsequent column accesses.
    source as *const u8
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
    let dbg_db = get_orig_sqlite3_db_handle()
        .map(|f| unsafe { f(p_stmt) })
        .unwrap_or(ptr::null_mut());
    unsafe {
        crate::db_interpose_common::note_column_phase(
            b"column_text\0",
            dbg_sql,
            p_stmt as *const c_void,
            dbg_db as *const c_void,
            idx,
        );
    }

    validate_type_consistency(dbg_stmt, p_stmt, idx, "column_text");

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

        if !pg_stmt.cached_result.is_null() {
            match unsafe { load_cached_text_state(pg_stmt, idx) } {
                Some(state) => unsafe { write_cached_text_output(pg_stmt, idx, &state) },
                // SQL NULL: return empty string instead of NULL to prevent
                // Plex's std::string(nullptr) → basic_string crash.
                // Real SQLite returns NULL here, but Plex doesn't always check.
                None => empty_text_buffer(),
            }
        } else if pg_stmt.result.is_null() {
            empty_text_buffer()
        } else if idx < 0 || idx >= pg_stmt.num_cols {
            empty_text_buffer()
        } else {
            let row = pg_stmt.current_row;
            if row < 0 || row >= pg_stmt.num_rows {
                empty_text_buffer()
            } else {
                match unsafe { load_live_text_state(pg_stmt, idx) } {
                    Some(state) => unsafe { write_live_text_output(pg_stmt, idx, &state) },
                    None => empty_text_buffer(),
                }
            }
        }
    }
    // Mutex released here.
}

#[cfg(test)]
mod lifetime_tests {
    use super::*;
    use crate::libpq_helpers::{PGconn, PGresult};
    #[repr(C)]
    struct AttDesc {
        name: *mut c_char, tableid: u32, columnid: c_int, format: c_int,
        typid: u32, typlen: c_int, atttypmod: c_int,
    }
    extern "C" {
        fn PQmakeEmptyPGresult(conn: *mut PGconn, status: c_int) -> *mut PGresult;
        fn PQsetResultAttrs(res: *mut PGresult, count: c_int, attrs: *mut AttDesc) -> c_int;
        fn PQsetvalue(res: *mut PGresult, row: c_int, col: c_int, value: *mut c_char, len: c_int) -> c_int;
    }
    #[test]
    fn transformed_column_pointers_survive_other_columns_and_reset_releases_ownership() {
        let name = CString::new("uri").unwrap();
        let mut attrs: Vec<_> = (0..70).map(|_| AttDesc { name: name.as_ptr() as *mut _,
            tableid: 0, columnid: 0, format: 0, typid: 25, typlen: -1, atttypmod: -1 }).collect();
        let mut stmt = PgStmt::new();
        stmt.result = unsafe { PQmakeEmptyPGresult(ptr::null_mut(), 2) };
        stmt.num_cols = attrs.len() as c_int; stmt.num_rows = 1; stmt.current_row = 0;
        assert_eq!(unsafe { PQsetResultAttrs(stmt.result, stmt.num_cols, attrs.as_mut_ptr()) }, 1);
        let mut pointers = Vec::new();
        for col in 0..stmt.num_cols {
            let suffix = format!("{col}/{}", "é".repeat(5000));
            let input = CString::new(format!("server://m/com.plexapp.plugins.library/library/{suffix}")).unwrap();
            assert_eq!(unsafe { PQsetvalue(stmt.result, 0, col, input.as_ptr() as *mut _, input.as_bytes().len() as c_int) }, 1);
            let state = LiveTextState { row: 0, col_name: name.as_ptr(), _oid: 25, oid_u: 25 };
            pointers.push((unsafe { write_live_text_output(&mut stmt, col, &state) }, format!("library://{suffix}")));
        }
        for (pointer, expected) in pointers {
            assert_eq!(unsafe { CStr::from_ptr(pointer as *const c_char) }.to_bytes(), expected.as_bytes());
        }
        assert_eq!(stmt.owned_column_text.len(), 70);
        crate::pg_statement::rust_stmt_clear_result(&mut stmt);
        assert!(stmt.owned_column_text.is_empty());
        assert!(stmt.result.is_null());
    }
}
