#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

#[allow(unused_imports)]
use crate::c_abi;
use crate::db_interpose_common;
use crate::db_interpose_common::stderr_ptr;
use crate::env_utils;
use crate::exception_what::pg_exception_install_terminate_logger;
#[allow(unused_imports)]
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value};
use crate::runtime_common::{handle_exception_with_tls, log_shim_unloading, shim_init_common};

type SigactionFn =
    unsafe extern "C" fn(c_int, *const libc::sigaction, *mut libc::sigaction) -> c_int;
type CxaThrowFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, Option<unsafe extern "C" fn(*mut c_void)>) -> !;
/// Pass-through hook for create_simple_converter (ASCII path handled at the
/// create_simple_codecvt level by the AArch64 asm hook below).
type CreateSimpleConverterFn = unsafe extern "C" fn(*mut u8) -> *mut c_void;

static mut ORIG_SIGACTION: Option<SigactionFn> = None;
static mut ORIG_CXA_THROW: Option<CxaThrowFn> = None;
static mut ORIG_CREATE_SIMPLE_CONVERTER: Option<CreateSimpleConverterFn> = None;

/// Function-pointer statics for the AArch64 global_asm hook below.
/// #[no_mangle] makes them addressable by their exact name from assembler.
#[no_mangle]
pub static mut SHIM_CREATE_UTF8_CODECVT_PTR: usize = 0;
#[no_mangle]
pub static mut SHIM_CREATE_SIMPLE_CODECVT_PTR: usize = 0;
/// The converter pair, for the x86-64 hooks below. Boost reaches the failing
/// charset through `create_simple_converter` on this architecture, where
/// AArch64 reaches it through `create_simple_codecvt`.
#[no_mangle]
pub static mut SHIM_CREATE_UTF8_CONVERTER_PTR: usize = 0;
#[no_mangle]
pub static mut SHIM_CREATE_SIMPLE_CONVERTER_PTR: usize = 0;

static FORCE_IGNORE_SIGCHLD: AtomicI32 = AtomicI32::new(1);
static INTERCEPT_SIGACTION: AtomicI32 = AtomicI32::new(1);
static SIGNAL_LOG_ENABLED_CACHED: AtomicI32 = AtomicI32::new(-1);
static EXCEPTION_CATCHER_ENABLED_CACHED: AtomicI32 = AtomicI32::new(-1);

pub(crate) fn disable_postfork_signal_overrides_fast() {
    FORCE_IGNORE_SIGCHLD.store(0, Ordering::Relaxed);
    INTERCEPT_SIGACTION.store(0, Ordering::Relaxed);
}

fn signal_log_enabled() -> bool {
    let cached = SIGNAL_LOG_ENABLED_CACHED.load(Ordering::Acquire);
    if cached != -1 {
        return cached != 0;
    }
    let enabled = env_utils::env_truthy(b"PLEX_PG_ENABLE_SIGNAL_LOG\0");
    SIGNAL_LOG_ENABLED_CACHED.store(if enabled { 1 } else { 0 }, Ordering::Release);
    enabled
}

/// Whether to print a backtrace from every `__cxa_throw`.
///
/// Separate from the catcher: the catcher decides whether the shim inspects
/// the exception at all, and this is diagnostic output that is far too loud to
/// leave on. It is read on every throw rather than cached, so it can be turned
/// on for one run without a rebuild.
fn exception_backtrace_enabled() -> bool {
    env_utils::env_truthy(b"PLEX_PG_EXCEPTION_BACKTRACE\0")
}

fn exception_catcher_enabled() -> bool {
    let cached = EXCEPTION_CATCHER_ENABLED_CACHED.load(Ordering::Acquire);
    if cached != -1 {
        return cached != 0;
    }
    let enabled = env_utils::env_truthy(b"PLEX_PG_ENABLE_EXCEPTION_CATCHER\0");
    EXCEPTION_CATCHER_ENABLED_CACHED.store(if enabled { 1 } else { 0 }, Ordering::Release);
    enabled
}

