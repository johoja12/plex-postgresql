use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::libpq_helpers::{PGconn, PGresult};
use crate::pg_query_cache::CachedResult;

/// Guard type returned by `PgStmt::lock_mutex()`.
/// Uses `'static` lifetime because the lock is obtained through a raw pointer
/// (not tracked by the borrow checker), matching the old PthreadMutexGuard semantics.
pub type StmtGuard = std::sync::MutexGuard<'static, ()>;

pub const DB_PATH_LEN: usize = 1024;
pub const LAST_ERROR_LEN: usize = 1024;
pub const STMT_NAME_LEN: usize = 32;
pub const PARAM_BUF_LEN: usize = 32;

#[repr(C)]
pub struct sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
pub struct sqlite3_stmt {
    _private: [u8; 0],
}

#[repr(C)]
pub struct sqlite3_value {
    _private: [u8; 0],
}

#[repr(C)]
pub struct PgConnection {
    pub conn: *mut PGconn,
    pub shadow_db: *mut sqlite3,
    pub db_path: [c_char; DB_PATH_LEN],
    pub is_pg_active: c_int,
    pub mutex: libc::pthread_mutex_t,
    pub last_changes: c_int,
    pub last_insert_rowid: i64,
    pub last_generator_metadata_id: i64,
    pub last_error: [c_char; LAST_ERROR_LEN],
    pub last_error_code: c_int,
    pub streaming_active: AtomicI32,
    /// How many holders still point at this struct.
    ///
    /// PgStmt has always been reference counted; the connection it points at
    /// was not, so the pool was free to `libc::free` a struct that live
    /// statements still held — and that `errmsg_impl` had handed Plex a
    /// pointer into. One holder is the pool slot, and one is every statement
    /// pointer that names this connection.
    ///
    /// Last on the struct on purpose: `repr(C)` is vestigial here — nothing
    /// in C reads this type any more, and the legacy header has already
    /// drifted — but appending rather than inserting keeps that true for
    /// anything that reads the older fields by offset.
    pub ref_count: AtomicI32,
}

/// Take a reference to a pooled connection. Null is ignored, because the
/// pointers a statement holds are null more often than not.
pub fn conn_ref(conn: *mut PgConnection) {
    if conn.is_null() {
        return;
    }
    // SAFETY: non-null, and the caller holds a reference of its own or is the
    // pool creating this connection.
    unsafe { &*conn }.ref_count.fetch_add(1, Ordering::AcqRel);
}

/// Drop a reference, freeing the struct when the last holder lets go.
///
/// Returns whether this call did the freeing, which is what the tests assert
/// on: freed memory cannot be examined afterwards.
pub fn conn_unref(conn: *mut PgConnection) -> bool {
    if conn.is_null() {
        return false;
    }
    // SAFETY: non-null, and the caller holds the reference it is dropping.
    let previous = unsafe { &*conn }.ref_count.fetch_sub(1, Ordering::AcqRel);
    if previous > 1 {
        return false;
    }
    if previous < 1 {
        // Nobody held this, so something released it twice. Freeing now would
        // turn a book-keeping bug into the double free this counting exists
        // to prevent, so put the count back and leave the struct alone. A
        // leaked connection is survivable; a double free is not.
        unsafe { &*conn }.ref_count.fetch_add(1, Ordering::AcqRel);
        return false;
    }
    // SAFETY: the count reached zero, so nothing else points here any more.
    unsafe {
        libc::pthread_mutex_destroy(&mut (*conn).mutex as *mut _);
        libc::free(conn as *mut libc::c_void);
    }
    true
}

/// Swap one held connection pointer for another, counting both.
///
/// The new reference is taken before the old one is dropped, so replacing a
/// pointer with itself cannot briefly reach zero and free the struct the
/// caller is still using.
fn replace_conn(old: *mut PgConnection, new: *mut PgConnection) -> *mut PgConnection {
    if old == new {
        return new;
    }
    conn_ref(new);
    conn_unref(old);
    new
}

/// How many holders a connection has. For tests and for logging.
pub fn conn_refs(conn: *const PgConnection) -> i32 {
    if conn.is_null() {
        return 0;
    }
    // SAFETY: non-null, and the caller holds a reference.
    unsafe { &*conn }.ref_count.load(Ordering::Acquire)
}

