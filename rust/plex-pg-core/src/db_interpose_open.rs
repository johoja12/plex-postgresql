use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use crate::db_interpose_common::{
    get_orig_sqlite3_close, get_orig_sqlite3_close_v2, get_orig_sqlite3_open,
    get_orig_sqlite3_open_v2, stderr_ptr,
};
use crate::db_interpose_conn_utils::cstr_to_string_or;
use crate::ffi_types::{sqlite3, PgConnection};
use crate::log_info_lazy;

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

static NEEDLE_LIBRARY_DB: &[u8] = b"com.plexapp.plugins.library.db";
static DB_HANDLE_FILENAMES: OnceLock<Mutex<HashMap<usize, CString>>> = OnceLock::new();

use crate::env_utils::loadone_trace_enabled;

fn db_handle_filenames() -> &'static Mutex<HashMap<usize, CString>> {
    DB_HANDLE_FILENAMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn filename_for_registry(filename: *const c_char) -> CString {
    if filename.is_null() {
        CString::new("<null>").expect("literal contains no NUL")
    } else {
        unsafe { CStr::from_ptr(filename).to_owned() }
    }
}

fn lock_db_handle_filenames() -> std::sync::MutexGuard<'static, HashMap<usize, CString>> {
    db_handle_filenames()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn opened_db_handle(rc: c_int, pp_db: *mut *mut sqlite3) -> *mut sqlite3 {
    if rc != SQLITE_OK || pp_db.is_null() {
        return ptr::null_mut();
    }
    unsafe { *pp_db }
}

pub(crate) fn track_db_handle_filename(db: *mut sqlite3, filename: *const c_char) {
    if db.is_null() {
        return;
    }
    let mut map = lock_db_handle_filenames();
    map.insert(db as usize, filename_for_registry(filename));
}

pub(crate) fn untrack_db_handle_filename(db: *mut sqlite3) {
    if db.is_null() {
        return;
    }
    let mut map = lock_db_handle_filenames();
    map.remove(&(db as usize));
}

