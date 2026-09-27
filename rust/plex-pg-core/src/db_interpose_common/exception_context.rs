use super::*;

/// Cell breadcrumbs contain only static phase labels and pointer identities;
/// they never retain or dereference a result/query pointer after its lifetime.
#[derive(Clone, Copy)]
struct ColumnBreadcrumb {
    phase: *const c_char,
    stmt: *const c_void,
    db: *const c_void,
    idx: c_int,
}
thread_local! {
    static COLUMN_BREADCRUMB: std::cell::Cell<ColumnBreadcrumb> = const {
        std::cell::Cell::new(ColumnBreadcrumb {
            phase: ptr::null(), stmt: ptr::null(), db: ptr::null(), idx: -1,
        })
    };
}

pub(crate) fn note_column_phase(
    phase: &'static [u8], sql: *const c_char, stmt: *const c_void,
    db: *const c_void, idx: c_int,
) {
    COLUMN_BREADCRUMB.with(|v| v.set(ColumnBreadcrumb {
        phase: phase.as_ptr() as *const c_char, stmt, db, idx,
    }));
    // Full per-cell history is diagnostic-only. Query prefixes and fatal-signal
    // context are still recorded by the existing prepare/execute/step paths.
    static FULL_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *FULL_TRACE.get_or_init(|| env_utils::env_truthy_str("PLEX_PG_TRACE_COLUMN_PHASES")) {
        rust_pg_exception_note_phase(phase.as_ptr() as *const c_char, sql, stmt, db);
    }
}

fn dump_column_breadcrumb() {
    let _ = COLUMN_BREADCRUMB.try_with(|v| {
        let v = v.get();
        if !v.phase.is_null() {
            unsafe { libc::fprintf(stderr_ptr(),
                b"[EXC_CONTEXT] Current thread column: phase=%s stmt=%p db=%p idx=%d\n\0".as_ptr() as *const c_char,
                v.phase, v.stmt, v.db, v.idx); }
        }
    });
}

/// Equivalent to a precision-limited %s copy, without printf parsing. Bounds
/// the input scan as well as the output; preserves byte-based truncation.
pub(crate) unsafe fn copy_context(out: *mut c_char, cap: usize, input: *const c_char) -> c_int {
    if cap == 0 || out.is_null() { return 0; }
    let n = if input.is_null() { 0 } else { libc::strnlen(input, cap - 1) };
    if n != 0 { ptr::copy_nonoverlapping(input, out, n); }
    *out.add(n) = 0;
    n as c_int
}

pub fn rust_pg_exception_note_query(sql: *const c_char) {
    if sql.is_null() {
        return;
    }
    unsafe {
        if *sql == 0 {
            return;
        }
        let mut ring_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_QUERY_RING_MUTEX));
        copy_context(EXC_QUERY_RING[EXC_QUERY_RING_NEXT as usize].as_mut_ptr(),
            EXC_QUERY_MAX_LEN, sql);
        EXC_QUERY_RING_NEXT = (EXC_QUERY_RING_NEXT + 1) % (EXC_QUERY_RING_SIZE as c_int);
        ring_guard.unlock();
    }
}

pub fn rust_pg_exception_dump_recent_queries() {
    unsafe {
        let mut ring_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_QUERY_RING_MUTEX));
        libc::fprintf(
            stderr_ptr(),
            b"[EXC_CONTEXT] Recent SQL (oldest -> newest):\n\0".as_ptr() as *const c_char,
        );
        for i in 0..EXC_QUERY_RING_SIZE {
            let idx = (EXC_QUERY_RING_NEXT + i as c_int) % (EXC_QUERY_RING_SIZE as c_int);
            let entry = EXC_QUERY_RING[idx as usize];
            if entry[0] != 0 {
                libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_CONTEXT]   [%02d] %.319s\n\0".as_ptr() as *const c_char,
                    i as c_int,
                    entry.as_ptr(),
                );
            }
        }
        libc::fflush(stderr_ptr());
        ring_guard.unlock();
    }
}

