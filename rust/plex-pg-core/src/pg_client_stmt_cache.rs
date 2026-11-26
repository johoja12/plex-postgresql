use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::ffi_types::{PgConnection, STMT_NAME_LEN};
use crate::libpq_helpers::{rust_pq_clear, rust_pq_exec};
use crate::log_debug_lazy;
use crate::log_info_lazy;

/// How many prepared statements one connection keeps, unless
/// `PLEX_PG_STMT_CACHE_SIZE` says otherwise.
///
/// It was 512, and on the owner's server (2026-11-26) a connection that had
/// filled it held 233 MB of plans in its PostgreSQL backend: Plex's queries
/// are wide, a prepared one costs the server about 0.4 MB (plan source, query
/// tree and generic plan), and the backend does not give that memory back to
/// the operating system when a statement is deallocated. Thirty-four
/// connections that full are 9.7 GB.
pub(crate) const STMT_CACHE_SIZE_DEFAULT: usize = 128;

/// The largest cache `PLEX_PG_STMT_CACHE_SIZE` can ask for.
pub(crate) const STMT_CACHE_SIZE_MAX: usize = 4096;

/// Which use of a statement on a connection prepares it, unless
/// `PLEX_PG_PREPARE_THRESHOLD` says otherwise: the second. Its first use is
/// sent unprepared, so a statement a connection sees once never takes a place
/// in the cache or memory in the backend. 1 prepares on first use, as the
/// shim did before.
pub(crate) const PREPARE_THRESHOLD_DEFAULT: u32 = 2;

/// How many not-yet-prepared statements a connection counts the uses of. Past
/// it the counts start again, which costs a statement one more unprepared use.
const SEEN_LIMIT: usize = 4096;

fn env_number(name: &str, default_value: u64, min: u64, max: u64) -> u64 {
    crate::env_utils::env_string(name)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|n| n.clamp(min, max))
        .unwrap_or(default_value)
}

/// The cache size in force: `PLEX_PG_STMT_CACHE_SIZE`, read once.
pub(crate) fn stmt_cache_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        env_number(
            "PLEX_PG_STMT_CACHE_SIZE",
            STMT_CACHE_SIZE_DEFAULT as u64,
            1,
            STMT_CACHE_SIZE_MAX as u64,
        ) as usize
    })
}

/// The prepare threshold in force: `PLEX_PG_PREPARE_THRESHOLD`, read once.
pub(crate) fn prepare_threshold() -> u32 {
    static THRESHOLD: OnceLock<u32> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        env_number(
            "PLEX_PG_PREPARE_THRESHOLD",
            PREPARE_THRESHOLD_DEFAULT as u64,
            1,
            1000,
        ) as u32
    })
}

// What the caches did since the process started, for the pool's periodic
// line (`stats_line`): without them the size and the threshold are guesses.
static STAT_HITS: AtomicU64 = AtomicU64::new(0);
static STAT_PREPARED: AtomicU64 = AtomicU64::new(0);
static STAT_UNPREPARED: AtomicU64 = AtomicU64::new(0);
static STAT_EVICTED: AtomicU64 = AtomicU64::new(0);

/// One line of the counters: executions that found their statement prepared,
/// statements prepared, executions sent unprepared because the statement had
/// not yet been used often enough, and statements deallocated to make room.
pub(crate) fn stats_line() -> String {
    format!(
        "hits={} prepared={} unprepared={} evicted={} (size {}, threshold {})",
        STAT_HITS.load(Ordering::Relaxed),
        STAT_PREPARED.load(Ordering::Relaxed),
        STAT_UNPREPARED.load(Ordering::Relaxed),
        STAT_EVICTED.load(Ordering::Relaxed),
        stmt_cache_limit(),
        prepare_threshold()
    )
}

/// Per-connection prepared statement cache entry.
#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct StmtCacheEntry {
    pub sql_hash: u64,
    pub stmt_name: [c_char; STMT_NAME_LEN],
    pub param_count: i32,
    pub last_used: i64,
}

