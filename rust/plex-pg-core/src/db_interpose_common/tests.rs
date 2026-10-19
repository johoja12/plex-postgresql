#[cfg(target_os = "linux")]
use super::{linux_process_name_is_primary, linux_process_name_requires_passthrough};
use super::{
    rust_common_handle_exception, rust_common_load_sqlite_symbols, rust_delegate_prepare_to_worker,
    rust_get_exception_tracker, rust_reset_exception_tracking, rust_simple_str_replace,
    rust_worker_cleanup, rust_worker_init, tls_column_type_calls_ptr, tls_in_interpose_call_ptr,
    tls_last_query_ptr, tls_value_type_calls_ptr, total_exception_count, worker_running,
    worker_thread,
};
use libc::{c_void, RTLD_DEFAULT, RTLD_LAZY};
use std::ffi::{CStr, CString};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Barrier, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

fn call_replace(input: Option<&str>, old: Option<&str>, new_str: Option<&str>) -> Option<String> {
    let input_cs = input.map(|s| CString::new(s).unwrap());
    let old_cs = old.map(|s| CString::new(s).unwrap());
    let new_cs = new_str.map(|s| CString::new(s).unwrap());

    let ptr = rust_simple_str_replace(
        input_cs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        old_cs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        new_cs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
    );

    if ptr.is_null() {
        return None;
    }

    let out = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        libc::free(ptr as *mut c_void);
    }
    Some(out)
}

#[test]
fn common_helpers_simple_str_replace_null_str_returns_none() {
    assert!(call_replace(None, Some("old"), Some("new")).is_none());
}

#[test]
fn common_helpers_simple_str_replace_null_old_returns_none() {
    assert!(call_replace(Some("hello"), None, Some("new")).is_none());
}

#[test]
fn common_helpers_simple_str_replace_null_new_returns_none() {
    assert!(call_replace(Some("hello"), Some("old"), None).is_none());
}

#[test]
fn common_helpers_simple_str_replace_no_match_returns_none() {
    assert!(call_replace(Some("hello world"), Some("xyz"), Some("abc")).is_none());
}

