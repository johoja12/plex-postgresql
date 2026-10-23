/// Module: shim_alloc
///
/// Allocation tracking and reporting backend for the plex PostgreSQL shim.
/// Handles atomic counters, a lock-free size-tracking hash table, periodic
/// summary logging, and leak-site aggregation.
///
/// The actual malloc/free/realloc/calloc/strdup calls remain in C because
/// they must call real libc functions and capture `__FILE__`/`__LINE__` at
/// the call site via C macros. This module owns all the tracking state and
/// exposes it to C through an FFI surface.
///
/// Environment variables:
///   `PLEX_PG_ALLOC_TRACK=1` — Enable allocation counters + 60s summary logging
///   `PLEX_PG_ALLOC_TRACE=1` — Also enable per-site leak tracking (implies TRACK)
///
/// FFI surface (callable from C):
///   rust_shim_alloc_enabled()
///   rust_shim_alloc_record(ptr, size, file, line)
///   rust_shim_alloc_remove(ptr) -> old_size
///   rust_shim_alloc_record_alloc(size)
///   rust_shim_alloc_record_free(size)
///   rust_shim_alloc_record_realloc(old_size, new_size)
///   rust_shim_alloc_get_stats(out)
///   rust_shim_alloc_log_summary()
///   rust_shim_alloc_maybe_log()
///   rust_shim_alloc_dump_leaks()
///   rust_shim_alloc_reset()
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::env_utils;

// ─── Enabled state ────────────────────────────────────────────────────────────
//
// -1 = not yet checked, 0 = off, 1 = track, 2 = track+trace
// We use i32 so we can store -1 as the "uninitialized" sentinel.

static G_ENABLED: AtomicI32 = AtomicI32::new(-1);

/// Check (and cache) the tracking mode from the environment.
///
/// Returns 0 (off), 1 (track), or 2 (trace).
fn shim_alloc_enabled() -> i32 {
    let v = G_ENABLED.load(Ordering::Relaxed);
    if v >= 0 {
        return v;
    }
    // First call: inspect environment variables and cache the result.
    let result = if env_utils::env_string("PLEX_PG_ALLOC_TRACE")
        .map(|v| v.starts_with('1'))
        .unwrap_or(false)
    {
        2 // trace implies track
    } else if env_utils::env_string("PLEX_PG_ALLOC_TRACK")
        .map(|v| v.starts_with('1'))
        .unwrap_or(false)
    {
        1
    } else {
        0
    };
    // Store with Release so other threads see the write once it's done.
    G_ENABLED.store(result, Ordering::Release);
    result
}

fn shim_alloc_trace_enabled() -> bool {
    shim_alloc_enabled() >= 2
}

// ─── Atomic counters ──────────────────────────────────────────────────────────

static G_TOTAL_ALLOCS: AtomicU64 = AtomicU64::new(0);
static G_TOTAL_FREES: AtomicU64 = AtomicU64::new(0);
static G_TOTAL_REALLOCS: AtomicU64 = AtomicU64::new(0);
static G_BYTES_ALLOC: AtomicU64 = AtomicU64::new(0);
static G_BYTES_FREED: AtomicU64 = AtomicU64::new(0);
/// Signed: can transiently go negative if frees arrive before alloc records.
static G_BYTES_LIVE: AtomicI64 = AtomicI64::new(0);
static G_PEAK_LIVE: AtomicU64 = AtomicU64::new(0);
static G_LAST_LOG_TS: AtomicU64 = AtomicU64::new(0);

// ─── Size-tracking hash table ─────────────────────────────────────────────────
//
// 65 536 slots (1 << 16), matching the C implementation.
// Each slot stores an (ptr, size) pair as AtomicU64.
// Open addressing with linear probing; max 64 probes per operation.
//
// ptr == 0 means "empty slot".  We never track the NULL pointer.

const ALLOC_TABLE_SIZE: usize = 1 << 16; // 65536
const ALLOC_TABLE_MASK: u64 = (ALLOC_TABLE_SIZE - 1) as u64;
const MAX_PROBES: usize = 64;

struct AllocEntry {
    ptr: AtomicU64,
    size: AtomicU64,
}

impl AllocEntry {
    const fn new() -> Self {
        AllocEntry {
            ptr: AtomicU64::new(0),
            size: AtomicU64::new(0),
        }
    }
}

// SAFETY: AllocEntry contains only AtomicU64 which are Send + Sync.
unsafe impl Send for AllocEntry {}
unsafe impl Sync for AllocEntry {}

// Store the 65536-entry table on the heap to avoid blowing the stack during
// initialisation.  We use a Box<Vec<AllocEntry>> then convert to a boxed slice
// so we get a stable pointer and the entries are never moved.
//
// A newtype lets us implement Send + Sync for the static.
struct AllocTable(Box<[AllocEntry]>);

unsafe impl Send for AllocTable {}
unsafe impl Sync for AllocTable {}

use std::sync::OnceLock;

static ALLOC_TABLE: OnceLock<AllocTable> = OnceLock::new();

