use std::os::raw::c_void;
use std::sync::atomic::Ordering;

use crate::ffi_types::PgConnection;

use super::super::super::connection_helpers::conn_is_streaming_active_ptr;
use super::super::super::connection_lifecycle::{check_conn_ok, reconnect_conn};
use super::super::super::threading::threads_equal;
use super::super::super::tls_cache::{tls_pool_cache_clear, tls_pool_cache_get};
use super::super::super::{rust_stmt_cache_clear, SLOT_READY};
use super::super::shared::{AcquireCtx, AcquireDecision};
use super::common::{cache_existing_ready_slot, mark_ready_and_cache_slot};
use crate::log_debug_lazy;

pub(crate) fn phase1_existing_ready(ctx: &AcquireCtx<'_>) -> AcquireDecision {
    if let Some((idx, generation)) = tls_pool_cache_get(0) {
        let idx = idx as usize;
        if idx < ctx.pool_size {
            let slot = &ctx.pm.slots[idx];
            if !slot.owner_retired()
                && slot.state.load(Ordering::Acquire) == SLOT_READY
                && slot.generation.load(Ordering::Acquire) == generation
            {
                let owner = slot.owner_thread.load(Ordering::Acquire);
                if threads_equal(owner, ctx.current_thread) {
                    let conn = slot.conn.load(Ordering::Acquire);
                    if conn == ctx.exclude_conn as *mut c_void {
                        log_debug_lazy!(
                            "Pool FAST PATH: excluded slot {} for alternate acquisition",
                            idx
                        );
                    } else if !conn.is_null() && check_conn_ok(conn) {
                        if conn_is_streaming_active_ptr(conn as *mut PgConnection) {
                            log_debug_lazy!(
                                "Pool FAST PATH: streaming_active on slot {}, falling through",
                                idx
                            );
                        } else {
                            return cache_existing_ready_slot(ctx, idx, slot, conn);
                        }
                    }
                }
            }
        }
        tls_pool_cache_clear();
    }

    for i in 0..ctx.pool_size {
        let slot = &ctx.pm.slots[i];
        let state = slot.state.load(Ordering::Acquire);
        if state != SLOT_READY || slot.owner_retired() {
            continue;
        }
        let owner = slot.owner_thread.load(Ordering::Acquire);
        if !threads_equal(owner, ctx.current_thread) {
            continue;
        }

        let conn = slot.conn.load(Ordering::Acquire);
        if conn == ctx.exclude_conn as *mut c_void {
            log_debug_lazy!(
                "Pool PHASE 1: slot {} excluded for alternate acquisition",
                i
            );
            continue;
        }
        if !conn.is_null() && check_conn_ok(conn) {
            if conn_is_streaming_active_ptr(conn as *mut PgConnection) {
                log_debug_lazy!("Pool: slot {} streaming_active, skipping for thread", i);
                continue;
            }
            return cache_existing_ready_slot(ctx, i, slot, conn);
        }

        if slot.try_begin_reconnect() {
            rust_stmt_cache_clear(conn);
            if reconnect_conn(conn) {
                return mark_ready_and_cache_slot(i, slot, conn);
            }

            slot.mark_error();
            return AcquireDecision::Return(std::ptr::null_mut());
        }
    }

    AcquireDecision::Continue
}
