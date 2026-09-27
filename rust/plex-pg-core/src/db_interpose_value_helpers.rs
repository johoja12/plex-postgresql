use std::os::raw::{c_char, c_int};

pub(crate) const SQLITE_INTEGER_CONST: i32 = 1;
pub(crate) const SQLITE_FLOAT_CONST: i32 = 2;
pub(crate) const SQLITE_TEXT_CONST: i32 = 3;
pub(crate) const SQLITE_BLOB_CONST: i32 = 4;

pub(crate) fn pg_oid_to_sqlite_type_impl(oid: u32) -> i32 {
    match oid {
        20 | 21 | 23 | 26 | 16 => SQLITE_INTEGER_CONST, // int8, int2, int4, oid, bool
        1114 | 1184 => SQLITE_INTEGER_CONST,            // timestamp, timestamptz → epoch int
        700 | 701 | 1700 => SQLITE_FLOAT_CONST,         // float4, float8, numeric
        17 => SQLITE_BLOB_CONST,                        // bytea
        _ => SQLITE_TEXT_CONST,
    }
}

fn is_pg_bool_text_true_false(value: *const c_char) -> Option<i32> {
    // Only the one-byte libpq bool spellings need special handling. Do not
    // strlen every ordinary number before atoi/atoll/atof scans it again.
    unsafe {
        match *value as u8 {
            b't' if *value.add(1) == 0 => Some(1),
            b'f' if *value.add(1) == 0 => Some(0),
            _ => None,
        }
    }
}

pub(crate) fn pg_text_to_int_impl(value: *const c_char) -> c_int {
    if value.is_null() {
        return 0;
    }
    if let Some(v) = is_pg_bool_text_true_false(value) {
        return v;
    }
    unsafe { libc::atoi(value) }
}

pub(crate) fn pg_text_to_int64_impl(value: *const c_char) -> i64 {
    if value.is_null() {
        return 0;
    }
    if let Some(v) = is_pg_bool_text_true_false(value) {
        return v as i64;
    }
    unsafe { libc::atoll(value) }
}

pub(crate) fn pg_text_to_double_impl(value: *const c_char) -> f64 {
    if value.is_null() {
        return 0.0;
    }
    if let Some(v) = is_pg_bool_text_true_false(value) {
        return v as f64;
    }
    unsafe { libc::atof(value) }
}
