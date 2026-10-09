use super::*;
use crate::log_debug_lazy;

pub(super) unsafe fn pg_map_param_index(
    pg_stmt: *mut PgStmt,
    p_stmt: *mut sqlite3_stmt,
    sqlite_idx: c_int,
) -> c_int {
    if pg_stmt.is_null() {
        log_debug_lazy!(
            "pg_map_param_index: no pg_stmt, using direct mapping idx={} -> {}",
            sqlite_idx,
            sqlite_idx - 1
        );
        return sqlite_idx - 1;
    }
    let s = &*pg_stmt;

    if !s.param_names.is_null() && s.param_count > 0 {
        // For PG-routed non-cached stmts, read param name directly from PgStmt
        // to avoid LD_PRELOAD re-entry of orig_sqlite3_bind_parameter_name.
        let param_name = if s.is_pg != 0
            && s.is_cached == 0
            && sqlite_idx > 0
            && (sqlite_idx as usize) <= s.param_count as usize
        {
            *s.param_names.add((sqlite_idx - 1) as usize)
        } else {
            crate::db_interpose_metadata::rust_my_sqlite3_bind_parameter_name(p_stmt, sqlite_idx)
        };
        log_debug_lazy!(
            "pg_map_param_index: sqlite_idx={}, param_name={}, param_count={}",
            sqlite_idx,
            cstr_to_string_or(param_name, "NULL"),
            s.param_count
        );

        if !param_name.is_null() {
            let mut clean_name = param_name;
            if *param_name == b':' as c_char {
                clean_name = param_name.add(1);
            }

            let param_count = s.param_count as usize;
            let max_debug = param_count.min(5);
            for i in 0..max_debug {
                let cur = *s.param_names.add(i);
                log_debug_lazy!("  param_names[{}] = {}", i, cstr_to_string_or(cur, "NULL"));
            }

            for i in 0..param_count {
                let cur = *s.param_names.add(i);
                if !cur.is_null() && libc::strcmp(cur, clean_name) == 0 {
                    log_debug_lazy!("  -> Found match at pg_idx={}", i);
                    return i as c_int;
                }
            }
            log_debug_lazy!(
                "Named parameter '{}' not found in translation (sqlite_idx={})",
                cstr_to_string_or(clean_name, "NULL"),
                sqlite_idx
            );
        } else {
            log_debug("  -> No parameter name, using direct mapping");
        }
    } else {
        log_debug_lazy!(
            "pg_map_param_index: no param_names (count={}), using direct mapping idx={} -> {}",
            s.param_count,
            sqlite_idx,
            sqlite_idx - 1
        );
    }

    sqlite_idx - 1
}

pub(super) fn note_bind_phase(phase: &[u8], p_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) {
    let mut sql: *const c_char = ptr::null();
    let mut db: *mut sqlite3 = ptr::null_mut();

    if !pg_stmt.is_null() {
        let s = unsafe { &*pg_stmt };
        sql = if !s.pg_sql.is_null() { s.pg_sql } else { s.sql };
    }

    if sql.is_null() {
        if let Some(f) = get_orig_sqlite3_sql() {
            sql = unsafe { f(p_stmt) };
        }
    }

    if let Some(f) = get_orig_sqlite3_db_handle() {
        db = unsafe { f(p_stmt) };
    }
    pg_exception_note_phase(
        phase.as_ptr() as *const c_char,
        sql,
        p_stmt as *const c_void,
        db as *const c_void,
    );
}

pub(super) fn bind_reset_disabled() -> bool {
    let cached = BIND_RESET_DISABLED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let name = b"PLEX_PG_DISABLE_BIND_RESET\0";
    let val = unsafe {
        let env = libc::getenv(name.as_ptr() as *const c_char);
        crate::db_interpose_helpers::rust_env_truthy(env)
    };
    let flag = if val != 0 { 1 } else { 0 };
    BIND_RESET_DISABLED.store(flag, Ordering::Relaxed);
    flag == 1
}

pub(super) fn contains_binary_bytes(data: *const u8, len: usize) -> bool {
    crate::db_interpose_helpers::rust_contains_binary_bytes(data, len) != 0
}

pub(super) unsafe fn bytes_to_pg_hex(data: *const u8, len: usize) -> *mut c_char {
    let hex_rust = crate::db_interpose_helpers::rust_bytes_to_pg_hex(data, len);
    if hex_rust.is_null() {
        return ptr::null_mut();
    }
    let hex = libc::strdup(hex_rust);
    crate::db_interpose_helpers::rust_free_cstring(hex_rust);
    if hex.is_null() {
        return ptr::null_mut();
    }
    if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
        let bytes = libc::strlen(hex) as u64 + 1;
        crate::pg_mem_telemetry::rust_mem_telemetry_add(PMT_BIND_HEX_ALLOC, bytes, 1);
    }
    hex
}

pub(super) fn should_reset_stmt(pg_stmt: *mut PgStmt) -> bool {
    if bind_reset_disabled() || pg_stmt.is_null() {
        return false;
    }
    let s = unsafe { &*pg_stmt };
    s.is_pg != 0
}

