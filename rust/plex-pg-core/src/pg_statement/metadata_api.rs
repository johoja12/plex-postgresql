use std::ffi::CString;
use std::os::raw::c_char;

use crate::db_interpose_helpers::cstr_to_str_or_empty;

use super::{
    convert_metadata_settings_upsert, extract_metadata_id, oid_to_sqlite_decltype,
    oid_to_sqlite_type, DECLTYPE_CASE_DT_INTEGER_8, DECLTYPE_CASE_NONE, DECLTYPE_CASE_NULL,
};

fn is_aggregate_expression_name(col: &str) -> bool {
    let col = col.trim();
    if col.is_empty() {
        return false;
    }

    let lower = col.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "count" | "cnt" | "sum" | "max" | "min" | "avg"
    ) || lower.rsplit_once('_').is_some_and(|(_, suffix)| {
        matches!(suffix, "count" | "cnt" | "sum" | "max" | "min" | "avg")
    }) || ["count(", "sum(", "max(", "min(", "avg("]
        .iter()
        .any(|needle| lower.contains(needle))
}

pub fn rust_oid_to_sqlite_type(oid: u32) -> i32 {
    oid_to_sqlite_type(oid)
}

pub fn rust_oid_to_sqlite_decltype(oid: u32) -> *const c_char {
    oid_to_sqlite_decltype(oid).as_ptr()
}

pub fn rust_decltype_special_case(
    oid: u32,
    col_name: *const c_char,
    pg_sql: *const c_char,
    table_oid: u32,
) -> i32 {
    let col = unsafe { cstr_to_str_or_empty(col_name) };

    if oid == 20 && !col.is_empty() {
        if col.contains("_at") || col.contains("timestamp") || col.contains("time") {
            return DECLTYPE_CASE_DT_INTEGER_8;
        }
        if col == "greatest" {
            let sql = unsafe { cstr_to_str_or_empty(pg_sql) };
            if sql.contains("metadata_items.changed_at") {
                return DECLTYPE_CASE_DT_INTEGER_8;
            }
        }
    }

    // SQLite has no declared type for aggregate expressions, including Plex's
    // table-prefixed aliases (count(tags.id) AS tags_count). Exposing PostgreSQL
    // bigint here makes SOCI allocate an int64 holder where Plex requests int,
    // causing std::bad_cast despite column_type correctly reporting INTEGER.
    // Only classify as aggregate when table_oid==0 (expression column).
    // Real table columns named "count", "max", etc. have table_oid != 0
    // and must keep their normal decltype.
    if table_oid == 0 && is_aggregate_expression_name(col) {
        return DECLTYPE_CASE_NULL;
    }

    DECLTYPE_CASE_NONE
}

pub fn rust_convert_metadata_settings_upsert(sql: *const c_char) -> *mut c_char {
    let s = unsafe { cstr_to_str_or_empty(sql) };
    if s.is_empty() {
        return std::ptr::null_mut();
    }
    match convert_metadata_settings_upsert(s) {
        Some(result) => CString::new(result)
            .map(|cs| cs.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

pub fn rust_extract_metadata_id(sql: *const c_char) -> i64 {
    let s = unsafe { cstr_to_str_or_empty(sql) };
    if s.is_empty() {
        return 0;
    }
    extract_metadata_id(s)
}