/// Eagerly resolve all interposition hooks that live in this module.
/// Called once from `shim_init()` before any other thread can call the
/// interposed wrappers, eliminating the data race on lazy init.
#[allow(static_mut_refs)]
unsafe fn resolve_interposition_hooks() {
    // sigaction
    let sym = libc::dlsym(libc::RTLD_NEXT, b"sigaction\0".as_ptr() as *const c_char);
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(ORIG_SIGACTION),
            Some(std::mem::transmute::<*mut c_void, SigactionFn>(sym)),
        );
    }

    // __cxa_throw
    let sym = libc::dlsym(libc::RTLD_NEXT, b"__cxa_throw\0".as_ptr() as *const c_char);
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(ORIG_CXA_THROW),
            Some(std::mem::transmute::<*mut c_void, CxaThrowFn>(sym)),
        );
    }

    // create_simple_converter — pass-through hook (ASCII handled at codecvt level)
    let sym = libc::dlsym(
        libc::RTLD_NEXT,
        b"_ZN5boost6locale4util23create_simple_converterERKNSt3__212basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEE\0".as_ptr() as *const c_char
    );
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(ORIG_CREATE_SIMPLE_CONVERTER),
            Some(std::mem::transmute::<*mut c_void, CreateSimpleConverterFn>(
                sym,
            )),
        );
    }

    // create_simple_codecvt — original target for the asm hook pass-through path.
    // Stored as a raw usize so the AArch64 assembler can read it directly.
    let sym = libc::dlsym(
        libc::RTLD_NEXT,
        b"_ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE\0".as_ptr() as *const c_char
    );
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(SHIM_CREATE_SIMPLE_CODECVT_PTR),
            sym as usize,
        );
    }

    // create_utf8_codecvt — ASCII redirect target for the asm hook.
    let sym = libc::dlsym(
        libc::RTLD_NEXT,
        b"_ZN5boost6locale4util19create_utf8_codecvtERKNSt3__26localeENS0_12char_facet_tE\0"
            .as_ptr() as *const c_char,
    );
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(SHIM_CREATE_UTF8_CODECVT_PTR),
            sym as usize,
        );
    }

    // create_simple_converter — pass-through target for the x86-64 hook.
    let sym = libc::dlsym(
        libc::RTLD_NEXT,
        b"_ZN5boost6locale4util23create_simple_converterERKNSt3__212basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEE\0".as_ptr() as *const c_char
    );
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(SHIM_CREATE_SIMPLE_CONVERTER_PTR),
            sym as usize,
        );
    }

    // create_utf8_converter — ASCII redirect target for the x86-64 hook.
    let sym = libc::dlsym(
        libc::RTLD_NEXT,
        b"_ZN5boost6locale4util21create_utf8_converterEv\0".as_ptr() as *const c_char,
    );
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(SHIM_CREATE_UTF8_CONVERTER_PTR),
            sym as usize,
        );
    }
}

#[allow(static_mut_refs)]
unsafe fn read_cxa_throw() -> Option<CxaThrowFn> {
    ptr::read(ptr::addr_of!(ORIG_CXA_THROW))
}

#[allow(static_mut_refs)]
unsafe fn read_sigaction() -> Option<SigactionFn> {
    ptr::read(ptr::addr_of!(ORIG_SIGACTION))
}

fn setup_exception_catcher_if_enabled() {
    if !exception_catcher_enabled() {
        return;
    }
    unsafe {
        if read_cxa_throw().is_some() {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Exception catcher enabled (__cxa_throw interposed)\n\0".as_ptr()
                    as *const c_char,
            );
            pg_exception_install_terminate_logger();
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Exception terminate logger requested (see [EXC_TERMINATE])\n\0"
                    .as_ptr() as *const c_char,
            );
        } else {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] WARNING: failed to resolve __cxa_throw\n\0".as_ptr() as *const c_char,
            );
        }
        let _ = libc::fflush(stderr_ptr());
    }
}

/// Interposing `__cxa_throw` puts a frame belonging to this library between
/// every `throw` in Plex and the `catch` that was meant to handle it, and
/// Plex does not survive that: an unauthenticated `GET /media/providers`
/// returns a clean 401 without the shim and terminates with the same
/// exception uncaught with it.
///
///     libc++abi: terminating with uncaught exception of type
///     UnauthorizedException: HTTP status code 401
///
/// Plex throws and catches routinely, so the first throw of the process is
/// fatal whatever it was -- which is why this has surfaced as three unrelated
/// looking crashes (`std::out_of_range`, `std::domain_error: Invalid uuid
/// length`, and the one above), each of them an exception Plex handles
/// normally.
///
/// Built behind a feature so the hook and the backtrace that depends on it can
/// be turned back on for diagnosis, off by default.
#[cfg_attr(feature = "exception-hook", no_mangle)]
/// # Safety
/// This is an ABI-level interposition hook for C++ exceptions.
/// Callers must follow the platform C++ ABI for `__cxa_throw`.
pub unsafe extern "C" fn __cxa_throw(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
    dest: Option<unsafe extern "C" fn(*mut c_void)>,
) -> ! {
    let orig = match read_cxa_throw() {
        Some(f) => f,
        None => libc::abort(),
    };

    // A backtrace taken here is the only view of where the exception came
    // from. Once it has been thrown the frames below the throw are gone, and
    // a minidump written by the terminate handler shows the unwinder rather
    // than the code that threw.
    if exception_backtrace_enabled() {
        crate::platform_backtrace::platform_print_backtrace(
            b"__cxa_throw\0".as_ptr() as *const c_char,
            1,
        );
    }

    if !exception_catcher_enabled() {
        orig(thrown_exception, tinfo, dest);
    }

    let (handled, _should_call_original) = handle_exception_with_tls(thrown_exception, tinfo);

    if handled == 0 {
        orig(thrown_exception, tinfo, dest);
    }

    orig(thrown_exception, tinfo, dest);
}

#[cfg(target_env = "musl")]
unsafe fn install_signal_handler(signum: c_int) {
    let handler: extern "C" fn(c_int) = db_interpose_common::common_signal_handler;
    libc::signal(signum, handler as libc::sighandler_t);
}

#[cfg(not(target_env = "musl"))]
unsafe fn install_signal_handler(signum: c_int) {
    let handler: extern "C" fn(c_int) = db_interpose_common::common_signal_handler;
    libc::signal(signum, handler as libc::sighandler_t);
}

