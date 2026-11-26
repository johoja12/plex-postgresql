use std::sync::atomic::Ordering;

use crate::ffi_types::PgConnection;

use super::super::connection_helpers::conn_is_streaming_active_ptr;
use super::super::connection_lifecycle::{
    conn_has_session, destroy_pool_connection, park_connection,
};
use super::super::{PoolManager, SLOT_READY};
use super::shared::AcquireCtx;
use crate::log_info_lazy;

/// How often the idle reaper runs, in seconds. Zombie reclaim and the reap
/// together are one scan of the slots, so this is also the cadence of the
/// periodic pass that keeps a warm pool shrinking.
pub(crate) const REAP_INTERVAL_SECS: i64 = 60;

/// How many idle connections one pass closes at most. The pass runs inside
/// some thread's acquire, and that thread should not pay for a whole pool.
const PARK_PER_PASS: usize = 4;

/// How often the pool says, at INFO, how many slots and prepared statements
/// it has.
const STATS_INTERVAL_SECS: i64 = 600;

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
        // Referenced or recently used settles it; nothing asks whether the
        // owner thread is still alive. Plex opens twenty database handles when
        // it starts and keeps them for the life of the process while the
        // worker threads that used them come and go, so a dead owner means
        // nothing -- and asking musl about one segfaults, because its
        // pthread_t is a pointer into a stack that is no longer mapped. See
        // PoolManager::zombie_reclaimable.
        let last_used = slot.last_used.load(Ordering::Acquire);
        if !super::super::PoolManager::zombie_reclaimable(
            ctx.pm.slot_is_referenced(i),
            ctx.now - last_used,
            idle_timeout,
        ) {
            continue;
        }

        let conn = slot.conn.load(Ordering::Acquire);
        if !conn.is_null() && conn_is_streaming_active_ptr(conn as *mut PgConnection) {
            log_info_lazy!(
                "Pool PHASE 0: slot {} unreferenced but streaming_active, skipping reclaim",
                i
            );
            continue;
        }

        if slot.try_reclaim_zombie() {
            log_info_lazy!(
                "Pool PHASE 0: Freed zombie slot {} (unreferenced, idle {} sec)",
                i,
                ctx.now - last_used
            );
        }
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

    for (slot, idle) in ctx
        .pm
        .park_idle(ctx.now, PARK_PER_PASS, conn_has_session, park_connection)
    {
        log_info_lazy!(
            "Pool: closed the connection of slot {} (unused for {} sec); its thread reconnects it",
            slot,
            idle
        );
    }

    let last_stats = ctx.pm.last_stats_time.load(Ordering::Relaxed);
    if ctx.now - last_stats >= STATS_INTERVAL_SECS
        && ctx
            .pm
            .last_stats_time
            .compare_exchange(last_stats, ctx.now, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    {
        let (ready, parked) = ctx.pm.ready_and_parked(conn_has_session);
        log_info_lazy!(
            "Pool: {} slots in use, {} of them with their connection closed ({} closed since start); statements: {}",
            ready,
            parked,
            ctx.pm.parked_total.load(Ordering::Relaxed),
            crate::pg_client_stmt_cache::stats_line()
        );
    }
}
