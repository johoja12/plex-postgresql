use super::*;
use crate::db_interpose_value::support::{
    fake_value_has_result, helpers_result_ptr, load_fake_value_context,
};
use crate::log_debug_lazy;

pub(super) fn value_int_impl(p_val: *mut sqlite3_value) -> c_int {
    unsafe {
        pg_exception_note_phase(
            b"value_int\0".as_ptr() as *const c_char,
            ptr::null(),
            p_val as *mut sqlite3_stmt,
            ptr::null_mut(),
        );
    }

    if p_val.is_null() {
        return 0;
    }

    let Some(ctx) = (unsafe { load_fake_value_context(p_val, "VALUE_INT") }) else {
        return get_orig_sqlite3_value_int()
            .map(|f| unsafe { f(p_val) })
            .unwrap_or(0);
    };

    let _call_num = VALUE_INT_CALLS.fetch_add(1, Ordering::Relaxed);
    let pg_stmt_ref = unsafe { &mut *ctx.pg_stmt };
    let _guard = unsafe { PgStmt::lock_mutex(ctx.pg_stmt) };
    if unsafe { !fake_value_has_result(&ctx) } {
        return 0;
    }

    let result_ptr = pg_stmt_ref.result;
    let result = crate::db_interpose_helpers::rust_pg_result_int(
        helpers_result_ptr(result_ptr),
        ctx.row,
        ctx.col,
    );

    let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
        helpers_result_ptr(result_ptr),
        ctx.col,
    );
    if !col_name.is_null() {
        let needle = NEEDLE_TYPE.as_ptr() as *const c_char;
        if unsafe { !libc::strstr(col_name, needle).is_null() } {
            let mut raw_buf = [0 as c_char; 128];
            let mut raw_val = "?".to_string();
            let raw_len = crate::db_interpose_helpers::rust_pg_result_text_copy(
                helpers_result_ptr(result_ptr),
                ctx.row,
                ctx.col,
                raw_buf.as_mut_ptr(),
                raw_buf.len(),
            );
            if raw_len >= 0 {
                raw_val = cstr_to_string_or(raw_buf.as_ptr(), "?");
            }
            log_debug_lazy!(
                "TYPE_DEBUG_VALUE_INT: col='{}' idx={} row={} raw_val='{}' result={} sql={}",
                cstr_to_string_or(col_name, "?"),
                ctx.col,
                ctx.row,
                raw_val,
                result,
                cstr_prefix(pg_stmt_ref.pg_sql, 200, "?")
            );
        }
    }

    result
}

pub(super) fn value_int64_impl(p_val: *mut sqlite3_value) -> i64 {
    unsafe {
        pg_exception_note_phase(
            b"value_int64\0".as_ptr() as *const c_char,
            ptr::null(),
            p_val as *mut sqlite3_stmt,
            ptr::null_mut(),
        );
    }

    if p_val.is_null() {
        return 0;
    }

    let Some(ctx) = (unsafe { load_fake_value_context(p_val, "VALUE_INT64") }) else {
        return get_orig_sqlite3_value_int64()
            .map(|f| unsafe { f(p_val) })
            .unwrap_or(0);
    };

    let pg_stmt_ref = unsafe { &mut *ctx.pg_stmt };
    let _guard = unsafe { PgStmt::lock_mutex(ctx.pg_stmt) };
    if unsafe { !fake_value_has_result(&ctx) } {
        return 0;
    }

    crate::db_interpose_helpers::rust_pg_result_int64(
        helpers_result_ptr(pg_stmt_ref.result),
        ctx.row,
        ctx.col,
    )
}

pub(super) fn value_double_impl(p_val: *mut sqlite3_value) -> f64 {
    if p_val.is_null() {
        return 0.0;
    }

    let Some(ctx) = (unsafe { load_fake_value_context(p_val, "VALUE_DOUBLE") }) else {
        return get_orig_sqlite3_value_double()
            .map(|f| unsafe { f(p_val) })
            .unwrap_or(0.0);
    };

    let pg_stmt_ref = unsafe { &mut *ctx.pg_stmt };
    let _guard = unsafe { PgStmt::lock_mutex(ctx.pg_stmt) };
    if unsafe { !fake_value_has_result(&ctx) } {
        return 0.0;
    }

    crate::db_interpose_helpers::rust_pg_result_double(
        helpers_result_ptr(pg_stmt_ref.result),
        ctx.row,
        ctx.col,
    )
}

pub(super) fn value_bytes_impl(p_val: *mut sqlite3_value) -> c_int {
    if p_val.is_null() {
        return 0;
    }

    let Some(ctx) = (unsafe { load_fake_value_context(p_val, "VALUE_BYTES") }) else {
        return get_orig_sqlite3_value_bytes()
            .map(|f| unsafe { f(p_val) })
            .unwrap_or(0);
    };

    let pg_stmt_ref = unsafe { &mut *ctx.pg_stmt };
    let _guard = unsafe { PgStmt::lock_mutex(ctx.pg_stmt) };
    if unsafe { !fake_value_has_result(&ctx) } {
        return 0;
    }

    if let Some((_, len)) = unsafe { super::support::transformed_fake_text(&ctx) } {
        return len as c_int;
    }
    let len = crate::db_interpose_helpers::rust_pg_result_length(
        helpers_result_ptr(pg_stmt_ref.result),
        ctx.row,
        ctx.col,
    );
    // Paired with VALUE_TEXT_TRACE: this is the length Plex builds its string
    // from, and the copy above is what it builds it out of. They have to agree.
    crate::log_debug_lazy!(
        "VALUE_BYTES_TRACE: col={} row={} len={}",
        ctx.col,
        ctx.row,
        len
    );
    if len > 0 {
        len
    } else {
        0
    }
}

pub(super) fn value_blob_impl(p_val: *mut sqlite3_value) -> *const c_void {
    if p_val.is_null() {
        return ptr::null();
    }

    let Some(ctx) = (unsafe { load_fake_value_context(p_val, "VALUE_BLOB") }) else {
        return get_orig_sqlite3_value_blob()
            .map(|f| unsafe { f(p_val) })
            .unwrap_or(ptr::null());
    };

    let pg_stmt_ref = unsafe { &mut *ctx.pg_stmt };
    let _guard = unsafe { PgStmt::lock_mutex(ctx.pg_stmt) };
    if unsafe { !fake_value_has_result(&ctx) } {
        return ptr::null();
    }

    if let Some((value, _)) = unsafe { super::support::transformed_fake_text(&ctx) } {
        return value.cast();
    }
    let result_ptr = pg_stmt_ref.result;
    // The decoded blob is never longer than libpq's text for it (bytea hex
    // is twice the bytes), so a slot of that size takes it whole; the slot
    // is this thread's, so no other thread's call overwrites it.
    let raw_len = crate::db_interpose_helpers::rust_pg_result_length(
        helpers_result_ptr(result_ptr),
        ctx.row,
        ctx.col,
    );
    if raw_len <= 0 {
        return ptr::null();
    }
    let (buf, capacity) = value_blob_ring_take(raw_len as usize + 1);
    if buf.is_null() || capacity == 0 {
        return ptr::null();
    }
    let len = crate::db_interpose_helpers::rust_pg_result_blob_copy(
        helpers_result_ptr(result_ptr),
        ctx.row,
        ctx.col,
        buf,
        capacity - 1,
    );
    if len <= 0 {
        return ptr::null();
    }

    buf as *const c_void
}
