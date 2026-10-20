use std::collections::HashSet;
use std::os::raw::c_void;
use std::sync::atomic::{
    AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::{Mutex, OnceLock};

use crate::ffi_types::PgConnection;
use crate::sync_utils::mutex_lock;

use super::registry::{ConnectionRegistry, DbToPool};
use super::tls_cache::tls_pool_cache_clear;
use crate::log_info_lazy;

pub(crate) const SLOT_FREE: u8 = 0;
pub(crate) const SLOT_RESERVED: u8 = 1;
pub(crate) const SLOT_READY: u8 = 2;
pub(crate) const SLOT_RECONNECTING: u8 = 3;
pub(crate) const SLOT_ERROR: u8 = 4;

pub(crate) const POOL_SIZE_DEFAULT: usize = 50;

/// A single slot in the connection pool.
/// All fields are atomic for lock-free CAS-based state transitions.
pub(crate) struct PoolSlot {
    /// Opaque pointer to C-allocated pg_connection_t (null = no connection)
    pub conn: AtomicPtr<c_void>,
    /// Thread ID of owner (0 = unowned)
    pub owner_thread: AtomicU64,
    /// Unix timestamp of last use
    pub last_used: AtomicI64,
    /// State machine: SLOT_FREE=0, SLOT_RESERVED=1, SLOT_READY=2, etc.
    pub state: AtomicU8,
    /// Monotonically increasing generation counter (detects stale TLS refs)
    pub generation: AtomicU32,
}

impl PoolSlot {
    pub fn new() -> Self {
        Self {
            conn: AtomicPtr::new(std::ptr::null_mut()),
            owner_thread: AtomicU64::new(0),
            last_used: AtomicI64::new(0),
            state: AtomicU8::new(SLOT_FREE),
            generation: AtomicU32::new(0),
        }
    }

    /// CAS: FREE → RESERVED. Returns true on success.
    pub fn try_claim_free(&self) -> bool {
        self.state
            .compare_exchange(
                SLOT_FREE,
                SLOT_RESERVED,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// CAS: FREE → RESERVED, but only if `conn` still holds `expected`.
    ///
    /// The acquire phases read `slot.conn` before they claim the slot, and the
    /// reaper empties a FREE slot and puts it straight back to FREE before it
    /// destroys the connection it took. A bare `try_claim_free` therefore says
    /// nothing about whether the pointer the caller sampled is still the
    /// slot's — the caller could walk away with one that is being PQfinished.
    /// Re-reading `conn` under the claim closes that window: by then nobody
    /// else can transition the slot, so what we see is what we get.
    ///
    /// Returns false with the slot left FREE if the connection moved.
    pub fn try_claim_free_with_conn(&self, expected: *mut c_void) -> bool {
        if !self.try_claim_free() {
            return false;
        }
        if self.conn.load(Ordering::Acquire) == expected {
            return true;
        }
        self.state.store(SLOT_FREE, Ordering::Release);
        false
    }

    /// CAS: READY → RECONNECTING. Returns true on success.
    pub fn try_begin_reconnect(&self) -> bool {
        self.state
            .compare_exchange(
                SLOT_READY,
                SLOT_RECONNECTING,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// CAS: ERROR → RESERVED. Returns true on success.
    pub fn try_reclaim_error(&self) -> bool {
        self.state
            .compare_exchange(
                SLOT_ERROR,
                SLOT_RESERVED,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// CAS: READY → FREE (zombie reclaim for dead threads).
    pub fn try_reclaim_zombie(&self) -> bool {
        self.state
            .compare_exchange(SLOT_READY, SLOT_FREE, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    }

    /// Set slot to READY (after successful connection creation).
    pub fn mark_ready(&self) {
        self.state.store(SLOT_READY, Ordering::Release);
    }

    /// Set slot to ERROR (after failed connection/reconnect).
    pub fn mark_error(&self) {
        self.state.store(SLOT_ERROR, Ordering::Release);
    }

    /// Release slot back to FREE (on close or cleanup).
    pub fn release(&self) {
        self.owner_thread.store(0, Ordering::Release);
        self.state.store(SLOT_FREE, Ordering::Release);
    }
}

/// Central pool manager holding all connection pool state.
pub(crate) struct PoolManager {
    pub slots: Vec<PoolSlot>,
    pub configured_size: AtomicUsize,
    pub configured_max_size: AtomicUsize,
    pub idle_timeout_secs: AtomicU32,
    pub library_db_path: Mutex<Option<String>>,
    pub last_reap_time: AtomicI64,
    pub init_pid: AtomicU32,
    pub registry: ConnectionRegistry,
    pub db_to_pool: DbToPool,
    pub global_metadata_id: AtomicI64,
    pub global_last_insert_rowid: AtomicI64,
    pub live_pool_conns: Mutex<HashSet<usize>>,
}

impl PoolManager {
    pub fn new(pool_size: usize, pool_max: usize) -> Self {
        // Allocate slots up to runtime max so auto-grow can expand
        // without reallocation. Only `configured_size` limits active use.
        let effective_max = pool_max.max(1);
        let mut slots = Vec::with_capacity(effective_max);
        for _ in 0..effective_max {
            slots.push(PoolSlot::new());
        }
        let effective_size = pool_size.clamp(1, effective_max);
        Self {
            slots,
            configured_size: AtomicUsize::new(effective_size),
            configured_max_size: AtomicUsize::new(effective_max),
            idle_timeout_secs: AtomicU32::new(300),
            library_db_path: Mutex::new(None),
            last_reap_time: AtomicI64::new(0),
            init_pid: AtomicU32::new(std::process::id()),
            registry: ConnectionRegistry::new(),
            db_to_pool: DbToPool::new(),
            global_metadata_id: AtomicI64::new(0),
            global_last_insert_rowid: AtomicI64::new(0),
            live_pool_conns: Mutex::new(HashSet::new()),
        }
    }

    /// Whether a READY slot may be taken back from a thread that has moved on.
    ///
    /// Two inputs, on purpose: whether any handle still references the slot,
    /// and how long it has sat idle. There is deliberately no third input for
    /// whether the owner thread is still alive. The pool used to probe that
    /// with `pthread_kill(owner, 0)`, and on musl -- which is what Plex runs
    /// on -- a `pthread_t` is a pointer to the thread's control block that
    /// `pthread_kill` dereferences to lock. Once the owner has exited and its
    /// stack is unmapped, that is a segmentation fault in whichever thread is
    /// acquiring a connection at the time. Plex's request, webhook and pool
    /// threads are exactly that short-lived, so every pod died within minutes
    /// of its first playback, in `sqlite3_prepare_v2` or `sqlite3_step`, with
    /// `pthread_kill` as the frame under the pool. Two core dumps, same stack.
    ///
    /// The probe was never sound anywhere -- thread ids are recycled, and a
    /// live handle does not need a live thread behind it -- and the reference
    /// count already says what the probe was guessing at.
    pub fn zombie_reclaimable(referenced: bool, idle_secs: i64, idle_timeout_secs: i64) -> bool {
        !referenced && idle_secs > idle_timeout_secs
    }

    /// Whether any database handle still holds this slot.
    ///
    /// The pool's reference count, and the only sound basis for deciding a
    /// slot is free to take back. It is deliberately not a guess about idle
    /// time or thread liveness: Plex keeps database handles open for the life
    /// of the process while the threads that used them come and go, so
    /// neither of those says whether the connection is still in use.
    pub fn slot_is_referenced(&self, slot_index: usize) -> bool {
        self.db_to_pool.references(slot_index) > 0
    }

    /// Get configured pool size.
    pub fn pool_size(&self) -> usize {
        self.configured_size
            .load(Ordering::Relaxed)
            .min(self.pool_max())
    }

    /// Get configured maximum pool size (runtime cap).
    pub fn pool_max(&self) -> usize {
        self.configured_max_size.load(Ordering::Relaxed)
    }

    fn find_connection_slot(&self, conn_ptr: *const c_void) -> Option<(usize, u8)> {
        if conn_ptr.is_null() {
            return None;
        }

        for (i, slot) in self.slots.iter().enumerate() {
            if slot.conn.load(Ordering::Acquire) == conn_ptr as *mut c_void {
                return Some((i, slot.state.load(Ordering::Acquire)));
            }
        }

        None
    }

    /// Where a connection pointer sits in the pool, for a log line: its slot,
    /// the slot's state, whether this thread owns it and whether a handle is
    /// still recorded on it; "not pooled" when no slot holds it.
    pub fn describe_connection(&self, conn_ptr: *const c_void, this_thread: u64) -> String {
        match self.find_connection_slot(conn_ptr) {
            None => "not pooled".to_string(),
            Some((i, state)) => {
                let state = match state {
                    SLOT_FREE => "FREE",
                    SLOT_RESERVED => "RESERVED",
                    SLOT_READY => "READY",
                    SLOT_RECONNECTING => "RECONNECTING",
                    SLOT_ERROR => "ERROR",
                    _ => "?",
                };
                let owner = self.slots[i].owner_thread.load(Ordering::Acquire);
                format!(
                    "slot {} {} owner={} refs={}",
                    i,
                    state,
                    if owner == this_thread {
                        "this thread"
                    } else {
                        "another thread"
                    },
                    self.db_to_pool.references(i)
                )
            }
        }
    }

    /// Check if a connection pointer is in any READY pool slot.
    pub fn validate_connection(&self, conn_ptr: *const c_void) -> bool {
        matches!(self.find_connection_slot(conn_ptr), Some((_, SLOT_READY)))
    }

    /// Whether a statement letting go of its stream may settle the
    /// connection it streamed on (drain it, cancelling a query still sending
    /// rows): a READY slot's connection, or a handle's own connection, which
    /// no slot holds and the registry still lists.
    ///
    /// Plex's blobs database streams on its handle's own connection, never a
    /// pooled one, so `validate_connection` alone refused every one of its
    /// streams: 2,022 "streaming_conn invalid" lines in four hours on talos
    /// (2026-10-20), nearly all one cached `SELECT COUNT(*) FROM blobs`
    /// that Plex resets after its one row.
    pub fn stream_settles_on(&self, conn_ptr: *const c_void) -> bool {
        if conn_ptr.is_null() {
            return false;
        }
        match self.find_connection_slot(conn_ptr) {
            Some((_, state)) => state == SLOT_READY,
            None => self.registry.contains_conn(conn_ptr as usize),
        }
    }

    /// Best-effort release of a stale streaming flag for a tracked pool connection.
    ///
    /// This is intentionally more permissive than `validate_connection`: it scans all
    /// allocated slots and does not require the slot to be READY, because cleanup can
    /// race with slot state transitions even while the connection pointer is still live
    /// in the pool.
    pub fn clear_streaming_active(&self, conn_ptr: *const c_void) -> bool {
        if conn_ptr.is_null() {
            return false;
        }

        if let Some((i, state)) = self.find_connection_slot(conn_ptr) {
            unsafe {
                (*(conn_ptr as *mut PgConnection))
                    .streaming_active
                    .store(0, Ordering::Release);
            }

            log_info_lazy!(
                "Pool: cleared stale streaming_active flag for slot {} (state={})",
                i,
                state
            );
            return true;
        }

        false
    }

    pub fn note_live_pool_connection(&self, conn_ptr: *const c_void) {
        if conn_ptr.is_null() {
            return;
        }
        mutex_lock(&self.live_pool_conns).insert(conn_ptr as usize);
    }

    pub fn forget_live_pool_connection(&self, conn_ptr: *const c_void) {
        if conn_ptr.is_null() {
            return;
        }
        mutex_lock(&self.live_pool_conns).remove(&(conn_ptr as usize));
    }

    pub fn is_live_pool_connection(&self, conn_ptr: *const c_void) -> bool {
        if conn_ptr.is_null() {
            return false;
        }
        mutex_lock(&self.live_pool_conns).contains(&(conn_ptr as usize))
    }

    /// Check whether a connection pointer is one we still track, either as a
    /// live pool connection or as a registered non-pooled handle connection.
    pub fn is_tracked_connection(&self, conn_ptr: *const c_void) -> bool {
        if conn_ptr.is_null() {
            return false;
        }

        self.is_live_pool_connection(conn_ptr) || self.registry.contains_conn(conn_ptr as usize)
    }

    /// Update last_used timestamp for a connection in the pool.
    pub fn touch_connection(&self, conn_ptr: *const c_void, now: i64) {
        let size = self.pool_size();
        for i in 0..size {
            let slot = &self.slots[i];
            if slot.conn.load(Ordering::Acquire) == conn_ptr as *mut c_void {
                slot.last_used.store(now, Ordering::Release);
                return;
            }
        }
    }

    /// Reset all pool state for child process after fork.
    /// Does NOT close connections (they belong to the parent).
    pub fn reset_for_child(&self) {
        let size = self.pool_size();
        for i in 0..size {
            let slot = &self.slots[i];
            slot.conn.store(std::ptr::null_mut(), Ordering::Release);
            slot.owner_thread.store(0, Ordering::Release);
            slot.last_used.store(0, Ordering::Release);
            slot.state.store(SLOT_FREE, Ordering::Release);
            slot.generation.fetch_add(1, Ordering::SeqCst);
        }
        self.registry.clear();
        self.db_to_pool.clear();
        self.init_pid.store(std::process::id(), Ordering::Release);
        tls_pool_cache_clear();
    }

    /// Scan pool for idle connections past the timeout.
    /// Returns a vec of (slot_index, conn_ptr) pairs that should be destroyed
    /// by calling C-side PQfinish. The slot's generation is bumped and state
    /// set to FREE before returning, so no other thread can use the conn.
    pub fn reap_idle(&self, now: i64) -> Vec<(usize, *mut c_void)> {
        let timeout = self.idle_timeout_secs.load(Ordering::Relaxed) as i64;
        let size = self.pool_size();
        let mut to_destroy = Vec::new();

        for i in 0..size {
            let slot = &self.slots[i];
            let state = slot.state.load(Ordering::Acquire);

            if state != SLOT_FREE {
                continue;
            }
            let conn = slot.conn.load(Ordering::Acquire);
            if conn.is_null() {
                continue;
            }
            let last_used = slot.last_used.load(Ordering::Acquire);
            if now - last_used < timeout {
                continue;
            }

            // Belt and braces. A FREE slot should have no handles left on it,
            // because releasing a handle is what frees the slot — but the
            // zombie reclaim used to put a slot back to FREE without touching
            // the map, and what follows here is PQfinish. Destroying a
            // connection somebody still holds is the worst version of this
            // bug, so it is worth the lookup to refuse.
            if self.slot_is_referenced(i) {
                continue;
            }

            if !slot.try_claim_free() {
                continue;
            }

            slot.generation.fetch_add(1, Ordering::SeqCst);

            let conn = slot.conn.swap(std::ptr::null_mut(), Ordering::SeqCst);

            slot.owner_thread.store(0, Ordering::Release);
            slot.state.store(SLOT_FREE, Ordering::Release);

            if !conn.is_null() {
                to_destroy.push((i, conn));
            }
        }

        to_destroy
    }
}

pub(crate) static POOL: OnceLock<PoolManager> = OnceLock::new();

/// Get or initialize the global pool manager.
pub(crate) fn pool() -> &'static PoolManager {
    POOL.get_or_init(|| PoolManager::new(POOL_SIZE_DEFAULT, POOL_SIZE_DEFAULT))
}