#[no_mangle]
/// # Safety
/// This is an ABI-level interposition hook for `sigaction`. The caller must
/// provide valid pointers (or NULL where allowed by the libc API).
pub unsafe extern "C" fn sigaction(
    signum: c_int,
    act: *const libc::sigaction,
    oldact: *mut libc::sigaction,
) -> c_int {
    let Some(orig) = read_sigaction() else {
        return -1;
    };

    if INTERCEPT_SIGACTION.load(Ordering::Relaxed) == 0 {
        return orig(signum, act, oldact);
    }

    if FORCE_IGNORE_SIGCHLD.load(Ordering::Relaxed) != 0
        && signum == libc::SIGCHLD
        && !act.is_null()
    {
        if !oldact.is_null() {
            orig(libc::SIGCHLD, ptr::null(), oldact);
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_IGN;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_NOCLDSTOP;
        return orig(libc::SIGCHLD, &sa, ptr::null_mut());
    }

    if signal_log_enabled()
        && !act.is_null()
        && (signum == libc::SIGSEGV
            || signum == libc::SIGABRT
            || signum == libc::SIGFPE
            || signum == libc::SIGILL
            || {
                #[cfg(target_os = "linux")]
                {
                    signum == libc::SIGBUS
                }
                #[cfg(not(target_os = "linux"))]
                {
                    false
                }
            })
    {
        if !oldact.is_null() {
            orig(signum, ptr::null(), oldact);
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction =
            db_interpose_common::common_signal_handler as extern "C" fn(c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        return orig(signum, &sa, ptr::null_mut());
    }

    orig(signum, act, oldact)
}

static mut REAL_SQLITE_HANDLE: *mut c_void = ptr::null_mut();

unsafe fn load_original_functions() {
    let sqlite_paths: [&[u8]; 3] = [
        b"/usr/local/lib/plex-postgresql/libsqlite3_real.so\0",
        b"/usr/lib/plexmediaserver/lib/libsqlite3.so.original\0",
        b"/usr/lib/plexmediaserver/lib/libsqlite3.so\0",
    ];

    let mut handle: *mut c_void = ptr::null_mut();
    for path in sqlite_paths.iter() {
        handle = libc::dlopen(
            path.as_ptr() as *const c_char,
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if !handle.is_null() {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Loaded real SQLite from %s\n\0".as_ptr() as *const c_char,
                path.as_ptr() as *const c_char,
            );
            ptr::write(ptr::addr_of_mut!(REAL_SQLITE_HANDLE), handle);
            break;
        }
    }

    if handle.is_null() {
        let _ = libc::fprintf(
            stderr_ptr(),
            b"[SHIM_INIT] Loading original SQLite functions via RTLD_NEXT...\n\0".as_ptr()
                as *const c_char,
        );
        handle = libc::RTLD_NEXT;
    }

    db_interpose_common::common_load_sqlite_symbols(handle);
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[SHIM_INIT] Original SQLite functions loaded\n\0".as_ptr() as *const c_char,
    );
}

#[no_mangle]
pub extern "C" fn ensure_real_sqlite_loaded() {
    unsafe {
        if ptr::read(ptr::addr_of!(db_interpose_common::shim_sqlite3_prepare_v2)).is_some() {
            return;
        }
        ptr::write(
            ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_prepare_v2),
            ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_prepare_v2)),
        );
        ptr::write(
            ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_errmsg),
            ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_errmsg)),
        );
        ptr::write(
            ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_errcode),
            ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_errcode)),
        );
    }
}

// /proc/self/cmdline is NUL-separated. Arguments may contain slashes (for
// example the scanner's --content-type=application/json); only argv[0]
// identifies the executable whose database calls we must interpose.
fn executable_basename(cmdline: &[u8]) -> &[u8] {
    let executable = cmdline.split(|&b| b == 0).next().unwrap_or_default();
    executable.rsplit(|&b| b == b'/').next().unwrap_or_default()
}

#[cfg(test)]
mod process_name_tests {
    use super::executable_basename;

    #[test]
    fn scanner_json_argument_does_not_disable_interposition() {
        assert_eq!(executable_basename(b"/usr/lib/plexmediaserver/Plex Media Scanner.real\0--match\0--content-type=application/json\0"), b"Plex Media Scanner.real");
    }

    #[test]
    fn media_path_argument_does_not_change_server_or_scanner_identity() {
        for name in ["Plex Media Server", "Plex Media Scanner.real"] {
            let cmdline =
                format!("/usr/lib/plexmediaserver/{name}\0--directory\0/mnt/plex/Movies\0");
            assert_eq!(executable_basename(cmdline.as_bytes()), name.as_bytes());
        }
        assert_eq!(
            executable_basename(b"/bin/helper\0/Plex Media Server\0"),
            b"helper"
        );
        assert_eq!(
            executable_basename(b"Plex Media Scanner\0"),
            b"Plex Media Scanner"
        );
        assert_eq!(executable_basename(b""), b"");
    }
}

