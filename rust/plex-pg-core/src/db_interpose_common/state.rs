use super::*;

pub(super) const EXC_QUERY_RING_SIZE: usize = 24;
pub(super) const EXC_QUERY_MAX_LEN: usize = 320;
pub(super) const EXC_PHASE_RING_SIZE: usize = 48;
pub(super) const EXC_PHASE_MAX_LEN: usize = 32;

pub(super) const CRASH_QUERY_MAX_LEN: usize = 512;
pub(super) const CRASH_PHASE_MAX_LEN: usize = 64;
pub(crate) const CRASH_COLUMN_MAX_LEN: usize = 64;

pub(super) const WORK_NONE: c_int = 0;
pub(super) const WORK_PREPARE_V2: c_int = 1;
pub(super) const WORK_SHUTDOWN: c_int = 2;

pub(super) const SQLITE_ERROR: c_int = 1;

pub(super) const UNKNOWN_STR: &[u8] = b"unknown\0";

#[repr(C)]
#[derive(Copy, Clone)]
pub(super) struct WorkerRequest {
    pub(super) type_: c_int,
    pub(super) db: *mut sqlite3,
    pub(super) z_sql: *const c_char,
    pub(super) n_byte: c_int,
    pub(super) stmt: *mut sqlite3_stmt,
    pub(super) tail: *const c_char,
    pub(super) result: c_int,
    pub(super) work_ready: c_int,
    pub(super) work_done: c_int,
}

pub(super) const EMPTY_WORKER_REQUEST: WorkerRequest = WorkerRequest {
    type_: WORK_NONE,
    db: ptr::null_mut(),
    z_sql: ptr::null(),
    n_byte: 0,
    stmt: ptr::null_mut(),
    tail: ptr::null(),
    result: SQLITE_ERROR,
    work_ready: 0,
    work_done: 0,
};

#[repr(C)]
#[derive(Copy, Clone)]
pub(super) struct ExcPhaseEntry {
    pub(super) phase: [c_char; EXC_PHASE_MAX_LEN],
    pub(super) sql: [c_char; EXC_QUERY_MAX_LEN],
    pub(super) stmt: *mut c_void,
    pub(super) db: *mut c_void,
    pub(super) tid: libc::c_ulong,
}

pub(super) static mut EXC_QUERY_RING: [[c_char; EXC_QUERY_MAX_LEN]; EXC_QUERY_RING_SIZE] =
    [[0; EXC_QUERY_MAX_LEN]; EXC_QUERY_RING_SIZE];
pub(super) static mut EXC_QUERY_RING_NEXT: c_int = 0;
pub(super) static mut EXC_QUERY_RING_MUTEX: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;

pub(super) static mut EXC_PHASE_RING: [ExcPhaseEntry; EXC_PHASE_RING_SIZE] = [ExcPhaseEntry {
    phase: [0; EXC_PHASE_MAX_LEN],
    sql: [0; EXC_QUERY_MAX_LEN],
    stmt: ptr::null_mut(),
    db: ptr::null_mut(),
    tid: 0,
};
    EXC_PHASE_RING_SIZE];
pub(super) static mut EXC_PHASE_RING_NEXT: c_int = 0;
pub(super) static mut EXC_PHASE_RING_MUTEX: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;

pub(super) static mut CRASH_LAST_QUERY: [c_char; CRASH_QUERY_MAX_LEN] = [0; CRASH_QUERY_MAX_LEN];
pub(super) static CRASH_LAST_QUERY_LEN: AtomicI32 = AtomicI32::new(0);
pub(super) static CRASH_LAST_QUERY_SEQ: AtomicU32 = AtomicU32::new(0);
pub(super) static mut CRASH_LAST_PHASE: [c_char; CRASH_PHASE_MAX_LEN] = [0; CRASH_PHASE_MAX_LEN];
pub(super) static CRASH_LAST_PHASE_LEN: AtomicI32 = AtomicI32::new(0);
pub(super) static CRASH_LAST_PHASE_SEQ: AtomicU32 = AtomicU32::new(0);
pub(crate) static mut CRASH_LAST_COLUMN: [c_char; CRASH_COLUMN_MAX_LEN] = [0; CRASH_COLUMN_MAX_LEN];
pub(crate) static CRASH_LAST_COLUMN_LEN: AtomicI32 = AtomicI32::new(0);
pub(crate) static CRASH_LAST_COLUMN_SEQ: AtomicU32 = AtomicU32::new(0);