pub(crate) fn lookup_db_handle_filename(db: *const sqlite3) -> Option<CString> {
    if db.is_null() {
        return None;
    }
    let map = lock_db_handle_filenames();
    map.get(&(db as usize)).cloned()
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn should_redirect(filename: *const c_char) -> bool {
    let passthrough = crate::db_interpose_common::SHIM_PASSTHROUGH_ONLY
        .load(std::sync::atomic::Ordering::Acquire);
    crate::pg_config::pg_config_should_redirect(filename, passthrough) != 0
}

unsafe fn handle_conn_path_contains(conn: *mut PgConnection, needle: &[u8]) -> bool {
    if conn.is_null() {
        return false;
    }
    let c = &*conn;
    let path_ptr = c.db_path.as_ptr();
    if path_ptr.is_null() {
        return false;
    }
    let bytes = CStr::from_ptr(path_ptr).to_bytes();
    contains_subslice(bytes, needle)
}

unsafe fn trace_open_event(
    api: *const c_char,
    filename: *const c_char,
    db: *mut sqlite3,
    redirect: bool,
    rc: c_int,
) {
    if !loadone_trace_enabled() {
        return;
    }

    let trace_filename = if filename.is_null() {
        b"<null>\0".as_ptr() as *const c_char
    } else {
        filename
    };
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[LOADONE_TRACE][open] api=%s db=%p redirect=%d rc=%d file=%.900s\n\0".as_ptr()
            as *const c_char,
        api,
        db as *mut c_void,
        redirect as c_int,
        rc,
        trace_filename,
    );
    let _ = libc::fflush(stderr_ptr());
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_open(filename: *const c_char, pp_db: *mut *mut sqlite3) -> c_int {
    let redirect = should_redirect(filename);
    log_info_lazy!(
        "OPEN: {} (redirect={})",
        cstr_to_string_or(filename, "(null)"),
        redirect as i32
    );

    let rc = get_orig_sqlite3_open()
        .map(|f| unsafe { f(filename, pp_db) })
        .unwrap_or(SQLITE_ERROR);
    let db = opened_db_handle(rc, pp_db);

    if !db.is_null() {
        track_db_handle_filename(db, filename);
    }

    unsafe {
        trace_open_event(
            b"open\0".as_ptr() as *const c_char,
            filename,
            db,
            redirect,
            rc,
        );
    }

    if rc == SQLITE_OK && redirect {
        if !db.is_null() {
            let pg_conn = crate::pg_client::rust_pg_connect(filename, db);
            if !pg_conn.is_null() {
                crate::pg_client::rust_pg_register_connection(pg_conn);
                log_info_lazy!(
                    "PostgreSQL connection established for: {}",
                    cstr_to_string_or(filename, "(null)")
                );
            }
        }
    }

    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_open_v2(
    filename: *const c_char,
    pp_db: *mut *mut sqlite3,
    flags: c_int,
    z_vfs: *const c_char,
) -> c_int {
    let redirect = should_redirect(filename);
    log_info_lazy!(
        "OPEN_V2: {} flags=0x{:x} (redirect={})",
        cstr_to_string_or(filename, "(null)"),
        flags,
        redirect as i32
    );

    let rc = get_orig_sqlite3_open_v2()
        .map(|f| unsafe { f(filename, pp_db, flags, z_vfs) })
        .unwrap_or(SQLITE_ERROR);
    let db = opened_db_handle(rc, pp_db);

    if !db.is_null() {
        track_db_handle_filename(db, filename);
    }

    unsafe {
        trace_open_event(
            b"open_v2\0".as_ptr() as *const c_char,
            filename,
            db,
            redirect,
            rc,
        );
    }

    if rc == SQLITE_OK && redirect {
        if !db.is_null() {
            let pg_conn = crate::pg_client::rust_pg_connect(filename, db);
            if !pg_conn.is_null() {
                crate::pg_client::rust_pg_register_connection(pg_conn);
                log_info_lazy!(
                    "PostgreSQL connection established for: {}",
                    cstr_to_string_or(filename, "(null)")
                );
            }
        }
    }

    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_close(db: *mut sqlite3) -> c_int {
    let handle_conn = crate::pg_client::rust_pg_find_handle_connection(db);
    if !handle_conn.is_null() {
        let hc = unsafe { &*handle_conn };
        log_info_lazy!(
            "CLOSE: PostgreSQL connection for {}",
            cstr_to_string_or(hc.db_path.as_ptr(), "(null)")
        );

        unsafe {
            if handle_conn_path_contains(handle_conn, NEEDLE_LIBRARY_DB) {
                crate::pg_client::rust_pool_release_for_db(db as *const _ as *const c_void);
            }
        }

        crate::pg_client::rust_pg_unregister_connection(handle_conn);
        crate::pg_client::rust_pg_close(handle_conn);
    }

    // Closing disconnects virtual tables, and FTS3 finalizes the statements
    // it cached: the finalize guard needs the connection, see ConnScope.
    let rc = {
        let _scope = crate::db_interpose_stmt_lifecycle::ConnScope::enter(db);
        get_orig_sqlite3_close()
            .map(|f| unsafe { f(db) })
            .unwrap_or(SQLITE_ERROR)
    };
    if rc == SQLITE_OK {
        untrack_db_handle_filename(db);
    }
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_close_v2(db: *mut sqlite3) -> c_int {
    let handle_conn = crate::pg_client::rust_pg_find_handle_connection(db);
    if !handle_conn.is_null() {
        unsafe {
            if handle_conn_path_contains(handle_conn, NEEDLE_LIBRARY_DB) {
                crate::pg_client::rust_pool_release_for_db(db as *const _ as *const c_void);
            }
        }

        crate::pg_client::rust_pg_unregister_connection(handle_conn);
        crate::pg_client::rust_pg_close(handle_conn);
    }

    // Closing disconnects virtual tables, and FTS3 finalizes the statements
    // it cached: the finalize guard needs the connection, see ConnScope.
    let rc = {
        let _scope = crate::db_interpose_stmt_lifecycle::ConnScope::enter(db);
        get_orig_sqlite3_close_v2()
            .map(|f| unsafe { f(db) })
            .unwrap_or(SQLITE_ERROR)
    };
    if rc == SQLITE_OK {
        untrack_db_handle_filename(db);
    }
    rc
}