#[test]
fn common_helpers_simple_str_replace_basic_replace() {
    assert_eq!(
        call_replace(Some("hello world"), Some("world"), Some("earth")),
        Some("hello earth".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_at_start() {
    assert_eq!(
        call_replace(Some("hello world"), Some("hello"), Some("goodbye")),
        Some("goodbye world".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_at_end() {
    assert_eq!(
        call_replace(Some("hello world"), Some("world"), Some("!")),
        Some("hello !".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_shorter_with_longer() {
    assert_eq!(
        call_replace(Some("ab"), Some("a"), Some("xyz")),
        Some("xyzb".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_longer_with_shorter() {
    assert_eq!(
        call_replace(Some("hello world"), Some("hello"), Some("hi")),
        Some("hi world".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_delete_segment() {
    assert_eq!(
        call_replace(Some("hello world"), Some("hello "), Some("")),
        Some("world".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_empty_old_prepends() {
    assert_eq!(
        call_replace(Some("hello"), Some(""), Some("X")),
        Some("Xhello".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_first_occurrence_only() {
    assert_eq!(
        call_replace(Some("aaa"), Some("a"), Some("b")),
        Some("baa".to_string())
    );
}

#[test]
fn common_helpers_simple_str_replace_sql_transform() {
    assert_eq!(
        call_replace(
            Some("INSERT OR REPLACE INTO tags"),
            Some("INSERT OR REPLACE INTO"),
            Some("INSERT INTO")
        ),
        Some("INSERT INTO tags".to_string())
    );
}

// Reset and count assertions operate on the same process-wide tracker.
static EXCEPTION_TRACKER_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn exception_tracker_increments_for_same_type() {
    let _serialised = EXCEPTION_TRACKER_TEST_LOCK.lock().unwrap();
    rust_reset_exception_tracking();
    let name = c"TestException";

    let t1 = rust_get_exception_tracker(name.as_ptr());
    assert!(!t1.is_null());
    assert_eq!(unsafe { (*t1).count }, 1);

    let t2 = rust_get_exception_tracker(name.as_ptr());
    assert!(!t2.is_null());
    assert_eq!(unsafe { (*t2).count }, 2);
}

#[test]
fn exception_tracking_reset_clears_counts() {
    let _serialised = EXCEPTION_TRACKER_TEST_LOCK.lock().unwrap();
    rust_reset_exception_tracking();
    let name = c"ResetException";
    let t1 = rust_get_exception_tracker(name.as_ptr());
    assert_eq!(unsafe { (*t1).count }, 1);

    rust_reset_exception_tracking();
    let t2 = rust_get_exception_tracker(name.as_ptr());
    assert_eq!(unsafe { (*t2).count }, 1);
}

#[test]
fn common_handle_exception_increments_total_count() {
    let _serialised = EXCEPTION_TRACKER_TEST_LOCK.lock().unwrap();
    rust_reset_exception_tracking();
    unsafe {
        *tls_column_type_calls_ptr() = 1;
        *tls_value_type_calls_ptr() = 0;
        *tls_last_query_ptr() = std::ptr::null();
    }

    let mut in_handler = 0;
    let mut should_call_original = 0;
    let rc = rust_common_handle_exception(
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut in_handler,
        &mut should_call_original,
    );
    assert_eq!(rc, 0);
    assert_eq!(should_call_original, 1);
    assert_eq!(total_exception_count.load(Ordering::SeqCst), 1);

    unsafe {
        *tls_column_type_calls_ptr() = 0;
    }
}

#[test]
fn common_load_sqlite_symbols_sets_pointers() {
    // Do not clear process-wide function pointers: other tests may be using
    // them concurrently, and unloading their backing DSO would leave dangling
    // pointers in the shared registry.
    rust_common_load_sqlite_symbols(std::ptr::null_mut());

    let mut handle = std::ptr::null_mut();
    let names = if cfg!(target_os = "macos") {
        vec![
            CString::new("libsqlite3.dylib").unwrap(),
            CString::new("/usr/lib/libsqlite3.dylib").unwrap(),
        ]
    } else {
        vec![
            CString::new("libsqlite3.so.0").unwrap(),
            CString::new("libsqlite3.so").unwrap(),
        ]
    };
    for name in names {
        unsafe {
            handle = libc::dlopen(name.as_ptr(), RTLD_LAZY);
        }
        if !handle.is_null() {
            break;
        }
    }
    if handle.is_null() {
        handle = RTLD_DEFAULT;
    }

    unsafe {
        rust_common_load_sqlite_symbols(handle);
        let open = super::orig_sqlite3_open;
        let prepare = super::orig_sqlite3_prepare_v2;
        let decltype = super::orig_sqlite3_column_decltype;
        assert!(open.is_some());
        assert!(prepare.is_some());
        assert!(decltype.is_some());
    }

    // Keep the handle alive for the lifetime of the test process because the
    // shared symbol slots contain pointers into this library.
}

#[test]
fn tls_state_is_thread_local() {
    unsafe {
        *tls_column_type_calls_ptr() = 111;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        unsafe {
            *tls_column_type_calls_ptr() = 222;
        }
        let val = unsafe { *tls_column_type_calls_ptr() };
        tx.send(val).unwrap();
    })
    .join()
    .unwrap();

    let other = rx.recv().unwrap();
    assert_eq!(other, 222);
    let main_val = unsafe { *tls_column_type_calls_ptr() };
    assert_eq!(main_val, 111);
}

#[test]
fn worker_init_does_not_spawn_a_second_worker() {
    // `rust_delegate_prepare_to_worker` checks `worker_running` outside the
    // worker mutex and initialises if it is 0. Two threads delegating at once
    // -- or any thread delegating just after a fork reset it -- both see 0 and
    // both run the initialiser. That leaves two workers consuming the single
    // global `worker_request` slot, and orphans the first `worker_thread`
    // handle so cleanup can never join it.
    //
    // The underlying defect is that init is not idempotent, which is testable
    // without racing anything: a second init must not replace the thread.
    let _serialised = worker_test_lock();

    unsafe {
        rust_worker_cleanup();
        assert_eq!(
            worker_running, 0,
            "worker should be stopped before the test"
        );

        assert_eq!(rust_worker_init(), 0);
        let first = worker_thread;
        assert_ne!(worker_running, 0);

        assert_eq!(rust_worker_init(), 0);
        let second = worker_thread;

        // Clean up before asserting so a failure does not strand a thread.
        rust_worker_cleanup();

        assert_eq!(
            first, second,
            "second init spawned another worker and orphaned the first"
        );
    }
}

/// There is one worker for the process, so the tests that stop and start it
/// cannot run beside each other: cargo runs tests on threads, and one test's
/// `rust_worker_cleanup` would otherwise kill the worker another is using.
static WORKER_TEST_LOCK: Mutex<()> = Mutex::new(());

fn worker_test_lock() -> MutexGuard<'static, ()> {
    // A test that fails while holding it poisons the lock; the next one still
    // needs to run, and it resets the worker itself anyway.
    WORKER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Opens a throwaway in-memory database through the SQLite the shim itself
/// resolved, or returns None when this build has no SQLite to talk to.
///
/// Delegation ends in a real `sqlite3_prepare_v2`, so the handle has to be a
/// real one: a null stands in for nothing and the call walks off it.
fn open_scratch_sqlite() -> Option<*mut c_void> {
    let names = if cfg!(target_os = "macos") {
        vec!["libsqlite3.dylib", "/usr/lib/libsqlite3.dylib"]
    } else {
        vec!["libsqlite3.so.0", "libsqlite3.so"]
    };
    let mut handle = std::ptr::null_mut();
    for name in names {
        let name = CString::new(name).unwrap();
        unsafe {
            handle = libc::dlopen(name.as_ptr(), RTLD_LAZY);
        }
        if !handle.is_null() {
            break;
        }
    }
    if handle.is_null() {
        handle = RTLD_DEFAULT;
    }
    rust_common_load_sqlite_symbols(handle);

    let open = unsafe { super::orig_sqlite3_open }?;
    let path = CString::new(":memory:").unwrap();
    let mut db: *mut c_void = std::ptr::null_mut();
    let rc = unsafe { open(path.as_ptr(), &mut db as *mut _ as *mut _) };
    if rc != 0 || db.is_null() {
        return None;
    }
    Some(db)
}

// Exercise the dynamically loaded entry points while other threads repeat
// symbol initialization. Published pointers must remain callable for the
// process lifetime, including after the loader test above has returned.
#[test]
fn concurrent_symbol_loads_keep_sqlite_entry_points_callable() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 100;
    let barrier = Arc::new(Barrier::new(THREADS));
    let threads: Vec<_> = (0..THREADS)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ROUNDS {
                    let db = open_scratch_sqlite().expect("SQLite is required for the CI tests");
                    let exec = super::get_orig_sqlite3_exec().expect("sqlite3_exec");
                    let close = super::get_orig_sqlite3_close().expect("sqlite3_close");
                    let sql = CString::new("CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (42)").unwrap();
                    assert_eq!(unsafe {
                        exec(db.cast(), sql.as_ptr(), None, std::ptr::null_mut(), std::ptr::null_mut())
                    }, 0);
                    assert_eq!(unsafe { close(db.cast()) }, 0);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn concurrent_delegations_each_get_their_own_answer() {
    // There is one global `worker_request`, and `rust_delegate_prepare_to_worker`
    // waits for its answer on `worker_cond_response` with `worker_mutex`
    // released -- which is exactly when a second caller can lock that mutex and
    // overwrite the slot, including clearing the `work_done` the first caller
    // has not read yet.
    //
    // Two ways that ends badly, both seen in the wild as Plex hanging in
    // "Running migrations": the first caller wakes to find `work_done` reset
    // and waits again for a response that has already been signalled, or it
    // reads the second caller's statement out of the shared slot.
    //
    // Neither can happen once delegation is serialised end to end, so every
    // caller must come back, and come back with the result of its own request.
    const THREADS: usize = 8;
    const ROUNDS: usize = 200;

    let _serialised = worker_test_lock();

    let Some(db) = open_scratch_sqlite() else {
        eprintln!("no SQLite available; skipping delegation test");
        return;
    };
    let db = db as usize; // raw pointers are not Send; the handle outlives the threads

    rust_worker_cleanup();
    assert_eq!(rust_worker_init(), 0);

    let barrier = Arc::new(Barrier::new(THREADS));
    let (tx, rx) = mpsc::channel::<usize>();

    let workers: Vec<_> = (0..THREADS)
        .map(|id| {
            let barrier = Arc::clone(&barrier);
            let tx = tx.clone();
            thread::spawn(move || {
                // Distinct per thread, so a slot that gets crossed shows up as
                // a tail pointing into somebody else's buffer.
                let sql = CString::new(format!("select {}", id)).unwrap();
                barrier.wait();
                for _ in 0..ROUNDS {
                    let mut stmt: *mut c_void = std::ptr::null_mut();
                    let mut tail: *const std::os::raw::c_char = std::ptr::null();
                    rust_delegate_prepare_to_worker(
                        db as *mut _,
                        sql.as_ptr(),
                        -1,
                        &mut stmt as *mut _ as *mut _,
                        &mut tail,
                    );
                    if !tail.is_null() {
                        let base = sql.as_ptr() as usize;
                        let within = (tail as usize) >= base
                            && (tail as usize) <= base + sql.as_bytes().len();
                        assert!(within, "thread {} was handed another caller's tail", id);
                    }
                }
                let _ = tx.send(id);
            })
        })
        .collect();
    drop(tx);

    // A lost wake-up parks a caller forever, so wait with a deadline rather
    // than joining: a hung test tells nobody anything.
    let mut finished = 0;
    while finished < THREADS {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(_) => finished += 1,
            Err(_) => break,
        }
    }

    let stranded = THREADS - finished;
    if stranded == 0 {
        for w in workers {
            let _ = w.join();
        }
    }
    rust_worker_cleanup();

    assert_eq!(
        stranded, 0,
        "{} of {} delegations never came back; a caller's completion was cleared by another caller",
        stranded, THREADS
    );
}

#[test]
fn tls_interpose_guard_does_not_leak_between_threads() {
    // `in_interpose_call` is the guard that stops the shim recursing into its
    // own interposed sqlite3 symbols. If it is shared, one thread entering the
    // shim makes every other thread believe it is already inside a call, and
    // the non-atomic increments on it are a plain data race. Plex drives this
    // from many threads at once, so prove it holds under contention, not just
    // for one spawned thread.
    const THREADS: usize = 8;
    const ROUNDS: i32 = 2000;

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
    let mut handles = Vec::with_capacity(THREADS);

    for _ in 0..THREADS {
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let guard = tls_in_interpose_call_ptr();
            barrier.wait();
            for _ in 0..ROUNDS {
                unsafe {
                    // Each thread owns its guard, so it must always observe
                    // exactly its own enter/leave, never another thread's.
                    assert_eq!(*guard, 0);
                    *guard += 1;
                    assert_eq!(*guard, 1);
                    *guard -= 1;
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("interpose guard leaked across threads");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_primary_process_names_stay_active() {
    assert!(linux_process_name_is_primary("Plex Media Server"));
    assert!(linux_process_name_is_primary("Plex Media Serv"));
    assert!(linux_process_name_is_primary("Plex Media Scanner"));
    assert!(!linux_process_name_requires_passthrough(
        "Plex Media Scanner"
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_helper_process_names_switch_to_passthrough() {
    assert!(linux_process_name_requires_passthrough("PMS CPM"));
    assert!(linux_process_name_requires_passthrough("PMS ReqHandler"));
    assert!(linux_process_name_requires_passthrough("PMS FileWatcher"));
    assert!(!linux_process_name_requires_passthrough(""));
}
