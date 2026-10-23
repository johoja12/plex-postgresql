use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uchar, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

#[path = "db_interpose_column_badcast.rs"]
mod badcast;
mod binary_accessors;
mod blob_cache;
mod decltype_accessor;
#[path = "db_interpose_column_decltype.rs"]
mod decltype_cache;
mod fake_value_slot;
mod metadata_accessors;
mod metadata_support;
mod resolve_tables;
mod scalar_accessors;
mod support;
mod text_accessor;
mod type_accessor;
mod value_accessor;

use crate::db_interpose_common::{
    get_orig_sqlite3_column_blob, get_orig_sqlite3_column_bytes, get_orig_sqlite3_column_count,
    get_orig_sqlite3_column_decltype, get_orig_sqlite3_column_double, get_orig_sqlite3_column_int,
    get_orig_sqlite3_column_int64, get_orig_sqlite3_column_name, get_orig_sqlite3_column_text,
    get_orig_sqlite3_column_type, get_orig_sqlite3_column_value, get_orig_sqlite3_data_count,
    tls_column_type_calls_ptr, tls_in_resolve_tables_ptr, tls_last_query_ptr, PgFakeValue,
    GLOBAL_COLUMN_TYPE_CALLS, MAX_FAKE_VALUES, PG_FAKE_VALUE_MAGIC,
};
use crate::db_interpose_conn_utils::{
    cstr_prefix, cstr_to_string_or, log_debug, log_error, log_info, PthreadMutexGuard,
};
use crate::db_interpose_helpers::PGresult as PgResultHelpers;
use crate::db_interpose_trace_helpers::{list_any_token_in_haystack, list_contains_idx};
use crate::db_interpose_value_helpers::{
    pg_oid_to_sqlite_type_impl, pg_text_to_double_impl, pg_text_to_int64_impl, pg_text_to_int_impl,
};
use crate::env_utils;
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value, PgConnection, PgStmt};
use crate::libpq_helpers::PGresult as PgResultLibpq;
use badcast::{trace_badcast_log_ctx, trace_badcast_should_log, trace_badcast_should_log_col};
use binary_accessors::{column_blob_impl, column_bytes_impl};
pub(crate) use binary_accessors::transformed_text_blob;
use blob_cache::pg_decode_bytea_cached_impl;
pub use blob_cache::rust_pg_decode_bytea_cached;
use decltype_accessor::column_decltype_impl;
use decltype_cache::{lookup_decltype_direct, lookup_sqlite_decltype};
use fake_value_slot::allocate_fake_sqlite_value;
use metadata_accessors::{column_count_impl, column_name_impl, data_count_impl};
#[allow(unused_imports)]
use metadata_support::{
    ensure_pg_result_for_metadata, mask_collection_metadata_type, set_metadata_result_state,
};
use resolve_tables::resolve_column_tables_impl;
use scalar_accessors::{column_double_impl, column_int64_impl, column_int_impl};
use support::{
    helpers_result_ptr, next_text_buffer_index, sqlite_type_name, validate_type_consistency,
};
use text_accessor::column_text_impl;
use type_accessor::column_type_impl;
use value_accessor::column_value_impl;

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_NULL: c_int = 5;

const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;

const DECLTYPE_MAX_KEY_LEN: usize = 128;
const NUM_TEXT_BUFFERS: usize = 64;
const TEXT_BUFFER_SIZE: usize = 8192;

const INVALID_OID: u32 = 0;
const PG_DECLTYPE_CASE_NULL: c_int = 1;
const PG_DECLTYPE_CASE_DT_INTEGER_8: c_int = 2;

const PMT_COLUMN_CACHED_BLOB_ALLOC: c_int = 3;
const PMT_COLUMN_DECODED_BLOB_ALLOC: c_int = 4;

static DECLTYPE_DT_INTEGER_8: &[u8] = b"dt_integer(8)\0";
static _DECLTYPE_INTEGER: &[u8] = b"INTEGER\0";
static _DECLTYPE_BIGINT: &[u8] = b"BIGINT\0";
static _NEEDLE_TYPE: &[u8] = b"type\0";
static _NEEDLE_METADATA_TYPE: &[u8] = b"metadata_type\0";

