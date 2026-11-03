use std::sync::atomic::Ordering;

use crate::ffi_types::conn_refs;

use super::super::connection_helpers::conn_is_streaming_active_ptr;
use super::super::connection_lifecycle::destroy_pool_connection;
use super::super::{PoolManager, SLOT_READY};
use super::shared::AcquireCtx;
use crate::log_info_lazy;

/// How often the idle reaper runs, in seconds. Zombie reclaim and the reap
/// together are one scan of the slots, so this is also the cadence of the
/// periodic pass that keeps a warm pool shrinking.
pub(crate) const REAP_INTERVAL_SECS: i64 = 60;

/// Whether the periodic pass is due: the reaper's interval has passed since
/// it last ran. One atomic load, because every acquire asks.
pub(crate) fn maintenance_due(pm: &PoolManager, now: i64) -> bool {
    now - pm.last_reap_time.load(Ordering::Relaxed) >= REAP_INTERVAL_SECS
}

pub(super) fn reclaim_zombies_and_reap(ctx: &AcquireCtx<'_>) {
    let idle_timeout = ctx.pm.idle_timeout_secs.load(Ordering::Relaxed) as i64;

    for i in 0..ctx.pool_size {
        let slot = &ctx.pm.slots[i];
        let state = slot.state.load(Ordering::Acquire);
        if state != SLOT_READY {
            continue;
        }
        let referenced = ctx.pm.slot_is_referenced(i);
        let retired = slot.owner_retired();
        let last_used = slot.last_used.load(Ordering::Acquire);
        let timed_out = super::super::PoolManager::zombie_reclaimable(
            referenced,
            ctx.now - last_used,
            idle_timeout,
        );
        if referenced || (!retired && !timed_out) {
            continue;
        }
        let conn = slot.conn.load(Ordering::Acquire);
        // A prepared statement can outlive its worker. The old exit cleanup
        // cancelled such reads; keep them until their final reference drops.
        if !conn.is_null()
            && (conn_refs(conn.cast()) > 1 || conn_is_streaming_active_ptr(conn.cast()))
        {
            continue;
        }
        if !slot.try_begin_reconnect() {
            continue;
        }
        // Validate again while this slot cannot be claimed by another worker.
        let conn = slot.conn.load(Ordering::Acquire);
        if ctx.pm.slot_is_referenced(i)
            || (!conn.is_null()
                && (conn_refs(conn.cast()) > 1 || conn_is_streaming_active_ptr(conn.cast())))
        {
            slot.mark_ready();
            continue;
        }
        slot.release();
        log_info_lazy!(
            "Pool: reclaimed slot {} (owner_retired={}, idle={} sec)",
            i,
            retired,
            ctx.now - last_used
        );
    }

    let last_reap = ctx.pm.last_reap_time.load(Ordering::Relaxed);
    if ctx.now - last_reap < REAP_INTERVAL_SECS {
        return;
    }
    if ctx
        .pm
        .last_reap_time
        .compare_exchange(last_reap, ctx.now, Ordering::SeqCst, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    log_info_lazy!(
        "Pool reaper: running (last run {} seconds ago)",
        ctx.now - last_reap
    );
    let to_destroy = ctx.pm.reap_idle(ctx.now);
    for (_slot_idx, conn_ptr) in to_destroy {
        destroy_pool_connection(conn_ptr);
    }
}

#[cfg(test)]
mod retired_tests {
    use super::*;
    use crate::ffi_types::{conn_ref, conn_unref, PgConnection};
    use crate::pg_client::thread_exit::retire_owned_slots;
    use crate::pg_client::threading::current_thread_id;
    use crate::pg_client::{PoolManager, SLOT_FREE, SLOT_READY};

    fn allocated() -> *mut PgConnection {
        let p =
            unsafe { libc::calloc(1, std::mem::size_of::<PgConnection>()).cast::<PgConnection>() };
        assert!(!p.is_null());
        conn_ref(p);
        p
    }
    fn scan(pm: &PoolManager) {
        reclaim_zombies_and_reap(&AcquireCtx {
            pm,
            current_thread: current_thread_id(),
            now: 1_000,
            pool_size: pm.pool_size(),
            db_path: std::ptr::null(),
            exclude_conn: std::ptr::null(),
        });
    }
    fn ready(pm: &PoolManager, conn: *mut PgConnection) {
        pm.last_reap_time.store(1_000, Ordering::Relaxed);
        pm.slots[0].conn.store(conn.cast(), Ordering::Relaxed);
        pm.slots[0].state.store(SLOT_READY, Ordering::Relaxed);
        pm.slots[0]
            .owner_thread
            .store(current_thread_id(), Ordering::Relaxed);
        pm.slots[0].last_used.store(1_000, Ordering::Relaxed);
    }
    #[test]
    fn retired_unreferenced_workers_do_not_reserve_recent_slots_for_five_minutes() {
        let pm = PoolManager::new(1, 1);
        let c = allocated();
        ready(&pm, c);
        scan(&pm);
        assert_eq!(
            pm.slots[0].state.load(Ordering::Acquire),
            SLOT_READY,
            "live worker retains its slot"
        );
        retire_owned_slots(&pm, current_thread_id());
        scan(&pm);
        assert_eq!(pm.slots[0].state.load(Ordering::Acquire), SLOT_FREE);
        assert!(conn_unref(c));
    }
    #[test]
    fn retirement_does_not_take_connections_from_open_handles_or_statements() {
        let pm = PoolManager::new(1, 1);
        let c = allocated();
        ready(&pm, c);
        pm.db_to_pool.assign(123, 0);
        retire_owned_slots(&pm, current_thread_id());
        scan(&pm);
        assert_eq!(pm.slots[0].state.load(Ordering::Acquire), SLOT_READY);
        pm.db_to_pool.release(123);
        conn_ref(c); // prepared statement outlives worker and logical handle
        scan(&pm);
        assert_eq!(pm.slots[0].state.load(Ordering::Acquire), SLOT_READY);
        assert!(!conn_unref(c));
        scan(&pm);
        assert_eq!(pm.slots[0].state.load(Ordering::Acquire), SLOT_FREE);
        assert!(conn_unref(c));
    }
    #[test]
    fn retirement_does_not_cancel_streams_or_touch_another_owner() {
        let pm = PoolManager::new(1, 1);
        let c = allocated();
        ready(&pm, c);
        retire_owned_slots(&pm, current_thread_id() ^ 1);
        assert!(!pm.slots[0].owner_retired());
        retire_owned_slots(&pm, current_thread_id());
        unsafe {
            (*c).streaming_active.store(1, Ordering::Release);
        }
        scan(&pm);
        assert_eq!(pm.slots[0].state.load(Ordering::Acquire), SLOT_READY);
        unsafe {
            (*c).streaming_active.store(0, Ordering::Release);
        }
        scan(&pm);
        assert_eq!(pm.slots[0].state.load(Ordering::Acquire), SLOT_FREE);
        assert!(conn_unref(c));
    }
    #[test]
    fn a_delayed_retirement_mark_does_not_retire_a_new_generation() {
        let pm = PoolManager::new(1, 1);
        let slot = &pm.slots[0];
        slot.generation.store(10, Ordering::Release);
        slot.retired_generation.store(10, Ordering::Release);
        assert!(slot.owner_retired());
        slot.generation.store(11, Ordering::Release);
        // A delayed store from the old owner's TLS destructor is harmless.
        slot.retired_generation.store(10, Ordering::Release);
        assert!(!slot.owner_retired());
        assert_eq!(slot.state.load(Ordering::Acquire), SLOT_FREE);
    }
}