/// Wrapper around `*const c_char` that is `Send + Sync` so it can be stored
/// inside a `OnceLock`.  The pointer is either a `&'static [u8]` literal or
/// the result of `libc::getenv` (whose lifetime matches the process).
#[derive(Copy, Clone)]
pub(super) struct SendCharPtr(pub(super) *const c_char);
unsafe impl Send for SendCharPtr {}
unsafe impl Sync for SendCharPtr {}

pub(super) static TRACE_LAST_QUERY_PATH: std::sync::OnceLock<SendCharPtr> =
    std::sync::OnceLock::new();

pub(super) static SYMBOLS_VERIFIED: AtomicI32 = AtomicI32::new(0);

#[no_mangle]
pub static mut sqlite_handle: *mut c_void = ptr::null_mut();

#[no_mangle]
pub static mut orig_sqlite3_open: Option<Sqlite3OpenFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_open_v2: Option<Sqlite3OpenV2Fn> = None;
#[no_mangle]
pub static mut orig_sqlite3_close: Option<Sqlite3DbToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_close_v2: Option<Sqlite3DbToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_exec: Option<Sqlite3ExecFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_changes: Option<Sqlite3DbToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_changes64: Option<Sqlite3DbToI64Fn> = None;
#[no_mangle]
pub static mut orig_sqlite3_last_insert_rowid: Option<Sqlite3DbToI64Fn> = None;
#[no_mangle]
pub static mut orig_sqlite3_get_table: Option<Sqlite3GetTableFn> = None;

#[no_mangle]
pub static mut orig_sqlite3_errmsg: Option<Sqlite3DbToCStrFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_errcode: Option<Sqlite3DbToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_extended_errcode: Option<Sqlite3DbToIntFn> = None;

#[no_mangle]
pub static mut orig_sqlite3_prepare: Option<Sqlite3PrepareFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_prepare_v2: Option<Sqlite3PrepareFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_prepare_v3: Option<Sqlite3PrepareV3Fn> = None;
#[no_mangle]
pub static mut orig_sqlite3_prepare16_v2: Option<Sqlite3Prepare16Fn> = None;

#[no_mangle]
pub static mut orig_sqlite3_bind_int: Option<Sqlite3BindIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_int64: Option<Sqlite3BindInt64Fn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_double: Option<Sqlite3BindDoubleFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_text: Option<Sqlite3BindTextFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_text64: Option<Sqlite3BindText64Fn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_blob: Option<Sqlite3BindBlobFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_blob64: Option<Sqlite3BindBlob64Fn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_value: Option<Sqlite3BindValueFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_null: Option<Sqlite3BindNullFn> = None;

#[no_mangle]
pub static mut orig_sqlite3_step: Option<Sqlite3StmtToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_reset: Option<Sqlite3StmtToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_finalize: Option<Sqlite3StmtToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_clear_bindings: Option<Sqlite3StmtToIntFn> = None;

#[no_mangle]
pub static mut orig_sqlite3_column_count: Option<Sqlite3StmtToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_type: Option<Sqlite3StmtIndexToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_int: Option<Sqlite3StmtIndexToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_int64: Option<Sqlite3StmtIndexToI64Fn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_double: Option<Sqlite3StmtIndexToDoubleFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_text: Option<Sqlite3StmtIndexToTextFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_blob: Option<Sqlite3StmtIndexToBlobFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_bytes: Option<Sqlite3StmtIndexToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_name: Option<Sqlite3StmtIndexToNameFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_decltype: Option<Sqlite3StmtIndexToNameFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_value: Option<Sqlite3StmtIndexToValueFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_data_count: Option<Sqlite3StmtToIntFn> = None;

#[no_mangle]
pub static mut orig_sqlite3_value_type: Option<Sqlite3ValueToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_value_text: Option<Sqlite3ValueToTextFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_value_int: Option<Sqlite3ValueToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_value_int64: Option<Sqlite3ValueToI64Fn> = None;
#[no_mangle]
pub static mut orig_sqlite3_value_double: Option<Sqlite3ValueToDoubleFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_value_bytes: Option<Sqlite3ValueToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_value_blob: Option<Sqlite3ValueToBlobFn> = None;

#[no_mangle]
pub static mut orig_sqlite3_create_collation: Option<Sqlite3CreateCollationFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_create_collation_v2: Option<Sqlite3CreateCollationV2Fn> = None;