thread_local! {
    // Use vec![].into_boxed_slice() to allocate on the heap directly.
    // Box::new([T; 64*8192]) would place 512KB on the stack, exceeding
    // Plex's 544K worker thread stacks.
    static COLUMN_TEXT_BUFFERS: RefCell<Box<[[u8; TEXT_BUFFER_SIZE]]>> =
        RefCell::new(vec![[0u8; TEXT_BUFFER_SIZE]; NUM_TEXT_BUFFERS].into_boxed_slice());
    static COLUMN_TEXT_BUF_IDX: Cell<usize> = Cell::new(0);
}

use crate::pg_statement::c_abi::pg_find_any_stmt;

extern "C" {
    fn pg_get_thread_connection(db_path: *const c_char) -> *mut PgConnection;
    fn pg_get_thread_connection_excluding(
        db_path: *const c_char,
        exclude_conn: *const c_void,
    ) -> *mut PgConnection;
    fn pg_stmt_cache_lookup(
        conn: *mut PgConnection,
        sql_hash: u64,
        stmt_name_out: *mut *const c_char,
    ) -> c_int;
    fn pg_stmt_cache_add(
        conn: *mut PgConnection,
        sql_hash: u64,
        stmt_name: *const c_char,
        param_count: c_int,
    ) -> c_int;
    fn pg_is_duplicate_prepared_stmt(res: *mut PgResultLibpq) -> c_int;

    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
}

