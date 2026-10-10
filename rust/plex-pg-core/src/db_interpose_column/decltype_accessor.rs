use super::*;

fn text_decltype() -> *const c_char {
    DECLTYPE_TEXT.as_ptr() as *const c_char
}

fn passthrough_decltype(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    get_orig_sqlite3_column_decltype()
        .map(|f| unsafe { f(p_stmt, idx) })
        .unwrap_or(ptr::null())
}

/// Check whether we have no result or idx is out of bounds.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
fn no_result_decltype(pg_stmt: &mut PgStmt, idx: c_int) -> Option<*const c_char> {
    if pg_stmt.result.is_null() || idx < 0 || idx >= pg_stmt.num_cols {
        return Some(text_decltype());
    }
    None
}

/// Look up cached decltype for a column.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
unsafe fn lookup_cached_decltype(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    col_name: *const c_char,
) -> *const c_char {
    let mut cached_type = lookup_sqlite_decltype(pg_stmt.conn(), col_name);

    if cached_type.is_null() && idx >= 0 && (idx as usize) < pg_stmt.col_table_names.len() {
        let table_ptr = pg_stmt.col_table_names[idx as usize];
        if !table_ptr.is_null() {
            let table = CStr::from_ptr(table_ptr).to_string_lossy();
            let column = cstr_to_string_or(col_name, "");
            let mut cache_key = String::with_capacity(DECLTYPE_MAX_KEY_LEN);
            cache_key.push_str(&table);
            cache_key.push('_');
            cache_key.push_str(&column);
            cached_type = lookup_decltype_direct(pg_stmt.conn(), &cache_key);
        }
    }

    cached_type
}

/// Return a previously cached decltype value.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
unsafe fn return_cached_decltype(cached_type: *const c_char) -> *const c_char {
    cached_type
}

/// Resolve special-case decltype overrides.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
unsafe fn resolve_special_case_decltype(
    pg_stmt: &mut PgStmt,
    idx: c_int,
    oid: u32,
    col_name: *const c_char,
) -> Option<*const c_char> {
    let table_oid = crate::db_interpose_helpers::rust_pg_result_col_table_oid(
        helpers_result_ptr(pg_stmt.result),
        idx,
    );
    let special_case =
        crate::pg_statement::rust_decltype_special_case(oid, col_name, pg_stmt.pg_sql, table_oid);

    if special_case == PG_DECLTYPE_CASE_DT_INTEGER_8 {
        return Some(DECLTYPE_DT_INTEGER_8.as_ptr() as *const c_char);
    }
    if special_case == PG_DECLTYPE_CASE_NULL {
        return Some(ptr::null());
    }
    None
}

/// Map a PostgreSQL OID to a SQLite decltype string.
/// SAFETY: Must be called while stmt mutex is held. Does NOT log to avoid
/// deadlock with the LOGGER mutex.
unsafe fn oid_decltype(oid: u32) -> *const c_char {
    crate::pg_statement::oid_to_sqlite_decltype(oid).as_ptr()
}

pub(super) fn column_decltype_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    let raw_pg_stmt = pg_find_any_stmt(p_stmt);

    if raw_pg_stmt.is_null() || unsafe { (&*raw_pg_stmt).is_pg == 0 } {
        return passthrough_decltype(p_stmt, idx);
    }

    let pg_stmt = unsafe { &mut *raw_pg_stmt };

    // Call ensure_metadata_result BEFORE acquiring stmt mutex to avoid
    // ABBA deadlock (stmt mutex -> conn mutex).
    if pg_stmt.result.is_null() && pg_stmt.cached_result.is_null() && !pg_stmt.pg_sql.is_null() {
        ensure_pg_result_for_metadata(raw_pg_stmt);
    }

    // Hold mutex only for data reads — no logging inside this block
    // to avoid ABBA deadlock between stmt mutex and LOGGER mutex.
    let _guard = unsafe { PgStmt::lock_mutex(raw_pg_stmt) };

    column_decltype_locked(pg_stmt, idx).0
}

/// Caller holds the statement mutex. Cached values are owned by the immutable
/// schema cache or static storage, not the libpq result itself.
pub(super) fn column_decltype_locked(pg_stmt: &mut PgStmt, idx: c_int) -> (*const c_char, c_int, c_int) {
    if pg_stmt.result.is_null() || idx < 0 || idx >= pg_stmt.num_cols {
        return (text_decltype(), SQLITE_TEXT, SQLITE_TEXT);
    }
    let epoch = crate::libpq_helpers::result_metadata_epoch();
    if pg_stmt.column_decltypes_epoch != epoch
        || pg_stmt.column_decltypes_result != pg_stmt.result as usize
        || pg_stmt.column_decltypes_sql != pg_stmt.pg_sql as usize
    {
        pg_stmt.column_decltypes.clear();
        pg_stmt.column_decltypes_epoch = epoch;
        pg_stmt.column_decltypes_result = pg_stmt.result as usize;
        pg_stmt.column_decltypes_sql = pg_stmt.pg_sql as usize;
    }
    let idx_usize = idx as usize;
    if let Some(Some(value)) = pg_stmt.column_decltypes.get(idx_usize) {
        return *value;
    }
    let decltype = resolve_column_decltype_locked(pg_stmt, idx);
    let expected = crate::db_interpose_helpers::rust_expected_sqlite_type_for_decltype(decltype);
    pg_stmt.column_decltypes.resize(pg_stmt.num_cols as usize, None);
    // libpq field OIDs are invariant across all rows of this PGresult. Cache
    // the non-NULL runtime type alongside the declared type, so successful
    // consistency checks need no per-row libpq calls. NULL remains valid too.
    let oid = crate::db_interpose_helpers::rust_pg_result_col_oid(
        helpers_result_ptr(pg_stmt.result), idx);
    let actual = pg_oid_to_sqlite_type_impl(oid);
    pg_stmt.column_decltypes[idx_usize] = Some((decltype, expected, actual));
    (decltype, expected, actual)
}

