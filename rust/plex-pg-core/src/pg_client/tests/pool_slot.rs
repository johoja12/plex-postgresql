use super::*;

// ═════════════════════════════════════════════════════════════════════════
// NEW TESTS: Pool State Machine (Stap 3)
// ═════════════════════════════════════════════════════════════════════════

#[test]
fn pool_slot_initial_state_is_free() {
    let slot = PoolSlot::new();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    assert!(slot.conn.load(Ordering::Relaxed).is_null());
    assert_eq!(slot.owner_thread.load(Ordering::Relaxed), 0);
    assert_eq!(slot.generation.load(Ordering::Relaxed), 0);
}

#[test]
fn pool_slot_claim_free_succeeds() {
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free());
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RESERVED);
}

#[test]
fn pool_slot_claim_free_fails_when_reserved() {
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free());
    // Second claim must fail
    assert!(!slot.try_claim_free());
}

#[test]
fn pool_slot_claim_free_fails_when_ready() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    assert!(!slot.try_claim_free());
}

#[test]
fn pool_slot_mark_ready_after_reserve() {
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free());
    slot.mark_ready();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_READY);
}

#[test]
fn pool_slot_mark_error_after_reserve() {
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free());
    slot.mark_error();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_ERROR);
}

#[test]
fn pool_slot_begin_reconnect_from_ready() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    assert!(slot.try_begin_reconnect());
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RECONNECTING);
}

#[test]
fn pool_slot_begin_reconnect_fails_from_free() {
    let slot = PoolSlot::new();
    assert!(!slot.try_begin_reconnect());
}

#[test]
fn pool_slot_reclaim_error_succeeds() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_ERROR, Ordering::Relaxed);
    assert!(slot.try_reclaim_error());
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RESERVED);
}

#[test]
fn pool_slot_reclaim_error_fails_from_ready() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    assert!(!slot.try_reclaim_error());
}

#[test]
fn pool_slot_reclaim_requires_exclusive_transition_before_release() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    assert!(slot.try_begin_reconnect());
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RECONNECTING);
    assert!(!slot.try_claim_free());
    slot.release();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
}

#[test]
fn pool_slot_reclaim_cannot_take_a_free_slot() {
    let slot = PoolSlot::new();
    assert!(!slot.try_begin_reconnect());
}

#[test]
fn pool_slot_release_clears_owner_and_state() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    slot.owner_thread.store(12345, Ordering::Relaxed);
    slot.release();
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    assert_eq!(slot.owner_thread.load(Ordering::Relaxed), 0);
}

#[test]
fn pool_slot_full_lifecycle() {
    // FREE → RESERVED → READY → RECONNECTING → READY → FREE
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free()); // FREE → RESERVED
    slot.mark_ready(); // RESERVED → READY
    assert!(slot.try_begin_reconnect()); // READY → RECONNECTING
    slot.mark_ready(); // RECONNECTING → READY
    slot.release(); // READY → FREE
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
}

#[test]
fn pool_slot_error_recovery_lifecycle() {
    // FREE → RESERVED → ERROR → RESERVED → READY → FREE
    let slot = PoolSlot::new();
    assert!(slot.try_claim_free()); // FREE → RESERVED
    slot.mark_error(); // RESERVED → ERROR
    assert!(slot.try_reclaim_error()); // ERROR → RESERVED
    slot.mark_ready(); // RESERVED → READY
    slot.release(); // READY → FREE
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
}

#[test]
fn pool_slot_concurrent_claim_only_one_wins() {
    use std::sync::Arc;
    let slot = Arc::new(PoolSlot::new());
    let mut handles = vec![];
    let wins = Arc::new(AtomicU32::new(0));

    for _ in 0..10 {
        let s = Arc::clone(&slot);
        let w = Arc::clone(&wins);
        handles.push(std::thread::spawn(move || {
            if s.try_claim_free() {
                w.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(wins.load(Ordering::Relaxed), 1);
}

// ═════════════════════════════════════════════════════════════════════════
// Regression: claiming a slot must revalidate the connection pointer.
//
// The acquire phases sample `slot.conn` *before* they CAS the slot away from
// FREE. The reaper (`PoolManager::reap_idle`) claims a FREE slot, swaps its
// connection out, and puts the slot back to FREE before the caller PQfinishes
// that connection. So a bare `try_claim_free()` can succeed on a slot whose
// connection the reaper is in the middle of destroying.
// ═════════════════════════════════════════════════════════════════════════

/// Drive a slot through exactly what `reap_idle` does to it.
fn simulate_reaper_emptying(slot: &PoolSlot) {
    assert!(slot.try_claim_free());
    slot.generation.fetch_add(1, Ordering::SeqCst);
    slot.conn.swap(std::ptr::null_mut(), Ordering::SeqCst);
    slot.owner_thread.store(0, Ordering::Release);
    slot.state.store(SLOT_FREE, Ordering::Release);
}

#[test]
fn claim_with_conn_rejects_a_slot_the_reaper_emptied() {
    let slot = PoolSlot::new();
    let conn = 0xDEAD_BEEF_usize as *mut c_void;
    slot.conn.store(conn, Ordering::Relaxed);

    // Phase 2 samples the connection while the slot is still FREE.
    let sampled = slot.conn.load(Ordering::Acquire);
    assert_eq!(sampled, conn);

    // The reaper gets in ahead of the claim and takes the connection away.
    simulate_reaper_emptying(&slot);

    // The claim must now fail: `sampled` is about to be PQfinished.
    assert!(!slot.try_claim_free_with_conn(sampled));
    // ...and the slot must be left claimable by whoever comes next.
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
}

#[test]
fn claim_with_conn_succeeds_when_the_connection_is_unchanged() {
    let slot = PoolSlot::new();
    let conn = 0xCAFE_F00D_usize as *mut c_void;
    slot.conn.store(conn, Ordering::Relaxed);

    let sampled = slot.conn.load(Ordering::Acquire);
    assert!(slot.try_claim_free_with_conn(sampled));
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RESERVED);
}

#[test]
fn claim_with_conn_rejects_a_slot_that_gained_a_connection() {
    // Phase 3 only wants genuinely empty slots. If another thread filled the
    // slot between the null check and the claim, overwriting `slot.conn` would
    // strand a live PostgreSQL connection with no owner.
    let slot = PoolSlot::new();
    assert!(slot.conn.load(Ordering::Relaxed).is_null());

    slot.conn
        .store(0x1234_5678_usize as *mut c_void, Ordering::Release);

    assert!(!slot.try_claim_free_with_conn(std::ptr::null_mut()));
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    assert!(!slot.conn.load(Ordering::Relaxed).is_null());
}

#[test]
fn claim_with_conn_fails_when_the_slot_is_not_free() {
    let slot = PoolSlot::new();
    slot.state.store(SLOT_READY, Ordering::Relaxed);
    assert!(!slot.try_claim_free_with_conn(std::ptr::null_mut()));
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_READY);
}