unsafe extern "C" fn shim_init() {
    // Eagerly resolve all interposition hooks before any other thread can
    // call the interposed wrappers.  This eliminates data races on the
    // lazy-init pattern that was previously used.
    resolve_interposition_hooks();
    crate::pms_child_env::init_child_env_hooks();
    crate::pms_net_compat::init_net_compat_hooks();

    shim_init_common(
        "Linux",
        || {
            // Process name filtering: skip non-server/scanner processes.
            if let Ok(cmdline) = std::fs::read("/proc/self/cmdline") {
                let base = executable_basename(&cmdline);
                let base_str = std::str::from_utf8(base).unwrap_or_default();
                if !base_str.contains("Plex Media Server")
                    && !base_str.contains("Plex Media Scanner")
                {
                    crate::pms_child_env::maybe_reexec_current_process_without_shim(
                        base_str, &cmdline,
                    );
                    FORCE_IGNORE_SIGCHLD.store(0, Ordering::Relaxed);
                    INTERCEPT_SIGACTION.store(0, Ordering::Relaxed);
                    db_interpose_common::SHIM_PASSTHROUGH_ONLY.store(1, Ordering::Release);
                    load_original_functions();
                    db_interpose_common::SHIM_INITIALIZED.store(1, Ordering::Release);
                    let base_c = CString::new(base_str).unwrap_or_default();
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] Not Plex Server/Scanner ('%s'), skipping entirely (PID %d)\n\0"
                            .as_ptr() as *const c_char,
                        base_c.as_ptr(),
                        libc::getpid(),
                    );
                    let _ = libc::fflush(stderr_ptr());
                    return false;
                }

                if env_utils::env_truthy(b"PLEX_PG_DISABLE_SIGCHLD_IGNORE\0") {
                    FORCE_IGNORE_SIGCHLD.store(0, Ordering::Relaxed);
                }
                if env_utils::env_truthy(b"PLEX_PG_FORCE_SIGCHLD_IGNORE\0") {
                    FORCE_IGNORE_SIGCHLD.store(1, Ordering::Relaxed);
                }
                if env_utils::env_truthy(b"PLEX_PG_DISABLE_SIGACTION_INTERCEPT\0") {
                    INTERCEPT_SIGACTION.store(0, Ordering::Relaxed);
                }
            }

            db_interpose_common::common_check_fork();

            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Fork safety: using PID-based detection (no pthread_atfork)\n\0"
                    .as_ptr() as *const c_char,
            );
            let _ = libc::fflush(stderr_ptr());

            load_original_functions();

            if ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_open)).is_none()
                || ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_prepare_v2)).is_none()
            {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] SQLite not found in this process, skipping initialization\n\0"
                        .as_ptr() as *const c_char,
                );
                let _ = libc::fflush(stderr_ptr());
                return false;
            }

            true
        },
        || {},
        || {
            setup_exception_catcher_if_enabled();
            crate::pms_child_env::configure_from_env();
            crate::pms_child_env::scrub_current_process_preload();
            crate::pms_process_compat::configure_from_env();
            crate::pms_net_compat::configure_from_env();

            if env_utils::env_truthy(b"PLEX_PG_ENABLE_SIGNAL_LOG\0") {
                install_signal_handler(libc::SIGSEGV);
                install_signal_handler(libc::SIGABRT);
                install_signal_handler(libc::SIGFPE);
                install_signal_handler(libc::SIGILL);
                #[cfg(target_os = "linux")]
                {
                    install_signal_handler(libc::SIGBUS);
                }
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] Signal logging ENABLED via PLEX_PG_ENABLE_SIGNAL_LOG (PID %d)\n\0"
                        .as_ptr() as *const c_char,
                    libc::getpid(),
                );
                let _ = libc::fflush(stderr_ptr());
            }

            if FORCE_IGNORE_SIGCHLD.load(Ordering::Relaxed) != 0 {
                if let Some(orig) = read_sigaction() {
                    let mut sa: libc::sigaction = std::mem::zeroed();
                    sa.sa_sigaction = libc::SIG_IGN;
                    libc::sigemptyset(&mut sa.sa_mask);
                    sa.sa_flags = libc::SA_NOCLDSTOP;
                    orig(libc::SIGCHLD, &sa, ptr::null_mut());
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] SIGCHLD forced to SIG_IGN (PID %d)\n\0".as_ptr()
                            as *const c_char,
                        libc::getpid(),
                    );
                } else {
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] WARNING: could not resolve sigaction; SIGCHLD policy unchanged (PID %d)\n\0"
                            .as_ptr() as *const c_char,
                        libc::getpid(),
                    );
                }
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] SIGCHLD force-ignore disabled via PLEX_PG_DISABLE_SIGCHLD_IGNORE (PID %d)\n\0"
                        .as_ptr() as *const c_char,
                    libc::getpid(),
                );
            }

            if INTERCEPT_SIGACTION.load(Ordering::Relaxed) != 0 {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] sigaction interpose ENABLED (PID %d)\n\0".as_ptr()
                        as *const c_char,
                    libc::getpid(),
                );
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] sigaction interpose DISABLED via PLEX_PG_DISABLE_SIGACTION_INTERCEPT (PID %d)\n\0"
                        .as_ptr() as *const c_char,
                    libc::getpid(),
                );
            }
            let _ = libc::fflush(stderr_ptr());
        },
        || {
            if !env_utils::env_truthy(b"PLEX_PG_NO_INIT_DELAY\0") {
                let delay_ms = env_utils::env_string("PLEX_PG_INIT_DELAY_MS")
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(200);
                if delay_ms > 0 {
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] Waiting %d ms for symbol resolution (PID %d)...\n\0".as_ptr()
                            as *const c_char,
                        delay_ms,
                        libc::getpid(),
                    );
                    let _ = libc::fflush(stderr_ptr());
                    libc::usleep((delay_ms as u32) * 1000);
                }
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] Init delay DISABLED via PLEX_PG_NO_INIT_DELAY\n\0".as_ptr()
                        as *const c_char,
                );
                let _ = libc::fflush(stderr_ptr());
            }
        },
    );
}

