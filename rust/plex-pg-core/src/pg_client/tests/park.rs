use super::*;
use std::cell::Cell;

// ═════════════════════════════════════════════════════════════════════════
// The idle parker: PoolManager::park_idle and park_connection
// ═════════════════════════════════════════════════════════════════════════

const TIMEOUT: u32 = 1800;

fn pool_with_ready_slot(last_used: i64) -> (PoolManager, *mut c_void) {
    let pm = PoolManager::new(5, 64);
    pm.park_timeout_secs.store(TIMEOUT, Ordering::Relaxed);
    let fake = 0xBEEF as *mut c_void;
    pm.slots[0].conn.store(fake, Ordering::Relaxed);
    pm.slots[0].state.store(SLOT_READY, Ordering::Relaxed);
    pm.slots[0].owner_thread.store(42, Ordering::Relaxed);
    pm.slots[0].last_used.store(last_used, Ordering::Relaxed);
    (pm, fake)
}

#[test]
fn park_closes_a_ready_slot_unused_past_the_timeout_and_keeps_its_place() {
    let (pm, fake) = pool_with_ready_slot(1000);
    // A handle still points at the slot: the reaper would never touch it.
    pm.db_to_pool.assign(0xDB, 0);
    let generation = pm.slots[0].generation.load(Ordering::Relaxed);
    let closed = Cell::new(0usize);

    let parked = pm.park_idle(
        1000 + TIMEOUT as i64 + 1,
        4,
        |_| true,
        |conn| {
            assert_eq!(conn, fake);
            closed.set(closed.get() + 1);
            true
        },
    );

    assert_eq!(parked, vec![(0, TIMEOUT as i64 + 1)]);
    assert_eq!(closed.get(), 1);
    assert_eq!(pm.parked_total.load(Ordering::Relaxed), 1);
    // Everything but the server session is as it was.
    assert_eq!(pm.slots[0].state.load(Ordering::Relaxed), SLOT_READY);
    assert_eq!(pm.slots[0].conn.load(Ordering::Relaxed), fake);
    assert_eq!(pm.slots[0].owner_thread.load(Ordering::Relaxed), 42);
    assert_eq!(pm.slots[0].generation.load(Ordering::Relaxed), generation);
    assert_eq!(pm.db_to_pool.references(0), 1);
}

#[test]
fn park_leaves_a_slot_used_within_the_timeout() {
    let (pm, _fake) = pool_with_ready_slot(1000);
    let parked = pm.park_idle(
        1000 + TIMEOUT as i64,
        4,
        |_| true,
        |_| panic!("a slot used within the timeout was closed"),
    );
    assert!(parked.is_empty());
}

#[test]
fn park_holds_the_slot_while_it_closes_so_no_acquire_takes_it() {
    let (pm, _fake) = pool_with_ready_slot(1000);
    let slot = &pm.slots[0];
    pm.park_idle(
        10_000,
        4,
        |_| {
            // Looked at before the hold: still READY.
            assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_READY);
            true
        },
        |_| {
            // phase 1 only takes a READY slot, the zombie reclaim only
            // frees one, and phase 2 only claims a FREE one.
            assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RECONNECTING);
            true
        },
    );
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_READY);
}

#[test]
fn park_leaves_a_slot_an_acquire_stamped_before_the_hold() {
    let (pm, _fake) = pool_with_ready_slot(1000);
    let slot = &pm.slots[0];
    let parked = pm.park_idle(
        10_000,
        4,
        |_| {
            // The slot's thread acquires between the parker's first look
            // and its hold: phase 1 stamps last_used before anything else.
            slot.last_used.store(10_000, Ordering::SeqCst);
            true
        },
        |_| panic!("a connection just handed to its thread was closed"),
    );
    assert!(parked.is_empty());
    assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_READY);
}

#[test]
fn park_does_not_count_a_connection_that_refused() {
    let (pm, _fake) = pool_with_ready_slot(1000);
    // In use: park_connection found its mutex held, a stream or a
    // transaction open.
    let parked = pm.park_idle(10_000, 4, |_| true, |_| false);
    assert!(parked.is_empty());
    assert_eq!(pm.parked_total.load(Ordering::Relaxed), 0);
    assert_eq!(pm.slots[0].state.load(Ordering::Relaxed), SLOT_READY);
}

