use super::*;

// ═════════════════════════════════════════════════════════════════════════
// Pool Manager
// ═════════════════════════════════════════════════════════════════════════

unsafe fn alloc_fake_pg_connection() -> *mut PgConnection {
    let conn = libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection;
    assert!(!conn.is_null());
    conn
}

#[test]
fn a_connection_is_freed_only_when_the_last_reference_lets_go() {
    // PgStmt is reference counted; the connection it points at was not, so
    // the pool freed connections out from under live statements. The count
    // has one holder for the pool slot and one for every statement pointing
    // at it, and the struct goes when the last of them lets go.
    use crate::ffi_types::{conn_ref, conn_refs, conn_unref};

    let conn = unsafe { alloc_fake_pg_connection() };
    conn_ref(conn); // the pool slot
    assert_eq!(conn_refs(conn), 1);

    conn_ref(conn); // a statement takes it
    conn_ref(conn); // and another
    assert_eq!(conn_refs(conn), 3);

    assert!(
        !conn_unref(conn),
        "the slot letting go is not the last reference"
    );
    assert!(!conn_unref(conn), "one statement still holds it");
    assert_eq!(conn_refs(conn), 1);

    assert!(
        conn_unref(conn),
        "the last statement letting go is what frees it"
    );
}

#[test]
fn a_statement_holds_a_reference_for_every_connection_pointer_it_keeps() {
    // A statement can name three different connections at once: the one it
    // was prepared on, the one its current result belongs to, and the one it
    // has claimed for streaming. Each is a pointer the pool must not free
    // underneath, so each is counted.
    use crate::ffi_types::{conn_ref, conn_refs, PgStmt};

    let conn = unsafe { alloc_fake_pg_connection() };
    conn_ref(conn); // the pool slot

    let mut stmt = PgStmt::new();
    stmt.set_conn(conn);
    stmt.set_result_conn(conn);
    stmt.set_streaming_conn(conn);
    assert_eq!(conn_refs(conn), 4, "the slot plus the statement's three");

    // Pointing a slot at what it already holds must not double count, or the
    // connection would never reach zero and never be freed.
    stmt.set_result_conn(conn);
    assert_eq!(conn_refs(conn), 4);

    stmt.release_conns();
    assert_eq!(
        conn_refs(conn),
        1,
        "freeing a statement gives back every reference it held"
    );
}

#[test]
fn releasing_a_null_connection_is_harmless() {
    // The three pointers a statement can hold are null far more often than
    // not, and every release path would otherwise need to check first.
    use crate::ffi_types::{conn_ref, conn_unref};
    conn_ref(std::ptr::null_mut());
    assert!(!conn_unref(std::ptr::null_mut()));
}

#[test]
fn a_connection_a_statement_still_holds_survives_the_pool_letting_go() {
    // The crash this whole change exists for: reap_idle closed a connection
    // and freed the struct, while statements were still pointing at it — and
    // errmsg_impl had handed Plex a pointer into conn.last_error.
    //
    // The pool letting go now only drops its own reference. The connection is
    // closed and marked unusable, which the sixty-four is_pg_active checks
    // are built to handle, and the struct stays until the statement lets go.
    use crate::ffi_types::{conn_refs, PgStmt};

    let conn = unsafe { alloc_fake_pg_connection() };
    unsafe {
        (*conn).is_pg_active = 1;
    }
    crate::ffi_types::conn_ref(conn); // the pool slot

    let mut stmt = PgStmt::new();
    stmt.set_conn(conn);

    super::super::connection_lifecycle::destroy_pool_connection(conn as *mut c_void);

    // Reading these at all is the point: before this change the struct had
    // been freed by now, and this read is the one that corrupted Plex.
    assert_eq!(conn_refs(conn), 1, "the statement still holds it");
    assert_eq!(
        unsafe { (*conn).is_pg_active },
        0,
        "and it reports itself unusable"
    );
    assert!(unsafe { (*conn).conn.is_null() });

    stmt.release_conns();
}