unsafe extern "C" fn shim_cleanup() {
    if db_interpose_common::SHIM_INITIALIZED.load(Ordering::Acquire) == 0 {
        return;
    }
    log_shim_unloading("Linux");
    db_interpose_common::common_shim_cleanup();
}

extern "C" fn shim_init_wrapper() {
    unsafe { shim_init() }
}

extern "C" fn shim_cleanup_wrapper() {
    unsafe { shim_cleanup() }
}

#[used]
#[cfg_attr(target_os = "linux", link_section = ".init_array")]
static INIT: extern "C" fn() = shim_init_wrapper;

#[used]
#[cfg_attr(target_os = "linux", link_section = ".fini_array")]
static FINI: extern "C" fn() = shim_cleanup_wrapper;

// ────────────────────────────────────────────────────────────────────────────
// ────────────────────────────────────────────────────────────────────────────
// AArch64 assembly hook for boost::locale::util::create_simple_codecvt.
//
// Why assembly? On AArch64 the Itanium C++ ABI returns std::locale (a
// non-trivially copyable 8-byte struct) via the x8 "indirect result"
// register (SRET), NOT in x0. Plex's Boost saves x8 on entry, so any Rust
// wrapper that clobbers x8 before forwarding the call writes the new locale
// object to a garbage address → SIGSEGV in locale::operator=.
//
// This hook:
//   - Saves x8 + all args on the stack without touching x8 mid-flight.
//   - Detects ASCII in x1 (the string ptr) and redirects to create_utf8_codecvt
//     (x0=locale, x1=facet, x8 restored from stack).
//   - Falls through to the original create_simple_codecvt otherwise.
// ────────────────────────────────────────────────────────────────────────────
// ────────────────────────────────────────────────────────────────────────────
// x86-64 assembly hooks for boost::locale::util's ASCII charset requests.
//
// Plex asks Boost for the "ASCII" charset, and Boost's simple backend is the
// one thing that will not provide it:
//
//	boost::locale::conv::invalid_charset_error:
//	  Invalid or unsupported charset:Invalid simple encoding ASCII
//
// Plex aborts there while loading its translations and never finishes
// starting. AArch64 already redirects ASCII to UTF-8 (below); this is the
// same redirect for x86-64, where the request arrives through
// create_simple_converter rather than create_simple_codecvt.
//
// Why assembly, again? Both functions return a class type -- a
// std::unique_ptr and a std::locale -- so the SysV ABI hands them a hidden
// result pointer in RDI and shifts every real argument one register along. A
// wrapper written in Rust would have to name that pointer to forward it, and
// getting it wrong is how the old create_simple_converter wrapper silently
// passed its own return slot to Boost as the encoding name.
//
//   create_simple_converter: RDI=sret, RSI=const std::string&
//   create_utf8_converter:   RDI=sret
//   create_simple_codecvt:   RDI=sret, RSI=locale, RDX=string, RCX=facet
//   create_utf8_codecvt:     RDI=sret, RSI=locale, RDX=facet
//
// The encoding is read straight out of the string object, which holds short
// strings inline from offset 0; "ASCII" is five bytes, so it is always short.
// A heap string starts with a pointer there instead and will not match, which
// is the safe way to be wrong.
// ────────────────────────────────────────────────────────────────────────────
#[cfg(all(feature = "interpose", target_arch = "x86_64"))]
std::arch::global_asm!(
    ".global _ZN5boost6locale4util23create_simple_converterERKNSt3__212basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEE",
    ".type   _ZN5boost6locale4util23create_simple_converterERKNSt3__212basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEE, @function",
    "_ZN5boost6locale4util23create_simple_converterERKNSt3__212basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEE:",
    "cmpb $65, 0(%rsi)",
    "jne  2f",
    "cmpb $83, 1(%rsi)",
    "jne  2f",
    "cmpb $67, 2(%rsi)",
    "jne  2f",
    "cmpb $73, 3(%rsi)",
    "jne  2f",
    "cmpb $73, 4(%rsi)",
    "jne  2f",
    "cmpb $0,  5(%rsi)",
    "jne  2f",
    // ASCII: tail-call create_utf8_converter(sret). RDI is already the sret
    // pointer and the callee takes nothing else.
    "movq SHIM_CREATE_UTF8_CONVERTER_PTR@GOTPCREL(%rip), %rax",
    "movq (%rax), %rax",
    "testq %rax, %rax",
    "jz   2f",
    "jmp  *%rax",
    "2:",
    "movq SHIM_CREATE_SIMPLE_CONVERTER_PTR@GOTPCREL(%rip), %rax",
    "movq (%rax), %rax",
    "testq %rax, %rax",
    "jz   3f",
    "jmp  *%rax",
    // Nothing resolved: hand back the caller's own result slot rather than
    // leaving RAX undefined. The caller destroys an empty unique_ptr.
    "3:",
    "movq %rdi, %rax",
    "ret",

    ".global _ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE",
    ".type   _ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE, @function",
    "_ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE:",
    "cmpb $65, 0(%rdx)",
    "jne  5f",
    "cmpb $83, 1(%rdx)",
    "jne  5f",
    "cmpb $67, 2(%rdx)",
    "jne  5f",
    "cmpb $73, 3(%rdx)",
    "jne  5f",
    "cmpb $73, 4(%rdx)",
    "jne  5f",
    "cmpb $0,  5(%rdx)",
    "jne  5f",
    "movq SHIM_CREATE_UTF8_CODECVT_PTR@GOTPCREL(%rip), %rax",
    "movq (%rax), %rax",
    "testq %rax, %rax",
    "jz   5f",
    // create_utf8_codecvt(sret, locale, facet): the facet moves down a
    // register now that the encoding is gone.
    "movq %rcx, %rdx",
    "jmp  *%rax",
    "5:",
    "movq SHIM_CREATE_SIMPLE_CODECVT_PTR@GOTPCREL(%rip), %rax",
    "movq (%rax), %rax",
    "testq %rax, %rax",
    "jz   6f",
    "jmp  *%rax",
    "6:",
    "movq %rdi, %rax",
    "ret",
    options(att_syntax),
);

