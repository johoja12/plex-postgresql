use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::{log_debug, log_error};
use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::ffi_types::PgStmt;
use crate::log_debug_lazy;
use crate::sync_utils::{rwlock_read, rwlock_write};

use super::{
    leak_enabled, rust_stmt_free, stmt_cache_disabled, stmt_unref_ptr, with_tls_cache, REGISTRY,
    STMT_INIT,
};

static REGISTRY_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
pub(super) fn invalidate_fast_lookup() {
    REGISTRY_EPOCH.fetch_add(1, Ordering::AcqRel);
}

struct FastLookup {
    sqlite_stmt: usize, pg_stmt: usize, epoch: u64,
}
impl Drop for FastLookup {
    fn drop(&mut self) { rust_stmt_unref(self.pg_stmt as *mut PgStmt); }
}
thread_local! {
    // One strong reference per thread; bounded memory and no dangling pointer
    // if another thread unregisters/finalizes the cached statement.
    static FAST_LOOKUP: std::cell::RefCell<Option<FastLookup>> = const { std::cell::RefCell::new(None) };
}
fn clear_fast_lookup() {
    if let Ok(old) = FAST_LOOKUP.try_with(|v| v.borrow_mut().take()) {
        drop(old); // Never unref under the registry lock or a TLS borrow.
    }
}

pub fn rust_stmt_ref(pg_stmt: *mut PgStmt) {
    if pg_stmt.is_null() {
        return;
    }
    unsafe {
        let stmt = &*pg_stmt;
        stmt.ref_count.fetch_add(1, Ordering::AcqRel);
    }
}

pub fn rust_stmt_unref(pg_stmt: *mut PgStmt) {
    if pg_stmt.is_null() {
        return;
    }

    // Atomically decrement, rejecting any transition that would go below 0.
    let result = unsafe {
        let stmt = &*pg_stmt;
        stmt.ref_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current <= 0 {
                    None // reject: already at 0 or below
                } else {
                    Some(current - 1)
                }
            })
    };

    let old = match result {
        Ok(prev) => prev,
        Err(observed) => {
            // The ref_count was already 0 (or below). The object may be freed —
            // do NOT dereference any fields. Log only the raw pointer.
            log_error(&format!(
                "pg_stmt_unref: CRITICAL BUG - ref_count was {}, refusing decrement. \
                 stmt={:p} (not dereferencing freed memory)",
                observed, pg_stmt
            ));
            return;
        }
    };

    // At this point we successfully decremented from `old` to `old - 1`.
    // If old > 1 the object is still alive (other refs exist) — safe to read fields.
    // If old == 1 we did the 1 -> 0 transition atomically; we are the sole owner
    // and can safely read fields before freeing.
    let new = old - 1;
    let sql = unsafe {
        let stmt = &*pg_stmt;
        if stmt.sql.is_null() {
            "NULL"
        } else {
            cstr_to_str_or_empty(stmt.sql)
        }
    };
    log_debug_lazy!(
        "pg_stmt_unref: stmt={:p} old_ref={} new_ref={} sql={:.40}",
        pg_stmt,
        old,
        new,
        sql
    );

    if old == 1 {
        // We performed the 1 -> 0 transition — sole owner, free the statement.
        if leak_enabled() {
            log_error(&format!(
                "pg_stmt_unref: leak enabled via PLEX_PG_LEAK_STMTS, skipping free stmt={:p} sql={:.40}",
                pg_stmt, sql
            ));
            unsafe {
                let stmt = &*pg_stmt;
                stmt.ref_count.store(1, Ordering::Release);
            }
            return;
        }
        log_debug_lazy!("pg_stmt_unref: last reference, freeing stmt={:p}", pg_stmt);
        rust_stmt_free(pg_stmt);
    }
}