pub fn rust_pg_exception_note_phase(
    phase: *const c_char,
    sql: *const c_char,
    stmt: *const c_void,
    db: *const c_void,
) {
    unsafe {
        let mut phase_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_PHASE_RING_MUTEX));

        let slot = &mut EXC_PHASE_RING[EXC_PHASE_RING_NEXT as usize];
        copy_context(slot.phase.as_mut_ptr(), slot.phase.len(),
            if phase.is_null() { UNKNOWN_STR.as_ptr() as *const c_char } else { phase });
        copy_context(slot.sql.as_mut_ptr(), slot.sql.len(), sql);
        slot.stmt = stmt as *mut c_void;
        slot.db = db as *mut c_void;
        slot.tid = libc::pthread_self() as libc::c_ulong;

        EXC_PHASE_RING_NEXT = (EXC_PHASE_RING_NEXT + 1) % (EXC_PHASE_RING_SIZE as c_int);

        // Keep the writer lock through the shared crash-buffer updates.
        // --- seqlock: begin CRASH_LAST_QUERY write ---
        let q_seq = CRASH_LAST_QUERY_SEQ.load(Ordering::Relaxed);
        CRASH_LAST_QUERY_SEQ.store(q_seq.wrapping_add(1), Ordering::Release); // odd = writing
        let qlen = copy_context(ptr::addr_of_mut!(CRASH_LAST_QUERY) as *mut c_char,
            CRASH_QUERY_MAX_LEN, sql);
        CRASH_LAST_QUERY_LEN.store(qlen, Ordering::SeqCst);
        CRASH_LAST_QUERY_SEQ.store(q_seq.wrapping_add(2), Ordering::Release); // even = done
                                                                              // --- seqlock: end CRASH_LAST_QUERY write ---

        // --- seqlock: begin CRASH_LAST_PHASE write ---
        let p_seq = CRASH_LAST_PHASE_SEQ.load(Ordering::Relaxed);
        CRASH_LAST_PHASE_SEQ.store(p_seq.wrapping_add(1), Ordering::Release); // odd = writing
        let plen = copy_context(ptr::addr_of_mut!(CRASH_LAST_PHASE) as *mut c_char,
            CRASH_PHASE_MAX_LEN, phase);
        CRASH_LAST_PHASE_LEN.store(plen, Ordering::SeqCst);
        CRASH_LAST_PHASE_SEQ.store(p_seq.wrapping_add(2), Ordering::Release); // even = done
                                                                              // --- seqlock: end CRASH_LAST_PHASE write ---

        let trace_path = TRACE_LAST_QUERY_PATH
            .get()
            .map(|p| p.0)
            .unwrap_or(ptr::null());
        if trace_last_query_enabled() && !trace_path.is_null() && qlen > 0 {
            let fd = libc::open(
                trace_path,
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o644,
            );
            if fd >= 0 {
                if plen > 0 {
                    let _ = libc::write(
                        fd,
                        ptr::addr_of!(CRASH_LAST_PHASE) as *const c_void,
                        plen as usize,
                    );
                    let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
                }
                let _ = libc::write(
                    fd,
                    ptr::addr_of!(CRASH_LAST_QUERY) as *const c_void,
                    qlen as usize,
                );
                let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
                libc::close(fd);
            }
        }
        phase_guard.unlock();
    }
}

pub fn rust_pg_exception_dump_recent_phases() {
    dump_column_breadcrumb();
    unsafe {
        let mut phase_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_PHASE_RING_MUTEX));

        libc::fprintf(
            stderr_ptr(),
            b"[EXC_CONTEXT] Recent phases (oldest -> newest):\n\0".as_ptr() as *const c_char,
        );
        for i in 0..EXC_PHASE_RING_SIZE {
            let idx = (EXC_PHASE_RING_NEXT + i as c_int) % (EXC_PHASE_RING_SIZE as c_int);
            let entry = &EXC_PHASE_RING[idx as usize];
            if entry.phase[0] == 0 {
                continue;
            }
            if entry.sql[0] != 0 {
                libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_CONTEXT]   [%02d] phase=%s tid=0x%lx stmt=%p db=%p sql=%.200s\n\0"
                        .as_ptr() as *const c_char,
                    i as c_int,
                    entry.phase.as_ptr(),
                    entry.tid,
                    entry.stmt,
                    entry.db,
                    entry.sql.as_ptr(),
                );
            } else {
                libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_CONTEXT]   [%02d] phase=%s tid=0x%lx stmt=%p db=%p\n\0".as_ptr()
                        as *const c_char,
                    i as c_int,
                    entry.phase.as_ptr(),
                    entry.tid,
                    entry.stmt,
                    entry.db,
                );
            }
        }

        libc::fflush(stderr_ptr());
        phase_guard.unlock();
    }
}

#[cfg(test)]
mod hotpath_tests {
    use super::*;
    #[test]
    fn context_copy_handles_null_zero_capacity_and_byte_truncation() {
        let src = b"a\xc3\xa9z\0";
        let mut out = [7 as c_char; 4];
        unsafe {
            assert_eq!(copy_context(out.as_mut_ptr(), 3, src.as_ptr() as *const _), 2);
            assert_eq!([out[0] as u8, out[1] as u8, out[2] as u8], [b'a', 0xc3, 0]);
            assert_eq!(out[3], 7);
            assert_eq!(copy_context(out.as_mut_ptr(), 0, src.as_ptr() as *const _), 0);
            assert_eq!(out[0], b'a' as c_char);
            assert_eq!(copy_context(out.as_mut_ptr(), out.len(), ptr::null()), 0);
            assert_eq!(out[0], 0);
        }
    }
    #[test]
    fn column_breadcrumb_is_thread_local_and_owns_no_query_pointer() {
        note_column_phase(b"column_text\0", ptr::null(), ptr::null(), ptr::null(), 7);
        std::thread::spawn(|| {
            COLUMN_BREADCRUMB.with(|v| assert_eq!(v.get().idx, -1));
            note_column_phase(b"column_type\0", ptr::null(), ptr::null(), ptr::null(), 3);
            COLUMN_BREADCRUMB.with(|v| assert_eq!(v.get().idx, 3));
        }).join().unwrap();
        COLUMN_BREADCRUMB.with(|v| assert_eq!(v.get().idx, 7));
    }
}