#[cfg(all(feature = "interpose", target_arch = "aarch64"))]
std::arch::global_asm!(
    // Export the symbol so LD_PRELOAD interposition takes effect.
    ".global _ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE",
    ".type   _ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE, %function",
    "_ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE:",
    // ABI on entry:
    //   x8  = SRET pointer (output locale buffer allocated by caller)
    //   x0  = const std::locale& in    (input locale)
    //   x1  = const std::string& encoding
    //   x2  = char_facet_t type
    //   x30 = return address
    //
    // Stack frame layout (48 bytes, 16-byte aligned):
    //   [sp+0]  x29 (frame ptr)    [sp+8]  x30 (lr)
    //   [sp+16] x8  (SRET ptr)
    //   [sp+24] x0  (locale ptr)
    //   [sp+32] x1  (string ptr)   [sp+40] x2  (facet)
    "stp  x29, x30, [sp, #-48]!",
    "mov  x29, sp",
    "str  x8,  [sp, #16]",
    "str  x0,  [sp, #24]",
    "stp  x1,  x2,  [sp, #32]",
    // Check: is *x1 == 'A','S','C','I','I','\0'?
    "ldrb w9,  [x1]",
    "cmp  w9,  #65",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #1]",
    "cmp  w9,  #83",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #2]",
    "cmp  w9,  #67",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #3]",
    "cmp  w9,  #73",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #4]",
    "cmp  w9,  #73",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #5]",
    "cbnz w9,  .Lshim_csc_orig",
    // ASCII detected — GOT-indirect load of fn ptr, then tail-call
    // create_utf8_codecvt(locale, facet) with x8 = original SRET pointer.
    // We must use the GOT (:got: / :got_lo12:) because SHIM_* are exported
    // symbols; direct ADRP generates R_AARCH64_ADR_PREL_PG_HI21 which the
    // linker rejects in a PIC shared object.
    "adrp x9,  :got:SHIM_CREATE_UTF8_CODECVT_PTR",
    "ldr  x9,  [x9, :got_lo12:SHIM_CREATE_UTF8_CODECVT_PTR]",
    "ldr  x9,  [x9]",                     // x9 = fn-ptr value
    "cbz  x9,  .Lshim_csc_orig",          // if not resolved, fall through
    "ldr  x0,  [sp, #24]",                // locale ptr
    "ldr  x1,  [sp, #40]",                // facet (was x2)
    "ldr  x8,  [sp, #16]",                // restore SRET!
    "ldp  x29, x30, [sp], #48",
    "br   x9",                            // tail call
    ".Lshim_csc_orig:",
    // Not ASCII — tail-call original create_simple_codecvt unchanged.
    "adrp x9,  :got:SHIM_CREATE_SIMPLE_CODECVT_PTR",
    "ldr  x9,  [x9, :got_lo12:SHIM_CREATE_SIMPLE_CODECVT_PTR]",
    "ldr  x9,  [x9]",                     // x9 = fn-ptr value
    "cbz  x9,  .Lshim_csc_abort",
    "ldr  x0,  [sp, #24]",
    "ldr  x1,  [sp, #32]",
    "ldr  x2,  [sp, #40]",
    "ldr  x8,  [sp, #16]",                // restore SRET!
    "ldp  x29, x30, [sp], #48",
    "br   x9",
    ".Lshim_csc_abort:",
    // Resolver didn't run — hard abort.
    "bl   abort",
);