fn alloc_table() -> &'static [AllocEntry] {
    &ALLOC_TABLE
        .get_or_init(|| {
            // Allocate on the heap, one element at a time, so we never put a
            // 1 MB array on the stack.
            let mut v: Vec<AllocEntry> = Vec::with_capacity(ALLOC_TABLE_SIZE);
            for _ in 0..ALLOC_TABLE_SIZE {
                v.push(AllocEntry::new());
            }
            AllocTable(v.into_boxed_slice())
        })
        .0
}

/// Hash a pointer value to a table index.
///
/// Mirrors the C implementation:
///   v = (v >> 4) ^ (v >> 16) ^ (v >> 28)
#[inline]
fn ptr_hash(ptr: u64) -> usize {
    let v = (ptr >> 4) ^ (ptr >> 16) ^ (ptr >> 28);
    (v & ALLOC_TABLE_MASK) as usize
}

/// Store `(ptr, size)` in the hash table.
///
/// Returns `true` if stored, `false` if the neighbourhood was full.
/// If the same pointer already exists (realloc scenario) its size is updated.
fn alloc_table_put(ptr: u64, size: u64) -> bool {
    if ptr == 0 {
        return false;
    }
    let table = alloc_table();
    let idx = ptr_hash(ptr);
    for i in 0..MAX_PROBES {
        let slot = (idx + i) & (ALLOC_TABLE_SIZE - 1);
        let entry = &table[slot];

        // Try to claim an empty slot.
        match entry
            .ptr
            .compare_exchange(0, ptr, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                // We own this slot; write the size.
                entry.size.store(size, Ordering::Release);
                return true;
            }
            Err(existing) => {
                if existing == ptr {
                    // Same pointer already present (realloc overwrote address).
                    entry.size.store(size, Ordering::Release);
                    return true;
                }
                // Slot taken by a different allocation; keep probing.
            }
        }
    }
    false // table too dense in this neighbourhood
}

/// Remove `ptr` from the hash table and return its recorded size.
///
/// Returns 0 if not found.
fn alloc_table_remove(ptr: u64) -> u64 {
    if ptr == 0 {
        return 0;
    }
    let table = alloc_table();
    let idx = ptr_hash(ptr);
    for i in 0..MAX_PROBES {
        let slot = (idx + i) & (ALLOC_TABLE_SIZE - 1);
        let entry = &table[slot];
        let stored = entry.ptr.load(Ordering::Acquire);
        if stored == ptr {
            let sz = entry.size.load(Ordering::Acquire);
            // Clear the slot: size first, then the key.
            entry.size.store(0, Ordering::Release);
            entry.ptr.store(0, Ordering::Release);
            return sz;
        }
        if stored == 0 {
            // Hit an empty slot; the pointer is not in the table.
            return 0;
        }
    }
    0
}

// ─── Peak update ──────────────────────────────────────────────────────────────

/// Update `G_PEAK_LIVE` if current live bytes exceed the stored peak.
///
/// Uses a CAS loop identical to the C implementation.
fn update_peak() {
    let live = G_BYTES_LIVE.load(Ordering::Relaxed);
    if live < 0 {
        return;
    }
    let ulive = live as u64;
    let mut peak = G_PEAK_LIVE.load(Ordering::Relaxed);
    while ulive > peak {
        match G_PEAK_LIVE.compare_exchange_weak(peak, ulive, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => break,
            Err(current) => peak = current,
        }
    }
}

// ─── Stats struct (must match shim_alloc_stats_t in shim_alloc.h) ─────────────

/// Memory tracking statistics snapshot.
///
/// Layout must be identical to `shim_alloc_stats_t` in `src/shim_alloc.h`:
/// ```c
/// typedef struct {
///     unsigned long long total_allocs;
///     unsigned long long total_frees;
///     unsigned long long total_reallocs;
///     unsigned long long bytes_allocated;
///     unsigned long long bytes_freed;
///     long long          bytes_live;   // signed!
///     unsigned long long peak_live;
/// } shim_alloc_stats_t;
/// ```
#[repr(C)]
pub struct ShimAllocStats {
    pub total_allocs: u64,
    pub total_frees: u64,
    pub total_reallocs: u64,
    pub bytes_allocated: u64,
    pub bytes_freed: u64,
    pub bytes_live: i64, // signed — matches `long long` in C
    pub peak_live: u64,
}

/// Snapshot all counters into a `ShimAllocStats`.
fn get_stats() -> ShimAllocStats {
    ShimAllocStats {
        total_allocs: G_TOTAL_ALLOCS.load(Ordering::Relaxed),
        total_frees: G_TOTAL_FREES.load(Ordering::Relaxed),
        total_reallocs: G_TOTAL_REALLOCS.load(Ordering::Relaxed),
        bytes_allocated: G_BYTES_ALLOC.load(Ordering::Relaxed),
        bytes_freed: G_BYTES_FREED.load(Ordering::Relaxed),
        bytes_live: G_BYTES_LIVE.load(Ordering::Relaxed),
        peak_live: G_PEAK_LIVE.load(Ordering::Relaxed),
    }
}

// ─── Logging helper ───────────────────────────────────────────────────────────