/// Returns true if this statement is PG-routed (read/write, not skip/no-op)
/// and non-cached, meaning bind/reset/clear should skip orig_sqlite3_* calls.
/// Excludes is_pg==3 (skip-SQL like BEGIN/COMMIT/PRAGMA) which are effectively
/// bindless and don't need the deadlock guard.
pub(super) fn is_pg_routed_noncached(pg_stmt: *mut PgStmt) -> bool {
    if pg_stmt.is_null() {
        return false;
    }
    let s = unsafe { &*pg_stmt };
    (s.is_pg == 1 || s.is_pg == 2) && s.is_cached == 0
}

/// When skipping orig_sqlite3_bind_text/blob for PG-routed stmts,
/// invoke the destructor if it's a custom function pointer.
/// SQLITE_STATIC = NULL, SQLITE_TRANSIENT = (void(*)(void*))-1
pub(super) unsafe fn invoke_destructor_if_custom(val: *const c_void, destructor: *mut c_void) {
    if destructor.is_null() || destructor as usize == usize::MAX {
        return;
    }
    let dtor: unsafe extern "C" fn(*mut c_void) =
        std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void)>(destructor);
    dtor(val as *mut c_void);
}

pub(super) unsafe fn wait_for_stmt_ready(p_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) -> bool {
    if should_reset_stmt(pg_stmt) && !is_pg_routed_noncached(pg_stmt) {
        if let Some(f) = get_orig_sqlite3_reset() {
            f(p_stmt);
        }
    }
    libc::usleep(500);
    true
}

pub(super) unsafe fn ensure_stmt_not_busy(p_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) {
    if should_reset_stmt(pg_stmt) && !is_pg_routed_noncached(pg_stmt) {
        if let Some(f) = get_orig_sqlite3_reset() {
            f(p_stmt);
        }
    }
}

pub(super) unsafe fn clear_metadata_result_if_needed(pg_stmt: *mut PgStmt) {
    if pg_stmt.is_null() {
        return;
    }
    let s = &mut *pg_stmt;
    if s.metadata_only_result != 0 && !s.result.is_null() {
        log_debug("BIND: Marking metadata-only result for re-execution with bound params");
        s.metadata_only_result = 2;
    }
}

#[allow(dead_code)]
pub(super) unsafe fn is_preallocated_buffer(stmt: *mut PgStmt, idx: usize) -> bool {
    if stmt.is_null() {
        return false;
    }
    (&*stmt).is_preallocated_buffer(idx)
}

pub(super) unsafe fn retry_on_misuse<F>(
    mut rc: c_int,
    p_stmt: *mut sqlite3_stmt,
    pg_stmt: *mut PgStmt,
    mut bind_call: F,
) -> c_int
where
    F: FnMut() -> c_int,
{
    if rc != SQLITE_MISUSE {
        return rc;
    }
    if is_pg_routed_noncached(pg_stmt) {
        return SQLITE_OK;
    }
    for _ in 0..3 {
        if wait_for_stmt_ready(p_stmt, pg_stmt) {
            rc = bind_call();
            if rc == SQLITE_OK {
                break;
            }
        }
        if rc != SQLITE_MISUSE {
            break;
        }
    }
    rc
}

pub(super) unsafe fn begin_bind(
    phase: &[u8],
    p_stmt: *mut sqlite3_stmt,
) -> (*mut PgStmt, Option<StmtGuard>) {
    let pg_stmt = pg_find_any_stmt(p_stmt);
    note_bind_phase(phase, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(PgStmt::lock_mutex(pg_stmt))
    } else {
        None
    };

    clear_metadata_result_if_needed(pg_stmt);
    ensure_stmt_not_busy(p_stmt, pg_stmt);
    (pg_stmt, guard)
}

pub(super) unsafe fn mapped_param_index(
    pg_stmt: *mut PgStmt,
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> Option<usize> {
    if pg_stmt.is_null() || idx <= 0 {
        return None;
    }
    let stmt = &*pg_stmt;
    let param_len = stmt.param_values.len();
    if param_len == 0 {
        log_debug("mapped_param_index: param_values Vec is empty (not sized yet)");
        return None;
    }
    let pg_idx = pg_map_param_index(pg_stmt, p_stmt, idx);
    if pg_idx < 0 || (pg_idx as usize) >= param_len {
        return None;
    }
    Some(pg_idx as usize)
}

/// Make parameter `pg_idx` NULL, freeing a value a bind allocated. Every bind
/// starts here, so a slot never carries the previous binding's value, length
/// or format into the next. A value in the preallocated buffer (an int or a
/// double) must be forgotten as well as a heap one: when it was left in
/// place, `sqlite3_bind_null` was a no-op after an integer, and Plex's reused
/// `UPDATE metadata_items` sent the previous row's `parent_id` -- an episode's
/// season -- for the movie or show it saved next.
pub(super) unsafe fn clear_param_value(pg_stmt: *mut PgStmt, pg_idx: usize) {
    let stmt = &mut *pg_stmt;
    if pg_idx >= stmt.param_values.len() {
        log_debug("clear_param_value: pg_idx out of bounds");
        return;
    }
    if !stmt.param_values[pg_idx].is_null() && !stmt.is_preallocated_buffer(pg_idx) {
        libc::free(stmt.param_values[pg_idx] as *mut c_void);
    }
    stmt.param_values[pg_idx] = ptr::null_mut();
    if let Some(len) = stmt.param_lengths.get_mut(pg_idx) {
        *len = 0;
    }
    if let Some(format) = stmt.param_formats.get_mut(pg_idx) {
        *format = 0;
    }
}
