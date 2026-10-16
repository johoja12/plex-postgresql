use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

pub(crate) const SQLITE_INTEGER_CONST: i32 = 1;
pub(crate) const SQLITE_FLOAT_CONST: i32 = 2;
pub(crate) const SQLITE_TEXT_CONST: i32 = 3;
pub(crate) const SQLITE_BLOB_CONST: i32 = 4;

pub(crate) fn pg_oid_to_sqlite_type_impl(oid: u32) -> i32 {
    match oid {
        20 | 21 | 23 | 26 | 16 => SQLITE_INTEGER_CONST, // int8, int2, int4, oid, bool
        // timestamp and timestamptz are text: their value is "2024-01-02
        // 03:04:05", which read as INTEGER was atoll'd to 2024.
        1114 | 1184 => SQLITE_TEXT_CONST,
        700 | 701 | 1700 => SQLITE_FLOAT_CONST, // float4, float8, numeric
        17 => SQLITE_BLOB_CONST,                // bytea
        _ => SQLITE_TEXT_CONST,
    }
}

fn is_pg_bool_text_true_false(bytes: &[u8]) -> Option<i32> {
    if bytes.len() == 1 {
        if bytes[0] == b't' {
            return Some(1);
        }
        if bytes[0] == b'f' {
            return Some(0);
        }
    }
    None
}

pub(crate) fn pg_text_to_int_impl(value: *const c_char) -> c_int {
    if value.is_null() {
        return 0;
    }
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    if let Some(v) = is_pg_bool_text_true_false(bytes) {
        return v;
    }
    unsafe { libc::atoi(value) }
}

pub(crate) fn pg_text_to_int64_impl(value: *const c_char) -> i64 {
    if value.is_null() {
        return 0;
    }
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    if let Some(v) = is_pg_bool_text_true_false(bytes) {
        return v as i64;
    }
    unsafe { libc::atoll(value) }
}

pub(crate) fn pg_text_to_double_impl(value: *const c_char) -> f64 {
    if value.is_null() {
        return 0.0;
    }
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    if let Some(v) = is_pg_bool_text_true_false(bytes) {
        return v as f64;
    }
    unsafe { libc::atof(value) }
}