/// Per-connection prepared statement cache: the statements this connection
/// has prepared on its server, at most `limit` of them, and a count of uses
/// for the ones it has not prepared yet.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct StmtCache {
    entries: HashMap<u64, StmtCacheEntry>,
    limit: usize,
    seen: HashMap<u64, u32>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl StmtCache {
    pub fn new() -> Self {
        Self::with_limit(stmt_cache_limit())
    }

    pub fn with_limit(limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            limit: limit.max(1),
            seen: HashMap::new(),
        }
    }

    /// Lookup by sql_hash (mutable) to update last_used.
    pub fn lookup_mut(&mut self, sql_hash: u64) -> Option<&mut StmtCacheEntry> {
        if sql_hash == 0 {
            return None;
        }
        self.entries.get_mut(&sql_hash)
    }

    /// Lookup by sql_hash. Returns Some(&entry) on hit, None on miss.
    pub fn lookup(&self, sql_hash: u64) -> Option<&StmtCacheEntry> {
        if sql_hash == 0 {
            return None;
        }
        self.entries.get(&sql_hash)
    }

    /// Add or update an entry. Returns (0, evicted_name): the name is the
    /// least recently used statement's when the cache was full, and the
    /// caller deallocates it on the server.
    pub fn add(
        &mut self,
        sql_hash: u64,
        stmt_name: [c_char; STMT_NAME_LEN],
        param_count: i32,
        now: i64,
    ) -> (i32, Option<[c_char; STMT_NAME_LEN]>) {
        if sql_hash == 0 {
            return (-1, None);
        }
        self.seen.remove(&sql_hash);

        if let Some(entry) = self.entries.get_mut(&sql_hash) {
            entry.stmt_name = stmt_name;
            entry.param_count = param_count;
            entry.last_used = now;
            return (0, None);
        }

        let mut evicted = None;
        if self.entries.len() >= self.limit {
            let oldest = self
                .entries
                .values()
                .min_by_key(|e| e.last_used)
                .map(|e| e.sql_hash);
            if let Some(hash) = oldest {
                evicted = self.entries.remove(&hash).map(|e| e.stmt_name);
            }
        }
        self.entries.insert(
            sql_hash,
            StmtCacheEntry {
                sql_hash,
                stmt_name,
                param_count,
                last_used: now,
            },
        );
        (0, evicted)
    }

    /// A statement that is not in the cache is about to run: whether this
    /// use is the one that prepares it. Counts the use otherwise.
    pub fn note_miss(&mut self, sql_hash: u64, threshold: u32) -> bool {
        if threshold <= 1 || sql_hash == 0 {
            return true;
        }
        let uses = self.seen.get(&sql_hash).copied().unwrap_or(0) + 1;
        if uses >= threshold {
            self.seen.remove(&sql_hash);
            return true;
        }
        if self.seen.len() >= SEEN_LIMIT && !self.seen.contains_key(&sql_hash) {
            self.seen.clear();
        }
        self.seen.insert(sql_hash, uses);
        false
    }

    /// Clear all entries. Returns names of all evicted statements (for DEALLOCATE).
    pub fn clear(&mut self) -> Vec<[c_char; STMT_NAME_LEN]> {
        self.seen.clear();
        self.entries.drain().map(|(_, e)| e.stmt_name).collect()
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }
}

// ─── Per-connection StmtCache registry (keyed by conn ptr) ───────────────────

/// Maps pg_connection_t* (as usize) → StmtCache.
/// Each pool connection has its own prepared statement cache.
static STMT_CACHES: OnceLock<Mutex<HashMap<usize, StmtCache>>> = OnceLock::new();

fn stmt_caches() -> &'static Mutex<HashMap<usize, StmtCache>> {
    STMT_CACHES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn clear_all_stmt_caches() {
    if let Some(caches) = STMT_CACHES.get() {
        caches.lock().unwrap().clear();
    }
}

thread_local! {
    /// Return buffer for `rust_stmt_cache_lookup`.
    ///
    /// The cache lives behind a mutex that the lookup releases before it
    /// returns, so a pointer borrowed from a cache entry is stable only for as
    /// long as nobody touches that connection's cache. Callers hold the name
    /// across PQprepare and PQexecPrepared, and a pooled connection can be
    /// re-prepared, cleared or dropped by another thread in that window.
    /// Copying into thread-local storage keeps the `const char *` ABI while
    /// giving callers a lifetime they can actually rely on: the name stays
    /// valid until this thread's next lookup.
    static LOOKUP_NAME_BUF: Cell<[c_char; STMT_NAME_LEN]> =
        const { Cell::new([0; STMT_NAME_LEN]) };
}

