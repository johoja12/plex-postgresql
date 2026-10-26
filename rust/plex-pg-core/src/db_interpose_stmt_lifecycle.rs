use std::os::raw::{c_char, c_int};

use crate::ffi_types::sqlite3_stmt;

mod ring_tracker;
mod statement_ops;

#[cfg(test)]
use ring_tracker::{remember_finalized_stmt, reset_test_state};
use statement_ops::{clear_bindings_impl, finalize_impl, note_stmt_prepare_impl, reset_impl};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

use crate::pg_statement::c_abi::{
    pg_clear_cached_stmt, pg_find_any_stmt, pg_find_cached_stmt, pg_find_stmt,
    pg_stmt_clear_result, pg_stmt_unref, pg_unregister_stmt,
};

extern "C" {
    static mut orig_sqlite3_reset: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_finalize: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_clear_bindings:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char>;

    fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int);
}

#[no_mangle]
pub extern "C" fn rust_pg_note_stmt_prepare(p_stmt: *mut sqlite3_stmt, sql: *const c_char) {
    note_stmt_prepare_impl(p_stmt, sql)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_reset(p_stmt: *mut sqlite3_stmt) -> c_int {
    reset_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_finalize(p_stmt: *mut sqlite3_stmt) -> c_int {
    finalize_impl(p_stmt)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_clear_bindings(p_stmt: *mut sqlite3_stmt) -> c_int {
    clear_bindings_impl(p_stmt)
}

thread_local! {
    static CURRENT_CONN: std::cell::Cell<*mut crate::ffi_types::sqlite3> =
        const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// While the shim runs real SQLite -- `sqlite3_exec`, a prepare or a step --
/// on a connection, the connection it runs on.
///
/// SQLite runs statements of its own inside those calls (FTS3's
/// `PRAGMA %Q.page_size` when a CREATE VIRTUAL TABLE's step creates an FTS4
/// table or a prepare connects one), prepared inside libsqlite3, where the
/// shim never sees them, but finalized through the shim. The finalize guard
/// reads this to tell such a live statement at a reused address from a double
/// finalize (statement_ops::finalize_impl).
pub(crate) struct ConnScope {
    prev: *mut crate::ffi_types::sqlite3,
}

impl ConnScope {
    /// Marks this thread as running real SQLite on `db` until dropped.
    pub(crate) fn enter(db: *mut crate::ffi_types::sqlite3) -> Self {
        let prev = CURRENT_CONN.with(|c| c.replace(db));
        Self { prev }
    }
}

impl Drop for ConnScope {
    fn drop(&mut self) {
        CURRENT_CONN.with(|c| c.set(self.prev));
    }
}

/// The connection the shim is running real SQLite on in this thread, or null.
pub(crate) fn current_conn() -> *mut crate::ffi_types::sqlite3 {
    CURRENT_CONN.with(|c| c.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_interpose_common::{
        Sqlite3DbToIntFn, Sqlite3ExecCallback, Sqlite3ExecFn, Sqlite3NextStmtFn,
        Sqlite3Prepare16Fn, Sqlite3PrepareFn, Sqlite3StmtToCStrFn, Sqlite3StmtToDbFn,
        Sqlite3StmtToIntFn,
    };
    use crate::ffi_types::sqlite3;
    use std::os::raw::{c_char, c_void};
    use std::ptr;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
    use std::sync::{LazyLock, Mutex};

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    static FINALIZE_CALLS: AtomicI32 = AtomicI32::new(0);
    /// What `sqlite3_next_stmt` reports as the fake connection's live statement.
    static LIVE_STMT: AtomicUsize = AtomicUsize::new(0);
    /// What the fake exec, step and prepares finalize inside themselves.
    static FINALIZE_TARGET: AtomicUsize = AtomicUsize::new(0);

    const FAKE_DB: *mut sqlite3 = 0x4000usize as *mut sqlite3;

    /// These tests' made-up statements and connection; a real one is a heap
    /// address far above them.
    fn is_fake<T>(p: *mut T) -> bool {
        (p as usize) < 0x10000
    }

    unsafe extern "C" {
        fn sqlite3_sql(stmt: *mut sqlite3_stmt) -> *const c_char;
        fn sqlite3_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3;
    }

    /// The real functions the fakes stand in for while installed.
    #[derive(Clone, Copy)]
    struct Reals {
        finalize: Option<Sqlite3StmtToIntFn>,
        sql: Option<Sqlite3StmtToCStrFn>,
        next_stmt: Option<Sqlite3NextStmtFn>,
        step: Option<Sqlite3StmtToIntFn>,
        db_handle: Option<Sqlite3StmtToDbFn>,
        prepare_v2: Option<Sqlite3PrepareFn>,
        prepare16_v2: Option<Sqlite3Prepare16Fn>,
        exec: Option<Sqlite3ExecFn>,
        close: Option<Sqlite3DbToIntFn>,
        close_v2: Option<Sqlite3DbToIntFn>,
    }

    static mut REALS: Reals = Reals {
        finalize: None,
        sql: None,
        next_stmt: None,
        step: None,
        db_handle: None,
        prepare_v2: None,
        prepare16_v2: None,
        exec: None,
        close: None,
        close_v2: None,
    };

    unsafe fn reals() -> Reals {
        ptr::addr_of!(REALS).read()
    }

    /// The fake SQLite these tests run the shim against, installed until
    /// dropped. The shim reads its SQLite through process-wide pointers, and
    /// other modules' tests run the shim's prepare and step on real SQLite in
    /// parallel, so every fake hands a real statement or connection to the
    /// real function, falling back as the shim does when it is unset.
    struct FakeSqlite;

    impl FakeSqlite {
        unsafe fn install() -> Self {
            use crate::db_interpose_common as c;
            ptr::addr_of_mut!(REALS).write(Reals {
                finalize: c::orig_sqlite3_finalize,
                sql: c::orig_sqlite3_sql,
                next_stmt: c::orig_sqlite3_next_stmt,
                step: c::orig_sqlite3_step,
                db_handle: c::orig_sqlite3_db_handle,
                prepare_v2: c::shim_sqlite3_prepare_v2,
                prepare16_v2: c::orig_sqlite3_prepare16_v2,
                exec: c::orig_sqlite3_exec,
                close: c::orig_sqlite3_close,
                close_v2: c::orig_sqlite3_close_v2,
            });
            FINALIZE_CALLS.store(0, Ordering::Relaxed);
            reset_test_state();
            c::orig_sqlite3_finalize = Some(fake_finalize);
            c::orig_sqlite3_sql = Some(fake_sql);
            c::orig_sqlite3_next_stmt = Some(fake_next_stmt);
            c::orig_sqlite3_step = Some(fake_step);
            c::orig_sqlite3_db_handle = Some(fake_db_handle);
            c::shim_sqlite3_prepare_v2 = Some(fake_prepare_v2);
            c::orig_sqlite3_prepare16_v2 = Some(fake_prepare16_v2);
            c::orig_sqlite3_exec = Some(fake_exec);
            c::orig_sqlite3_close = Some(fake_close);
            c::orig_sqlite3_close_v2 = Some(fake_close_v2);
            FakeSqlite
        }

        /// The real finalizes the shim made of these tests' statements.
        fn finalize_calls(&self) -> i32 {
            FINALIZE_CALLS.load(Ordering::Acquire)
        }
    }

    impl Drop for FakeSqlite {
        fn drop(&mut self) {
            use crate::db_interpose_common as c;
            unsafe {
                let r = reals();
                c::orig_sqlite3_finalize = r.finalize;
                c::orig_sqlite3_sql = r.sql;
                c::orig_sqlite3_next_stmt = r.next_stmt;
                c::orig_sqlite3_step = r.step;
                c::orig_sqlite3_db_handle = r.db_handle;
                c::shim_sqlite3_prepare_v2 = r.prepare_v2;
                c::orig_sqlite3_prepare16_v2 = r.prepare16_v2;
                c::orig_sqlite3_exec = r.exec;
                c::orig_sqlite3_close = r.close;
                c::orig_sqlite3_close_v2 = r.close_v2;
                FINALIZE_CALLS.store(0, Ordering::Relaxed);
                reset_test_state();
            }
        }
    }

    unsafe extern "C" fn fake_finalize(stmt: *mut sqlite3_stmt) -> c_int {
        if !is_fake(stmt) {
            return reals().finalize.map(|f| f(stmt)).unwrap_or(SQLITE_ERROR);
        }
        FINALIZE_CALLS.fetch_add(1, Ordering::AcqRel);
        SQLITE_OK
    }

    unsafe extern "C" fn fake_sql(stmt: *mut sqlite3_stmt) -> *const c_char {
        if !is_fake(stmt) {
            return match reals().sql {
                Some(f) => f(stmt),
                None => sqlite3_sql(stmt),
            };
        }
        ptr::null()
    }

    unsafe extern "C" fn fake_next_stmt(
        db: *mut sqlite3,
        prev: *mut sqlite3_stmt,
    ) -> *mut sqlite3_stmt {
        if !is_fake(db) {
            return reals()
                .next_stmt
                .map(|f| f(db, prev))
                .unwrap_or(ptr::null_mut());
        }
        if prev.is_null() {
            LIVE_STMT.load(Ordering::Acquire) as *mut sqlite3_stmt
        } else {
            ptr::null_mut()
        }
    }

    unsafe extern "C" fn fake_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3 {
        if !is_fake(stmt) {
            return match reals().db_handle {
                Some(f) => f(stmt),
                None => sqlite3_db_handle(stmt),
            };
        }
        FAKE_DB
    }

    /// SQLite finalizing, through the shim, a statement it ran inside one of
    /// its own calls.
    unsafe fn finalize_internal_stmt() -> c_int {
        rust_my_sqlite3_finalize(FINALIZE_TARGET.load(Ordering::Acquire) as *mut sqlite3_stmt)
    }

    /// Real step running what FTS3's xCreate runs inside a CREATE VIRTUAL
    /// TABLE: `PRAGMA %Q.page_size`, prepared inside libsqlite3, stepped and
    /// finalized through the shim.
    unsafe extern "C" fn fake_step(stmt: *mut sqlite3_stmt) -> c_int {
        if !is_fake(stmt) {
            return reals().step.map(|f| f(stmt)).unwrap_or(SQLITE_ERROR);
        }
        finalize_internal_stmt();
        101
    }

    /// Real prepare connecting an FTS4 table (xConnect), which runs the same
    /// PRAGMA.
    unsafe extern "C" fn fake_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        pp_stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int {
        if !is_fake(db) {
            return reals()
                .prepare_v2
                .map(|f| f(db, sql, n, pp_stmt, tail))
                .unwrap_or(SQLITE_ERROR);
        }
        finalize_internal_stmt();
        if !pp_stmt.is_null() {
            *pp_stmt = ptr::null_mut();
        }
        SQLITE_OK
    }

    unsafe extern "C" fn fake_prepare16_v2(
        db: *mut sqlite3,
        sql: *const c_void,
        n: c_int,
        pp_stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_void,
    ) -> c_int {
        if !is_fake(db) {
            return reals()
                .prepare16_v2
                .map(|f| f(db, sql, n, pp_stmt, tail))
                .unwrap_or(SQLITE_ERROR);
        }
        finalize_internal_stmt();
        if !pp_stmt.is_null() {
            *pp_stmt = ptr::null_mut();
        }
        SQLITE_OK
    }

    /// Real close disconnecting an FTS4 table, which finalizes the statements
    /// FTS3 prepared and cached inside libsqlite3.
    unsafe extern "C" fn fake_close(db: *mut sqlite3) -> c_int {
        if !is_fake(db) {
            return reals().close.map(|f| f(db)).unwrap_or(SQLITE_ERROR);
        }
        finalize_internal_stmt();
        SQLITE_OK
    }

    unsafe extern "C" fn fake_close_v2(db: *mut sqlite3) -> c_int {
        if !is_fake(db) {
            return reals().close_v2.map(|f| f(db)).unwrap_or(SQLITE_ERROR);
        }
        finalize_internal_stmt();
        SQLITE_OK
    }

    unsafe extern "C" fn fake_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        callback: Sqlite3ExecCallback,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int {
        if !is_fake(db) {
            return reals()
                .exec
                .map(|f| f(db, sql, callback, arg, errmsg))
                .unwrap_or(SQLITE_ERROR);
        }
        finalize_internal_stmt()
    }

    /// Makes `stmt` an address an earlier statement was finalized at moments
    /// ago, and `live` the fake connection's one live statement; the shim then
    /// finalizes `stmt` inside SQLite's own call.
    unsafe fn reused_address(stmt: *mut sqlite3_stmt, live: usize) {
        LIVE_STMT.store(live, Ordering::Release);
        FINALIZE_TARGET.store(stmt as usize, Ordering::Release);
        remember_finalized_stmt(stmt, ptr::null(), 0);
    }

    const STMT: *mut sqlite3_stmt = 0x9abcusize as *mut sqlite3_stmt;

    #[test]
    fn finalize_unknown_stmt_still_calls_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            let stmt = 0x1234usize as *mut sqlite3_stmt;
            assert_eq!(rust_my_sqlite3_finalize(stmt), SQLITE_OK);
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    #[test]
    fn finalize_recently_finalized_stmt_skips_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            let stmt = 0x5678usize as *mut sqlite3_stmt;
            remember_finalized_stmt(stmt, ptr::null(), 0);
            assert_eq!(rust_my_sqlite3_finalize(stmt), SQLITE_OK);
            assert_eq!(fakes.finalize_calls(), 0);
        }
    }

    /// A read the shim answers from PostgreSQL (`is_pg == 2`) still hands
    /// Plex a statement SQLite prepared -- its own SQL, or a dummy
    /// `SELECT 1 WHERE 0` -- and that statement has to be finalized like any
    /// other. `is_pg == 2` once meant a handle that was no SQLite statement at
    /// all, and its finalize skipped SQLite's: the statement lived on, and the
    /// connection it was prepared on could never close. Every Live TV guide
    /// swap left the old guide's connections open on the replaced file, and
    /// their shared -wal was replayed into each new guide, corrupting it
    /// (talos, 2026-10-23 to 10-26: 43 generations, 902 descriptors).
    #[test]
    fn a_read_answered_from_postgresql_is_finalized_in_sqlite_too() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            let stmt = 0x2468usize as *mut sqlite3_stmt;
            let pg = crate::pg_statement::rust_stmt_create(
                ptr::null_mut(),
                c"SELECT id FROM metadata_items".as_ptr(),
                stmt,
            );
            (*pg).is_pg = 2;
            crate::pg_statement::pg_register_stmt(stmt, pg);
            assert_eq!(rust_my_sqlite3_finalize(stmt), SQLITE_OK);
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    /// The cached read steps the statement in SQLite before PostgreSQL
    /// answers it, so what it wraps is a real, stepped statement.
    #[test]
    fn a_cached_read_is_finalized_in_sqlite_too() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            let conn = libc::calloc(1, std::mem::size_of::<crate::ffi_types::PgConnection>())
                as *mut crate::ffi_types::PgConnection;
            assert!(!conn.is_null());
            crate::ffi_types::conn_ref(conn);
            let stmt = 0x2470usize as *mut sqlite3_stmt;
            let sql = c"SELECT id FROM metadata_items";
            let wrapped =
                crate::db_interpose_step_cached_read_utils::rust_step_cached_read_prepare_stmt(
                    ptr::null_mut(),
                    conn,
                    sql.as_ptr(),
                    stmt,
                    sql.as_ptr(),
                );
            assert!(!wrapped.is_null());
            assert_eq!(rust_my_sqlite3_finalize(stmt), SQLITE_OK);
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    /// An address finalized several times holds several records. A prepare
    /// the shim sees at that address must clear every one: with one left, the
    /// new statement's own finalize was skipped and the statement leaked, so
    /// PMS's guide staging database stayed open across its rename, lost its
    /// WAL, and both it and the library file read as corrupt (2026-10-07).
    /// VACUUM's internal statements, really finalized since clusterplex.25,
    /// reuse one address hundreds of times.
    #[test]
    fn a_seen_prepare_clears_every_finalize_record_of_its_address() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            let stmt = 0x6789usize as *mut sqlite3_stmt;
            remember_finalized_stmt(stmt, ptr::null(), 0);
            remember_finalized_stmt(stmt, ptr::null(), 0);
            rust_pg_note_stmt_prepare(stmt, c"SELECT 1".as_ptr());
            assert_eq!(rust_my_sqlite3_finalize(stmt), SQLITE_OK);
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    /// SQLite prepares statements of its own inside libsqlite3, where the shim
    /// never sees them, but finalizes them through the shim. One prepared at
    /// an address an earlier statement freed is live, and must really be
    /// finalized: left open, it is "in progress" and the connection's next
    /// VACUUM fails -- which is how Plex's XMLTV EPG migration failed (DVR
    /// creation 500, 2026-10-07).
    #[test]
    fn finalize_inside_exec_of_a_live_stmt_at_a_reused_address_calls_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            reused_address(STMT, STMT as usize);
            let rc = crate::db_interpose_exec::orig_exec_for_tests(
                FAKE_DB,
                ptr::null(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            assert_eq!(rc, SQLITE_OK);
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    /// The double-finalize guard still holds: an address that is no live
    /// statement of the connection is not finalized again.
    #[test]
    fn finalize_inside_exec_of_a_freed_stmt_still_skips() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            reused_address(STMT, 0xdef0);
            let rc = crate::db_interpose_exec::orig_exec_for_tests(
                FAKE_DB,
                ptr::null(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            assert_eq!(rc, SQLITE_OK);
            assert_eq!(fakes.finalize_calls(), 0);
        }
    }

    /// The same inside real step: CREATE VIRTUAL TABLE's step creates an FTS4
    /// table, whose page-size PRAGMA was the statement left in progress in
    /// Plex's XMLTV EPG migration (2026-10-07).
    #[test]
    fn finalize_inside_step_of_a_live_stmt_at_a_reused_address_calls_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            reused_address(STMT, STMT as usize);
            let outer = 0x7770usize as *mut sqlite3_stmt;
            assert_eq!(crate::db_interpose_step::orig_step_for_tests(outer), 101);
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    /// The same inside real prepare, entered as the prepare worker thread
    /// enters it (`from_worker`), so the scope holds on that thread too. Null
    /// SQL is the shortest route to the real prepare.
    #[test]
    fn finalize_inside_prepare_of_a_live_stmt_at_a_reused_address_calls_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            reused_address(STMT, STMT as usize);
            let mut out: *mut sqlite3_stmt = ptr::null_mut();
            let rc = crate::db_interpose_prepare::rust_my_sqlite3_prepare_v2_internal(
                FAKE_DB,
                ptr::null(),
                -1,
                &mut out,
                ptr::null_mut(),
                1,
            );
            assert_eq!(rc, SQLITE_OK);
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    /// The same inside real prepare16, which the shim calls directly.
    #[test]
    fn finalize_inside_prepare16_of_a_live_stmt_at_a_reused_address_calls_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            reused_address(STMT, STMT as usize);
            let mut out: *mut sqlite3_stmt = ptr::null_mut();
            let rc = crate::db_interpose_prepare::rust_my_sqlite3_prepare16_v2(
                FAKE_DB,
                ptr::null(),
                -1,
                &mut out,
                ptr::null_mut(),
            );
            assert_eq!(rc, SQLITE_OK);
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    /// The same inside real close: sqlite3_close disconnects every virtual
    /// table first, and FTS3 finalizes the statements it cached. Skipped,
    /// one leaks, the connection cannot close, and its WAL outlives it -- so
    /// PMS's guide staging database lost its newest pages when PMS renamed
    /// it into place (2026-10-07, "disk I/O error" on the swapped guide).
    #[test]
    fn finalize_inside_close_of_a_live_stmt_at_a_reused_address_calls_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            reused_address(STMT, STMT as usize);
            assert_eq!(
                crate::db_interpose_open::rust_my_sqlite3_close(FAKE_DB),
                SQLITE_OK
            );
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    #[test]
    fn finalize_inside_close_v2_of_a_live_stmt_at_a_reused_address_calls_original_finalize() {
        let _guard = TEST_LOCK.lock().unwrap();
        unsafe {
            let fakes = FakeSqlite::install();
            reused_address(STMT, STMT as usize);
            assert_eq!(
                crate::db_interpose_open::rust_my_sqlite3_close_v2(FAKE_DB),
                SQLITE_OK
            );
            assert_eq!(fakes.finalize_calls(), 1);
        }
    }

    #[test]
    fn conn_scopes_nest_and_restore() {
        let a = 0x1000usize as *mut sqlite3;
        let b = 0x2000usize as *mut sqlite3;
        assert!(current_conn().is_null());
        {
            let _outer = ConnScope::enter(a);
            assert_eq!(current_conn(), a);
            {
                let _inner = ConnScope::enter(b);
                assert_eq!(current_conn(), b);
            }
            assert_eq!(current_conn(), a);
        }
        assert!(current_conn().is_null());
    }
}