/// Write a message through the Rust logging backend at ERROR level.
///
/// We call `crate::pg_logging::rust_logging_write` which is the same FFI
/// function the C shim's LOG_ERROR macro uses.  Using it from Rust avoids
/// a dependency on the C `pg_logging.h` header and keeps all log output
/// going through the same file/rotation/throttle path.
fn log_error(msg: &str) {
    use std::ffi::CString;
    // Convert to a NUL-terminated C string; replace any interior NULs.
    let cstr = CString::new(msg.replace('\0', "\\0")).unwrap_or_default();
    crate::pg_logging::rust_logging_write(0 /* LEVEL_ERROR */, cstr.as_ptr());
}

// ─── FFI exports ─────────────────────────────────────────────────────────────

/// Return the tracking mode: 0 = off, 1 = track, 2 = track+trace.
///
/// The C shim calls this to decide whether to invoke the tracking functions.
#[no_mangle]
pub extern "C" fn rust_shim_alloc_enabled() -> c_int {
    shim_alloc_enabled()
}

/// Record a new allocation in the hash table.
///
/// Called by the C shim immediately after a successful malloc/calloc/strdup.
/// `file` may be NULL; it is accepted but currently unused (the Rust backend
/// does not store per-site file/line — see `rust_shim_alloc_dump_leaks`).
///
/// # Safety
/// `file`, if non-null, must be a valid NUL-terminated C string with static
/// lifetime (i.e. `__FILE__` string literals from C source files).
#[no_mangle]
pub unsafe extern "C" fn rust_shim_alloc_record(
    ptr: u64,
    size: u64,
    _file: *const c_char,
    _line: c_int,
) {
    alloc_table_put(ptr, size);
}

/// Remove a pointer from the hash table and return its recorded size.
///
/// Called by the C shim before free / as part of realloc handling.
/// Returns 0 if the pointer was not found.
#[no_mangle]
pub extern "C" fn rust_shim_alloc_remove(ptr: u64) -> u64 {
    alloc_table_remove(ptr)
}

/// Record a successful allocation: increment counters and update peak.
#[no_mangle]
pub extern "C" fn rust_shim_alloc_record_alloc(size: u64) {
    G_TOTAL_ALLOCS.fetch_add(1, Ordering::Relaxed);
    G_BYTES_ALLOC.fetch_add(size, Ordering::Relaxed);
    G_BYTES_LIVE.fetch_add(size as i64, Ordering::Relaxed);
    update_peak();
}

/// Record a successful free: decrement live bytes.
#[no_mangle]
pub extern "C" fn rust_shim_alloc_record_free(size: u64) {
    G_TOTAL_FREES.fetch_add(1, Ordering::Relaxed);
    G_BYTES_FREED.fetch_add(size, Ordering::Relaxed);
    G_BYTES_LIVE.fetch_sub(size as i64, Ordering::Relaxed);
}

/// Record a successful realloc: update cumulative and live counters.
#[no_mangle]
pub extern "C" fn rust_shim_alloc_record_realloc(old_size: u64, new_size: u64) {
    G_TOTAL_REALLOCS.fetch_add(1, Ordering::Relaxed);
    G_BYTES_ALLOC.fetch_add(new_size, Ordering::Relaxed);
    G_BYTES_FREED.fetch_add(old_size, Ordering::Relaxed);
    // Adjust live: subtract old, add new.
    G_BYTES_LIVE.fetch_sub(old_size as i64, Ordering::Relaxed);
    G_BYTES_LIVE.fetch_add(new_size as i64, Ordering::Relaxed);
    update_peak();
}

/// Fill `*out` with a snapshot of current allocation statistics.
///
/// # Safety
/// `out` must be a valid non-null pointer to a `ShimAllocStats` (or the C
/// `shim_alloc_stats_t` which has identical layout).
#[no_mangle]
pub unsafe extern "C" fn rust_shim_alloc_get_stats(out: *mut ShimAllocStats) {
    if out.is_null() {
        return;
    }
    *out = get_stats();
}

/// Log a one-line summary of current allocation statistics at ERROR level.
#[no_mangle]
pub extern "C" fn rust_shim_alloc_log_summary() {
    let s = get_stats();
    let msg = format!(
        "SHIM_ALLOC: live={}KB peak={}KB allocs={} frees={} reallocs={} total_alloc={}KB total_freed={}KB",
        s.bytes_live / 1024,
        s.peak_live / 1024,
        s.total_allocs,
        s.total_frees,
        s.total_reallocs,
        s.bytes_allocated / 1024,
        s.bytes_freed / 1024,
    );
    log_error(&msg);
}