// ────────────────────────────────────────────────────────────────────────────
#[cfg(feature = "interpose")]
mod ld_preload_wrappers {
    use super::*;

    macro_rules! wrap_db_ret {
        ($name:ident, $ret:ty, $my:ident) => {
            #[no_mangle]
            pub extern "C" fn $name(db: *mut sqlite3) -> $ret {
                c_abi::$my(db)
            }
        };
    }

    macro_rules! wrap_stmt_ret {
        ($name:ident, $ret:ty, $my:ident) => {
            #[no_mangle]
            pub extern "C" fn $name(stmt: *mut sqlite3_stmt) -> $ret {
                c_abi::$my(stmt)
            }
        };
    }

    macro_rules! wrap_stmt_idx {
        ($name:ident, $ret:ty, $my:ident) => {
            #[no_mangle]
            pub extern "C" fn $name(stmt: *mut sqlite3_stmt, idx: c_int) -> $ret {
                c_abi::$my(stmt, idx)
            }
        };
    }

    macro_rules! wrap_val_ret {
        ($name:ident, $ret:ty, $my:ident) => {
            #[no_mangle]
            pub extern "C" fn $name(val: *mut sqlite3_value) -> $ret {
                c_abi::$my(val)
            }
        };
    }

    wrap_db_ret!(sqlite3_changes, c_int, my_sqlite3_changes);
    wrap_db_ret!(sqlite3_changes64, i64, my_sqlite3_changes64);
    wrap_db_ret!(sqlite3_last_insert_rowid, i64, my_sqlite3_last_insert_rowid);
    wrap_db_ret!(sqlite3_errmsg, *const c_char, my_sqlite3_errmsg);
    wrap_db_ret!(sqlite3_errcode, c_int, my_sqlite3_errcode);
    wrap_db_ret!(sqlite3_extended_errcode, c_int, my_sqlite3_extended_errcode);

    wrap_stmt_ret!(sqlite3_step, c_int, my_sqlite3_step);
    wrap_stmt_ret!(sqlite3_reset, c_int, my_sqlite3_reset);
    wrap_stmt_ret!(sqlite3_finalize, c_int, my_sqlite3_finalize);
    wrap_stmt_ret!(sqlite3_clear_bindings, c_int, my_sqlite3_clear_bindings);
    wrap_stmt_ret!(sqlite3_column_count, c_int, my_sqlite3_column_count);
    wrap_stmt_ret!(sqlite3_data_count, c_int, my_sqlite3_data_count);
    wrap_stmt_ret!(
        sqlite3_bind_parameter_count,
        c_int,
        my_sqlite3_bind_parameter_count
    );
    wrap_stmt_ret!(sqlite3_stmt_readonly, c_int, my_sqlite3_stmt_readonly);
    wrap_stmt_ret!(sqlite3_stmt_busy, c_int, my_sqlite3_stmt_busy);
    wrap_stmt_ret!(sqlite3_db_handle, *mut sqlite3, my_sqlite3_db_handle);
    wrap_stmt_ret!(sqlite3_expanded_sql, *mut c_char, my_sqlite3_expanded_sql);
    wrap_stmt_ret!(sqlite3_sql, *const c_char, my_sqlite3_sql);

    wrap_stmt_idx!(sqlite3_column_type, c_int, my_sqlite3_column_type);
    wrap_stmt_idx!(sqlite3_column_int, c_int, my_sqlite3_column_int);
    wrap_stmt_idx!(sqlite3_column_int64, i64, my_sqlite3_column_int64);
    wrap_stmt_idx!(sqlite3_column_double, f64, my_sqlite3_column_double);
    wrap_stmt_idx!(sqlite3_column_text, *const u8, my_sqlite3_column_text);
    wrap_stmt_idx!(sqlite3_column_blob, *const c_void, my_sqlite3_column_blob);
    wrap_stmt_idx!(sqlite3_column_bytes, c_int, my_sqlite3_column_bytes);
    wrap_stmt_idx!(sqlite3_column_name, *const c_char, my_sqlite3_column_name);
    wrap_stmt_idx!(
        sqlite3_column_value,
        *mut sqlite3_value,
        my_sqlite3_column_value
    );
    wrap_stmt_idx!(
        sqlite3_bind_parameter_name,
        *const c_char,
        my_sqlite3_bind_parameter_name
    );

    wrap_val_ret!(sqlite3_value_type, c_int, my_sqlite3_value_type);
    wrap_val_ret!(sqlite3_value_text, *const u8, my_sqlite3_value_text);
    wrap_val_ret!(sqlite3_value_int, c_int, my_sqlite3_value_int);
    wrap_val_ret!(sqlite3_value_int64, i64, my_sqlite3_value_int64);
    wrap_val_ret!(sqlite3_value_double, f64, my_sqlite3_value_double);
    wrap_val_ret!(sqlite3_value_bytes, c_int, my_sqlite3_value_bytes);
    wrap_val_ret!(sqlite3_value_blob, *const c_void, my_sqlite3_value_blob);