#[test]
fn releasing_a_connection_twice_refuses_rather_than_freeing_it_twice() {
    // Belt and braces for the counting itself. A double release is a
    // book-keeping bug, but turning it into a double free would be the very
    // corruption this is meant to prevent.
    use crate::ffi_types::{conn_ref, conn_refs, conn_unref};

    let conn = unsafe { alloc_fake_pg_connection() };
    conn_ref(conn);
    assert!(conn_unref(conn), "the only holder letting go frees it");

    // The struct is gone, so use a fresh one to prove the guard.
    let other = unsafe { alloc_fake_pg_connection() };
    assert!(
        !conn_unref(other),
        "nobody held it, so it must not be freed"
    );
    assert_eq!(conn_refs(other), 0, "and the count is left where it was");
}

#[test]
fn pool_manager_creates_slots() {
    let pm = PoolManager::new(10, 64);
    assert_eq!(pm.pool_size(), 10);
    // slots.len() tracks runtime max for auto-grow support
    assert_eq!(pm.slots.len(), 64);
    for slot in &pm.slots {
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    }
}

#[test]
fn pool_manager_validate_connection_found() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *mut c_void;
    pm.slots[2].conn.store(fake_conn, Ordering::Relaxed);
    pm.slots[2].state.store(SLOT_READY, Ordering::Relaxed);
    assert!(pm.validate_connection(fake_conn));
}

#[test]
fn pool_manager_validate_connection_not_found() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *mut c_void;
    assert!(!pm.validate_connection(fake_conn));
}

#[test]
fn pool_manager_validate_connection_not_ready() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *mut c_void;
    pm.slots[2].conn.store(fake_conn, Ordering::Relaxed);
    pm.slots[2].state.store(SLOT_FREE, Ordering::Relaxed); // not READY
    assert!(!pm.validate_connection(fake_conn));
}

// A stream is settled on a READY slot's connection and on a handle's own
// connection (Plex's blobs database never streams on a pooled one), and on
// nothing else: a slot that has left READY may be reconnecting or already
// another thread's, and a connection the registry no longer lists belongs to
// a closed handle.
#[test]
fn pool_manager_stream_settles_on_a_ready_slot_or_a_handles_own_connection() {
    let pm = PoolManager::new(5, 64);
    let ready = 0xBEE0 as *mut c_void;
    pm.slots[1].conn.store(ready, Ordering::Relaxed);
    pm.slots[1].state.store(SLOT_READY, Ordering::Relaxed);
    assert!(pm.stream_settles_on(ready), "a READY slot's connection");

    let left = 0xBEE1 as *mut c_void;
    pm.slots[2].conn.store(left, Ordering::Relaxed);
    pm.slots[2].state.store(SLOT_FREE, Ordering::Relaxed);
    pm.registry.register(0x200, left as usize);
    assert!(
        !pm.stream_settles_on(left),
        "a pooled connection whose slot left READY, registered or not"
    );

    let handle = 0xBEE2 as *const c_void;
    pm.registry.register(0x100, handle as usize);
    assert!(pm.stream_settles_on(handle), "a handle's own connection");
    assert!(
        !pm.validate_connection(handle),
        "which validate_connection refuses: the blobs streams on talos"
    );

    pm.registry.unregister(0x100);
    assert!(!pm.stream_settles_on(handle), "a closed handle's");
    assert!(!pm.stream_settles_on(0xBEE3 as *const c_void), "an unknown one");
    assert!(!pm.stream_settles_on(std::ptr::null()));
}

#[test]
fn pool_manager_clear_streaming_active_returns_false_for_unknown_conn() {
    let pm = PoolManager::new(5, 64);
    assert!(!pm.clear_streaming_active(0xBEEF as *const c_void));
}

#[test]
fn pool_manager_clear_streaming_active_releases_non_ready_slot() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = unsafe { alloc_fake_pg_connection() };
    unsafe {
        (*fake_conn).streaming_active.store(1, Ordering::Release);
    }
    pm.slots[1]
        .conn
        .store(fake_conn as *mut c_void, Ordering::Relaxed);
    pm.slots[1].state.store(SLOT_RESERVED, Ordering::Relaxed);

    assert!(pm.clear_streaming_active(fake_conn as *const c_void));
    unsafe {
        assert_eq!((*fake_conn).streaming_active.load(Ordering::Acquire), 0);
        libc::free(fake_conn as *mut c_void);
    }
}

