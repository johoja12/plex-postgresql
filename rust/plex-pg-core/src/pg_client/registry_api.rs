use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::atomic::Ordering;

use crate::ffi_types::{sqlite3, PgConnection};

use super::config::env_nonzero;
use super::connection_helpers::{conn_db_path, conn_is_pg_active, conn_is_pg_active_ptr};
use super::pool_lookup::{is_library_db, pool_find_connection_for_db};
use super::{log_debug, pool, rust_find_any_library_connection, rust_find_registered_connection};

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

// Metadata belongs to sqlite3*, not to a pooled libpq session. Store a
// snapshot on the registered handle so another handle reusing that session
// cannot change last_insert_rowid()/changes()/errmsg() after a transaction.
pub(crate) unsafe fn save_metadata(db: *const sqlite3, conn: *const PgConnection) {
    if db.is_null() || conn.is_null() {
        return;
    }
    let handle = rust_pg_find_handle_connection(db);
    if handle.is_null() || handle == conn.cast_mut() {
        return;
    }
    let src = &*conn;
    let dst = &mut *handle;
    dst.last_changes = src.last_changes;
    dst.last_insert_rowid = src.last_insert_rowid;
    dst.last_insert_failed.store(
        src.last_insert_failed.load(Ordering::Acquire),
        Ordering::Release,
    );
    dst.last_error = src.last_error;
    dst.last_error_code = src.last_error_code;
}

/// Publish on every return, including failed writes and transaction endings.
/// The caller keeps `conn` and `db` alive until the guard is dropped.
pub(crate) struct MetadataGuard {
    db: *const sqlite3,
    conn: *const PgConnection,
}
impl MetadataGuard {
    pub(crate) fn new(db: *const sqlite3, conn: *const PgConnection) -> Self {
        // SELECT and transaction control preserve changes()/rowid. Seed a
        // reused session with this handle's values rather than another
        // handle's most recent INSERT. Execution updates them on writes.
        unsafe {
            let handle = rust_pg_find_handle_connection(db);
            if !conn.is_null() && !handle.is_null() && handle != conn.cast_mut() {
                let dst = &mut *conn.cast_mut();
                let src = &*handle;
                dst.last_changes = src.last_changes;
                dst.last_insert_rowid = src.last_insert_rowid;
                dst.last_error = src.last_error;
                dst.last_error_code = src.last_error_code;
                dst.last_insert_failed.store(
                    src.last_insert_failed.load(Ordering::Acquire),
                    Ordering::Release,
                );
            }
        }
        Self { db, conn }
    }
    pub(crate) unsafe fn for_statement(
        stmt: *const crate::ffi_types::PgStmt,
        conn: *const PgConnection,
    ) -> Self {
        let db = if stmt.is_null() || (*stmt).shadow_stmt.is_null() {
            std::ptr::null_mut()
        } else {
            match crate::db_interpose_common::get_orig_sqlite3_db_handle() {
                Some(f) => f((*stmt).shadow_stmt),
                None => std::ptr::null_mut(),
            }
        };
        Self::new(db, conn)
    }
}
impl Drop for MetadataGuard {
    fn drop(&mut self) {
        unsafe { save_metadata(self.db, self.conn) };
    }
}

/// The metadata state for a logical SQLite handle, without acquiring a slot.
/// Active transactions must read the connection executing their statements,
/// even when prepare has cached another slot on this thread. After execution
/// MetadataGuard publishes a snapshot on the registered handle, so COMMIT,
/// ROLLBACK and pool reuse cannot redirect accessors to another handle's state.
pub(crate) fn peek_connection(db_handle: *const sqlite3) -> *mut PgConnection {
    let handle = rust_pg_find_handle_connection(db_handle);
    if !conn_is_pg_active_ptr(handle) {
        return std::ptr::null_mut();
    }
    if is_library_db(&conn_db_path(unsafe { &*handle })) {
        if env_nonzero("PLEX_PG_FORCE_SQLITE_LIBRARY") {
            return std::ptr::null_mut();
        }
        let pinned = super::txn::pinned_conn(db_handle);
        if !pinned.is_null() {
            return pinned;
        }
    }
    handle
}

/// `peek_connection` for C callers.
#[no_mangle]
pub extern "C" fn rust_pg_peek_connection(db_handle: *const sqlite3) -> *mut PgConnection {
    peek_connection(db_handle)
}
