use std::os::raw::{c_char, c_int, c_void};

use crate::ffi_types::{sqlite3, sqlite3_stmt};

#[repr(C)]
pub(super) struct SqlTranslation {
    pub(super) sql: *mut c_char,
    pub(super) param_names: *mut *mut c_char,
    pub(super) param_count: c_int,
    pub(super) success: c_int,
    pub(super) error: [c_char; 256],
}

unsafe extern "C" {
    static mut orig_sqlite3_step: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char>;
    static mut orig_sqlite3_db_handle:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut sqlite3>;
    static mut orig_sqlite3_expanded_sql:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut c_char>;
    static mut orig_sqlite3_free: Option<unsafe extern "C" fn(*mut c_void)>;

    fn sqlite3_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3;
    fn sqlite3_sql(stmt: *mut sqlite3_stmt) -> *const c_char;
    fn sqlite3_expanded_sql(stmt: *mut sqlite3_stmt) -> *mut c_char;
    fn sqlite3_free(ptr: *mut c_void);

    pub(super) fn shim_alloc_maybe_log();
    pub(super) fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        p_stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );

    pub(super) fn sql_translate(sql: *const c_char) -> SqlTranslation;
    pub(super) fn sql_translation_free(result: *mut SqlTranslation);
}

pub(super) unsafe fn orig_step(p_stmt: *mut sqlite3_stmt) -> c_int {
    match orig_sqlite3_step {
        Some(f) => crate::db_interpose_stmt_lifecycle::shadow_step::with_shadow_step(
            p_stmt, || f(p_stmt),
        ),
        None => super::SQLITE_ERROR,
    }
}

pub(super) unsafe fn call_sqlite3_sql(p_stmt: *mut sqlite3_stmt) -> *const c_char {
    match orig_sqlite3_sql {
        Some(f) => f(p_stmt),
        None => sqlite3_sql(p_stmt),
    }
}

pub(super) unsafe fn call_sqlite3_db_handle(p_stmt: *mut sqlite3_stmt) -> *mut sqlite3 {
    match orig_sqlite3_db_handle {
        Some(f) => f(p_stmt),
        None => sqlite3_db_handle(p_stmt),
    }
}

pub(super) unsafe fn call_sqlite3_expanded_sql(p_stmt: *mut sqlite3_stmt) -> *mut c_char {
    match orig_sqlite3_expanded_sql {
        Some(f) => f(p_stmt),
        None => sqlite3_expanded_sql(p_stmt),
    }
}

pub(super) unsafe fn call_sqlite3_free(ptr: *mut c_void) {
    if let Some(f) = orig_sqlite3_free {
        f(ptr);
    } else {
        sqlite3_free(ptr);
    }
}