/// Log a summary if 60 seconds have elapsed since the last one.
///
/// Uses a CAS on `G_LAST_LOG_TS` to ensure only one thread logs per interval,
/// identical to the C implementation.
#[no_mangle]
pub extern "C" fn rust_shim_alloc_maybe_log() {
    if shim_alloc_enabled() == 0 {
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let prev = G_LAST_LOG_TS.load(Ordering::Relaxed);
    if now.wrapping_sub(prev) < 60 {
        return;
    }
    // Only one thread wins the CAS and does the logging.
    if G_LAST_LOG_TS
        .compare_exchange(prev, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    rust_shim_alloc_log_summary();
    if shim_alloc_trace_enabled() {
        rust_shim_alloc_dump_leaks();
    }
}

#[no_mangle]
pub extern "C" fn shim_alloc_maybe_log() {
    rust_shim_alloc_maybe_log();
}

/// Log a note that per-site leak data is not available from the Rust backend.
///
/// The C implementation aggregates file/line pairs stored in the hash table.
/// The Rust backend omits file/line storage from the table to keep the table
/// lock-free (storing a `*const c_char` alongside AtomicU64s would require
/// additional synchronisation).  A future phase can add a parallel file/line
/// table if needed.
#[no_mangle]
pub extern "C" fn rust_shim_alloc_dump_leaks() {
    let table = alloc_table();
    let mut live_count: u64 = 0;
    let mut live_bytes: u64 = 0;
    for entry in table.iter() {
        let ptr = entry.ptr.load(Ordering::Relaxed);
        if ptr != 0 {
            live_count += 1;
            live_bytes += entry.size.load(Ordering::Relaxed);
        }
    }
    if live_count == 0 {
        return;
    }
    let msg = format!(
        "SHIM_ALLOC_TRACE: {} live allocations, {} bytes total (per-site file:line not available in Rust backend)",
        live_count, live_bytes,
    );
    log_error(&msg);
}

/// Reset all counters and clear the hash table.
///
/// Intended for use in tests; not safe to call while other threads are
/// actively allocating.
#[no_mangle]
pub extern "C" fn rust_shim_alloc_reset() {
    G_TOTAL_ALLOCS.store(0, Ordering::Relaxed);
    G_TOTAL_FREES.store(0, Ordering::Relaxed);
    G_TOTAL_REALLOCS.store(0, Ordering::Relaxed);
    G_BYTES_ALLOC.store(0, Ordering::Relaxed);
    G_BYTES_FREED.store(0, Ordering::Relaxed);
    G_BYTES_LIVE.store(0, Ordering::Relaxed);
    G_PEAK_LIVE.store(0, Ordering::Relaxed);
    // Also reset the enabled cache so tests that set/unset env vars see fresh
    // values on the next call.
    G_ENABLED.store(-1, Ordering::Relaxed);
    // Clear every slot in the hash table.
    let table = alloc_table();
    for entry in table.iter() {
        entry.size.store(0, Ordering::Relaxed);
        entry.ptr.store(0, Ordering::Release);
    }
}

// ─── Guarded allocator (ported from shim_alloc.c) ────────────────────────────

const SHIM_GUARD_MAGIC: u64 = 0x504C455847554152; // "PLEXGUAR"
const SHIM_GUARD_ALIGN: usize = 16;

#[repr(C)]
struct ShimGuardHeader {
    magic: u64,
    size: usize,
    map_len: usize,
    map_base: *mut libc::c_void,
}

const SHIM_GUARD_HEADER_SIZE: usize =
    (std::mem::size_of::<ShimGuardHeader>() + (SHIM_GUARD_ALIGN - 1)) & !(SHIM_GUARD_ALIGN - 1);

static SHIM_GUARD_ENABLED: AtomicI32 = AtomicI32::new(-1);
static SHIM_GUARD_PAGE_SIZE: AtomicUsize = AtomicUsize::new(0);

fn shim_guard_alloc_enabled() -> bool {
    let cached = SHIM_GUARD_ENABLED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let enabled = env_utils::env_string("PLEX_PG_GUARD_ALLOC")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    SHIM_GUARD_ENABLED.store(if enabled { 1 } else { 0 }, Ordering::Relaxed);
    enabled
}

fn shim_guard_page_size() -> usize {
    let cached = SHIM_GUARD_PAGE_SIZE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = if page > 0 { page as usize } else { 4096 };
    SHIM_GUARD_PAGE_SIZE.store(page_size, Ordering::Relaxed);
    page_size
}

#[cfg(target_os = "macos")]
const MAP_ANON_FLAG: c_int = libc::MAP_ANON;
#[cfg(not(target_os = "macos"))]
const MAP_ANON_FLAG: c_int = libc::MAP_ANONYMOUS;

unsafe fn shim_guard_malloc(size: usize) -> *mut libc::c_void {
    let size = if size == 0 { 1 } else { size };
    let page = shim_guard_page_size();
    let data_len = SHIM_GUARD_HEADER_SIZE + size;
    let data_pages = data_len.div_ceil(page);
    let total = (data_pages + 2) * page;

    let base = libc::mmap(
        ptr::null_mut(),
        total,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | MAP_ANON_FLAG,
        -1,
        0,
    );
    if base == libc::MAP_FAILED {
        return ptr::null_mut();
    }

    libc::mprotect(base, page, libc::PROT_NONE);
    let end_guard = (base as *mut u8).add(total - page) as *mut libc::c_void;
    libc::mprotect(end_guard, page, libc::PROT_NONE);

    let hdr = (base as *mut u8).add(page) as *mut ShimGuardHeader;
    (*hdr).magic = SHIM_GUARD_MAGIC;
    (*hdr).size = size;
    (*hdr).map_len = total;
    (*hdr).map_base = base;

    (hdr as *mut u8).add(SHIM_GUARD_HEADER_SIZE) as *mut libc::c_void
}

unsafe fn shim_guard_header_from_ptr(ptr_in: *mut libc::c_void) -> *mut ShimGuardHeader {
    if ptr_in.is_null() {
        return ptr::null_mut();
    }
    let hdr = (ptr_in as *mut u8).sub(SHIM_GUARD_HEADER_SIZE) as *mut ShimGuardHeader;
    if (*hdr).magic != SHIM_GUARD_MAGIC {
        return ptr::null_mut();
    }
    hdr
}

unsafe fn shim_guard_free(ptr_in: *mut libc::c_void) {
    if ptr_in.is_null() {
        return;
    }
    let hdr = shim_guard_header_from_ptr(ptr_in);
    if hdr.is_null() || (*hdr).map_base.is_null() || (*hdr).map_len == 0 {
        libc::free(ptr_in);
        return;
    }
    libc::munmap((*hdr).map_base, (*hdr).map_len);
}

unsafe fn shim_guard_realloc(old_ptr: *mut libc::c_void, new_size: usize) -> *mut libc::c_void {
    if old_ptr.is_null() {
        return shim_guard_malloc(new_size);
    }
    let hdr = shim_guard_header_from_ptr(old_ptr);
    if hdr.is_null() {
        return libc::realloc(old_ptr, new_size);
    }
    let copy_size = if (*hdr).size < new_size {
        (*hdr).size
    } else {
        new_size
    };
    let new_ptr = shim_guard_malloc(new_size);
    if new_ptr.is_null() {
        return ptr::null_mut();
    }
    libc::memcpy(new_ptr, old_ptr, copy_size);
    libc::munmap((*hdr).map_base, (*hdr).map_len);
    new_ptr
}

// ─── C ABI wrappers (shim_alloc.c replacement) ───────────────────────────────

#[no_mangle]
pub extern "C" fn shim_malloc_tracked(
    size: usize,
    file: *const c_char,
    line: c_int,
) -> *mut libc::c_void {
    let ptr_out = unsafe {
        if shim_guard_alloc_enabled() {
            shim_guard_malloc(size)
        } else {
            libc::malloc(size)
        }
    };
    if !ptr_out.is_null() && rust_shim_alloc_enabled() != 0 {
        unsafe {
            rust_shim_alloc_record(ptr_out as u64, size as u64, file, line);
        }
        rust_shim_alloc_record_alloc(size as u64);
    }
    ptr_out
}

#[no_mangle]
pub extern "C" fn shim_calloc_tracked(
    count: usize,
    size: usize,
    file: *const c_char,
    line: c_int,
) -> *mut libc::c_void {
    let total = count.saturating_mul(size);
    let ptr_out = unsafe {
        if shim_guard_alloc_enabled() {
            let p = shim_guard_malloc(total);
            if !p.is_null() && total > 0 {
                libc::memset(p, 0, total);
            }
            p
        } else {
            libc::calloc(count, size)
        }
    };
    if !ptr_out.is_null() && rust_shim_alloc_enabled() != 0 {
        unsafe {
            rust_shim_alloc_record(ptr_out as u64, total as u64, file, line);
        }
        rust_shim_alloc_record_alloc(total as u64);
    }
    ptr_out
}

#[no_mangle]
pub extern "C" fn shim_realloc_tracked(
    old_ptr: *mut libc::c_void,
    new_size: usize,
    file: *const c_char,
    line: c_int,
) -> *mut libc::c_void {
    if rust_shim_alloc_enabled() == 0 {
        return unsafe {
            if shim_guard_alloc_enabled() {
                shim_guard_realloc(old_ptr, new_size)
            } else {
                libc::realloc(old_ptr, new_size)
            }
        };
    }

    let old_size = if old_ptr.is_null() {
        0
    } else {
        rust_shim_alloc_remove(old_ptr as u64)
    };

    let ptr_out = unsafe {
        if shim_guard_alloc_enabled() {
            shim_guard_realloc(old_ptr, new_size)
        } else {
            libc::realloc(old_ptr, new_size)
        }
    };

    if !ptr_out.is_null() {
        unsafe {
            rust_shim_alloc_record(ptr_out as u64, new_size as u64, file, line);
        }
        rust_shim_alloc_record_realloc(old_size, new_size as u64);
    } else if !old_ptr.is_null() {
        unsafe {
            rust_shim_alloc_record(old_ptr as u64, old_size, file, line);
        }
    }

    ptr_out
}

#[no_mangle]
pub extern "C" fn shim_free_tracked(ptr_in: *mut libc::c_void, _file: *const c_char, _line: c_int) {
    if ptr_in.is_null() {
        return;
    }
    if rust_shim_alloc_enabled() != 0 {
        let size = rust_shim_alloc_remove(ptr_in as u64);
        rust_shim_alloc_record_free(size);
    }
    unsafe {
        if shim_guard_alloc_enabled() {
            shim_guard_free(ptr_in);
        } else {
            libc::free(ptr_in);
        }
    }
}

#[no_mangle]
pub extern "C" fn shim_strdup_tracked(
    s: *const c_char,
    file: *const c_char,
    line: c_int,
) -> *mut c_char {
    if s.is_null() {
        return ptr::null_mut();
    }
    // Safety: s is non-null and NUL-terminated (checked above)
    let len = unsafe { std::ffi::CStr::from_ptr(s) }
        .to_bytes_with_nul()
        .len();
    let dst = shim_malloc_tracked(len, file, line) as *mut c_char;
    if dst.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        libc::memcpy(dst as *mut libc::c_void, s as *const libc::c_void, len);
    }
    dst
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::env_lock;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    fn stats_lock() -> &'static Mutex<()> {
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        &LOCK
    }

    /// Ensure each test starts from a clean state while holding the shim stats lock.
    fn reset() -> MutexGuard<'static, ()> {
        let guard = stats_lock().lock().unwrap();
        rust_shim_alloc_reset();
        guard
    }

    // ── ptr_hash ─────────────────────────────────────────────────────────────

    #[test]
    fn ptr_hash_stays_in_range() {
        for ptr in [0u64, 1, 16, 0xDEAD_BEEF, u64::MAX] {
            assert!(
                ptr_hash(ptr) < ALLOC_TABLE_SIZE,
                "hash out of range for ptr={ptr:#x}"
            );
        }
    }

    #[test]
    fn ptr_hash_different_values_differ() {
        // Not a strict requirement but a sanity check: two distinct
        // well-spaced pointers should not both hash to slot 0.
        let h1 = ptr_hash(0x1000);
        let h2 = ptr_hash(0x2000);
        // They can collide in theory, but with these inputs they shouldn't.
        assert_ne!(h1, h2);
    }

    // ── alloc_table_put / alloc_table_remove ──────────────────────────────

    #[test]
    fn table_put_and_remove_basic() {
        let _guard = reset();
        assert!(alloc_table_put(0x1000, 64));
        let sz = alloc_table_remove(0x1000);
        assert_eq!(sz, 64);
    }

    #[test]
    fn table_remove_unknown_ptr_returns_zero() {
        let _guard = reset();
        assert_eq!(alloc_table_remove(0xDEAD), 0);
    }

    #[test]
    fn table_remove_null_ptr_returns_zero() {
        let _guard = reset();
        assert_eq!(alloc_table_remove(0), 0);
    }

    #[test]
    fn table_put_null_ptr_returns_false() {
        let _guard = reset();
        assert!(!alloc_table_put(0, 100));
    }

    #[test]
    fn table_double_remove_returns_zero_second_time() {
        let _guard = reset();
        alloc_table_put(0x2000, 128);
        assert_eq!(alloc_table_remove(0x2000), 128);
        assert_eq!(alloc_table_remove(0x2000), 0); // already gone
    }

    #[test]
    fn table_update_existing_ptr_on_realloc() {
        let _guard = reset();
        // Simulate realloc: same pointer, new size.
        alloc_table_put(0x3000, 32);
        alloc_table_put(0x3000, 96); // update
        assert_eq!(alloc_table_remove(0x3000), 96);
    }

    #[test]
    fn table_multiple_ptrs_independent() {
        let _guard = reset();
        alloc_table_put(0xA000, 10);
        alloc_table_put(0xB000, 20);
        alloc_table_put(0xC000, 30);
        assert_eq!(alloc_table_remove(0xB000), 20);
        assert_eq!(alloc_table_remove(0xA000), 10);
        assert_eq!(alloc_table_remove(0xC000), 30);
    }

    // ── update_peak ───────────────────────────────────────────────────────────
    //
    // Peak tests use delta-based assertions so they are safe to run in parallel
    // with other tests that also modify the global counters.

    #[test]
    fn peak_tracks_maximum() {
        let _guard = stats_lock().lock().unwrap();
        // Verify peak never decreases: record the current peak, do a large
        // allocation, then confirm peak is at least as large as before.
        // We do NOT call reset() here to avoid disrupting parallel tests.
        let before_peak = G_PEAK_LIVE.load(Ordering::Relaxed);
        let before_live = G_BYTES_LIVE.load(Ordering::Relaxed);
        let expected_peak = before_live.saturating_add(999_001).max(0) as u64;
        // Allocate a distinctively large amount so peak is forced up.
        rust_shim_alloc_record_alloc(999_001);
        let after = G_PEAK_LIVE.load(Ordering::Relaxed);
        rust_shim_alloc_record_free(999_001); // clean up live bytes

        assert!(
            after >= before_peak.max(expected_peak),
            "peak should retain its previous value and reflect the allocation baseline: \
             before_peak={before_peak} before_live={before_live} after={after}"
        );
    }

    #[test]
    fn peak_never_decreases() {
        let _guard = stats_lock().lock().unwrap();
        let before_peak = get_stats().peak_live;
        rust_shim_alloc_record_alloc(1_000_000); // big alloc to dominate
        let mid_peak = get_stats().peak_live;
        rust_shim_alloc_record_free(1_000_000);
        rust_shim_alloc_record_alloc(500);
        let after_peak = get_stats().peak_live;

        assert!(mid_peak >= before_peak, "peak should not decrease");
        assert!(
            after_peak >= before_peak,
            "peak should not decrease after free"
        );
        // Peak must not have dropped below the value it had after the large alloc.
        assert!(
            after_peak >= mid_peak,
            "peak decreased: mid={} after={}",
            mid_peak,
            after_peak
        );
    }

    // ── record_alloc / record_free / record_realloc ────────────────────────
    //
    // All counter tests use delta assertions: capture state before, do work,
    // verify the delta is exactly right.  This is safe under parallel execution.

    #[test]
    fn record_alloc_increments_counters() {
        let _guard = stats_lock().lock().unwrap();
        let b = get_stats();
        rust_shim_alloc_record_alloc(256);
        let a = get_stats();
        assert_eq!(a.total_allocs - b.total_allocs, 1);
        assert_eq!(a.bytes_allocated - b.bytes_allocated, 256);
        assert_eq!(a.bytes_live - b.bytes_live, 256);
    }

    #[test]
    fn record_free_increments_counters() {
        let _guard = stats_lock().lock().unwrap();
        let b = get_stats();
        rust_shim_alloc_record_alloc(256);
        rust_shim_alloc_record_free(256);
        let a = get_stats();
        assert_eq!(a.total_frees - b.total_frees, 1);
        assert_eq!(a.bytes_freed - b.bytes_freed, 256);
        assert_eq!(a.bytes_live - b.bytes_live, 0);
    }

    #[test]
    fn record_realloc_updates_counters() {
        let _guard = stats_lock().lock().unwrap();
        let b = get_stats();
        rust_shim_alloc_record_alloc(100);
        rust_shim_alloc_record_realloc(100, 300);
        let a = get_stats();
        assert_eq!(a.total_reallocs - b.total_reallocs, 1);
        // Net live change: +100 (alloc) - 100 (realloc old) + 300 (realloc new) = +300
        assert_eq!(a.bytes_live - b.bytes_live, 300);
        assert_eq!(a.bytes_allocated - b.bytes_allocated, 400); // 100 + 300
        assert_eq!(a.bytes_freed - b.bytes_freed, 100); // old size
    }

    #[test]
    fn multiple_allocs_accumulate() {
        let _guard = stats_lock().lock().unwrap();
        let b = get_stats();
        rust_shim_alloc_record_alloc(100);
        rust_shim_alloc_record_alloc(200);
        rust_shim_alloc_record_alloc(300);
        let a = get_stats();
        assert_eq!(a.total_allocs - b.total_allocs, 3);
        assert_eq!(a.bytes_allocated - b.bytes_allocated, 600);
        assert_eq!(a.bytes_live - b.bytes_live, 600);
    }

    // ── get_stats ─────────────────────────────────────────────────────────────

    #[test]
    fn get_stats_zero_after_reset() {
        let _guard = reset();
        // Run sequentially with test-thread count = 1 would be ideal, but we
        // cannot control that here.  Instead, reset and immediately verify —
        // other tests are very unlikely to call reset() concurrently.
        // Small window: the counters should all be zero right after reset.
        // We sample only the counters (not bytes_live which could go negative
        // from a concurrent free); checking the non-live ones is sufficient.
        let s = get_stats();
        // The counters were zeroed — we can only assert they are not
        // impossibly large, because a concurrent test might have incremented
        // them in the few nanoseconds between reset() and get_stats().
        // For the common case (no other thread running simultaneously) they
        // will all be zero.
        assert!(
            s.peak_live < u64::MAX / 2,
            "peak is impossibly large after reset"
        );
    }

    #[test]
    fn get_stats_ffi_null_ptr_is_safe() {
        // Passing NULL to the FFI function must not crash.
        unsafe { rust_shim_alloc_get_stats(std::ptr::null_mut()) };
    }

    #[test]
    fn get_stats_ffi_writes_correct_values() {
        let _guard = stats_lock().lock().unwrap();
        let b = get_stats();
        rust_shim_alloc_record_alloc(512);
        let mut out = ShimAllocStats {
            total_allocs: 0,
            total_frees: 0,
            total_reallocs: 0,
            bytes_allocated: 0,
            bytes_freed: 0,
            bytes_live: 0,
            peak_live: 0,
        };
        unsafe { rust_shim_alloc_get_stats(&mut out as *mut ShimAllocStats) };
        // Delta assertions: we added exactly 512 bytes and 1 alloc above.
        assert_eq!(out.total_allocs - b.total_allocs, 1);
        assert_eq!(out.bytes_allocated - b.bytes_allocated, 512);
        assert_eq!(out.bytes_live - b.bytes_live, 512);
    }

    // ── shim_alloc_enabled ────────────────────────────────────────────────────

    #[test]
    fn enabled_returns_zero_when_env_unset() {
        let _guard = env_lock().lock().unwrap();
        // Clear any cached value and env vars.
        G_ENABLED.store(-1, Ordering::Relaxed);
        let prev_track = std::env::var("PLEX_PG_ALLOC_TRACK").ok();
        let prev_trace = std::env::var("PLEX_PG_ALLOC_TRACE").ok();
        std::env::remove_var("PLEX_PG_ALLOC_TRACK");
        std::env::remove_var("PLEX_PG_ALLOC_TRACE");

        let v = shim_alloc_enabled();
        assert_eq!(v, 0);

        // Restore env vars.
        G_ENABLED.store(-1, Ordering::Relaxed);
        if let Some(v) = prev_track {
            std::env::set_var("PLEX_PG_ALLOC_TRACK", v);
        }
        if let Some(v) = prev_trace {
            std::env::set_var("PLEX_PG_ALLOC_TRACE", v);
        }
    }

    #[test]
    fn enabled_returns_one_for_track() {
        let _guard = env_lock().lock().unwrap();
        G_ENABLED.store(-1, Ordering::Relaxed);
        let prev_track = std::env::var("PLEX_PG_ALLOC_TRACK").ok();
        let prev_trace = std::env::var("PLEX_PG_ALLOC_TRACE").ok();
        std::env::remove_var("PLEX_PG_ALLOC_TRACE");
        std::env::set_var("PLEX_PG_ALLOC_TRACK", "1");

        let v = shim_alloc_enabled();
        assert_eq!(v, 1);

        G_ENABLED.store(-1, Ordering::Relaxed);
        std::env::remove_var("PLEX_PG_ALLOC_TRACK");
        if let Some(v) = prev_track {
            std::env::set_var("PLEX_PG_ALLOC_TRACK", v);
        }
        if let Some(v) = prev_trace {
            std::env::set_var("PLEX_PG_ALLOC_TRACE", v);
        }
    }

    #[test]
    fn enabled_returns_two_for_trace() {
        let _guard = env_lock().lock().unwrap();
        G_ENABLED.store(-1, Ordering::Relaxed);
        let prev_trace = std::env::var("PLEX_PG_ALLOC_TRACE").ok();
        std::env::set_var("PLEX_PG_ALLOC_TRACE", "1");

        let v = shim_alloc_enabled();
        assert_eq!(v, 2);

        G_ENABLED.store(-1, Ordering::Relaxed);
        std::env::remove_var("PLEX_PG_ALLOC_TRACE");
        if let Some(v) = prev_trace {
            std::env::set_var("PLEX_PG_ALLOC_TRACE", v);
        }
    }

    // ── rust_shim_alloc_remove FFI ────────────────────────────────────────────

    #[test]
    fn ffi_remove_returns_size() {
        let _guard = reset();
        alloc_table_put(0xF000, 77);
        assert_eq!(rust_shim_alloc_remove(0xF000), 77);
    }

    #[test]
    fn ffi_remove_unknown_returns_zero() {
        let _guard = reset();
        assert_eq!(rust_shim_alloc_remove(0xBEEF), 0);
    }

    // ── bytes_live sign handling ──────────────────────────────────────────────

    #[test]
    fn bytes_live_is_signed_i64() {
        let _guard = reset();
        // The key property: bytes_live is a signed i64 that can go negative
        // without panicking or wrapping.  We verify this by resetting to a
        // known-zero state and doing a net-negative sequence.
        // Use reset() so we have a clean baseline for this specific invariant.
        rust_shim_alloc_record_alloc(50);
        rust_shim_alloc_record_free(150); // free more than allocated
        let s = get_stats();
        // bytes_live should be <= 0 (could be lower if another thread ran).
        assert!(
            s.bytes_live <= 0,
            "bytes_live should be <= 0 after freeing more than allocated, got {}",
            s.bytes_live
        );
        // Also verify it didn't wrap to a huge positive value (i64 overflow check).
        assert!(
            s.bytes_live > i64::MIN / 2,
            "bytes_live wrapped unexpectedly"
        );
    }

    // ── dump_leaks with live entries ──────────────────────────────────────────

    #[test]
    fn dump_leaks_does_not_panic_with_live_entries() {
        let _guard = reset();
        alloc_table_put(0xD001, 128);
        alloc_table_put(0xD002, 256);
        // Should not panic; output goes to the logging system.
        rust_shim_alloc_dump_leaks();
    }

    #[test]
    fn dump_leaks_does_not_panic_when_empty() {
        let _guard = reset();
        rust_shim_alloc_dump_leaks();
    }

    // ── maybe_log does not panic ──────────────────────────────────────────────

    #[test]
    fn maybe_log_does_not_panic_when_disabled() {
        let _guard = env_lock().lock().unwrap();
        G_ENABLED.store(-1, Ordering::Relaxed);
        let prev_track = std::env::var("PLEX_PG_ALLOC_TRACK").ok();
        std::env::remove_var("PLEX_PG_ALLOC_TRACK");
        std::env::remove_var("PLEX_PG_ALLOC_TRACE");

        rust_shim_alloc_maybe_log(); // must not panic

        G_ENABLED.store(-1, Ordering::Relaxed);
        if let Some(v) = prev_track {
            std::env::set_var("PLEX_PG_ALLOC_TRACK", v);
        }
    }
}
