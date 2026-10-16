use std::os::raw::c_char;

use crate::db_interpose_helpers::cstr_to_str_or_empty;

/// FNV-1a hash over the bytes of `s`.
///
/// Parameters match the C implementation in `pg_client.c`:
///   - offset basis : 14695981039346656037
///   - prime        : 1099511628211
pub(crate) fn fnv1a_str(s: &str) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

/// Returns `true` when `sqlstate` is exactly `"26000"`.
///
/// The whole stale-plan test, which also takes `0A000` "cached plan must not
/// change result type", is `error_class::is_stale_plan`; this is the
/// SQLSTATE-only half for callers that have no message.
pub(crate) fn is_stale_sqlstate(sqlstate: &str) -> bool {
    super::error_class::is_stale_plan(sqlstate, "")
}

/// Returns `true` when `sqlstate` is exactly `"42P05"`.
pub(crate) fn is_duplicate_sqlstate(sqlstate: &str) -> bool {
    sqlstate == "42P05"
}

#[no_mangle]
pub extern "C" fn rust_hash_sql(sql: *const c_char) -> u64 {
    if sql.is_null() {
        return 0;
    }
    let s = unsafe { cstr_to_str_or_empty(sql) };
    fnv1a_str(s)
}

#[no_mangle]
pub extern "C" fn rust_is_stale_sqlstate(sqlstate: *const c_char) -> i32 {
    let s = unsafe { cstr_to_str_or_empty(sqlstate) };
    i32::from(is_stale_sqlstate(s))
}

#[no_mangle]
pub extern "C" fn rust_is_duplicate_sqlstate(sqlstate: *const c_char) -> i32 {
    let s = unsafe { cstr_to_str_or_empty(sqlstate) };
    i32::from(is_duplicate_sqlstate(s))
}