// PgStmt is no longer repr(C) — all access is from Rust.
// Allocated via Box::new() + Box::into_raw(), freed via Box::from_raw().
// Vec fields are empty by default; call ensure_param_capacity() / ensure_column_capacity()
// before indexing.
pub struct PgStmt {
    pub mutex: std::sync::Mutex<()>,
    pub ref_count: AtomicI32,
    conn: *mut PgConnection,
    pub shadow_stmt: *mut sqlite3_stmt,
    pub sql: *mut c_char,
    pub pg_sql: *mut c_char,
    pub result: *mut PGresult,
    pub cached_result: *mut CachedResult,
    pub sql_hash: u64,
    pub stmt_name: [c_char; STMT_NAME_LEN],
    pub use_prepared: c_int,
    pub current_row: c_int,
    pub num_rows: c_int,
    pub num_cols: c_int,
    pub is_pg: c_int,
    pub is_cached: c_int,
    pub is_count_query: c_int,
    pub needs_requery: c_int,
    pub write_executed: c_int,
    pub read_done: c_int,
    pub metadata_only_result: c_int,
    pub in_step: AtomicI32,
    pub executing_thread: libc::pthread_t,
    result_conn: *mut PgConnection,
    pub col_names: *mut *mut c_char,
    pub num_col_names: c_int,
    pub streaming_mode: c_int,
    streaming_conn: *mut PgConnection,
    // Parameter arrays — sized to param_count via ensure_param_capacity()
    pub param_values: Vec<*mut c_char>,
    pub param_lengths: Vec<c_int>,
    pub param_formats: Vec<c_int>,
    pub param_buffers: Vec<[c_char; PARAM_BUF_LEN]>,
    pub param_count: c_int,
    pub param_names: *mut *mut c_char,
    // Column result caches — sized to num_cols via ensure_column_capacity()
    pub decoded_blobs: Vec<*mut c_void>,
    pub decoded_blob_lens: Vec<c_int>,
    pub decoded_blob_row: c_int,
    pub cached_text: Vec<*mut c_char>,
    pub cached_blob: Vec<*mut c_void>,
    pub cached_blob_len: Vec<c_int>,
    pub cached_row: c_int,
    pub col_table_names: Vec<*mut c_char>,
    pub col_tables_resolved: c_int,
    // (declared type, expected runtime type); Some(NULL, -1) is a cached
    // expression decltype, distinct from an unresolved entry.
    pub column_decltypes: Vec<Option<(*const c_char, c_int, c_int)>>,
    pub owned_column_text: Vec<Option<Vec<u8>>>,
    pub column_decltypes_epoch: u64,
    pub column_decltypes_result: usize,
    pub column_decltypes_sql: usize,
}

impl Default for PgStmt {
    fn default() -> Self {
        Self::new()
    }
}

impl PgStmt {
    /// Create a new PgStmt with all fields zeroed/empty.
    pub fn new() -> Self {
        Self {
            mutex: std::sync::Mutex::new(()),
            ref_count: AtomicI32::new(0),
            conn: std::ptr::null_mut(),
            shadow_stmt: std::ptr::null_mut(),
            sql: std::ptr::null_mut(),
            pg_sql: std::ptr::null_mut(),
            result: std::ptr::null_mut(),
            cached_result: std::ptr::null_mut(),
            sql_hash: 0,
            stmt_name: [0; STMT_NAME_LEN],
            use_prepared: 0,
            current_row: 0,
            num_rows: 0,
            num_cols: 0,
            is_pg: 0,
            is_cached: 0,
            is_count_query: 0,
            needs_requery: 0,
            write_executed: 0,
            read_done: 0,
            metadata_only_result: 0,
            in_step: AtomicI32::new(0),
            executing_thread: 0 as libc::pthread_t,
            result_conn: std::ptr::null_mut(),
            col_names: std::ptr::null_mut(),
            num_col_names: 0,
            streaming_mode: 0,
            streaming_conn: std::ptr::null_mut(),
            param_values: Vec::new(),
            param_lengths: Vec::new(),
            param_formats: Vec::new(),
            param_buffers: Vec::new(),
            param_count: 0,
            param_names: std::ptr::null_mut(),
            decoded_blobs: Vec::new(),
            decoded_blob_lens: Vec::new(),
            decoded_blob_row: -1,
            cached_text: Vec::new(),
            cached_blob: Vec::new(),
            cached_blob_len: Vec::new(),
            cached_row: -1,
            col_table_names: Vec::new(),
            col_tables_resolved: 0,
            column_decltypes: Vec::new(),
            owned_column_text: Vec::new(),
            column_decltypes_epoch: 0,
            column_decltypes_result: 0,
            column_decltypes_sql: 0,
        }
    }