#[no_mangle]
pub static mut orig_sqlite3_free: Option<Sqlite3FreeFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_malloc: Option<Sqlite3MallocFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_db_handle: Option<Sqlite3StmtToDbFn> = None;
/// `sqlite3_next_stmt`: the finalize guard walks a connection's live
/// statements with it (db_interpose_stmt_lifecycle).
#[no_mangle]
pub static mut orig_sqlite3_next_stmt: Option<Sqlite3NextStmtFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_sql: Option<Sqlite3StmtToCStrFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_expanded_sql: Option<Sqlite3StmtToMutCStrFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_parameter_count: Option<Sqlite3StmtToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_parameter_index: Option<Sqlite3StmtNameToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_stmt_readonly: Option<Sqlite3StmtToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_stmt_busy: Option<Sqlite3StmtToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_stmt_status: Option<Sqlite3StmtIdx2ToIntFn> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_parameter_name: Option<Sqlite3StmtIndexToNameFn> = None;

#[no_mangle]
pub static mut shim_sqlite3_prepare_v2: Option<Sqlite3PrepareFn> = None;
#[no_mangle]
pub static mut shim_sqlite3_errmsg: Option<Sqlite3DbToCStrFn> = None;
#[no_mangle]
pub static mut shim_sqlite3_errcode: Option<Sqlite3DbToIntFn> = None;

#[no_mangle]
pub(super) static mut worker_thread: libc::pthread_t = 0 as libc::pthread_t;
#[no_mangle]
pub(super) static mut worker_mutex: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;
/// Serialises `rust_worker_init` so the lazy start in
/// `rust_delegate_prepare_to_worker` cannot spawn two workers for the one
/// global `worker_request` slot. Always taken *before* `worker_mutex`, never
/// while holding it.
pub(super) static mut WORKER_INIT_MUTEX: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;
/// Gives one delegation at a time exclusive use of `worker_request`.
///
/// `worker_mutex` alone cannot: the caller releases it inside
/// `pthread_cond_wait` while waiting for its answer, which is precisely when
/// the next caller can take it and overwrite the slot -- clearing a `work_done`
/// its owner has not read yet, and leaving that owner parked on a response
/// that was already signalled.
///
/// Taken *outside* `WORKER_INIT_MUTEX` and `worker_mutex`, never within
/// either. The worker thread itself never takes it, and cannot: delegation is
/// guarded by `from_worker == 0`, so the worker never re-enters this path.
pub(super) static mut WORKER_CALL_MUTEX: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;
#[no_mangle]
pub(super) static mut worker_cond_request: libc::pthread_cond_t = libc::PTHREAD_COND_INITIALIZER;
#[no_mangle]
pub(super) static mut worker_cond_response: libc::pthread_cond_t = libc::PTHREAD_COND_INITIALIZER;
#[no_mangle]
pub(super) static mut worker_request: WorkerRequest = WorkerRequest {
    type_: EMPTY_WORKER_REQUEST.type_,
    db: EMPTY_WORKER_REQUEST.db,
    z_sql: EMPTY_WORKER_REQUEST.z_sql,
    n_byte: EMPTY_WORKER_REQUEST.n_byte,
    stmt: EMPTY_WORKER_REQUEST.stmt,
    tail: EMPTY_WORKER_REQUEST.tail,
    result: EMPTY_WORKER_REQUEST.result,
    work_ready: EMPTY_WORKER_REQUEST.work_ready,
    work_done: EMPTY_WORKER_REQUEST.work_done,
};
#[no_mangle]
pub(super) static mut worker_running: c_int = 0;

pub static SHIM_INITIALIZED: AtomicI32 = AtomicI32::new(0);
pub static SHIM_PASSTHROUGH_ONLY: AtomicI32 = AtomicI32::new(0);

pub(crate) static GLOBAL_VALUE_TYPE_CALLS: AtomicI64 = AtomicI64::new(0);
pub(crate) static GLOBAL_COLUMN_TYPE_CALLS: AtomicI64 = AtomicI64::new(0);

pub type CxaDemangleFn =
    unsafe extern "C" fn(*const c_char, *mut c_char, *mut libc::size_t, *mut c_int) -> *mut c_char;

pub static CXA_DEMANGLE_FN: std::sync::OnceLock<Option<CxaDemangleFn>> = std::sync::OnceLock::new();

#[no_mangle]
pub(super) static mut shim_init_pid: libc::pid_t = 0;

#[no_mangle]
pub(super) static total_exception_count: AtomicI32 = AtomicI32::new(0);
// exception_tracker_mutex removed — replaced by Mutex<ExceptionTrackerState> in exception_tracker.rs