fn resolve_column_decltype_locked(pg_stmt: &mut PgStmt, idx: c_int) -> *const c_char {
    if let Some(result) = no_result_decltype(pg_stmt, idx) {
        return result;
    }

    let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
        helpers_result_ptr(pg_stmt.result),
        idx,
    );

    let cached_type = unsafe { lookup_cached_decltype(pg_stmt, idx, col_name) };
    if !cached_type.is_null() {
        return unsafe { return_cached_decltype(cached_type) };
    }

    let oid = crate::db_interpose_helpers::rust_pg_result_col_oid(
        helpers_result_ptr(pg_stmt.result),
        idx,
    );

    if let Some(result) = unsafe { resolve_special_case_decltype(pg_stmt, idx, oid, col_name) } {
        return result;
    }

    unsafe { oid_decltype(oid) }
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;
    use crate::libpq_helpers::{PGconn, PGresult, rust_pq_clear, invalidate_result_metadata};

    #[repr(C)]
    struct AttDesc {
        name: *mut c_char, tableid: u32, columnid: c_int, format: c_int,
        typid: u32, typlen: c_int, atttypmod: c_int,
    }
    extern "C" {
        fn PQmakeEmptyPGresult(conn: *mut PGconn, status: c_int) -> *mut PGresult;
        fn PQsetResultAttrs(res: *mut PGresult, count: c_int, attrs: *mut AttDesc) -> c_int;
    }
    fn result(name: &str, oid: u32, table: u32) -> *mut PGresult {
        let name = CString::new(name).unwrap();
        let mut attr = AttDesc { name: name.as_ptr() as *mut _, tableid: table,
            columnid: 1, format: 0, typid: oid, typlen: -1, atttypmod: -1 };
        let res = unsafe { PQmakeEmptyPGresult(ptr::null_mut(), 2) };
        assert!(!res.is_null());
        assert_eq!(unsafe { PQsetResultAttrs(res, 1, &mut attr) }, 1);
        res
    }
    #[test]
    fn null_expression_decltype_is_cached_without_changing_type() {
        let mut stmt = PgStmt::new();
        stmt.result = result("descriptor_count", 20, 0);
        stmt.num_cols = 1;
        let first = column_decltype_locked(&mut stmt, 0);
        assert!(first.0.is_null());
        assert_eq!(first.1, -1);
        assert_eq!(first.2, SQLITE_INTEGER);
        assert_eq!(stmt.column_decltypes[0], Some(first));
        assert_eq!(column_decltype_locked(&mut stmt, 0), first);
        rust_pq_clear(stmt.result);
    }
    #[test]
    fn descriptor_invalidates_on_result_replacement_and_epoch_change() {
        let mut stmt = PgStmt::new();
        stmt.result = result("descriptor_unaliased", 25, 0);
        stmt.num_cols = 1;
        assert_eq!(column_decltype_locked(&mut stmt, 0).1, SQLITE_TEXT);
        rust_pq_clear(stmt.result);
        stmt.result = result("descriptor_unaliased", 23, 0);
        assert_eq!(column_decltype_locked(&mut stmt, 0).1, SQLITE_INTEGER);
        assert_eq!(column_decltype_locked(&mut stmt, 0).2, SQLITE_INTEGER);
        // Simulate reuse of the exact result address with a stale descriptor.
        stmt.column_decltypes[0] = Some((text_decltype(), SQLITE_TEXT, SQLITE_TEXT));
        invalidate_result_metadata();
        assert_eq!(column_decltype_locked(&mut stmt, 0).1, SQLITE_INTEGER);
        assert_eq!(column_decltype_locked(&mut stmt, 0).2, SQLITE_INTEGER);
        rust_pq_clear(stmt.result);
    }
    #[test]
    fn schema_publication_invalidates_an_oid_fallback_descriptor() {
        let mut stmt = PgStmt::new();
        let name = CString::new("descriptor_schema_epoch_unique").unwrap();
        stmt.result = result(name.to_str().unwrap(), 25, 0);
        stmt.num_cols = 1;
        assert_eq!(column_decltype_locked(&mut stmt, 0).1, SQLITE_TEXT);
        crate::db_interpose_helpers::rust_decltype_cache_insert(
            name.as_ptr(), b"INTEGER\0".as_ptr() as *const c_char);
        assert_eq!(column_decltype_locked(&mut stmt, 0).1, SQLITE_INTEGER);
        // The declared type changes, but the libpq field remains TEXT: this
        // must still take the mismatch/NULL validation path.
        assert_eq!(column_decltype_locked(&mut stmt, 0).2, SQLITE_TEXT);
        rust_pq_clear(stmt.result);
    }
}