    /// The connection this statement was prepared on.
    pub fn conn(&self) -> *mut PgConnection {
        self.conn
    }

    /// The connection the current result belongs to, which need not be the one
    /// the statement was prepared on.
    pub fn result_conn(&self) -> *mut PgConnection {
        self.result_conn
    }

    /// The connection claimed for streaming, which must be drained before it
    /// goes back to the pool.
    pub fn streaming_conn(&self) -> *mut PgConnection {
        self.streaming_conn
    }

    /// Point this statement at a connection, taking a reference to it and
    /// letting go of the one it held.
    ///
    /// These three pointers are private, and reached only through here, so
    /// that no assignment can quietly skip the counting. That is the whole
    /// mechanism: the compiler enumerates the call sites rather than a grep,
    /// and a missed one is a build error instead of either a leak or a
    /// connection freed under a live statement.
    pub fn set_conn(&mut self, conn: *mut PgConnection) {
        self.conn = replace_conn(self.conn, conn);
    }

    pub fn set_result_conn(&mut self, conn: *mut PgConnection) {
        self.result_conn = replace_conn(self.result_conn, conn);
    }

    pub fn set_streaming_conn(&mut self, conn: *mut PgConnection) {
        self.streaming_conn = replace_conn(self.streaming_conn, conn);
    }

    /// Let go of all three, for when the statement itself is going away.
    pub fn release_conns(&mut self) {
        self.set_conn(std::ptr::null_mut());
        self.set_result_conn(std::ptr::null_mut());
        self.set_streaming_conn(std::ptr::null_mut());
    }

    /// Ensure param arrays are sized to at least `count` elements.
    /// Called at prepare time when param_count is known.
    pub fn ensure_param_capacity(&mut self, count: usize) {
        if count > self.param_values.len() {
            self.param_values.resize(count, std::ptr::null_mut());
            self.param_lengths.resize(count, 0);
            self.param_formats.resize(count, 0);
            self.param_buffers.resize(count, [0; PARAM_BUF_LEN]);
        }
    }

    /// Ensure column cache arrays are sized to at least `count` elements.
    /// Called at first step when num_cols is known.
    pub fn ensure_column_capacity(&mut self, count: usize) {
        if count > self.decoded_blobs.len() {
            self.decoded_blobs.resize(count, std::ptr::null_mut());
            self.decoded_blob_lens.resize(count, 0);
        }
        if count > self.cached_text.len() {
            self.cached_text.resize(count, std::ptr::null_mut());
            self.cached_blob.resize(count, std::ptr::null_mut());
            self.cached_blob_len.resize(count, 0);
        }
        if count > self.col_table_names.len() {
            self.col_table_names.resize(count, std::ptr::null_mut());
        }
    }

    /// Lock the stmt mutex via a raw pointer so the returned guard does not
    /// participate in Rust's borrow checker.  This mirrors the old
    /// PthreadMutexGuard::lock() semantics — the caller must ensure the PgStmt
    /// outlives the guard.
    ///
    /// # Safety
    /// `ptr` must be a valid, non-null pointer to a live PgStmt.
    #[inline]
    pub unsafe fn lock_mutex(ptr: *const PgStmt) -> std::sync::MutexGuard<'static, ()> {
        // Transmute to 'static so the guard doesn't borrow through any Rust reference
        // the compiler tracks.  This is sound as long as the PgStmt allocation
        // outlives the guard — exactly the same invariant PthreadMutexGuard relied on.
        let mutex: &'static std::sync::Mutex<()> = std::mem::transmute(&(*ptr).mutex);
        mutex.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Check if a param value pointer is into the preallocated param_buffers.
    #[inline]
    pub fn is_preallocated_buffer(&self, idx: usize) -> bool {
        if idx >= self.param_values.len() || idx >= self.param_buffers.len() {
            return false;
        }
        let val = self.param_values[idx];
        if val.is_null() {
            return false;
        }
        let buf_ptr = self.param_buffers[idx].as_ptr();
        let buf_end = unsafe { buf_ptr.add(PARAM_BUF_LEN) };
        val as *const c_char >= buf_ptr && (val as *const c_char) < buf_end
    }
}

// Safety: PgStmt contains raw pointers used as opaque handles.
// Thread safety is managed by the mutex field.
unsafe impl Send for PgStmt {}
