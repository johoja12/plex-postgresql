use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::log_error;
use crate::ffi_types::PgConnection;

use super::connection_helpers::conn_is_streaming_active_ptr;
use super::connection_lifecycle::{check_conn_ok, get_txn_status, reconnect_conn, reset_conn};
use super::session::{exec_simple, PQTRANS_INERROR, PQTRANS_INTRANS};
use super::threading::{current_thread_id, threads_equal};
use super::tls_cache::tls_pool_cache_clear;
use super::{log_info, pool, rust_stmt_cache_clear, SLOT_FREE, SLOT_READY};
use crate::log_info_lazy;

pub(super) fn pool_release_for_db_inner(db_handle: usize) {
    let pm = pool();
    super::txn::forget_db(db_handle);
    let slot_opt = pm.db_to_pool.release(db_handle);

    if let Some(slot_idx) = slot_opt {
        let pool_size = pm.pool_size();
        if slot_idx < pool_size {
            let slot = &pm.slots[slot_idx];
            let current_thread = current_thread_id();
            let owner = slot.owner_thread.load(Ordering::Acquire);

            if threads_equal(owner, current_thread) {
                let state = slot.state.load(Ordering::Acquire);
                if state == SLOT_READY {
                    let conn = slot.conn.load(Ordering::Acquire);
                    settle_connection_before_release(
                        conn as *mut PgConnection,
                        slot_idx,
                        "handle close",
                    );

                    slot.owner_thread.store(0, Ordering::Release);
                    slot.state.store(SLOT_FREE, Ordering::Release);
                    log_info_lazy!("Pool: releasing slot {} for db {:x}", slot_idx, db_handle);
                }
            }
        }
    }

    tls_pool_cache_clear();
}

// A thread's slots are NOT given back when the thread exits (tried on
// 2026-10-16 as clusterplex.31 and reverted the same day): Plex keeps
// statements prepared on the slot's connection alive past the thread that
// made them, so a released slot went to another thread while those
// statements still streamed on it, and /library/sections answered 500 with
// std::exception. The idle reaper, which checks for open handles, is the
// only release besides sqlite3_close.

/// Give back every READY slot `thread_id` owns: a stream still open on it is
/// cancelled and drained, a transaction still open is rolled back (the
/// thread that began it is gone, so nothing can commit it), and the slot
/// goes FREE for the next thread. The connection stays, for reuse.
#[cfg(test)]
pub(crate) fn release_slots_owned_by(pm: &super::PoolManager, thread_id: u64, why: &str) -> usize {
    let mut released = 0usize;
    for i in 0..pm.pool_size() {
        let slot = &pm.slots[i];
        if slot.state.load(Ordering::Acquire) != SLOT_READY {
            continue;
        }
        if !threads_equal(slot.owner_thread.load(Ordering::Acquire), thread_id) {
            continue;
        }
        let conn = slot.conn.load(Ordering::Acquire);
        if !conn.is_null() {
            settle_connection_before_release(conn as *mut PgConnection, i, why);
        }
        slot.owner_thread.store(0, Ordering::Release);
        slot.state.store(SLOT_FREE, Ordering::Release);
        released += 1;
        log_info_lazy!("Pool: released slot {} on {}", i, why);
    }
    released
}

/// Leave a connection idle before its slot is given back: no stream in
/// flight, no transaction open.
pub(crate) fn settle_connection_before_release(
    conn: *mut PgConnection,
    slot_idx: usize,
    why: &str,
) {
    if conn.is_null() {
        return;
    }
    if conn_is_streaming_active_ptr(conn) {
        let scope = CString::new("POOL RELEASE").unwrap();
        log_error(&format!(
            "Pool: releasing slot {} while streaming_active=1 ({}), forcing cancel/drain",
            slot_idx, why
        ));
        crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(conn, scope.as_ptr());
        // SAFETY: conn is non-null (checked above).
        unsafe { &*conn }
            .streaming_active
            .store(0, Ordering::Release);
    }
    let txn = get_txn_status(conn as *mut c_void);
    if txn == PQTRANS_INTRANS || txn == PQTRANS_INERROR {
        // Never COMMIT here: whoever began this transaction has let go of
        // the slot without ending it, and committing half a Plex write is
        // the harm the transaction was for.
        log_info_lazy!(
            "Pool: slot {} has an open transaction (status={}) on {}; rolling it back",
            slot_idx,
            txn,
            why
        );
        let _ = exec_simple(conn as *mut c_void, c"ROLLBACK".as_ptr());
    }
    super::txn::forget_connection(conn);
}

pub(super) fn pool_check_health_inner(conn: *mut c_void) -> i32 {
    if conn.is_null() {
        return 0;
    }

    if check_conn_ok(conn) {
        return 0;
    }

    log_info("Pool: connection health check failed, resetting");

    let pm = pool();
    let current_thread = current_thread_id();
    let pool_size = pm.pool_size();

    for i in 0..pool_size {
        let slot = &pm.slots[i];
        if slot.conn.load(Ordering::Acquire) != conn {
            continue;
        }
        let owner = slot.owner_thread.load(Ordering::Acquire);
        if !threads_equal(owner, current_thread) {
            continue;
        }

        if !slot.try_begin_reconnect() {
            break;
        }

        rust_stmt_cache_clear(conn);

        let reset_ok = reset_conn(conn);
        if reset_ok {
            log_info_lazy!("Pool: connection reset successful for slot {}", i);
            slot.last_used.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                Ordering::Release,
            );
            slot.mark_ready();
            return 1;
        }

        log_error(&format!(
            "Pool: PQreset failed for slot {}, trying fresh connection...",
            i
        ));
        let reconn_ok = reconnect_conn(conn);
        if reconn_ok {
            slot.last_used.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                Ordering::Release,
            );
            log_info_lazy!(
                "Pool: fresh connection succeeded for slot {} (reconnected)",
                i
            );
            slot.mark_ready();
            return 1;
        } else {
            log_error(&format!(
                "Pool: fresh connection also failed for slot {}",
                i
            ));
            slot.mark_error();
            return 1;
        }
    }

    0
}