    #[no_mangle]
    pub extern "C" fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int {
        c_abi::my_sqlite3_open(filename, db)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_open_v2(
        filename: *const c_char,
        db: *mut *mut sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_open_v2(filename, db, flags, vfs)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_close(db: *mut sqlite3) -> c_int {
        c_abi::my_sqlite3_close(db)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_close_v2(db: *mut sqlite3) -> c_int {
        c_abi::my_sqlite3_close_v2(db)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        cb: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int {
        c_abi::my_sqlite3_exec(db, sql, cb, arg, errmsg)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_get_table(
        db: *mut sqlite3,
        sql: *const c_char,
        results: *mut *mut *mut c_char,
        nrow: *mut c_int,
        ncol: *mut c_int,
        errmsg: *mut *mut c_char,
    ) -> c_int {
        c_abi::my_sqlite3_get_table(db, sql, results, nrow, ncol, errmsg)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_prepare(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_prepare(db, sql, n, stmt, tail)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_prepare_v2(db, sql, n, stmt, tail)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_prepare_v3(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        flags: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_prepare_v3(db, sql, n, flags as u32, stmt, tail)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_prepare16_v2(
        db: *mut sqlite3,
        sql: *const c_void,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_void,
    ) -> c_int {
        c_abi::my_sqlite3_prepare16_v2(db, sql, n, stmt, tail)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_int(stmt: *mut sqlite3_stmt, idx: c_int, val: c_int) -> c_int {
        c_abi::my_sqlite3_bind_int(stmt, idx, val)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, idx: c_int, val: i64) -> c_int {
        c_abi::my_sqlite3_bind_int64(stmt, idx, val)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_double(stmt: *mut sqlite3_stmt, idx: c_int, val: f64) -> c_int {
        c_abi::my_sqlite3_bind_double(stmt, idx, val)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
        c_abi::my_sqlite3_bind_null(stmt, idx)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_text(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        n: c_int,
        dtor: *mut c_void,
    ) -> c_int {
        c_abi::my_sqlite3_bind_text(stmt, idx, val, n, dtor)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_text64(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        n: u64,
        dtor: *mut c_void,
        enc: u8,
    ) -> c_int {
        c_abi::my_sqlite3_bind_text64(stmt, idx, val, n, dtor, enc)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_blob(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_void,
        n: c_int,
        dtor: *mut c_void,
    ) -> c_int {
        c_abi::my_sqlite3_bind_blob(stmt, idx, val, n, dtor)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_blob64(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_void,
        n: u64,
        dtor: *mut c_void,
    ) -> c_int {
        c_abi::my_sqlite3_bind_blob64(stmt, idx, val, n, dtor)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_value(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const sqlite3_value,
    ) -> c_int {
        c_abi::my_sqlite3_bind_value(stmt, idx, val)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_parameter_index(
        stmt: *mut sqlite3_stmt,
        name: *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_bind_parameter_index(stmt, name)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_stmt_status(
        stmt: *mut sqlite3_stmt,
        op: c_int,
        reset: c_int,
    ) -> c_int {
        c_abi::my_sqlite3_stmt_status(stmt, op, reset)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_free(ptr: *mut c_void) {
        c_abi::my_sqlite3_free(ptr);
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_malloc(n: c_int) -> *mut c_void {
        c_abi::my_sqlite3_malloc(n)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_create_collation(
        db: *mut sqlite3,
        name: *const c_char,
        enc: c_int,
        arg: *mut c_void,
        cmp: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
        >,
    ) -> c_int {
        c_abi::my_sqlite3_create_collation(db, name, enc, arg, cmp)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_create_collation_v2(
        db: *mut sqlite3,
        name: *const c_char,
        enc: c_int,
        arg: *mut c_void,
        cmp: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
        >,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int {
        c_abi::my_sqlite3_create_collation_v2(db, name, enc, arg, cmp, destroy)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_column_decltype(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
    ) -> *const c_char {
        c_abi::my_sqlite3_column_decltype(stmt, idx)
    }

    // boost::locale::util::create_simple_converter is deliberately NOT
    // wrapped on x86-64.
    //
    // It returns std::unique_ptr<base_converter>, a class type, so the SysV
    // ABI passes a hidden return slot in RDI and the real argument -- the
    // encoding name, a const std::string& -- in RSI. A wrapper declared as
    // `extern "C" fn(*mut u8) -> *mut c_void` reads RDI, so it forwarded the
    // return slot to boost as if it were the encoding and dropped the name
    // entirely. Boost read whatever was in that memory, could make no sense of
    // it, and fell back to the one encoding its simple backend refuses:
    //
    //	boost::locale::conv::invalid_charset_error:
    //	  Invalid or unsupported charset:Invalid simple encoding ASCII
    //
    // Plex aborts there while loading its translations, a second or two after
    // its plug-ins come up, and never finishes starting.
    //
    // The wrapper described itself as a pass-through, which is precisely what
    // not interposing the symbol achieves -- correctly, and for every ABI. The
    // AArch64 asm hook below is a different mechanism and keeps its own
    // handling of the sret pointer in x8.
    // Note: create_simple_codecvt is implemented as a global_asm hook above
    // (AArch64 only) to correctly preserve the x8 SRET pointer while
    // redirecting ASCII charset requests to create_utf8_codecvt.
} // mod ld_preload_wrappers