/// Initialize the statement registry.
pub fn rust_stmt_registry_init() {
    STMT_INIT.call_once(|| {
        let _reg = rwlock_read(&REGISTRY);
        log_debug("pg_statement registry initialized (Rust HashMap)");
    });
}

/// Clear all entries from the registry.
/// Each pg_stmt_t gets unref'd.
pub fn rust_stmt_registry_cleanup() {
    let mut reg = rwlock_write(&REGISTRY);
    let pg_stmts: Vec<usize> = reg.forward.values().copied().collect();
    reg.clear();
    drop(reg);
    clear_fast_lookup();
    for pg_stmt in pg_stmts {
        stmt_unref_ptr(pg_stmt);
    }
}

/// Register a sqlite3_stmt -> pg_stmt_t mapping.
///
/// # Safety
/// Both pointers must be valid. The pg_stmt_t must remain valid until
/// `rust_stmt_unregister` is called.
pub fn rust_stmt_register(sqlite_stmt: usize, pg_stmt: usize) {
    if sqlite_stmt == 0 || pg_stmt == 0 {
        return;
    }
    let mut reg = rwlock_write(&REGISTRY);
    reg.register(sqlite_stmt, pg_stmt);
}

/// Remove a sqlite3_stmt -> pg_stmt_t mapping.
pub fn rust_stmt_unregister(sqlite_stmt: usize) {
    if sqlite_stmt == 0 {
        return;
    }
    let mut reg = rwlock_write(&REGISTRY);
    reg.unregister(sqlite_stmt);
    drop(reg);
    clear_fast_lookup();
}

/// Look up pg_stmt_t by sqlite3_stmt pointer.
/// Returns 0 if not found.
pub fn rust_stmt_find(sqlite_stmt: usize) -> usize {
    if sqlite_stmt == 0 {
        return 0;
    }
    let reg = rwlock_read(&REGISTRY);
    reg.find(sqlite_stmt).unwrap_or(0)
}

/// Look up pg_stmt_t by sqlite3_stmt pointer - first in registry, then TLS cache.
/// Returns 0 if not found anywhere.
pub fn rust_stmt_find_any(sqlite_stmt: usize) -> usize {
    if sqlite_stmt == 0 {
        return 0;
    }

    if stmt_cache_disabled() { return rust_stmt_find(sqlite_stmt); }

    let epoch = REGISTRY_EPOCH.load(Ordering::Acquire);
    if let Ok(Some(hit)) = FAST_LOOKUP.try_with(|v| {
        v.borrow().as_ref().filter(|c| c.sqlite_stmt == sqlite_stmt && c.epoch == epoch)
            .map(|c| c.pg_stmt)
    }) { return hit; }

    let found = {
        let reg = rwlock_read(&REGISTRY);
        reg.find(sqlite_stmt).map(|pg_stmt| {
            rust_stmt_ref(pg_stmt as *mut PgStmt);
            FastLookup { sqlite_stmt, pg_stmt, epoch: REGISTRY_EPOCH.load(Ordering::Acquire) }
        })
    };
    if let Some(found) = found {
        let pg_stmt = found.pg_stmt;
        let _ = FAST_LOOKUP.try_with(|v| {
            let old = v.borrow_mut().replace(found);
            drop(old);
        });
        return pg_stmt;
    }
    clear_fast_lookup();

    if stmt_cache_disabled() {
        return 0;
    }
    with_tls_cache(|cache| cache.find(sqlite_stmt).unwrap_or(0)).unwrap_or(0)
}

/// Check if a pg_stmt_t pointer is registered.
pub fn rust_stmt_is_ours(pg_stmt: usize) -> i32 {
    if pg_stmt == 0 {
        return 0;
    }
    let reg = rwlock_read(&REGISTRY);
    if reg.is_ours(pg_stmt) {
        1
    } else {
        0
    }
}

/// Get the current number of registered statements.
pub fn rust_stmt_registry_count() -> usize {
    let reg = rwlock_read(&REGISTRY);
    reg.len()
}
