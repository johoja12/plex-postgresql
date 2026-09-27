use super::*;
use crate::log_debug_lazy;

static TRACE_DEFAULT_IDX: &[u8] = b"5,6\0";
static TRACE_DEFAULT_THREAD: &[u8] = b"any\0";

static TRACE_BADCAST_INIT: Once = Once::new();
static mut TRACE_BADCAST_ENABLED: c_int = -1;
const TRACE_IDX_BUF_LEN: usize = 256;
const TRACE_SQL_BUF_LEN: usize = 256;
const TRACE_THREAD_BUF_LEN: usize = 128;
const TRACE_COL_BUF_LEN: usize = 128;
static mut TRACE_BADCAST_IDX_BUF: [c_char; TRACE_IDX_BUF_LEN] = [0; TRACE_IDX_BUF_LEN];
static mut TRACE_BADCAST_SQL_BUF: [c_char; TRACE_SQL_BUF_LEN] = [0; TRACE_SQL_BUF_LEN];
static mut TRACE_BADCAST_THREAD_BUF: [c_char; TRACE_THREAD_BUF_LEN] = [0; TRACE_THREAD_BUF_LEN];
static mut TRACE_BADCAST_COL_BUF: [c_char; TRACE_COL_BUF_LEN] = [0; TRACE_COL_BUF_LEN];

static mut TRACE_BADCAST_IDX_LIST: *const c_char = ptr::null();
static mut TRACE_BADCAST_THREAD_SUBSTR: *const c_char = ptr::null();
static mut TRACE_BADCAST_SQL_CONTAINS: *const c_char = ptr::null();
static mut TRACE_BADCAST_COL_CONTAINS: *const c_char = ptr::null();

fn trace_badcast_init() {
    TRACE_BADCAST_INIT.call_once(|| unsafe {
        let enabled_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_ENABLED);
        let idx_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_IDX_BUF) as *mut c_char;
        let thread_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_THREAD_BUF) as *mut c_char;
        let sql_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_SQL_BUF) as *mut c_char;
        let col_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_COL_BUF) as *mut c_char;

        crate::db_interpose_helpers::rust_load_badcast_config(
            enabled_ptr,
            idx_ptr,
            TRACE_IDX_BUF_LEN,
            thread_ptr,
            TRACE_THREAD_BUF_LEN,
            sql_ptr,
            TRACE_SQL_BUF_LEN,
            col_ptr,
            TRACE_COL_BUF_LEN,
        );

        let idx_first = *std::ptr::addr_of!(TRACE_BADCAST_IDX_BUF).cast::<c_char>();
        TRACE_BADCAST_IDX_LIST = if idx_first != 0 {
            std::ptr::addr_of!(TRACE_BADCAST_IDX_BUF) as *const c_char
        } else {
            TRACE_DEFAULT_IDX.as_ptr() as *const c_char
        };

        let thread_first = *std::ptr::addr_of!(TRACE_BADCAST_THREAD_BUF).cast::<c_char>();
        TRACE_BADCAST_THREAD_SUBSTR = if thread_first != 0 {
            std::ptr::addr_of!(TRACE_BADCAST_THREAD_BUF) as *const c_char
        } else {
            TRACE_DEFAULT_THREAD.as_ptr() as *const c_char
        };

        let sql_first = *std::ptr::addr_of!(TRACE_BADCAST_SQL_BUF).cast::<c_char>();
        TRACE_BADCAST_SQL_CONTAINS = if sql_first != 0 {
            std::ptr::addr_of!(TRACE_BADCAST_SQL_BUF) as *const c_char
        } else {
            ptr::null()
        };

        let col_first = *std::ptr::addr_of!(TRACE_BADCAST_COL_BUF).cast::<c_char>();
        TRACE_BADCAST_COL_CONTAINS = if col_first != 0 {
            std::ptr::addr_of!(TRACE_BADCAST_COL_BUF) as *const c_char
        } else {
            ptr::null()
        };
    });
}

pub(super) fn trace_badcast_enabled() -> bool {
    trace_badcast_init();
    unsafe { TRACE_BADCAST_ENABLED != 0 }
}