#[no_mangle]
pub extern "C" fn rust_resolve_column_tables(
    pg_stmt: *mut PgStmt,
    pg_conn: *mut PgConnection,
) -> c_int {
    resolve_column_tables_impl(pg_stmt, pg_conn)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    column_count_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_type(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    column_type_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_int(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    column_int_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_int64(p_stmt: *mut sqlite3_stmt, idx: c_int) -> i64 {
    column_int64_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_double(p_stmt: *mut sqlite3_stmt, idx: c_int) -> f64 {
    column_double_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_text(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_uchar {
    column_text_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_blob(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_void {
    column_blob_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_bytes(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    column_bytes_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_name(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    column_name_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_decltype(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    column_decltype_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_value(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *mut sqlite3_value {
    column_value_impl(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_data_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    data_count_impl(p_stmt)
}

#[cfg(test)]
mod tests {
    use super::{
        next_text_buffer_index, rust_my_sqlite3_column_bytes, rust_my_sqlite3_column_count,
        rust_my_sqlite3_column_decltype, rust_my_sqlite3_column_text, set_metadata_result_state,
        COLUMN_TEXT_BUF_IDX, NUM_TEXT_BUFFERS,
    };
    use crate::db_interpose_common::{orig_sqlite3_column_count, orig_sqlite3_column_decltype};
    use crate::ffi_types::{sqlite3_stmt, PgConnection, PgStmt};
    use crate::libpq_helpers::PGresult as PgResultLibpq;
    use crate::pg_query_cache::{CachedResult, CachedRow};
    use crate::pg_statement::{
        rust_stmt_create, rust_stmt_free, rust_stmt_register, rust_stmt_unregister,
    };
    use std::collections::HashSet;
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int};
    use std::sync::atomic::AtomicI32;

    fn reset_text_buffer_idx() {
        COLUMN_TEXT_BUF_IDX.with(|idx| idx.set(0));
    }

    fn make_stmt() -> *mut PgStmt {
        let sql = CString::new("SELECT 1").unwrap();
        let stmt = rust_stmt_create(std::ptr::null_mut(), sql.as_ptr(), std::ptr::null_mut());
        assert!(!stmt.is_null());
        stmt
    }

    // Plex keeps a DVR device's channel map in
    // media_provider_resources.extra_data: 16,685 bytes for a block of 300
    // channels (kind-cluster-plex, 2026-10-14). The text accessor copied
    // every value into an 8 KiB buffer, so Plex read the JSON cut at 8,191
    // bytes, parsed nothing and listed the DVR with no channels. SQLite
    // returns a column's whole value.
    #[test]
    fn column_text_returns_a_value_longer_than_its_buffer_whole() {
        let stmt = make_stmt();
        let shadow_stmt = 0x30000usize as *mut sqlite3_stmt;
        let long = format!("{{\"pv:channelMapping\":\"{}\"}}", "M".repeat(20_000));
        let value = CString::new(long.clone()).unwrap();
        let col = CString::new("extra_data").unwrap();
        let mut values = [value.as_ptr() as *mut c_char];
        let mut lengths = [long.len() as i32];
        let mut is_null = [0i32];
        let mut row = CachedRow {
            values: values.as_mut_ptr(),
            lengths: lengths.as_mut_ptr(),
            is_null: is_null.as_mut_ptr(),
        };
        let mut types = [25u32];
        let mut names = [col.as_ptr() as *mut c_char];
        let mut cached = CachedResult {
            cache_key: 1,
            created_ms: 0,
            ref_count: AtomicI32::new(1),
            num_rows: 1,
            num_cols: 1,
            col_types: types.as_mut_ptr(),
            col_names: names.as_mut_ptr(),
            rows: &mut row,
            hit_count: 0,
        };
        unsafe {
            let s = &mut *stmt;
            s.is_pg = 2;
            s.cached_result = &mut cached;
            s.current_row = 0;
        }
        rust_stmt_register(shadow_stmt as usize, stmt as usize);

        let got = rust_my_sqlite3_column_text(shadow_stmt, 0);
        let text = unsafe { CStr::from_ptr(got as *const c_char) }
            .to_str()
            .unwrap();
        assert_eq!(
            text.len(),
            long.len(),
            "the whole value, not the buffer's 8,191 bytes"
        );
        assert_eq!(text, long);
        assert_eq!(
            rust_my_sqlite3_column_bytes(shadow_stmt, 0) as usize,
            long.len()
        );

        rust_stmt_unregister(shadow_stmt as usize);
        unsafe {
            (&mut *stmt).cached_result = std::ptr::null_mut();
        }
        rust_stmt_free(stmt);
    }

    #[repr(C)]
    struct PgResAttDesc {
        name: *const c_char,
        tableid: u32,
        columnid: c_int,
        format: c_int,
        typid: u32,
        typlen: c_int,
        atttypmod: c_int,
    }

    extern "C" {
        fn PQmakeEmptyPGresult(conn: *mut std::ffi::c_void, status: c_int) -> *mut PgResultLibpq;
        fn PQsetResultAttrs(res: *mut PgResultLibpq, n: c_int, descs: *mut PgResAttDesc) -> c_int;
        fn PQsetvalue(
            res: *mut PgResultLibpq,
            tup: c_int,
            field: c_int,
            value: *const c_char,
            len: c_int,
        ) -> c_int;
        fn PQclear(res: *mut PgResultLibpq);
    }

    // The same, read straight from libpq's result rather than the cache.
    #[test]
    fn column_text_returns_a_long_live_value_whole() {
        let stmt = make_stmt();
        let shadow_stmt = 0x40000usize as *mut sqlite3_stmt;
        let long = format!("{{\"pv:channelMapping\":\"{}\"}}", "M".repeat(20_000));
        let value = CString::new(long.clone()).unwrap();
        let col = CString::new("extra_data").unwrap();
        let res = unsafe { PQmakeEmptyPGresult(std::ptr::null_mut(), 2) };
        assert!(!res.is_null());
        let mut desc = PgResAttDesc {
            name: col.as_ptr(),
            tableid: 0,
            columnid: 0,
            format: 0,
            typid: 25,
            typlen: -1,
            atttypmod: -1,
        };
        unsafe {
            assert_eq!(PQsetResultAttrs(res, 1, &mut desc), 1);
            assert_eq!(
                PQsetvalue(res, 0, 0, value.as_ptr(), long.len() as c_int),
                1
            );
            let s = &mut *stmt;
            s.is_pg = 2;
            s.result = res;
            s.num_cols = 1;
            s.num_rows = 1;
            s.current_row = 0;
        }
        rust_stmt_register(shadow_stmt as usize, stmt as usize);

        let got = rust_my_sqlite3_column_text(shadow_stmt, 0);
        let text = unsafe { CStr::from_ptr(got as *const c_char) }
            .to_str()
            .unwrap();
        assert_eq!(
            text.len(),
            long.len(),
            "the whole value, not the buffer's 8,191 bytes"
        );
        assert_eq!(text, long);

        rust_stmt_unregister(shadow_stmt as usize);
        unsafe {
            (&mut *stmt).result = std::ptr::null_mut();
            PQclear(res);
        }
        rust_stmt_free(stmt);
    }

    // sqlite3_column_text's pointer lives until the statement's next step,
    // reset or finalize. The shim's lived until its 64-slot thread ring came
    // round, so a statement with many columns, or Plex re-reading a column
    // after sixty others, found the first one overwritten.
    #[test]
    fn column_text_pointer_survives_the_ring_going_round() {
        let stmt = make_stmt();
        let shadow_stmt = 0x50000usize as *mut sqlite3_stmt;
        let a = CString::new("first column").unwrap();
        let b = CString::new("second column").unwrap();
        let col_a = CString::new("a").unwrap();
        let col_b = CString::new("b").unwrap();
        let res = unsafe { PQmakeEmptyPGresult(std::ptr::null_mut(), 2) };
        assert!(!res.is_null());
        let mut descs = [
            PgResAttDesc {
                name: col_a.as_ptr(),
                tableid: 0,
                columnid: 0,
                format: 0,
                typid: 25,
                typlen: -1,
                atttypmod: -1,
            },
            PgResAttDesc {
                name: col_b.as_ptr(),
                tableid: 0,
                columnid: 0,
                format: 0,
                typid: 25,
                typlen: -1,
                atttypmod: -1,
            },
        ];
        unsafe {
            assert_eq!(PQsetResultAttrs(res, 2, descs.as_mut_ptr()), 1);
            assert_eq!(PQsetvalue(res, 0, 0, a.as_ptr(), 12), 1);
            assert_eq!(PQsetvalue(res, 0, 1, b.as_ptr(), 13), 1);
            let s = &mut *stmt;
            s.is_pg = 2;
            s.result = res;
            s.num_cols = 2;
            s.num_rows = 1;
            s.current_row = 0;
        }
        rust_stmt_register(shadow_stmt as usize, stmt as usize);

        let first = rust_my_sqlite3_column_text(shadow_stmt, 0);
        for _ in 0..(NUM_TEXT_BUFFERS * 2) {
            rust_my_sqlite3_column_text(shadow_stmt, 1);
        }
        let text = unsafe { CStr::from_ptr(first as *const c_char) }
            .to_str()
            .unwrap();
        assert_eq!(text, "first column", "overwritten by the ring");
        assert_eq!(
            rust_my_sqlite3_column_text(shadow_stmt, 0),
            first,
            "the same pointer for the same row, as SQLite gives"
        );

        // The next row (as a step leaves it) lets the copies go.
        unsafe {
            (&mut *stmt).current_row = 1;
            (&mut *stmt).num_rows = 2;
            assert_eq!(PQsetvalue(res, 1, 0, b.as_ptr(), 13), 1);
            assert_eq!(PQsetvalue(res, 1, 1, a.as_ptr(), 12), 1);
        }
        let next = rust_my_sqlite3_column_text(shadow_stmt, 0);
        assert_eq!(
            unsafe { CStr::from_ptr(next as *const c_char) }
                .to_str()
                .unwrap(),
            "second column"
        );

        rust_stmt_unregister(shadow_stmt as usize);
        unsafe {
            (&mut *stmt).result = std::ptr::null_mut();
            PQclear(res);
        }
        rust_stmt_free(stmt);
    }

    #[test]
    fn column_text_buffer_wraps_after_num_buffers() {
        reset_text_buffer_idx();

        let mut indices = Vec::with_capacity(NUM_TEXT_BUFFERS);
        for _ in 0..NUM_TEXT_BUFFERS {
            indices.push(next_text_buffer_index());
        }

        let unique: HashSet<usize> = indices.iter().copied().collect();
        assert_eq!(unique.len(), NUM_TEXT_BUFFERS);

        let wrapped = next_text_buffer_index();
        assert_eq!(wrapped, 0);
    }

    #[test]
    fn column_text_buffer_thread_local_indices_start_at_zero() {
        reset_text_buffer_idx();
        assert_eq!(next_text_buffer_index(), 0);

        let child_first = std::thread::spawn(|| {
            COLUMN_TEXT_BUF_IDX.with(|idx| idx.set(0));
            next_text_buffer_index()
        })
        .join()
        .expect("thread should join");

        assert_eq!(child_first, 0);
    }

    #[test]
    fn metadata_result_state_assigns_result_owner() {
        let stmt = make_stmt();
        let result = 0x1234usize as *mut PgResultLibpq;
        // A real allocation rather than a sentinel: holding a connection now
        // counts a reference on it, so the pointer has to be one that can be
        // dereferenced.
        let exec_conn =
            unsafe { libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection };
        assert!(!exec_conn.is_null());
        crate::ffi_types::conn_ref(exec_conn); // stand in for the pool slot

        unsafe {
            set_metadata_result_state(&mut *stmt, result, exec_conn, 0, 0);
        }
        let s = unsafe { &mut *stmt };
        assert_eq!(s.result, result);
        assert_eq!(s.result_conn(), exec_conn);
        assert_eq!(s.metadata_only_result, 1);

        s.result = std::ptr::null_mut();
        s.set_result_conn(std::ptr::null_mut());

        rust_stmt_free(stmt);
        assert!(
            crate::ffi_types::conn_unref(exec_conn),
            "the statement gave its reference back, so this frees it"
        );
    }

    extern "C" fn shadow_column_count_stub(_: *mut sqlite3_stmt) -> c_int {
        1
    }

    extern "C" fn shadow_column_decltype_stub(_: *mut sqlite3_stmt, _: c_int) -> *const c_char {
        c"INTEGER".as_ptr()
    }

    #[test]
    fn pg_column_count_without_metadata_does_not_fall_back_to_shadow_count() {
        let stmt = make_stmt();
        let shadow_stmt = 0x10000usize as *mut sqlite3_stmt;
        let pg_sql = CString::new(
            "SELECT plugins.*, plugin_prefixes.* FROM plugins \
             JOIN plugin_prefixes ON plugin_prefixes.plugin_id = plugins.id \
             WHERE plugin_prefixes.prefix = $1",
        )
        .unwrap();

        let old = unsafe { orig_sqlite3_column_count };
        unsafe {
            orig_sqlite3_column_count = Some(shadow_column_count_stub);
            let s = &mut *stmt;
            s.is_pg = 2;
            s.pg_sql = libc::strdup(pg_sql.as_ptr());
        }
        rust_stmt_register(shadow_stmt as usize, stmt as usize);

        let count = rust_my_sqlite3_column_count(shadow_stmt);
        assert_eq!(count, 0);

        rust_stmt_unregister(shadow_stmt as usize);
        unsafe {
            orig_sqlite3_column_count = old;
        }
        rust_stmt_free(stmt);
    }

    // Nothing is known about a column with no result: SQLite answers NULL
    // for an expression column, and SOCI then types it from the value.
    #[test]
    fn pg_column_decltype_without_metadata_is_null_not_the_shadows_or_text() {
        let stmt = make_stmt();
        let shadow_stmt = 0x20000usize as *mut sqlite3_stmt;
        let pg_sql = CString::new(
            "SELECT plugins.*, plugin_prefixes.* FROM plugins \
             JOIN plugin_prefixes ON plugin_prefixes.plugin_id = plugins.id \
             WHERE plugin_prefixes.prefix = $1",
        )
        .unwrap();

        let old = unsafe { orig_sqlite3_column_decltype };
        unsafe {
            orig_sqlite3_column_decltype = Some(shadow_column_decltype_stub);
            let s = &mut *stmt;
            s.is_pg = 2;
            s.pg_sql = libc::strdup(pg_sql.as_ptr());
        }
        rust_stmt_register(shadow_stmt as usize, stmt as usize);

        let decl = rust_my_sqlite3_column_decltype(shadow_stmt, 0);
        assert!(decl.is_null(), "answered {:?}", unsafe {
            CStr::from_ptr(decl)
        });

        rust_stmt_unregister(shadow_stmt as usize);
        unsafe {
            orig_sqlite3_column_decltype = old;
        }
        rust_stmt_free(stmt);
    }
    #[test]
    fn uri_text_has_consistent_blob_text_and_bytes_in_both_result_paths() {
        use super::rust_my_sqlite3_column_blob;
        let original = "server://m/com.plexapp.plugins.library/library/sections/2/all?type=2";
        let rewritten = "library:///directory/%2Flibrary%2Fsections%2F2%2Fall%3Ftype%3D2";
        for cached_path in [false, true] {
            for bytes_first in [false, true] {
                for oid in [25_u32, 17_u32] {
                    let stmt = make_stmt();
                    let mut key = Box::new(0u8);
                    let shadow = (&mut *key as *mut u8).cast::<sqlite3_stmt>();
                    let value = CString::new(original).unwrap();
                    let name = CString::new("extra_data").unwrap();
                    let mut values = [value.as_ptr() as *mut c_char];
                    let mut lengths = [original.len() as c_int];
                    let mut nulls = [0];
                    let mut row = CachedRow { values: values.as_mut_ptr(), lengths: lengths.as_mut_ptr(), is_null: nulls.as_mut_ptr() };
                    let mut types = [oid]; let mut names = [name.as_ptr() as *mut c_char];
                    let mut cached = CachedResult { cache_key: 1, created_ms: 0, ref_count: AtomicI32::new(1), num_rows: 1, num_cols: 1, col_types: types.as_mut_ptr(), col_names: names.as_mut_ptr(), rows: &mut row, hit_count: 0 };
                    let mut res = std::ptr::null_mut();
                    unsafe {
                        let s = &mut *stmt; s.is_pg = 2; s.num_cols = 1; s.num_rows = 1; s.current_row = 0; s.ensure_column_capacity(1);
                        if cached_path { s.cached_result = &mut cached; }
                        else {
                            res = PQmakeEmptyPGresult(std::ptr::null_mut(), 2);
                            let mut desc = PgResAttDesc { name: name.as_ptr(), tableid: 0, columnid: 0, format: 0, typid: oid, typlen: -1, atttypmod: -1 };
                            assert_eq!(PQsetResultAttrs(res, 1, &mut desc), 1);
                            assert_eq!(PQsetvalue(res, 0, 0, value.as_ptr(), original.len() as c_int), 1);
                            s.result = res;
                        }
                    }
                    rust_stmt_register(shadow as usize, stmt as usize);
                    let expected = if oid == 17 { original } else { rewritten };
                    if bytes_first { assert_eq!(rust_my_sqlite3_column_bytes(shadow, 0) as usize, expected.len()); }
                    let blob = rust_my_sqlite3_column_blob(shadow, 0);
                    assert!(!blob.is_null());
                    let len = rust_my_sqlite3_column_bytes(shadow, 0);
                    assert_eq!(len as usize, expected.len());
                    assert_eq!(unsafe { std::slice::from_raw_parts(blob.cast::<u8>(), len as usize) }, expected.as_bytes());
                    if oid != 17 {
                        let text = rust_my_sqlite3_column_text(shadow, 0);
                        assert_eq!(unsafe { CStr::from_ptr(text.cast()) }.to_str().unwrap(), expected);
                    } else { assert!(unsafe { &*stmt }.owned_column_text.is_empty()); }
                    if oid != 17 && !cached_path {
                        let fake = super::rust_my_sqlite3_column_value(shadow, 0);
                        assert!(!fake.is_null());
                        assert_eq!(crate::db_interpose_value::rust_my_sqlite3_value_bytes(fake) as usize, expected.len());
                        let text = crate::db_interpose_value::rust_my_sqlite3_value_text(fake);
                        assert_eq!(unsafe { CStr::from_ptr(text.cast()) }.to_str().unwrap(), expected);
                        let blob = crate::db_interpose_value::rust_my_sqlite3_value_blob(fake);
                        assert_eq!(unsafe { std::slice::from_raw_parts(blob.cast::<u8>(), expected.len()) }, expected.as_bytes());
                    }
                    rust_stmt_unregister(shadow as usize);
                    unsafe { (&mut *stmt).cached_result = std::ptr::null_mut(); (&mut *stmt).result = std::ptr::null_mut(); if !res.is_null() { PQclear(res); } }
                    rust_stmt_free(stmt);
                }
            }
        }
    }

}
