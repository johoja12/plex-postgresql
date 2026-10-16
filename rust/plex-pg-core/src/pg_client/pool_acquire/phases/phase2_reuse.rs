use crate::ffi_types::PgConnection;

use super::super::super::connection_helpers::{
    conn_is_pg_active_ptr, conn_is_streaming_active_ptr,
};
use super::super::super::connection_lifecycle::{get_txn_status, reconnect_conn, reset_conn};
use super::super::super::rust_stmt_cache_clear;
use super::super::super::session::{exec_simple, PQTRANS_INERROR, PQTRANS_INTRANS};
use super::super::shared::{AcquireCtx, AcquireDecision};
use super::common::{claim_slot_for_thread, mark_ready_and_cache_slot};
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(crate) fn phase2_reuse_existing(ctx: &AcquireCtx<'_>) -> AcquireDecision {
    for i in 0..ctx.pool_size {
        let slot = &ctx.pm.slots[i];
        let conn = slot.conn.load(std::sync::atomic::Ordering::Acquire);
        if conn.is_null() || conn == ctx.exclude_conn as *mut _ {
            continue;
        }
        if conn_is_streaming_active_ptr(conn as *mut PgConnection) {
            continue;
        }
        // Claim against the pointer we sampled: the reaper can empty this
        // slot and hand it back as FREE in the gap above, in which case `conn`
        // is already on its way to PQfinish.
        if !slot.try_claim_free_with_conn(conn) {
            continue;
        }

        claim_slot_for_thread(ctx, slot);

        let txn = get_txn_status(conn);
        if txn == PQTRANS_INTRANS || txn == PQTRANS_INERROR {
            // A FREE slot with a transaction open was abandoned by whoever
            // began it. It used to be COMMITted here; half a write is what a
            // transaction exists to prevent, so it is rolled back.
            log_info_lazy!(
                "Pool PHASE 2: slot {} has an abandoned transaction (status={}); rolling it back before reset",
                i, txn
            );
            let _ = exec_simple(conn, c"ROLLBACK".as_ptr());
        }

        rust_stmt_cache_clear(conn);
        if reset_conn(conn) {
            log_debug_lazy!("Pool: reusing reset connection in slot {}", i);
            return mark_ready_and_cache_slot(i, slot, conn);
        }

        rust_stmt_cache_clear(conn);
        if reconnect_conn(conn) && conn_is_pg_active_ptr(conn as *mut PgConnection) {
            return mark_ready_and_cache_slot(i, slot, conn);
        }

        slot.mark_error();
    }

    AcquireDecision::Continue
}
