use super::*;
use crate::db_interpose_common::{
    CRASH_LAST_COLUMN, CRASH_LAST_COLUMN_LEN, CRASH_LAST_COLUMN_MAX_LEN, CRASH_LAST_COLUMN_SEQ,
};
use crate::db_interpose_value::support::{
    fake_value_has_result, helpers_result_ptr, load_fake_value_context, sqlite_type_name,
};
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(super) fn value_type_impl(p_val: *mut sqlite3_value) -> c_int {
    unsafe {
        pg_exception_note_phase(
            b"value_type\0".as_ptr() as *const c_char,
            ptr::null(),
            p_val as *mut sqlite3_stmt,
            ptr::null_mut(),
        );
        GLOBAL_VALUE_TYPE_CALLS.fetch_add(1, Ordering::Relaxed);
        let tls_calls = tls_value_type_calls_ptr();
        *tls_calls = (*tls_calls).wrapping_add(1);
    }

    if p_val.is_null() {
        return SQLITE_NULL;
    }

    let Some(ctx) = (unsafe { load_fake_value_context(p_val, "VALUE_TYPE") }) else {
        return get_orig_sqlite3_value_type()
            .map(|f| unsafe { f(p_val) })
            .unwrap_or(SQLITE_NULL);
    };

    let call_num = VALUE_TYPE_CALLS.fetch_add(1, Ordering::Relaxed);
    unsafe {
        let tls_query = tls_last_query_ptr();
        *tls_query = (&*ctx.pg_stmt).pg_sql; // accessed before mutex lock
    }

    let pg_stmt_ref = unsafe { &mut *ctx.pg_stmt };
    let _guard = unsafe { PgStmt::lock_mutex(ctx.pg_stmt) };
    if unsafe { !fake_value_has_result(&ctx) } {
        log_info_lazy!(
            "VALUE_TYPE[{}]: FAKE VALUE but no result (row={} col={})",
            call_num,
            ctx.row,
            ctx.col
        );
        return SQLITE_NULL;
    }

    let result_ptr = pg_stmt_ref.result;
    let mut is_null = 0;
    let mut oid = 0u32;
    let mut sqlite_type = SQLITE_NULL;
    let ok = crate::db_interpose_helpers::rust_pg_result_type_info(
        helpers_result_ptr(result_ptr),
        ctx.row,
        ctx.col,
        &mut oid as *mut u32,
        &mut is_null as *mut c_int,
        &mut sqlite_type as *mut c_int,
    );
    let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
        helpers_result_ptr(result_ptr),
        ctx.col,
    );
    // --- seqlock: begin CRASH_LAST_COLUMN write ---
    unsafe {
        let c_seq = CRASH_LAST_COLUMN_SEQ.load(Ordering::Relaxed);
        CRASH_LAST_COLUMN_SEQ.store(c_seq.wrapping_add(1), Ordering::Release);
        let clen = if !col_name.is_null() && *col_name != 0 {
            let mut wrote = libc::snprintf(
                ptr::addr_of_mut!(CRASH_LAST_COLUMN) as *mut c_char,
                CRASH_LAST_COLUMN_MAX_LEN,
                b"%.63s\0".as_ptr() as *const c_char,
                col_name,
            );
            if wrote < 0 {
                wrote = 0;
            }
            if wrote >= CRASH_LAST_COLUMN_MAX_LEN as c_int {
                wrote = CRASH_LAST_COLUMN_MAX_LEN as c_int - 1;
            }
            wrote
        } else {
            CRASH_LAST_COLUMN[0] = 0;
            0
        };
        CRASH_LAST_COLUMN_LEN.store(clen, Ordering::SeqCst);
        CRASH_LAST_COLUMN_SEQ.store(c_seq.wrapping_add(2), Ordering::Release);
    }
    // --- seqlock: end CRASH_LAST_COLUMN write ---

    let result = if ok != 0 { sqlite_type } else { SQLITE_NULL };
    if call_num % 1000 == 0 {
        log_info_lazy!(
            "VALUE_TYPE[{}]: col='{}' row={} OID={} is_null={} -> {} sql={}",
            call_num,
            cstr_to_string_or(col_name, "?"),
            ctx.row,
            oid,
            is_null,
            sqlite_type_name(result),
            cstr_prefix(pg_stmt_ref.pg_sql, 60, "?")
        );
    }
    result
}

pub(super) fn value_text_impl(p_val: *mut sqlite3_value) -> *const c_uchar {
    unsafe {
        pg_exception_note_phase(
            b"value_text\0".as_ptr() as *const c_char,
            ptr::null(),
            p_val as *mut sqlite3_stmt,
            ptr::null_mut(),
        );
    }

    if p_val.is_null() {
        return ptr::null();
    }

    let Some(ctx) = (unsafe { load_fake_value_context(p_val, "VALUE_TEXT") }) else {
        return get_orig_sqlite3_value_text()
            .map(|f| unsafe { f(p_val) })
            .unwrap_or(ptr::null());
    };

    let call_num = VALUE_TEXT_CALLS.fetch_add(1, Ordering::Relaxed);
    let pg_stmt_ref = unsafe { &mut *ctx.pg_stmt };
    let _guard = unsafe { PgStmt::lock_mutex(ctx.pg_stmt) };
    if unsafe { !fake_value_has_result(&ctx) } {
        return ptr::null();
    }

    if let Some((value, _)) = unsafe { super::support::transformed_fake_text(&ctx) } {
        return value;
    }
    let result_ptr = pg_stmt_ref.result;
    let mut value: *const c_char = ptr::null();
    let mut len: c_int = 0;
    let mut is_null: c_int = 0;
    crate::db_interpose_helpers::rust_pg_result_value_ptr_len(
        helpers_result_ptr(result_ptr),
        ctx.row,
        ctx.col,
        &mut value,
        &mut len,
        &mut is_null,
    );
    if is_null != 0 || value.is_null() || len < 0 {
        if call_num % 100 == 0 {
            log_info_lazy!(
                "VALUE_TEXT[{}]: col={} row={} -> NULL (is_null)",
                call_num,
                ctx.col,
                ctx.row
            );
        }
        return ptr::null();
    }

    // The whole value, into this thread's own ring: a 16 KiB static used to
    // cut it, and every thread shared the ring.
    let bytes = unsafe { std::slice::from_raw_parts(value as *const u8, len as usize) };
    let out = value_text_ring_put(bytes);
    if out.is_null() {
        // The thread is tearing down its locals: libpq's own copy, valid
        // while the result is.
        return value as *const c_uchar;
    }

    if call_num % 100 == 0 {
        let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
            helpers_result_ptr(result_ptr),
            ctx.col,
        );
        let suffix = if len > 30 { "..." } else { "" };
        log_info_lazy!(
            "VALUE_TEXT[{}]: col='{}' row={} val='{:.30}{}'",
            call_num,
            cstr_to_string_or(col_name, "?"),
            ctx.row,
            String::from_utf8_lossy(bytes),
            suffix
        );
    }

    // Every call, not every hundredth: a value that comes back wrong is read
    // once, and the sampled line is almost never the one that mattered.
    log_debug_lazy!(
        "VALUE_TEXT_TRACE: col={} row={} copied={}",
        ctx.col,
        ctx.row,
        len
    );

    out as *const c_uchar
}