#[test]
fn park_does_not_take_a_slot_already_parked_away_from_its_thread() {
    let (pm, _fake) = pool_with_ready_slot(1000);
    let slot = &pm.slots[0];
    let parked = pm.park_idle(
        10_000,
        4,
        |_| {
            assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_READY);
            false
        },
        |_| panic!("a connection with no session was closed again"),
    );
    assert!(parked.is_empty());
}

#[test]
fn park_never_closes_anything_with_a_timeout_of_zero() {
    let (pm, _fake) = pool_with_ready_slot(0);
    pm.park_timeout_secs.store(0, Ordering::Relaxed);
    let parked = pm.park_idle(
        i64::MAX / 2,
        4,
        |_| true,
        |_| panic!("PLEX_PG_PARK_TIMEOUT=0 closed a connection"),
    );
    assert!(parked.is_empty());
}

#[test]
fn park_closes_only_so_many_in_one_pass() {
    let pm = PoolManager::new(5, 64);
    pm.park_timeout_secs.store(TIMEOUT, Ordering::Relaxed);
    for i in 0..5 {
        pm.slots[i]
            .conn
            .store((0xB000 + i) as *mut c_void, Ordering::Relaxed);
        pm.slots[i].state.store(SLOT_READY, Ordering::Relaxed);
        pm.slots[i].last_used.store(0, Ordering::Relaxed);
    }
    assert_eq!(pm.park_idle(10_000, 2, |_| true, |_| true).len(), 2);
    // The next pass takes the next ones: `has_session` is what tells it the
    // first two are done.
    let done = [0xB000usize, 0xB001];
    let parked = pm.park_idle(10_000, 2, |conn| !done.contains(&(conn as usize)), |_| true);
    assert_eq!(parked.iter().map(|p| p.0).collect::<Vec<_>>(), vec![2, 3]);
}

#[test]
fn park_leaves_free_and_empty_slots_to_the_reaper() {
    let pm = PoolManager::new(5, 64);
    pm.park_timeout_secs.store(TIMEOUT, Ordering::Relaxed);
    pm.slots[0]
        .conn
        .store(0xBEEF as *mut c_void, Ordering::Relaxed);
    pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
    pm.slots[1].state.store(SLOT_READY, Ordering::Relaxed); // no connection
    let parked = pm.park_idle(10_000, 4, |_| true, |_| panic!("not a READY connection"));
    assert!(parked.is_empty());
}

#[test]
fn park_counts_ready_and_parked_slots() {
    let pm = PoolManager::new(5, 64);
    for i in 0..3 {
        pm.slots[i]
            .conn
            .store((0xB000 + i) as *mut c_void, Ordering::Relaxed);
        pm.slots[i].state.store(SLOT_READY, Ordering::Relaxed);
    }
    let (ready, parked) = pm.ready_and_parked(|conn| conn as usize != 0xB001);
    assert_eq!((ready, parked), (3, 1));
}

#[test]
fn park_timeout_setting() {
    assert_eq!(park_timeout_from(None), PARK_TIMEOUT_DEFAULT_SECS);
    assert_eq!(park_timeout_from(Some("junk")), PARK_TIMEOUT_DEFAULT_SECS);
    assert_eq!(park_timeout_from(Some("0")), 0, "0 never closes one");
    // Below the floor a connection could be closed under the call that
    // acquired it: a library statement runs for five minutes.
    assert_eq!(park_timeout_from(Some("30")), PARK_TIMEOUT_MIN_SECS);
    assert_eq!(park_timeout_from(Some(" 7200 ")), 7200);
}

#[test]
fn park_connection_refuses_one_with_no_session() {
    let conn = super::connection_lifecycle::create_connection_struct(
        "/x/com.plexapp.plugins.library.db",
        std::ptr::null_mut(),
    );
    assert!(!conn.is_null());
    assert!(!super::connection_lifecycle::conn_has_session(
        conn as *mut c_void
    ));
    // Nothing to close, and the mutex is free again afterwards.
    assert!(!super::connection_lifecycle::park_connection(
        conn as *mut c_void
    ));
    assert!(!super::connection_lifecycle::park_connection(
        conn as *mut c_void
    ));
    super::connection_lifecycle::destroy_connection_struct(conn);
}
