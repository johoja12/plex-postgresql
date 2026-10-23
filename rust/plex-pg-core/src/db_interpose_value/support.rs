use super::*;

pub(super) struct FakeValueContext {
    pub pg_stmt: *mut PgStmt,
    pub row: c_int,
    pub col: c_int,
}

pub(super) fn sqlite_type_name(t: c_int) -> &'static str {
    match t {
        SQLITE_INTEGER => "INTEGER",
        SQLITE_FLOAT => "FLOAT",
        SQLITE_TEXT => "TEXT",
        SQLITE_BLOB => "BLOB",
        SQLITE_NULL => "NULL",
        _ => "UNKNOWN",
    }
}

pub(super) fn helpers_result_ptr(result: *mut PgResultLibpq) -> *const PgResultHelpers {
    result as *const PgResultHelpers
}

pub(super) fn fake_value_thread_ok(fake: *const PgFakeValue) -> bool {
    if fake.is_null() {
        return false;
    }
    let f = unsafe { &*fake };
    unsafe { libc::pthread_equal(f.owner_thread, libc::pthread_self()) != 0 }
}

pub(super) unsafe fn load_fake_value_context(
    p_val: *mut sqlite3_value,
    label: &str,
) -> Option<FakeValueContext> {
    if p_val.is_null() {
        return None;
    }

    let fake = pg_check_fake_value(p_val);
    if fake.is_null() {
        return None;
    }
    let f = &*fake;
    if f.pg_stmt.is_null() {
        return None;
    }
    if !fake_value_thread_ok(fake) {
        log_error(&format!(
            "{}: fake value from different thread (stmt={:p})",
            label, f.pg_stmt
        ));
        return None;
    }

    Some(FakeValueContext {
        pg_stmt: f.pg_stmt as *mut PgStmt,
        row: f.row_idx,
        col: f.col_idx,
    })
}

pub(super) unsafe fn fake_value_has_result(ctx: &FakeValueContext) -> bool {
    let s = &*ctx.pg_stmt;
    !s.result.is_null()
        && ctx.row >= 0
        && ctx.row < s.num_rows
        && ctx.col >= 0
        && ctx.col < s.num_cols
}


/// Normalize TEXT reached through sqlite3_column_value/value_* too. Plex's
/// native ORM uses these accessors instead of the SOCI column_* path.
/// The caller holds the statement mutex; the returned storage belongs to it.
pub(super) unsafe fn transformed_fake_text(ctx: &FakeValueContext) -> Option<(*const u8, usize)> {
    let s = &mut *ctx.pg_stmt;
    if ctx.row != s.current_row { return None; }
    let res = helpers_result_ptr(s.result);
    let mut value = ptr::null(); let mut len = 0; let mut is_null = 0;
    crate::db_interpose_helpers::rust_pg_result_value_ptr_len(
        res, ctx.row, ctx.col, &mut value, &mut len, &mut is_null,
    );
    if is_null != 0 || value.is_null() || len < 0 { return None; }
    let oid = crate::db_interpose_helpers::rust_pg_result_col_oid(res, ctx.col);
    let name = crate::db_interpose_helpers::rust_pg_result_col_name(res, ctx.col);
    let out = crate::db_interpose_column::transformed_text_blob(s, ctx.col, oid, name, value, len as usize)?;
    let len = s.owned_column_text[ctx.col as usize].as_ref()?.len().saturating_sub(1);
    Some((out.cast(), len))
}
