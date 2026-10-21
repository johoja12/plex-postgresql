use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

use crate::ffi_types::PgConnection;
use crate::libpq_helpers::{PGcancel, PGconn, PGresult};

/// Lazy-evaluating debug log macro: checks LOG_LEVEL before calling format!().
/// At production log level ERROR, this avoids the heap allocation entirely.
#[macro_export]
macro_rules! log_debug_lazy {
    ($($arg:tt)*) => {
        if $crate::pg_logging::LOG_LEVEL.load(::std::sync::atomic::Ordering::Relaxed) >= 2 {
            $crate::db_interpose_conn_utils::log_debug(&format!($($arg)*));
        }
    }
}

/// Lazy-evaluating info log macro: checks LOG_LEVEL before calling format!().
/// At production log level ERROR, this avoids the heap allocation entirely.
#[macro_export]
macro_rules! log_info_lazy {
    ($($arg:tt)*) => {
        if $crate::pg_logging::LOG_LEVEL.load(::std::sync::atomic::Ordering::Relaxed) >= 1 {
            $crate::db_interpose_conn_utils::log_info(&format!($($arg)*));
        }
    }
}

pub(crate) const STATEMENT_TIMEOUT_SQL: &str = "SET statement_timeout = '5min'";

#[repr(C)]
pub struct PgConnConfig {
    pub host: [c_char; 256],
    pub port: c_int,
    pub database: [c_char; 128],
    pub user: [c_char; 128],
    pub password: [c_char; 256],
    pub schema: [c_char; 64],
}

pub struct PthreadMutexGuard {
    mutex: *mut libc::pthread_mutex_t,
    locked: bool,
}

impl PthreadMutexGuard {
    /// # Safety
    /// `mutex` must be a valid, initialized pthread mutex pointer.
    /// The caller must ensure the mutex outlives the guard.
    pub unsafe fn lock(mutex: *mut libc::pthread_mutex_t) -> Self {
        libc::pthread_mutex_lock(mutex);
        Self {
            mutex,
            locked: true,
        }
    }

    /// # Safety
    /// `mutex` must be a valid, initialized pthread mutex pointer that is
    /// already locked by the current thread.
    pub unsafe fn adopt(mutex: *mut libc::pthread_mutex_t) -> Self {
        Self {
            mutex,
            locked: true,
        }
    }

    /// # Safety
    /// The guard must currently hold the lock and `mutex` must still be valid.
    pub unsafe fn unlock(&mut self) {
        if self.locked {
            libc::pthread_mutex_unlock(self.mutex);
            self.locked = false;
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn mutex_ptr(&self) -> *mut libc::pthread_mutex_t {
        self.mutex
    }
}

impl Drop for PthreadMutexGuard {
    fn drop(&mut self) {
        if self.locked {
            unsafe {
                libc::pthread_mutex_unlock(self.mutex);
            }
        }
    }
}

pub(crate) fn log_error(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(0, cs.as_ptr());
    }
}

pub(crate) fn log_info(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(1, cs.as_ptr());
    }
}

pub(crate) fn log_debug(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(2, cs.as_ptr());
    }
}

pub(crate) fn cstr_to_str(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "STEP";
    }
    unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("STEP") }
}

/// Safe wrapper: returns `""` if `ptr` is null, otherwise the CStr contents.
/// Falls back to `""` on non-UTF-8 data.
pub(crate) fn cstr_to_str_safe(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("") }
}

/// Safe wrapper: returns `None` if `ptr` is null or non-UTF-8,
/// otherwise `Some(str)`.
pub(crate) fn cstr_to_option(ptr: *const c_char) -> Option<&'static str> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr).to_str().ok() }
}

/// Safe wrapper: returns `Cow<str>` from a possibly-null C string pointer.
/// Returns `default` if `ptr` is null. Uses lossy UTF-8 conversion to
/// handle non-UTF-8 data gracefully.
pub(crate) fn cstr_to_lossy_or(
    ptr: *const c_char,
    default: &str,
) -> std::borrow::Cow<'static, str> {
    if ptr.is_null() {
        return std::borrow::Cow::Owned(default.to_string());
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy() }
}

