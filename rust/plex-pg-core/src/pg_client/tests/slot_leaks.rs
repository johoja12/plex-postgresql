//! The three ways a pool slot was lost for good (audit 2026-10-16, #7):
//! a thread exiting with its READY slots, a stream flag never cleared on a
//! connection that left the pool, and every errmsg()/changes() call
//! re-assigning the handle's slot (and acquiring one to do it).

use super::*;
use crate::ffi_types::{conn_ref, sqlite3, PgConnection};
use crate::pg_client::pool_runtime::release_slots_owned_by;
use crate::pg_client::threading::current_thread_id;

fn fake_conn(path: &str) -> *mut PgConnection {
    let c = unsafe { libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection };
    assert!(!c.is_null());
    conn_ref(c);
    unsafe {
        write_str_to_cbuf(&mut (*c).db_path, path);
        (*c).is_pg_active = 1;
    }
    c
}

#[test]
fn a_thread_exiting_gives_its_ready_slots_back() {
    let pm = PoolManager::new(4, 4);
    let me = current_thread_id();
    // Two slots this thread owns, one another thread does, one free.
    for i in 0..2 {
        pm.slots[i].state.store(SLOT_READY, Ordering::Relaxed);
        pm.slots[i].owner_thread.store(me, Ordering::Relaxed);
    }
    pm.slots[2].state.store(SLOT_READY, Ordering::Relaxed);
    pm.slots[2].owner_thread.store(me ^ 0x1, Ordering::Relaxed);

    assert_eq!(release_slots_owned_by(&pm, me, "test"), 2);
    for i in 0..2 {
        assert_eq!(pm.slots[i].state.load(Ordering::Relaxed), SLOT_FREE);
        assert_eq!(pm.slots[i].owner_thread.load(Ordering::Relaxed), 0);
    }
    assert_eq!(
        pm.slots[2].state.load(Ordering::Relaxed),
        SLOT_READY,
        "another thread's slot is not touched"
    );
    assert_eq!(pm.slots[3].state.load(Ordering::Relaxed), SLOT_FREE);
}

#[test]
fn releasing_a_slot_clears_a_stream_left_on_its_connection() {
    let pm = PoolManager::new(2, 2);
    let me = current_thread_id();
    // A connection struct with no libpq handle: nothing to cancel, but the
    // flag must still come off.
    let conn = fake_conn("/x/com.plexapp.plugins.library.db");
    unsafe { (*conn).streaming_active.store(1, Ordering::Relaxed) };
    pm.slots[0]
        .conn
        .store(conn as *mut c_void, Ordering::Relaxed);
    pm.slots[0].state.store(SLOT_READY, Ordering::Relaxed);
    pm.slots[0].owner_thread.store(me, Ordering::Relaxed);

    assert_eq!(release_slots_owned_by(&pm, me, "test"), 1);
    assert_eq!(
        unsafe { (*conn).streaming_active.load(Ordering::Relaxed) },
        0
    );
    assert_eq!(pm.slots[0].state.load(Ordering::Relaxed), SLOT_FREE);
    pm.slots[0]
        .conn
        .store(std::ptr::null_mut(), Ordering::Relaxed);
    assert!(crate::ffi_types::conn_unref(conn));
}

// A statement cleared on a connection the pool has let go of (not in any
// READY slot) used to leave `streaming_active` set on that connection for
// ever; the next thread handed the slot was told it was streaming and sent
// elsewhere, so the slot was dead to everyone.
#[test]
fn clearing_a_stream_on_a_connection_outside_the_pool_still_clears_the_flag() {
    let conn = fake_conn("/x/com.plexapp.plugins.library.db");
    let sql = CString::new("SELECT 1").unwrap();
    let stmt = crate::pg_statement::rust_stmt_create(
        std::ptr::null_mut(),
        sql.as_ptr(),
        std::ptr::null_mut(),
    );
    unsafe {
        let s = &mut *stmt;
        s.streaming_mode = 1;
        s.set_streaming_conn(conn);
        (*conn).streaming_active.store(1, Ordering::Relaxed);
    }
    assert_eq!(
        crate::pg_client::rust_pool_validate_connection(conn as *const c_void),
        0,
        "the connection is in no READY slot, so this is the invalid branch"
    );
    crate::pg_statement::rust_stmt_clear_result(stmt);
    unsafe {
        assert_eq!((*conn).streaming_active.load(Ordering::Relaxed), 0);
        assert_eq!((*stmt).streaming_mode, 0);
        assert!((*stmt).streaming_conn().is_null());
    }
    crate::pg_statement::rust_stmt_free(stmt);
    assert!(crate::ffi_types::conn_unref(conn));
}

// errmsg()/changes()/last_insert_rowid() read the connection their
// statement ran on without acquiring one: a handle that has no slot and no
// pooled connection is answered from its own registered connection, and a
// handle on a non-library path from its handle connection -- never by a
// trip through the pool's retry ladder.
#[test]
fn peeking_a_handle_never_acquires_a_pool_slot() {
    let handle = fake_conn("/x/com.plexapp.plugins.library.db");
    let db = 0x7a3e_0001usize as *const sqlite3;
    unsafe { (*handle).shadow_db = db as *mut sqlite3 };
    crate::pg_client::rust_pg_register_connection(handle);

    let before: usize = (0..pool().pool_size())
        .filter(|&i| pool().slots[i].state.load(Ordering::Relaxed) != SLOT_FREE)
        .count();
    let peeked = crate::pg_client::peek_connection(db);
    let after: usize = (0..pool().pool_size())
        .filter(|&i| pool().slots[i].state.load(Ordering::Relaxed) != SLOT_FREE)
        .count();
    assert_eq!(peeked, handle, "nothing pooled for it: the handle's own");
    assert_eq!(before, after, "no slot was taken to answer");
    assert!(
        pool().db_to_pool.find(db as usize).is_none(),
        "and the handle was not assigned a slot"
    );

    crate::pg_client::rust_pg_unregister_connection(handle);
    assert!(crate::pg_client::peek_connection(db).is_null());
    assert!(crate::ffi_types::conn_unref(handle));
}

#[test]
fn peeking_an_unknown_handle_answers_null() {
    assert!(crate::pg_client::peek_connection(std::ptr::null()).is_null());
    assert!(crate::pg_client::peek_connection(0x7a3e_0002usize as *const sqlite3).is_null());
}

#[test]
fn a_registered_library_handle_is_pg_backed() {
    let handle = fake_conn("/x/com.plexapp.plugins.library.db");
    let db = 0x7a3e_0003usize as *const sqlite3;
    unsafe { (*handle).shadow_db = db as *mut sqlite3 };
    crate::pg_client::rust_pg_register_connection(handle);
    assert!(crate::pg_client::handle_is_pg_library(db));
    crate::pg_client::rust_pg_unregister_connection(handle);
    assert!(!crate::pg_client::handle_is_pg_library(db));
    assert!(crate::ffi_types::conn_unref(handle));

    let other = fake_conn("/x/other.db");
    let db2 = 0x7a3e_0004usize as *const sqlite3;
    unsafe { (*other).shadow_db = db2 as *mut sqlite3 };
    crate::pg_client::rust_pg_register_connection(other);
    assert!(!crate::pg_client::handle_is_pg_library(db2));
    crate::pg_client::rust_pg_unregister_connection(other);
    assert!(crate::ffi_types::conn_unref(other));
}
