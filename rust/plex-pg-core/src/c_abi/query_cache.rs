use std::os::raw::{c_char, c_void};

use crate::ffi_types::PgStmt;
use crate::libpq_helpers::PGresult;
use crate::pg_query_cache::CachedResult;

#[no_mangle]
pub extern "C" fn pg_query_cache_init() {
    crate::pg_query_cache::rust_query_cache_init();
}

#[no_mangle]
pub extern "C" fn pg_query_cache_cleanup() {
    crate::pg_query_cache::rust_query_cache_cleanup();
}

#[no_mangle]
pub extern "C" fn pg_query_cache_key(stmt: *mut PgStmt) -> u64 {
    if stmt.is_null() {
        return 0;
    }
    let s = unsafe { &*stmt };
    let key = crate::pg_query_cache::rust_query_cache_key(
        s.pg_sql,
        s.param_values.as_ptr() as *const *const c_char,
        s.param_count,
    );
    if key == 0 {
        return 0;
    }
    // The cache is the thread's, shared by every database on it: the same
    // statement against a Live TV guide and against the library has two
    // answers.
    let conn = s.conn();
    if conn.is_null() {
        return key;
    }
    let path =
        unsafe { crate::db_interpose_helpers::cstr_to_str_or_empty((*conn).db_path.as_ptr()) };
    match crate::db_interpose_helpers::epg_schema_for_path(path) {
        Some(schema) => crate::pg_query_cache::rust_query_cache_key_in_schema(key, &schema),
        None => key,
    }
}

#[no_mangle]
pub extern "C" fn pg_query_cache_lookup(stmt: *mut PgStmt) -> *mut CachedResult {
    let key = pg_query_cache_key(stmt);
    if key == 0 {
        return std::ptr::null_mut();
    }
    crate::pg_query_cache::rust_query_cache_lookup(key)
}

#[no_mangle]
pub extern "C" fn pg_query_cache_store(stmt: *mut PgStmt, result_ptr: *mut c_void) {
    if stmt.is_null() || result_ptr.is_null() {
        return;
    }

    let result = result_ptr as *mut PGresult;
    let status = crate::libpq_helpers::rust_pq_result_status(result);
    if status != crate::libpq_helpers::PGRES_TUPLES_OK {
        return;
    }

    let num_rows = crate::libpq_helpers::rust_pq_ntuples(result);
    let num_cols = crate::libpq_helpers::rust_pq_nfields(result);
    if num_rows <= 0 || num_cols <= 0 {
        return;
    }

    let key = pg_query_cache_key(stmt);
    if key == 0 {
        return;
    }

    let s = unsafe { &*stmt };
    crate::db_interpose_helpers::rust_query_cache_store_from_pgresult(
        key, result, num_rows, num_cols, s.pg_sql,
    );
}

#[no_mangle]
pub extern "C" fn pg_query_cache_invalidate(stmt: *mut PgStmt) {
    let key = pg_query_cache_key(stmt);
    if key == 0 {
        return;
    }
    crate::pg_query_cache::rust_query_cache_invalidate(key);
}

#[no_mangle]
pub extern "C" fn pg_query_cache_stats(hits: *mut u64, misses: *mut u64) {
    crate::pg_query_cache::rust_query_cache_stats(hits, misses);
}

#[no_mangle]
pub extern "C" fn pg_query_cache_release(entry: *mut CachedResult) {
    crate::pg_query_cache::rust_query_cache_release(entry);
}