pub(crate) fn cstr_to_string_or(ptr: *const c_char, default: &str) -> String {
    if ptr.is_null() {
        return default.to_string();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

pub(crate) fn cstr_prefix(ptr: *const c_char, max_len: usize, default: &str) -> String {
    if ptr.is_null() {
        return default.to_string();
    }
    let bytes = unsafe { CStr::from_ptr(ptr).to_bytes() };
    let slice = &bytes[..bytes.len().min(max_len)];
    String::from_utf8_lossy(slice).into_owned()
}

pub(crate) fn cbuf_to_str(buf: &[c_char]) -> &str {
    if buf.is_empty() {
        return "";
    }
    unsafe { CStr::from_ptr(buf.as_ptr()).to_str().unwrap_or("") }
}

pub(crate) fn build_conninfo(cfg: &PgConnConfig) -> CString {
    let conninfo = format!(
        "host={} port={} dbname={} user={} password={} connect_timeout=5 keepalives=1 keepalives_idle=30 keepalives_interval=10 keepalives_count=3",
        cbuf_to_str(&cfg.host),
        cfg.port,
        cbuf_to_str(&cfg.database),
        cbuf_to_str(&cfg.user),
        cbuf_to_str(&cfg.password)
    );
    let safe = conninfo.replace('\0', "");
    CString::new(safe).unwrap_or_else(|_| CString::new(" ").unwrap())
}

pub(crate) unsafe fn connect_new(cfg: &PgConnConfig) -> *mut PGconn {
    let conninfo = build_conninfo(cfg);
    crate::libpq_helpers::rust_pq_connectdb(conninfo.as_ptr())
}

/// Sets a fresh or reset connection's session up for the database at
/// `db_path`: a Live TV guide's own schema, or the library's.
pub(crate) unsafe fn apply_pg_session_settings(
    conn: *mut PGconn,
    cfg: &PgConnConfig,
    db_path: *const c_char,
) {
    if conn.is_null() {
        return;
    }
    let guide = if db_path.is_null() {
        None
    } else {
        crate::db_interpose_helpers::epg_schema_for_path(
            crate::db_interpose_helpers::cstr_to_str_or_empty(db_path),
        )
    };
    let schema = guide.unwrap_or_else(|| cbuf_to_str(&cfg.schema).to_string());
    let schema_cmd = format!("SET search_path TO {}, public", schema);
    if let Ok(schema_c) = CString::new(schema_cmd) {
        let res = crate::libpq_helpers::rust_pq_exec(conn, schema_c.as_ptr());
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
    }
    if let Ok(timeout_c) = CString::new(STATEMENT_TIMEOUT_SQL) {
        let res = crate::libpq_helpers::rust_pq_exec(conn, timeout_c.as_ptr());
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
    }
}

pub fn rust_step_conn_cancel_and_drain(conn: *mut PgConnection, scope_tag: *const c_char) {
    if conn.is_null() {
        return;
    }
    let c = unsafe { &*conn };
    if c.conn.is_null() {
        return;
    }
    if c.streaming_active
        .load(std::sync::atomic::Ordering::Relaxed)
        != 0
    {
        return;
    }

    // Fast path: if connection is idle (not busy), skip the expensive
    // PQcancel + drain entirely. This saves a TCP roundtrip per call.
    crate::libpq_helpers::rust_pq_consume_input(c.conn);
    if crate::libpq_helpers::rust_pq_is_busy(c.conn) == 0 {
        // Quick drain: clear any already-buffered results without cancel.
        let pending = crate::libpq_helpers::rust_pq_get_result(c.conn);
        if pending.is_null() {
            return; // Nothing pending — skip entirely.
        }
        crate::libpq_helpers::rust_pq_clear(pending);
        // Drain any remaining.
        loop {
            let more = crate::libpq_helpers::rust_pq_get_result(c.conn);
            if more.is_null() {
                break;
            }
            crate::libpq_helpers::rust_pq_clear(more);
        }
        return;
    }

    // Slow path: connection is busy — cancel and drain.
    crate::libpq_helpers::rust_pq_set_nonblocking(c.conn, 0);
    let cancel: *mut PGcancel = crate::libpq_helpers::rust_pq_get_cancel(c.conn);
    if !cancel.is_null() {
        let mut errbuf = [0 as c_char; 256];
        crate::libpq_helpers::rust_pq_cancel(cancel, errbuf.as_mut_ptr(), errbuf.len() as c_int);
        crate::libpq_helpers::rust_pq_free_cancel(cancel);
    }

    let mut drain_count = 0;
    loop {
        let pending: *mut PGresult = crate::libpq_helpers::rust_pq_get_result(c.conn);
        if pending.is_null() {
            break;
        }
        drain_count += 1;
        crate::libpq_helpers::rust_pq_clear(pending);
        if drain_count > 1000 {
            break;
        }
    }
    if drain_count > 3 {
        let tag = cstr_to_str(scope_tag);
        log_info_lazy!(
            "{}: Drained {} orphaned results total from connection {:p}",
            tag,
            drain_count,
            conn
        );
    }
}
