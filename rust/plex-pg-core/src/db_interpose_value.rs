use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::db_interpose_common::{
    get_orig_sqlite3_value_blob, get_orig_sqlite3_value_bytes, get_orig_sqlite3_value_double,
    get_orig_sqlite3_value_int, get_orig_sqlite3_value_int64, get_orig_sqlite3_value_text,
    get_orig_sqlite3_value_type, tls_last_query_ptr, tls_value_type_calls_ptr,
    GLOBAL_VALUE_TYPE_CALLS,
};
use crate::db_interpose_conn_utils::{cstr_prefix, cstr_to_string_or, log_error};
use crate::db_interpose_helpers::PGresult as PgResultHelpers;
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value, PgStmt};
use crate::libpq_helpers::PGresult as PgResultLibpq;

mod scalar_accessors;
mod support;
mod type_text;

use scalar_accessors::{
    value_blob_impl, value_bytes_impl, value_double_impl, value_int64_impl, value_int_impl,
};
use type_text::{value_text_impl, value_type_impl};

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_NULL: c_int = 5;

const VALUE_TEXT_BUF_COUNT: usize = 256;
const VALUE_BLOB_BUF_COUNT: usize = 64;

static VALUE_TYPE_CALLS: AtomicI64 = AtomicI64::new(0);
static VALUE_TEXT_CALLS: AtomicI64 = AtomicI64::new(0);
static VALUE_INT_CALLS: AtomicI64 = AtomicI64::new(0);

/// A ring of buffers one thread hands out from: the pointer a call returns
/// stays valid until the ring has gone round once on that thread.
///
/// These were two process-global rings of fixed buffers (256 x 16 KiB for
/// text, 64 x 64 KiB for blobs), shared by every thread and indexed by one
/// atomic counter, so a value one thread was still reading could be
/// overwritten by another thread's next call, and a value over 16 KiB -- a
/// DVR's channel map is one -- came back cut. Each thread now has its own
/// ring, and a slot grows to the value it holds.
struct ValueRing {
    slots: std::cell::RefCell<Vec<Vec<u8>>>,
    next: std::cell::Cell<usize>,
}

impl ValueRing {
    const fn new() -> Self {
        Self {
            slots: std::cell::RefCell::new(Vec::new()),
            next: std::cell::Cell::new(0),
        }
    }

    /// Copy `bytes` into the next slot, NUL-terminated, and hand back its
    /// address.
    fn put(&self, count: usize, bytes: &[u8]) -> *const u8 {
        let mut slots = self.slots.borrow_mut();
        if slots.len() < count {
            slots.resize_with(count, Vec::new);
        }
        let i = self.next.get() % count;
        self.next.set((i + 1) % count);
        let slot = &mut slots[i];
        slot.clear();
        slot.extend_from_slice(bytes);
        slot.push(0);
        slot.as_ptr()
    }

    /// A slot of at least `capacity` bytes for a caller to fill; the
    /// address and the capacity it may use.
    fn take(&self, count: usize, capacity: usize) -> (*mut u8, usize) {
        let mut slots = self.slots.borrow_mut();
        if slots.len() < count {
            slots.resize_with(count, Vec::new);
        }
        let i = self.next.get() % count;
        self.next.set((i + 1) % count);
        let slot = &mut slots[i];
        slot.clear();
        slot.resize(capacity, 0);
        (slot.as_mut_ptr(), capacity)
    }
}

thread_local! {
    static VALUE_TEXT_RING: ValueRing = const { ValueRing::new() };
    static VALUE_BLOB_RING: ValueRing = const { ValueRing::new() };
}

/// Copy a text value into this thread's ring; see `ValueRing`.
fn value_text_ring_put(bytes: &[u8]) -> *const u8 {
    VALUE_TEXT_RING
        .try_with(|r| r.put(VALUE_TEXT_BUF_COUNT, bytes))
        .unwrap_or(ptr::null())
}

/// Reserve a blob slot of `capacity` bytes in this thread's ring.
fn value_blob_ring_take(capacity: usize) -> (*mut u8, usize) {
    VALUE_BLOB_RING
        .try_with(|r| r.take(VALUE_BLOB_BUF_COUNT, capacity))
        .unwrap_or((ptr::null_mut(), 0))
}

static NEEDLE_TYPE: &[u8] = b"type\0";

#[repr(C)]
struct PgFakeValue {
    magic: u32,
    pg_stmt: *mut c_void,
    col_idx: c_int,
    row_idx: c_int,
    owner_thread: libc::pthread_t,
}

extern "C" {
    fn pg_check_fake_value(p_val: *mut sqlite3_value) -> *mut PgFakeValue;
    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_type(p_val: *mut sqlite3_value) -> c_int {
    value_type_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_text(p_val: *mut sqlite3_value) -> *const c_uchar {
    value_text_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_int(p_val: *mut sqlite3_value) -> c_int {
    value_int_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_int64(p_val: *mut sqlite3_value) -> i64 {
    value_int64_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_double(p_val: *mut sqlite3_value) -> f64 {
    value_double_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_bytes(p_val: *mut sqlite3_value) -> c_int {
    value_bytes_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_blob(p_val: *mut sqlite3_value) -> *const c_void {
    value_blob_impl(p_val)
}
