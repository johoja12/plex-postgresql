use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::atomic::Ordering;

use crate::ffi_types::{sqlite3, PgConnection};

use super::config::env_nonzero;
use super::connection_helpers::{conn_db_path, conn_is_pg_active, conn_is_pg_active_ptr};
use super::pool_lookup::{is_library_db, pool_find_connection_for_db};
use super::threading::{current_thread_id, threads_equal};
use super::tls_cache::tls_pool_cache_get;
use super::{
    log_debug, pool, rust_find_any_library_connection, rust_find_registered_connection, SLOT_READY,
};

/// Register a non-pooled connection using its shadow DB handle.
#[no_mangle]
pub extern "C" fn rust_pg_register_connection(conn_ptr: *mut PgConnection) {
    if conn_ptr.is_null() {
        return;
    }
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &*conn_ptr };
    let db = conn.shadow_db;
    if db.is_null() {
        return;
    }
    pool().registry.register(db as usize, conn_ptr as usize);
}

/// Unregister a non-pooled connection using its shadow DB handle.
#[no_mangle]
pub extern "C" fn rust_pg_unregister_connection(conn_ptr: *mut PgConnection) {
    if conn_ptr.is_null() {
        return;
    }
    // SAFETY: conn_ptr is non-null after the check above.
    let conn = unsafe { &*conn_ptr };
    let db = conn.shadow_db;
    if db.is_null() {
        return;
    }
    pool().registry.unregister(db as usize);
}

/// Find the handle connection for a sqlite3* handle.
#[no_mangle]
pub extern "C" fn rust_pg_find_handle_connection(db_handle: *const sqlite3) -> *mut PgConnection {
    if db_handle.is_null() {
        return std::ptr::null_mut();
    }
    rust_find_registered_connection(db_handle as *const c_void) as *mut PgConnection
}

/// Find the active connection for a sqlite3* handle, including pool logic.
#[no_mangle]
pub extern "C" fn rust_pg_find_connection(db_handle: *const sqlite3) -> *mut PgConnection {
    if db_handle.is_null() {
        return std::ptr::null_mut();
    }

    let _ = super::rust_pool_check_fork();

    let handle_conn =
        rust_find_registered_connection(db_handle as *const c_void) as *mut PgConnection;
    if handle_conn.is_null() {
        return std::ptr::null_mut();
    }

    let handle_ref = unsafe { &*handle_conn };
    let path = conn_db_path(handle_ref);

    if is_library_db(&path) {
        if env_nonzero("PLEX_PG_FORCE_SQLITE_LIBRARY") {
            return std::ptr::null_mut();
        }

        if env_nonzero("PLEX_PG_DISABLE_POOL") {
            if conn_is_pg_active(handle_ref) {
                return handle_conn;
            }
            return std::ptr::null_mut();
        }

        if let Ok(cs) = CString::new(path) {
            let pool_conn = pool_find_connection_for_db(db_handle as usize, cs.as_ptr());
            if !pool_conn.is_null() && conn_is_pg_active_ptr(pool_conn as *mut PgConnection) {
                return pool_conn as *mut PgConnection;
            }
        }

        log_debug("Pool full for library.db, falling back to SQLite");
        return std::ptr::null_mut();
    }

    if conn_is_pg_active(handle_ref) {
        handle_conn
    } else {
        std::ptr::null_mut()
    }
}

/// Find any active library connection (pool or handle).
#[no_mangle]
pub extern "C" fn rust_pg_find_any_library_connection() -> *mut PgConnection {
    rust_find_any_library_connection() as *mut PgConnection
}

/// Whether `db_handle` is a library handle the shim took over -- one whose
/// statements are answered by PostgreSQL and never by the shadow SQLite.
///
/// `rust_pg_find_connection` answering null for such a handle means the pool
/// had no slot to give (full, or the thread's stack too low to acquire), not
/// that the handle is SQLite's; the prepare has to refuse rather than route
/// the statement to the empty shadow, where a read answers no rows and a
/// write is lost.
pub(crate) fn handle_is_pg_library(db_handle: *const sqlite3) -> bool {
    if db_handle.is_null() {
        return false;
    }
    let handle_conn =
        rust_find_registered_connection(db_handle as *const c_void) as *mut PgConnection;
    if handle_conn.is_null() {
        return false;
    }
    let handle_ref = unsafe { &*handle_conn };
    conn_is_pg_active(handle_ref)
        && is_library_db(&conn_db_path(handle_ref))
        && !env_nonzero("PLEX_PG_FORCE_SQLITE_LIBRARY")
}

/// The connection a handle's last statement ran on, without acquiring one.
///
/// `sqlite3_changes`, `sqlite3_errmsg`, `sqlite3_errcode` and
/// `sqlite3_last_insert_rowid` read state a step or exec left on the
/// connection it used. They went through `rust_pg_find_connection`, which
/// acquires: on a full pool that blocks through the retry ladder (about ten
/// seconds, for `errmsg()`), and every call re-assigned `db_to_pool`, so a
/// handle looked held by whichever thread last asked what the error was.
///
/// This reads what is already there: the thread's own READY slot first --
/// that is the connection its step just used -- then the slot the handle is
/// recorded on, then the handle's own non-pooled connection. Null means no
/// statement has run on this handle on a connection we still have, and the
/// caller answers as for an idle handle.
pub(crate) fn peek_connection(db_handle: *const sqlite3) -> *mut PgConnection {
    if db_handle.is_null() {
        return std::ptr::null_mut();
    }
    let handle_conn =
        rust_find_registered_connection(db_handle as *const c_void) as *mut PgConnection;
    if handle_conn.is_null() {
        return std::ptr::null_mut();
    }
    let handle_ref = unsafe { &*handle_conn };
    if !is_library_db(&conn_db_path(handle_ref)) || env_nonzero("PLEX_PG_DISABLE_POOL") {
        return if conn_is_pg_active(handle_ref) {
            handle_conn
        } else {
            std::ptr::null_mut()
        };
    }
    if env_nonzero("PLEX_PG_FORCE_SQLITE_LIBRARY") {
        return std::ptr::null_mut();
    }

    let pm = pool();
    let pool_size = pm.pool_size();
    let ready_conn = |idx: usize| -> *mut PgConnection {
        if idx >= pool_size {
            return std::ptr::null_mut();
        }
        let slot = &pm.slots[idx];
        if slot.state.load(Ordering::Acquire) != SLOT_READY {
            return std::ptr::null_mut();
        }
        let conn = slot.conn.load(Ordering::Acquire) as *mut PgConnection;
        if conn_is_pg_active_ptr(conn) {
            conn
        } else {
            std::ptr::null_mut()
        }
    };

    if let Some((idx, generation)) = tls_pool_cache_get(0) {
        let idx = idx as usize;
        if idx < pool_size
            && pm.slots[idx].generation.load(Ordering::Acquire) == generation
            && threads_equal(
                pm.slots[idx].owner_thread.load(Ordering::Acquire),
                current_thread_id(),
            )
        {
            let conn = ready_conn(idx);
            if !conn.is_null() {
                return conn;
            }
        }
    }
    if let Some(idx) = pm.db_to_pool.find(db_handle as usize) {
        let conn = ready_conn(idx);
        if !conn.is_null() {
            return conn;
        }
    }
    if conn_is_pg_active(handle_ref) {
        handle_conn
    } else {
        std::ptr::null_mut()
    }
}

/// `peek_connection` for C callers.
#[no_mangle]
pub extern "C" fn rust_pg_peek_connection(db_handle: *const sqlite3) -> *mut PgConnection {
    peek_connection(db_handle)
}