fn trace_badcast_thread_ok() -> bool {
    trace_badcast_init();
    if !trace_badcast_enabled() {
        return false;
    }
    let substr_ptr = unsafe { TRACE_BADCAST_THREAD_SUBSTR };
    if substr_ptr.is_null() {
        return true;
    }
    let substr = unsafe { CStr::from_ptr(substr_ptr) }.to_string_lossy();
    if substr.is_empty() || substr.eq_ignore_ascii_case("any") {
        return true;
    }

    #[cfg(target_os = "macos")]
    unsafe {
        let mut name_buf = [0u8; 64];
        let rc = libc::pthread_getname_np(
            libc::pthread_self(),
            name_buf.as_mut_ptr() as *mut c_char,
            name_buf.len(),
        );
        if rc != 0 {
            return false;
        }
        let tname = CStr::from_ptr(name_buf.as_ptr() as *const c_char).to_string_lossy();
        if tname.is_empty() {
            return false;
        }
        tname.contains(substr.as_ref())
    }

    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

fn trace_badcast_sql_ok(pg_stmt: *const PgStmt) -> bool {
    trace_badcast_init();
    if !trace_badcast_enabled() {
        return false;
    }
    let sql_filter = unsafe { TRACE_BADCAST_SQL_CONTAINS };
    if sql_filter.is_null() || unsafe { *sql_filter == 0 } {
        return true;
    }
    if pg_stmt.is_null() {
        return false;
    }
    let stmt = unsafe { &*pg_stmt };
    let sql_ptr = stmt.pg_sql;
    if sql_ptr.is_null() {
        return false;
    }
    let list = unsafe { CStr::from_ptr(sql_filter) }.to_string_lossy();
    let haystack = unsafe { CStr::from_ptr(sql_ptr) }.to_string_lossy();
    list_any_token_in_haystack(list.as_ref(), haystack.as_ref())
}

fn trace_badcast_col_ok(col_name: *const c_char) -> bool {
    trace_badcast_init();
    if !trace_badcast_enabled() {
        return false;
    }
    let col_filter = unsafe { TRACE_BADCAST_COL_CONTAINS };
    if col_filter.is_null() || unsafe { *col_filter == 0 } {
        return true;
    }
    if col_name.is_null() {
        return false;
    }
    let list = unsafe { CStr::from_ptr(col_filter) }.to_string_lossy();
    let haystack = unsafe { CStr::from_ptr(col_name) }.to_string_lossy();
    list_any_token_in_haystack(list.as_ref(), haystack.as_ref())
}

fn trace_badcast_list_contains_idx(idx: c_int) -> bool {
    trace_badcast_init();
    let list_ptr = unsafe { TRACE_BADCAST_IDX_LIST };
    if list_ptr.is_null() {
        return false;
    }
    let list = unsafe { CStr::from_ptr(list_ptr) }.to_string_lossy();
    list_contains_idx(list.as_ref(), idx)
}

pub(super) fn trace_badcast_should_log(pg_stmt: *const PgStmt, idx: c_int) -> bool {
    trace_badcast_init();
    if !trace_badcast_enabled() {
        return false;
    }
    if !trace_badcast_thread_ok() || !trace_badcast_sql_ok(pg_stmt) {
        return false;
    }
    trace_badcast_list_contains_idx(idx)
}

pub(super) fn trace_badcast_should_log_col(
    pg_stmt: *const PgStmt,
    idx: c_int,
    col_name: *const c_char,
) -> bool {
    if !trace_badcast_should_log(pg_stmt, idx) {
        return false;
    }
    trace_badcast_col_ok(col_name)
}

pub(super) fn trace_badcast_log_ctx(
    pg_stmt: *const PgStmt,
    stmt: *const sqlite3_stmt,
    idx: c_int,
    fn_name: &str,
    phase: &str,
    row: c_int,
    is_null: c_int,
    oid: u32,
    col_name: *const c_char,
) {
    if pg_stmt.is_null() {
        return;
    }
    let s = unsafe { &*pg_stmt };
    let sql = cstr_prefix(s.pg_sql, 200, "?");
    let col = cstr_to_string_or(col_name, "?");
    log_debug_lazy!(
        "TRACE_BADCAST_CTX: fn={} phase={} stmt={:p} pg_stmt={:p} idx={} col='{}' oid={} row={}/{} cols={} is_null={} sql={}",
        fn_name, phase, stmt, pg_stmt, idx, col, oid, row, s.num_rows, s.num_cols, is_null, sql
    );
}
