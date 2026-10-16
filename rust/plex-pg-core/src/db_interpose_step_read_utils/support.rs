use super::*;

pub(crate) fn cstr_to_str(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "?";
    }
    unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("?") }
}

pub(crate) unsafe fn owned_db_path(conn: *mut PgConnection) -> Option<CString> {
    if conn.is_null() {
        return None;
    }
    let conn = &*conn;
    if conn.db_path[0] == 0 {
        return None;
    }
    Some(CStr::from_ptr(conn.db_path.as_ptr()).to_owned())
}

pub(crate) unsafe fn param_at(param_values: *const *const c_char, idx: usize) -> *const c_char {
    if param_values.is_null() {
        return std::ptr::null();
    }
    *param_values.add(idx)
}

pub(crate) fn bytes_preview(bytes: &[u8], max_len: usize) -> (String, bool, usize) {
    let total_len = bytes.len();
    let cut = total_len.min(max_len);
    let mut out = String::new();
    for &b in &bytes[..cut] {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0 => out.push_str("\\0"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                out.push_str("\\x");
                out.push_str(&format!("{:02x}", b));
            }
        }
    }
    (out, total_len > max_len, total_len)
}

pub(crate) fn is_duplicate_prepared_stmt(res: *mut PGresult) -> bool {
    if res.is_null() {
        return false;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    crate::pg_client::rust_is_duplicate_sqlstate(sqlstate) != 0
}

pub(crate) fn step_read_clear_row_caches(stmt: *mut PgStmt) {
    if stmt.is_null() {
        return;
    }
    let stmt = unsafe { &mut *stmt };
    let num_slots = stmt.cached_text.len().max(stmt.decoded_blobs.len()) as c_int;
    crate::db_interpose_helpers::rust_step_clear_row_caches(
        stmt.cached_text.as_mut_ptr(),
        stmt.cached_blob.as_mut_ptr(),
        stmt.cached_blob_len.as_mut_ptr(),
        stmt.decoded_blobs.as_mut_ptr(),
        stmt.decoded_blob_lens.as_mut_ptr(),
        num_slots,
        &mut stmt.cached_row as *mut c_int,
        &mut stmt.decoded_blob_row as *mut c_int,
    );
}
