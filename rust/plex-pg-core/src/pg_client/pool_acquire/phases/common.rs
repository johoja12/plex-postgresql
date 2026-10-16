use std::os::raw::c_void;
use std::sync::atomic::Ordering;

use super::super::super::tls_cache::tls_pool_cache_set;
use super::super::super::PoolSlot;
use super::super::shared::{AcquireCtx, AcquireDecision};

pub(super) fn claim_slot_for_thread(ctx: &AcquireCtx<'_>, slot: &PoolSlot) {
    slot.owner_thread
        .store(ctx.current_thread, Ordering::Release);
    slot.last_used.store(ctx.now, Ordering::Release);
    slot.generation.fetch_add(1, Ordering::SeqCst);
    // The slot goes back when this thread exits (pool_runtime).
    super::super::super::pool_runtime::note_thread_owns_slots();
}

pub(super) fn cache_existing_ready_slot(
    ctx: &AcquireCtx<'_>,
    idx: usize,
    slot: &PoolSlot,
    conn: *mut c_void,
) -> AcquireDecision {
    slot.last_used.store(ctx.now, Ordering::Release);
    tls_pool_cache_set(0, idx as u32, slot.generation.load(Ordering::Acquire));
    AcquireDecision::Return(conn)
}

pub(super) fn mark_ready_and_cache_slot(
    idx: usize,
    slot: &PoolSlot,
    conn: *mut c_void,
) -> AcquireDecision {
    slot.mark_ready();
    tls_pool_cache_set(0, idx as u32, slot.generation.load(Ordering::Acquire));
    AcquireDecision::Return(conn)
}

pub(super) fn release_failed_new_connection(slot: &PoolSlot, new_conn: *mut c_void) {
    if !new_conn.is_null() {
        super::super::super::connection_lifecycle::destroy_pool_connection(new_conn);
    }
    slot.conn.store(std::ptr::null_mut(), Ordering::Release);
    slot.owner_thread.store(0, Ordering::Release);
    slot.release();
}