/// Copy `name` into this thread's lookup buffer and return a pointer to it.
/// Returns null if thread-local storage is already torn down.
fn stash_lookup_name(name: &[c_char; STMT_NAME_LEN]) -> *const c_char {
    LOOKUP_NAME_BUF
        .try_with(|buf| {
            buf.set(*name);
            buf.as_ptr() as *const c_char
        })
        .unwrap_or(std::ptr::null())
}

/// The order of uses, for the least-recently-used choice. A counter, not the
/// clock: a second holds hundreds of statements, and they all tied.
fn next_use() -> i64 {
    static USES: AtomicI64 = AtomicI64::new(0);
    USES.fetch_add(1, Ordering::Relaxed) + 1
}

fn stmt_name_from_c(ptr: *const c_char) -> Option<[c_char; STMT_NAME_LEN]> {
    if ptr.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    let mut buf = [0 as c_char; STMT_NAME_LEN];
    let len = bytes.len().min(STMT_NAME_LEN.saturating_sub(1));
    for (i, b) in bytes[..len].iter().enumerate() {
        buf[i] = *b as c_char;
    }
    buf[len] = 0;
    Some(buf)
}

fn stmt_name_to_string(name: &[c_char; STMT_NAME_LEN]) -> String {
    unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn deallocate_stmt(conn: *mut c_void, stmt_name: &[c_char; STMT_NAME_LEN]) {
    if conn.is_null() {
        return;
    }
    let pg_conn = unsafe { (*(conn as *mut PgConnection)).conn };
    if pg_conn.is_null() {
        return;
    }
    let name = stmt_name_to_string(stmt_name);
    if name.is_empty() {
        return;
    }
    let sql = format!("DEALLOCATE {}", name);
    if let Ok(cs) = CString::new(sql) {
        let res = rust_pq_exec(pg_conn, cs.as_ptr());
        if !res.is_null() {
            rust_pq_clear(res);
        }
    }
}

/// Lookup statement in cache by hash.
/// Returns 1 on hit, 0 on miss. Writes stmt_name_out to cached name on hit.
pub fn rust_stmt_cache_lookup(
    conn: *mut c_void,
    sql_hash: u64,
    stmt_name_out: *mut *const c_char,
) -> i32 {
    if stmt_name_out.is_null() {
        return 0;
    }
    unsafe {
        *stmt_name_out = std::ptr::null();
    }
    if conn.is_null() || sql_hash == 0 {
        return 0;
    }

    let name = {
        let mut caches = stmt_caches().lock().unwrap();
        let cache = match caches.get_mut(&(conn as usize)) {
            Some(c) => c,
            None => return 0,
        };
        match cache.lookup_mut(sql_hash) {
            Some(entry) => {
                entry.last_used = next_use();
                entry.stmt_name
            }
            None => return 0,
        }
    };

    let name_ptr = stash_lookup_name(&name);
    if name_ptr.is_null() {
        return 0;
    }
    unsafe {
        *stmt_name_out = name_ptr;
    }
    STAT_HITS.fetch_add(1, Ordering::Relaxed);
    1
}

/// Whether a statement the lookup just missed is to be prepared on this use.
///
/// False until the connection has run it `PLEX_PG_PREPARE_THRESHOLD` times:
/// the caller then sends it unprepared, as it does when a prepare fails. Each
/// call counts one use, so it is asked once per execution, after the miss.
pub fn rust_stmt_cache_should_prepare(conn: *mut c_void, sql_hash: u64) -> bool {
    let threshold = prepare_threshold();
    if threshold <= 1 || conn.is_null() || sql_hash == 0 {
        return true;
    }
    let prepare = {
        let mut caches = stmt_caches().lock().unwrap();
        let cache = caches.entry(conn as usize).or_insert_with(StmtCache::new);
        cache.note_miss(sql_hash, threshold)
    };
    if !prepare {
        STAT_UNPREPARED.fetch_add(1, Ordering::Relaxed);
    }
    prepare
}

/// Add statement to cache. Returns index on success, -1 on failure.
pub fn rust_stmt_cache_add(
    conn: *mut c_void,
    sql_hash: u64,
    stmt_name: *const c_char,
    param_count: i32,
) -> i32 {
    if conn.is_null() || sql_hash == 0 {
        return -1;
    }
    let name = match stmt_name_from_c(stmt_name) {
        Some(n) => n,
        None => return -1,
    };

    let now = next_use();
    let (idx, evicted, known) = {
        let mut caches = stmt_caches().lock().unwrap();
        let cache = caches.entry(conn as usize).or_insert_with(StmtCache::new);
        let known = cache.lookup(sql_hash).is_some();
        let (idx, evicted) = cache.add(sql_hash, name, param_count, now);
        (idx, evicted, known)
    };
    if !known && idx >= 0 {
        STAT_PREPARED.fetch_add(1, Ordering::Relaxed);
    }

    if let Some(evicted_name) = evicted {
        STAT_EVICTED.fetch_add(1, Ordering::Relaxed);
        deallocate_stmt(conn, &evicted_name);
        log_debug_lazy!(
            "Evicted prepared statement from cache: {}",
            stmt_name_to_string(&evicted_name)
        );
    }

    idx
}

/// Clear local prepared statement cache without sending DEALLOCATE to server.
pub fn rust_stmt_cache_clear_local(conn: *mut c_void) {
    if conn.is_null() {
        return;
    }
    let mut caches = stmt_caches().lock().unwrap();
    if let Some(cache) = caches.get_mut(&(conn as usize)) {
        cache.clear();
    }
    log_info_lazy!(
        "Cleared prepared statement cache (local only) for connection {:p}",
        conn
    );
}

/// Clear all cached statements for a connection (includes DEALLOCATE).
pub fn rust_stmt_cache_clear(conn: *mut c_void) {
    if conn.is_null() {
        return;
    }

    let evicted = {
        let mut caches = stmt_caches().lock().unwrap();
        if let Some(cache) = caches.get_mut(&(conn as usize)) {
            cache.clear()
        } else {
            Vec::new()
        }
    };

    for name in &evicted {
        deallocate_stmt(conn, name);
    }

    log_debug_lazy!("Cleared prepared statement cache for connection {:p}", conn);
}

/// Drop cache entry for a connection (no DEALLOCATE).
pub fn rust_stmt_cache_drop(conn: *mut c_void) {
    if conn.is_null() {
        return;
    }
    let mut caches = stmt_caches().lock().unwrap();
    caches.remove(&(conn as usize));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stmt_cache_initial_empty() {
        let cache = StmtCache::new();
        assert_eq!(cache.count(), 0);
    }

    #[test]
    fn stmt_cache_add_and_lookup() {
        let mut cache = StmtCache::new();
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'a' as c_char;
        let (idx, evicted) = cache.add(123, name, 1, 1);
        assert!(idx >= 0);
        assert!(evicted.is_none());
        let hit = cache.lookup(123);
        assert!(hit.is_some());
    }

    #[test]
    fn stmt_cache_lookup_miss() {
        let cache = StmtCache::new();
        assert!(cache.lookup(999).is_none());
    }

    #[test]
    fn stmt_cache_lookup_zero_hash_returns_none() {
        let cache = StmtCache::new();
        assert!(cache.lookup(0).is_none());
    }

    #[test]
    fn stmt_cache_add_zero_hash_is_noop() {
        let mut cache = StmtCache::new();
        let name = [0 as c_char; STMT_NAME_LEN];
        let (idx, evicted) = cache.add(0, name, 0, 0);
        assert_eq!(idx, -1);
        assert!(evicted.is_none());
    }

    #[test]
    fn stmt_cache_update_existing() {
        let mut cache = StmtCache::new();
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'a' as c_char;
        cache.add(123, name, 1, 1);
        let mut name2 = [0 as c_char; STMT_NAME_LEN];
        name2[0] = b'b' as c_char;
        let (idx, evicted) = cache.add(123, name2, 2, 2);
        assert!(idx >= 0);
        assert!(evicted.is_none());
        let hit = cache.lookup(123).unwrap();
        assert_eq!(hit.param_count, 2);
        assert_eq!(hit.stmt_name[0] as u8, b'b');
    }

    #[test]
    fn stmt_cache_multiple_entries() {
        let mut cache = StmtCache::new();
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'a' as c_char;
        cache.add(1, name, 1, 1);
        let mut name2 = [0 as c_char; STMT_NAME_LEN];
        name2[0] = b'b' as c_char;
        cache.add(2, name2, 1, 2);
        assert!(cache.lookup(1).is_some());
        assert!(cache.lookup(2).is_some());
    }

    #[test]
    fn stmt_cache_clear_returns_names() {
        let mut cache = StmtCache::new();
        let mut name = [0 as c_char; STMT_NAME_LEN];
        name[0] = b'a' as c_char;
        cache.add(1, name, 1, 1);
        let evicted = cache.clear();
        assert_eq!(evicted.len(), 1);
    }

    #[test]
    fn stmt_cache_clear_empty_returns_empty() {
        let mut cache = StmtCache::new();
        let evicted = cache.clear();
        assert!(evicted.is_empty());
    }

    fn name_of(text: &str) -> [c_char; STMT_NAME_LEN] {
        let mut name = [0 as c_char; STMT_NAME_LEN];
        for (i, b) in text.bytes().take(STMT_NAME_LEN - 1).enumerate() {
            name[i] = b as c_char;
        }
        name
    }

    #[test]
    fn stmt_cache_eviction_when_full() {
        let mut cache = StmtCache::with_limit(4);
        for i in 1..=4u64 {
            let (_idx, evicted) = cache.add(i, name_of(&format!("s{i}")), 1, i as i64);
            assert!(evicted.is_none());
        }
        assert_eq!(cache.count(), 4);
        let (_idx, evicted) = cache.add(9999, name_of("z"), 1, 5);
        assert_eq!(
            evicted.map(|n| stmt_name_to_string(&n)).as_deref(),
            Some("s1")
        );
        assert_eq!(
            cache.count(),
            4,
            "the cache never holds more than its limit"
        );
    }

    #[test]
    fn stmt_cache_evicts_the_least_recently_used_and_finds_the_rest() {
        let mut cache = StmtCache::with_limit(3);
        cache.add(1, name_of("a"), 0, 1);
        cache.add(2, name_of("b"), 0, 2);
        cache.add(3, name_of("c"), 0, 3);
        // `a` is used again, so `b` is now the oldest.
        cache.lookup_mut(1).unwrap().last_used = 4;
        let (_idx, evicted) = cache.add(4, name_of("d"), 0, 5);
        assert_eq!(
            evicted.map(|n| stmt_name_to_string(&n)).as_deref(),
            Some("b")
        );
        assert!(cache.lookup(2).is_none());
        // Every survivor is still found: the fixed table this replaced lost
        // an entry behind the hole an eviction left in its probe chain.
        for hash in [1u64, 3, 4] {
            assert!(cache.lookup(hash).is_some(), "lost statement {hash}");
        }
    }

    #[test]
    fn stmt_cache_default_limit_is_a_quarter_of_what_it_was() {
        assert_eq!(STMT_CACHE_SIZE_DEFAULT, 128);
        assert_eq!(
            StmtCache::with_limit(0).limit,
            1,
            "a limit of nothing is one"
        );
    }

    #[test]
    fn stmt_cache_a_statement_is_prepared_on_its_second_use() {
        let mut cache = StmtCache::with_limit(8);
        assert!(!cache.note_miss(7, 2), "the first use goes unprepared");
        assert!(cache.note_miss(7, 2), "the second prepares");
        // Another statement's uses are its own.
        assert!(!cache.note_miss(8, 2));
    }

    #[test]
    fn stmt_cache_a_threshold_of_one_prepares_on_first_use() {
        let mut cache = StmtCache::with_limit(8);
        assert!(cache.note_miss(7, 1));
        assert!(
            cache.seen.is_empty(),
            "nothing is counted when nothing waits"
        );
    }

    #[test]
    fn stmt_cache_a_higher_threshold_waits_that_many_uses() {
        let mut cache = StmtCache::with_limit(8);
        assert!(!cache.note_miss(7, 3));
        assert!(!cache.note_miss(7, 3));
        assert!(cache.note_miss(7, 3));
    }

    #[test]
    fn stmt_cache_a_prepared_statement_is_no_longer_counted() {
        let mut cache = StmtCache::with_limit(8);
        assert!(!cache.note_miss(7, 3));
        cache.add(7, name_of("s7"), 0, 1);
        assert!(cache.seen.is_empty());
    }

    #[test]
    fn stmt_cache_use_counts_are_bounded() {
        let mut cache = StmtCache::with_limit(8);
        for hash in 1..=(SEEN_LIMIT as u64 + 10) {
            cache.note_miss(hash, 2);
        }
        assert!(cache.seen.len() <= SEEN_LIMIT);
    }

    #[test]
    fn stmt_cache_clear_forgets_use_counts() {
        let mut cache = StmtCache::with_limit(8);
        cache.note_miss(7, 2);
        cache.clear();
        assert!(
            !cache.note_miss(7, 2),
            "a cleared connection counts from one"
        );
    }

    #[test]
    fn stmt_cache_should_prepare_counts_per_connection() {
        if prepare_threshold() != 2 {
            return; // the environment set another threshold
        }
        let a = 0x51A7_1001_usize as *mut c_void;
        let b = 0x51A7_1002_usize as *mut c_void;
        assert!(!rust_stmt_cache_should_prepare(a, 0xFEED));
        assert!(!rust_stmt_cache_should_prepare(b, 0xFEED));
        assert!(rust_stmt_cache_should_prepare(a, 0xFEED));
        rust_stmt_cache_drop(a);
        rust_stmt_cache_drop(b);
    }

    // ─── Regression: the name handed back by rust_stmt_cache_lookup ──────────
    //
    // `rust_stmt_cache_lookup` drops the STMT_CACHES mutex before it returns,
    // so a `*const c_char` borrowed from the cache entry is only as stable as
    // the cache itself. Callers hold that pointer across PQprepare and
    // PQexecPrepared; meanwhile another thread on the same pooled connection
    // can re-prepare, evict, clear or drop the entry. The returned name must
    // therefore be a copy the caller owns for the duration of the call.
    //
    // A fake connection pointer is fine here: neither lookup nor a
    // non-evicting add ever dereferences it.

    #[test]
    fn stmt_cache_lookup_name_is_stable_after_the_entry_is_rewritten() {
        let conn = 0x51A7_0001_usize as *mut c_void;
        let hash = 0xA11CE_u64;
        let first = CString::new("stmt_first").unwrap();
        let second = CString::new("stmt_second_name").unwrap();

        assert!(rust_stmt_cache_add(conn, hash, first.as_ptr(), 0) >= 0);

        let mut name: *const c_char = std::ptr::null();
        assert_eq!(rust_stmt_cache_lookup(conn, hash, &mut name), 1);
        assert!(!name.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(name) }.to_str().unwrap(),
            "stmt_first"
        );

        // Another thread re-prepares the same SQL under a different name.
        // `add` updates the existing entry in place, which is exactly the
        // memory an aliasing pointer would be looking at.
        assert!(rust_stmt_cache_add(conn, hash, second.as_ptr(), 0) >= 0);

        assert_eq!(
            unsafe { CStr::from_ptr(name) }.to_str().unwrap(),
            "stmt_first",
            "lookup handed back a pointer into the cache instead of a copy"
        );

        rust_stmt_cache_drop(conn);
    }

    #[test]
    fn stmt_cache_lookup_name_survives_dropping_the_connection_cache() {
        let conn = 0x51A7_0002_usize as *mut c_void;
        let hash = 0xB0B_u64;
        let only = CString::new("stmt_only").unwrap();

        assert!(rust_stmt_cache_add(conn, hash, only.as_ptr(), 0) >= 0);

        let mut name: *const c_char = std::ptr::null();
        assert_eq!(rust_stmt_cache_lookup(conn, hash, &mut name), 1);

        // Pool reuse drops the whole per-connection cache, freeing the Vec the
        // entry lived in. The caller's pointer must not have pointed there.
        rust_stmt_cache_drop(conn);

        assert_eq!(
            unsafe { CStr::from_ptr(name) }.to_str().unwrap(),
            "stmt_only"
        );
    }

    #[test]
    fn stmt_cache_lookup_miss_leaves_the_out_pointer_null() {
        let conn = 0x51A7_0003_usize as *mut c_void;
        // Seed with something non-null so the assertion proves the miss path
        // actively clears the out-parameter.
        let sentinel = CString::new("untouched").unwrap();
        let mut name: *const c_char = sentinel.as_ptr();
        assert_eq!(rust_stmt_cache_lookup(conn, 0x4E4F5045_u64, &mut name), 0);
        assert!(name.is_null());
    }
}