#[test]
fn pool_manager_clear_streaming_active_releases_ready_slot() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = unsafe { alloc_fake_pg_connection() };
    unsafe {
        (*fake_conn).streaming_active.store(1, Ordering::Release);
    }
    pm.slots[2]
        .conn
        .store(fake_conn as *mut c_void, Ordering::Relaxed);
    pm.slots[2].state.store(SLOT_READY, Ordering::Relaxed);

    assert!(pm.clear_streaming_active(fake_conn as *const c_void));
    unsafe {
        assert_eq!((*fake_conn).streaming_active.load(Ordering::Acquire), 0);
        libc::free(fake_conn as *mut c_void);
    }
}

#[test]
fn pool_manager_clear_streaming_active_null_returns_false() {
    let pm = PoolManager::new(5, 64);
    assert!(!pm.clear_streaming_active(std::ptr::null()));
}

#[test]
fn pool_manager_live_pool_connection_registry_tracks_membership() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *const c_void;
    assert!(!pm.is_live_pool_connection(fake_conn));
    pm.note_live_pool_connection(fake_conn);
    assert!(pm.is_live_pool_connection(fake_conn));
    pm.forget_live_pool_connection(fake_conn);
    assert!(!pm.is_live_pool_connection(fake_conn));
}

#[test]
fn pool_manager_is_live_pool_connection_rejects_unknown_conn() {
    let pm = PoolManager::new(5, 64);
    assert!(!pm.is_live_pool_connection(0xBEEF as *const c_void));
}

#[test]
fn pool_manager_is_tracked_connection_accepts_live_pool_conn() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *const c_void;
    pm.note_live_pool_connection(fake_conn);
    assert!(pm.is_tracked_connection(fake_conn));
}

#[test]
fn pool_manager_is_tracked_connection_accepts_registered_conn() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *const c_void;
    pm.registry.register(0x100, fake_conn as usize);
    assert!(pm.is_tracked_connection(fake_conn));
}

#[test]
fn pool_manager_is_tracked_connection_rejects_unknown_conn() {
    let pm = PoolManager::new(5, 64);
    assert!(!pm.is_tracked_connection(0xBEEF as *const c_void));
}

#[test]
fn pool_manager_touch_connection() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *mut c_void;
    pm.slots[1].conn.store(fake_conn, Ordering::Relaxed);
    pm.slots[1].last_used.store(100, Ordering::Relaxed);

    pm.touch_connection(fake_conn, 999);
    assert_eq!(pm.slots[1].last_used.load(Ordering::Relaxed), 999);
}

#[test]
fn pool_manager_touch_unknown_conn_is_noop() {
    let pm = PoolManager::new(5, 64);
    pm.touch_connection(0xBEEF as *const c_void, 999);
    // Should not panic or modify anything
}

#[test]
fn pool_manager_slot_held_by_a_database_handle_is_referenced() {
    // The zombie reclaim decided a slot was abandoned from its idle time and
    // whether the thread that opened it was still alive. Neither says
    // anything about whether Plex still holds the handle: it opens twenty at
    // startup and keeps them for the life of the process, while the worker
    // threads that used them come and go.
    //
    // Reclaiming one of those handed a live PGconn to a second thread, and
    // libpq is not thread-safe per connection, so Plex corrupted its heap and
    // died with no dump and no segfault left behind to explain it.
    let pm = PoolManager::new(5, 64);
    assert!(!pm.slot_is_referenced(3));

    pm.db_to_pool.assign(0x100, 3);
    assert!(pm.slot_is_referenced(3), "a handle holds this slot");
    assert!(!pm.slot_is_referenced(4), "and only this slot");

    pm.db_to_pool.release(0x100);
    assert!(
        !pm.slot_is_referenced(3),
        "once Plex closes the handle the slot is genuinely free"
    );
}
