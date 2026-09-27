use super::*;
#[inline]
pub(crate) fn helpers_result_ptr(result: *mut PgResultLibpq) -> *const PgResultHelpers {
    result as *const PgResultHelpers
}

pub(crate) fn sqlite_type_name(t: c_int) -> &'static str {
    match t {
        SQLITE_INTEGER => "INTEGER",
        SQLITE_FLOAT => "FLOAT",
        SQLITE_TEXT => "TEXT",
        SQLITE_BLOB => "BLOB",
        SQLITE_NULL => "NULL",
        _ => "UNKNOWN",
    }
}

pub(crate) fn next_text_buffer_index() -> usize {
    COLUMN_TEXT_BUF_IDX.with(|idx| {
        let cur = idx.get();
        idx.set((cur + 1) % NUM_TEXT_BUFFERS);
        cur
    })
}

pub(crate) fn validate_type_consistency(
    raw_pg_stmt: *mut PgStmt,
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    accessor_name: &str,
) {
    if raw_pg_stmt.is_null() || unsafe { (&*raw_pg_stmt).is_pg == 0 } {
        return;
    }

    let pg_stmt = unsafe { &mut *raw_pg_stmt };

    // Resolve once under the existing statement lock. Calling the public
    // column_type/decltype entry points here repeated registry lookups, crash
    // breadcrumbs and locks for every value read, without changing conversion.
    let mismatch_ctx = {
        let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };
        if pg_stmt.result.is_null() || idx < 0 || idx >= pg_stmt.num_cols {
            return;
        }
        let (col_decltype, expected) =
            super::decltype_accessor::column_decltype_locked(pg_stmt, idx);
        if col_decltype.is_null() || expected == -1 {
            return;
        }
        let mut oid = 0;
        let mut is_null = 0;
        let mut col_type = SQLITE_NULL;
        crate::db_interpose_helpers::rust_pg_result_type_info(
            helpers_result_ptr(pg_stmt.result), pg_stmt.current_row, idx,
            &mut oid, &mut is_null, &mut col_type,
        );
        if is_null != 0 {
            col_type = super::type_accessor::null_column_type(oid);
        }
        if col_type == SQLITE_NULL || col_type == expected {
            return;
        }
        let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
            helpers_result_ptr(pg_stmt.result), idx,
        );
        (oid, col_name, expected, pg_stmt.current_row, pg_stmt.pg_sql,
            trace_badcast_should_log(raw_pg_stmt, idx), col_decltype, col_type)
    };

    // MISMATCH DETECTED — always log at ERROR level so we can diagnose bad_cast.
    let (oid, col_name, expected, current_row, pg_sql, should_trace, col_decltype, col_type) = mismatch_ctx;
    log_error(&format!(
        "TYPE_MISMATCH: accessor={} col='{}' idx={} row={} decltype='{}' expects {} but column_type={} (OID={}) sql={}",
        accessor_name,
        cstr_to_string_or(col_name, "?"),
        idx,
        current_row,
        cstr_to_string_or(col_decltype, "?"),
        sqlite_type_name(expected),
        sqlite_type_name(col_type),
        oid,
        cstr_prefix(pg_sql, 200, "?")
    ));

    if should_trace {
        trace_badcast_log_ctx(
            raw_pg_stmt,
            p_stmt,
            idx,
            accessor_name,
            "type_mismatch",
            current_row,
            if col_type == SQLITE_NULL { 1 } else { 0 },
            oid,
            col_name,
        );
    }
}
