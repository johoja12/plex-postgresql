#![cfg(target_os = "linux")]

use std::borrow::Cow;
use std::ffi::{CStr, CString};
use std::mem;
use std::os::raw::{c_char, c_int};
use std::os::unix::ffi::OsStrExt;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

use crate::db_interpose_common::stderr_ptr;
use crate::env_utils;

type ExecveFn =
    unsafe extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int;
type ExecvpFn = unsafe extern "C" fn(*const c_char, *const *const c_char) -> c_int;
type ExecvpeFn =
    unsafe extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int;
type PosixSpawnFn = unsafe extern "C" fn(
    *mut libc::pid_t,
    *const c_char,
    *const libc::posix_spawn_file_actions_t,
    *const libc::posix_spawnattr_t,
    *const *const c_char,
    *const *const c_char,
) -> c_int;

unsafe extern "C" {
    static mut environ: *mut *mut c_char;
}

static mut ORIG_EXECVE: Option<ExecveFn> = None;
static mut ORIG_EXECVP: Option<ExecvpFn> = None;
static mut ORIG_EXECVPE: Option<ExecvpeFn> = None;
static mut ORIG_POSIX_SPAWN: Option<PosixSpawnFn> = None;
static mut ORIG_POSIX_SPAWNP: Option<PosixSpawnFn> = None;

static PMS_CHILD_ENV_SCRUB_ENABLED: AtomicI32 = AtomicI32::new(0);
static PMS_CHILD_ENV_SCRUB_LOG_BUDGET: AtomicI32 = AtomicI32::new(0);

static SELF_LD_PRELOAD: OnceLock<Option<CString>> = OnceLock::new();

/// `LD_PRELOAD=<the shim's own preload>`, built once at load and never freed,
/// for the one thing an `execve` in a vfork child may do about the scanner:
/// point at it (`exec_scanner_with_preload`). Null until
/// `scrub_current_process_preload` has taken the preload out of this
/// process's environment.
static SCANNER_PRELOAD_ENTRY: std::sync::atomic::AtomicPtr<c_char> =
    std::sync::atomic::AtomicPtr::new(ptr::null_mut());

/// The scanner's program name, as `execve`'s path ends.
const SCANNER_BASENAME: &[u8] = b"Plex Media Scanner";
/// The most environment entries the scanner's exec copies; Plex passes its
/// children some sixty. Past this the environment goes through as it came.
const SCANNER_ENV_MAX: usize = 510;

const DEFAULT_LOG_BUDGET: i32 = 16;
const _PRIVATE_LIB_DIR: &str = "/usr/local/lib/plex-postgresql";
const SHIM_SO_TOKEN: &str = "db_interpose_pg.so";
// Only the dedicated scanner binary needs shim reinjection in child execs.
// Reinjecting into self-spawned "Plex Media Server" children causes Plex to
// leave behind orphaned internal processes that keep 32400/32401 bound.
const KEEP_PROCESS_MARKERS: [&str; 1] = ["Plex Media Scanner"];
const LD_PRELOAD_ENV: &[u8] = b"LD_PRELOAD\0";

struct FilteredEnv {
    _storage: Vec<CString>,
    ptrs: Vec<*const c_char>,
    removed: usize,
    modified: usize,
    injected: usize,
}

impl FilteredEnv {
    fn changed(&self) -> bool {
        self.removed != 0 || self.modified != 0 || self.injected != 0
    }
}

fn is_enabled() -> bool {
    PMS_CHILD_ENV_SCRUB_ENABLED.load(Ordering::Acquire) != 0
}

/// The process the shim was loaded into, recorded once at startup.
///
/// Zero until `record_loader_pid` runs, which reads as "not us" and so errs
/// towards doing nothing.
static LOADER_PID: AtomicI32 = AtomicI32::new(0);

/// Records the process the shim is running in. Called from the constructor,
/// and after a fork the shim notices, so a genuinely forked child adopts its
/// own pid and keeps scrubbing.
pub fn record_loader_pid() {
    LOADER_PID.store(unsafe { libc::getpid() }, Ordering::Release);
}

/// Whether it is safe to build a filtered environment for a child.
///
/// It is not, once our pid has changed under us. `posix_spawn` is implemented
/// as `clone(CLONE_VM | CLONE_VFORK)` and then `execve` in the child, so our
/// `execve` wrapper can find itself running in a process that shares the
/// parent's address space while the parent's calling thread is suspended.
/// Only async-signal-safe work is allowed there. Allocating a new environment
/// takes the malloc lock and logging takes the stdio lock, either of which
/// another thread in the parent may already hold -- and Plex spawns its
/// plug-ins from a thread pool, so one usually does.
///
/// Stack is the harder limit. musl carves the clone child's stack out of the
/// caller's own frame -- about `1024 + PATH_MAX` bytes -- so anything that
/// reserves a Rust-sized frame in that child runs off the end of it and into
/// the parent's live frames. The parent then returns through a clobbered
/// address: the crash lands with a garbage instruction pointer well outside
/// every loaded module, which is what made it look like anything but this.
///
/// So callers must test this *before* calling anything with a real frame, not
/// inside it.
///
/// Passing the environment through unchanged is the whole of the fix: the
/// child is about to exec, and the shim has already removed `LD_PRELOAD` from
/// this process's own environment, so it does not follow the child anyway.
fn should_adjust_child_env() -> bool {
    if !is_enabled() {
        return false;
    }
    LOADER_PID.load(Ordering::Acquire) == unsafe { libc::getpid() }
}

pub fn configure_from_env() {
    let enabled = !env_utils::env_truthy(b"PLEX_PG_DISABLE_CHILD_ENV_SCRUB\0");
    PMS_CHILD_ENV_SCRUB_ENABLED.store(if enabled { 1 } else { 0 }, Ordering::Release);

    let budget = env_utils::env_usize(b"PLEX_PG_CHILD_ENV_SCRUB_LOG_LIMIT\0")
        .map(|v| v.min(i32::MAX as usize) as i32)
        .unwrap_or(DEFAULT_LOG_BUDGET)
        .max(0);
    PMS_CHILD_ENV_SCRUB_LOG_BUDGET.store(budget, Ordering::Release);
    record_loader_pid();

    unsafe {
        let _ = libc::fprintf(
            stderr_ptr(),
            if enabled {
                b"[SHIM_INIT] PMS child env scrub ENABLED (drop shim env for helper children; reinject only for scanner exec where needed)\n\0"
                    .as_ptr() as *const c_char
            } else {
                b"[SHIM_INIT] PMS child env scrub DISABLED via PLEX_PG_DISABLE_CHILD_ENV_SCRUB\n\0"
                    .as_ptr() as *const c_char
            },
        );
        let _ = libc::fflush(stderr_ptr());
    }
}

pub fn scrub_current_process_preload() {
    if !is_enabled() {
        return;
    }

    let had_preload = capture_self_ld_preload().is_some();
    unsafe {
        let raw = libc::getenv(LD_PRELOAD_ENV.as_ptr() as *const c_char);
        if raw.is_null() || *raw == 0 {
            return;
        }
        prepare_scanner_preload_entry();
        if libc::unsetenv(LD_PRELOAD_ENV.as_ptr() as *const c_char) == 0 && had_preload {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Removed LD_PRELOAD from current PMS process environment after shim load\n\0"
                    .as_ptr() as *const c_char,
            );
            let _ = libc::fflush(stderr_ptr());
        }
    }
}

/// Builds `SCANNER_PRELOAD_ENTRY` from the preload this process was started
/// with. Called at load, in the process the shim was loaded into, where
/// allocating is safe; the string is leaked on purpose, since a vfork child
/// may be pointing at it at any time.
fn prepare_scanner_preload_entry() {
    if !SCANNER_PRELOAD_ENTRY.load(Ordering::Acquire).is_null() {
        return;
    }
    let Some(preload) = capture_self_ld_preload() else {
        return;
    };
    let mut entry = b"LD_PRELOAD=".to_vec();
    entry.extend_from_slice(preload.as_bytes());
    let Ok(entry) = CString::new(entry) else {
        return;
    };
    let raw = entry.into_raw();
    if SCANNER_PRELOAD_ENTRY
        .compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Another caller published first: this copy was never shared.
        drop(unsafe { CString::from_raw(raw) });
    }
}

/// Whether `path` ends in the scanner's program name. Async-signal-safe and
/// frameless: it runs in a vfork child (`execve`).
///
/// # Safety
/// `path` is null or a NUL-terminated string.
#[inline(always)]
unsafe fn path_is_scanner(path: *const c_char) -> bool {
    if path.is_null() {
        return false;
    }
    let mut base = path;
    let mut cursor = path;
    while *cursor != 0 {
        if *cursor as u8 == b'/' {
            base = cursor.add(1);
        }
        cursor = cursor.add(1);
    }
    let mut i = 0;
    while i < SCANNER_BASENAME.len() {
        if *base.add(i) as u8 != SCANNER_BASENAME[i] {
            return false;
        }
        i += 1;
    }
    *base.add(i) == 0
}

/// Whether an environment names `LD_PRELOAD`. Async-signal-safe and
/// frameless, as `path_is_scanner`.
///
/// # Safety
/// `envp` is null or a null-terminated array of NUL-terminated strings.
#[inline(always)]
unsafe fn env_names_preload(envp: *const *const c_char) -> bool {
    const KEY: &[u8] = b"LD_PRELOAD=";
    let mut cursor = envp;
    while !cursor.is_null() && !(*cursor).is_null() {
        let entry = *cursor;
        let mut i = 0;
        while i < KEY.len() && *entry.add(i) as u8 == KEY[i] {
            i += 1;
        }
        if i == KEY.len() {
            return true;
        }
        cursor = cursor.add(1);
    }
    false
}

/// Copies `envp`'s pointers into `out` and adds `entry` and the terminator.
/// False when `out` cannot hold them all; `out` is then not to be used.
///
/// # Safety
/// `envp` is null or a null-terminated array.
#[inline(always)]
unsafe fn env_with_entry(
    envp: *const *const c_char,
    entry: *const c_char,
    out: &mut [*const c_char],
) -> bool {
    let mut n = 0;
    let mut cursor = envp;
    while !cursor.is_null() && !(*cursor).is_null() {
        if n + 2 >= out.len() {
            return false;
        }
        out[n] = *cursor;
        n += 1;
        cursor = cursor.add(1);
    }
    if n + 2 > out.len() {
        return false;
    }
    out[n] = entry;
    out[n + 1] = ptr::null();
    true
}

/// `execve` of the scanner from a vfork child, with the shim's preload added
/// to the environment Plex built for it.
///
/// PMS starts every job with `vfork` and `execve` (it imports no
/// `posix_spawn`), so the wrapper below runs in the child, where it may not
/// build an environment (`should_adjust_child_env`) -- and since the shim
/// takes `LD_PRELOAD` out of PMS's own environment at load, the scanner was
/// started without the shim: `Plex Media Scanner --analyze` and `--match`
/// opened the empty shadow SQLite file, found nothing to do and exited 0 in
/// 80 ms. No scanned file was ever analysed (no duration, no streams) and
/// every match job ended in "Scanner: error reading match results" (the
/// upgrade rig, 2026-11-16, PMS 1.43.5.11029 and 1.43.5.11079 alike; a stock
/// server analyses and matches). It has been so since the wrapper stopped
/// building environments in the child (2026-09-18).
///
/// Nothing here allocates, locks or logs: the pointers are copied into an
/// array in this frame, 4 KB of it, which a vfork child has (it runs on its
/// parent's stack). Kept out of line so that `execve`'s own frame stays
/// small for musl's `posix_spawn` child, which has about 5 KB in all and
/// never comes here: its parent's `posix_spawn` wrapper has added the
/// preload already.
///
/// # Safety
/// As `execve`.
#[inline(never)]
unsafe fn exec_scanner_with_preload(
    orig: ExecveFn,
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
    entry: *const c_char,
) -> c_int {
    let mut env: [*const c_char; SCANNER_ENV_MAX + 2] = [ptr::null(); SCANNER_ENV_MAX + 2];
    if !env_with_entry(envp, entry, &mut env) {
        return orig(path, argv, envp);
    }
    orig(path, argv, env.as_ptr())
}

/// Resolve a single symbol via `dlsym(RTLD_NEXT, ...)` and store it into the
/// provided slot.  Returns `true` if the symbol was found.
#[allow(static_mut_refs)]
unsafe fn resolve_symbol_into<T: Copy>(slot: *mut Option<T>, name: &'static [u8]) -> bool {
    let sym = libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const c_char);
    if sym.is_null() {
        return false;
    }
    let f: T = mem::transmute_copy(&sym);
    ptr::write(slot, Some(f));
    true
}

/// Eagerly resolve all interposition hooks.  Called once from the shim
/// constructor (`shim_init_wrapper`) so that the interposition wrappers below
/// never race on a lazy write.
///
/// # Safety
/// Must be called once during shim init (single-threaded `.init_array` context).
#[allow(static_mut_refs)]
pub unsafe fn init_child_env_hooks() {
    resolve_symbol_into(ptr::addr_of_mut!(ORIG_EXECVE), b"execve\0");
    resolve_symbol_into(ptr::addr_of_mut!(ORIG_EXECVP), b"execvp\0");
    resolve_symbol_into(ptr::addr_of_mut!(ORIG_EXECVPE), b"execvpe\0");
    resolve_symbol_into(ptr::addr_of_mut!(ORIG_POSIX_SPAWN), b"posix_spawn\0");
    resolve_symbol_into(ptr::addr_of_mut!(ORIG_POSIX_SPAWNP), b"posix_spawnp\0");
}

#[allow(static_mut_refs)]
unsafe fn read_execve() -> Option<ExecveFn> {
    ptr::read(ptr::addr_of!(ORIG_EXECVE))
}

#[allow(static_mut_refs)]
unsafe fn read_execvp() -> Option<ExecvpFn> {
    ptr::read(ptr::addr_of!(ORIG_EXECVP))
}

#[allow(static_mut_refs)]
unsafe fn read_execvpe() -> Option<ExecvpeFn> {
    ptr::read(ptr::addr_of!(ORIG_EXECVPE))
}

#[allow(static_mut_refs)]
unsafe fn read_posix_spawn() -> Option<PosixSpawnFn> {
    ptr::read(ptr::addr_of!(ORIG_POSIX_SPAWN))
}

#[allow(static_mut_refs)]
unsafe fn read_posix_spawnp() -> Option<PosixSpawnFn> {
    ptr::read(ptr::addr_of!(ORIG_POSIX_SPAWNP))
}

unsafe fn set_errno(err: c_int) {
    *libc::__errno_location() = err;
}

fn basename(input: &str) -> &str {
    input.rsplit('/').next().unwrap_or(input)
}

fn process_label_from_parts(path: Option<&str>, argv0: Option<&str>) -> String {
    let path_base = path.map(basename).filter(|s| !s.is_empty());
    let argv_base = argv0.map(basename).filter(|s| !s.is_empty());
    path_base.or(argv_base).unwrap_or_default().to_string()
}

unsafe fn cstr_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
}

unsafe fn process_label(path: *const c_char, argv: *const *const c_char) -> String {
    let argv0 = if argv.is_null() || (*argv).is_null() {
        None
    } else {
        cstr_to_string(*argv)
    };
    process_label_from_parts(cstr_to_string(path).as_deref(), argv0.as_deref())
}

fn should_keep_env_for_process(label: &str) -> bool {
    KEEP_PROCESS_MARKERS
        .iter()
        .any(|marker| label.contains(marker))
}

unsafe fn current_ld_preload() -> Option<CString> {
    let raw = libc::getenv(LD_PRELOAD_ENV.as_ptr() as *const c_char);
    if raw.is_null() || *raw == 0 {
        return None;
    }
    Some(CStr::from_ptr(raw).to_owned())
}

fn capture_self_ld_preload() -> Option<&'static CString> {
    SELF_LD_PRELOAD
        .get_or_init(|| unsafe { current_ld_preload() })
        .as_ref()
}

fn sanitize_colon_list<F>(value: &str, keep: F) -> Option<String>
where
    F: Fn(&str) -> bool,
{
    let kept: Vec<&str> = value
        .split(':')
        .filter(|item| !item.is_empty() && keep(item))
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join(":"))
    }
}

fn is_ld_preload_entry(entry: &str) -> bool {
    entry
        .split_once('=')
        .is_some_and(|(key, _)| key == "LD_PRELOAD")
}

fn is_helper_locale_env(key: &str) -> bool {
    key == "LANG" || key == "LANGUAGE" || key == "CHARSET" || key.starts_with("LC_")
}

fn rewrite_env_entry(entry: &str) -> Option<Cow<'_, str>> {
    let (key, value) = entry.split_once('=')?;

    if key == "LD_PRELOAD" {
        return sanitize_colon_list(value, |item| !item.contains(SHIM_SO_TOKEN))
            .map(|filtered| Cow::Owned(format!("{key}={filtered}")));
    }

    if key == "LD_LIBRARY_PATH" {
        let _ = value;
        return None;
    }

    if key.starts_with("PLEX_PG_") {
        return None;
    }

    if is_helper_locale_env(key) {
        return None;
    }

    Some(Cow::Borrowed(entry))
}

unsafe fn source_envp(envp: *const *const c_char) -> *const *const c_char {
    if envp.is_null() {
        environ as *const *const c_char
    } else {
        envp
    }
}

unsafe fn collect_env_entries(envp: *const *const c_char) -> Vec<String> {
    let mut entries = Vec::new();
    let mut cursor = source_envp(envp);
    while !cursor.is_null() && !(*cursor).is_null() {
        entries.push(CStr::from_ptr(*cursor).to_string_lossy().into_owned());
        cursor = cursor.add(1);
    }
    entries
}

fn inject_ld_preload_entry(entries: &[String], preload: &str) -> Option<Vec<String>> {
    if entries.iter().any(|entry| is_ld_preload_entry(entry)) {
        return None;
    }

    let mut rewritten = entries.to_vec();
    rewritten.push(format!("LD_PRELOAD={preload}"));
    Some(rewritten)
}

fn build_cstring_env(entries: &[String]) -> Option<(Vec<CString>, Vec<*const c_char>)> {
    let mut storage = Vec::with_capacity(entries.len());
    for entry in entries {
        let Ok(cstring) = CString::new(entry.as_str()) else {
            return None;
        };
        storage.push(cstring);
    }

    let mut ptrs = Vec::with_capacity(storage.len() + 1);
    ptrs.extend(storage.iter().map(|item| item.as_ptr()));
    ptrs.push(ptr::null());
    Some((storage, ptrs))
}

fn build_cstring_argv(cmdline: &[u8]) -> Option<(Vec<CString>, Vec<*const c_char>)> {
    let storage: Vec<CString> = cmdline
        .split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| CString::new(arg.to_vec()))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    if storage.is_empty() {
        return None;
    }

    let mut ptrs = Vec::with_capacity(storage.len() + 1);
    ptrs.extend(storage.iter().map(|item| item.as_ptr()));
    ptrs.push(ptr::null());
    Some((storage, ptrs))
}

unsafe fn build_scrubbed_env(envp: *const *const c_char) -> Option<FilteredEnv> {
    let entries = collect_env_entries(envp);
    let mut rewritten = Vec::with_capacity(entries.len());
    let mut removed = 0usize;
    let mut modified = 0usize;

    for entry in &entries {
        match rewrite_env_entry(entry) {
            Some(value) => {
                if value.as_ref() != entry {
                    modified += 1;
                }
                rewritten.push(value.into_owned());
            }
            None => removed += 1,
        }
    }

    if removed == 0 && modified == 0 {
        return None;
    }

    let (storage, ptrs) = build_cstring_env(&rewritten)?;
    Some(FilteredEnv {
        _storage: storage,
        ptrs,
        removed,
        modified,
        injected: 0,
    })
}

unsafe fn build_kept_env(envp: *const *const c_char) -> Option<FilteredEnv> {
    let preload = capture_self_ld_preload()?;
    let entries = collect_env_entries(envp);
    let rewritten = inject_ld_preload_entry(&entries, &preload.to_string_lossy())?;
    let (storage, ptrs) = build_cstring_env(&rewritten)?;
    Some(FilteredEnv {
        _storage: storage,
        ptrs,
        removed: 0,
        modified: 0,
        injected: 1,
    })
}

unsafe fn maybe_log_adjustment(
    label: &str,
    op: &'static [u8],
    removed: usize,
    modified: usize,
    injected: usize,
) {
    if removed == 0 && modified == 0 && injected == 0 {
        return;
    }

    let budget = PMS_CHILD_ENV_SCRUB_LOG_BUDGET.load(Ordering::Relaxed);
    if budget <= 0 {
        return;
    }
    PMS_CHILD_ENV_SCRUB_LOG_BUDGET.fetch_sub(1, Ordering::Relaxed);

    let label_c = CString::new(label).unwrap_or_else(|_| CString::new("").unwrap());
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[PMS_CHILD_ENV] adjusted env for %s -> %s (removed=%zu modified=%zu injected=%zu)\n\0"
            .as_ptr() as *const c_char,
        op.as_ptr() as *const c_char,
        label_c.as_ptr(),
        removed,
        modified,
        injected,
    );
    let _ = libc::fflush(stderr_ptr());
}

unsafe fn adjusted_env_for_process(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> Option<(String, FilteredEnv)> {
    if !should_adjust_child_env() {
        return None;
    }
    if !is_enabled() {
        return None;
    }

    let label = process_label(path, argv);
    let filtered = if should_keep_env_for_process(&label) {
        build_kept_env(envp)
    } else {
        build_scrubbed_env(envp)
    }?;

    if filtered.changed() {
        Some((label, filtered))
    } else {
        None
    }
}

pub fn maybe_reexec_current_process_without_shim(label: &str, cmdline: &[u8]) {
    if !is_enabled() || should_keep_env_for_process(label) {
        return;
    }

    unsafe {
        let Some(filtered) = build_scrubbed_env(ptr::null()) else {
            return;
        };
        let Some((argv_storage, argv_ptrs)) = build_cstring_argv(cmdline) else {
            return;
        };
        let Ok(exe_path) = std::fs::read_link("/proc/self/exe") else {
            return;
        };
        let Ok(exe_c) = CString::new(exe_path.as_os_str().as_bytes()) else {
            return;
        };
        let Some(orig_execve) = read_execve() else {
            return;
        };

        maybe_log_adjustment(
            label,
            b"reexec\0",
            filtered.removed,
            filtered.modified,
            filtered.injected,
        );

        let label_c = CString::new(label).unwrap_or_else(|_| CString::new("").unwrap());
        let _ = libc::fprintf(
            stderr_ptr(),
            b"[PMS_CHILD_ENV] re-execing helper without shim env: %s (pid=%d argc=%zu)\n\0".as_ptr()
                as *const c_char,
            label_c.as_ptr(),
            libc::getpid(),
            argv_storage.len(),
        );
        let _ = libc::fflush(stderr_ptr());

        orig_execve(exe_c.as_ptr(), argv_ptrs.as_ptr(), filtered.ptrs.as_ptr());

        let err = *libc::__errno_location();
        let _ = libc::fprintf(
            stderr_ptr(),
            b"[PMS_CHILD_ENV] WARNING: helper re-exec failed for %s (pid=%d errno=%d)\n\0".as_ptr()
                as *const c_char,
            label_c.as_ptr(),
            libc::getpid(),
            err,
        );
        let _ = libc::fflush(stderr_ptr());
    }
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `execve`. Callers must provide valid libc
/// pointers for path/argv/envp.
pub unsafe extern "C" fn execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let Some(orig) = read_execve() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    // Before anything with a frame: this may be a clone(CLONE_VM) child
    // running on a few KB carved out of the parent's stack.
    if !should_adjust_child_env() {
        // The scanner alone keeps the shim, and Plex starts it from a vfork
        // child, which is here.
        if path_is_scanner(path) {
            let entry = SCANNER_PRELOAD_ENTRY.load(Ordering::Acquire);
            let source = source_envp(envp);
            if !entry.is_null() && !env_names_preload(source) {
                return exec_scanner_with_preload(orig, path, argv, source, entry);
            }
        }
        return orig(path, argv, envp);
    }

    if let Some((label, filtered)) = adjusted_env_for_process(path, argv, envp) {
        maybe_log_adjustment(
            &label,
            b"execve\0",
            filtered.removed,
            filtered.modified,
            filtered.injected,
        );
        return orig(path, argv, filtered.ptrs.as_ptr());
    }

    orig(path, argv, envp)
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `execvp`. Callers must provide valid libc
/// pointers for file/argv.
pub unsafe extern "C" fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    let Some(orig_execvp) = read_execvp() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    let Some((label, filtered)) = adjusted_env_for_process(file, argv, ptr::null()) else {
        return orig_execvp(file, argv);
    };

    let Some(orig_execvpe) = read_execvpe() else {
        return orig_execvp(file, argv);
    };

    maybe_log_adjustment(
        &label,
        b"execvp\0",
        filtered.removed,
        filtered.modified,
        filtered.injected,
    );
    orig_execvpe(file, argv, filtered.ptrs.as_ptr())
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `execvpe`. Callers must provide valid libc
/// pointers for file/argv/envp.
pub unsafe extern "C" fn execvpe(
    file: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let Some(orig) = read_execvpe() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    // Before anything with a frame: this may be a clone(CLONE_VM) child
    // running on a few KB carved out of the parent's stack.
    if !should_adjust_child_env() {
        return orig(file, argv, envp);
    }

    if let Some((label, filtered)) = adjusted_env_for_process(file, argv, envp) {
        maybe_log_adjustment(
            &label,
            b"execvpe\0",
            filtered.removed,
            filtered.modified,
            filtered.injected,
        );
        return orig(file, argv, filtered.ptrs.as_ptr());
    }

    orig(file, argv, envp)
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `posix_spawn`. Callers must obey libc
/// preconditions for the provided pointers.
pub unsafe extern "C" fn posix_spawn(
    pid: *mut libc::pid_t,
    path: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attrp: *const libc::posix_spawnattr_t,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let Some(orig) = read_posix_spawn() else {
        return libc::ENOSYS;
    };

    if let Some((label, filtered)) = adjusted_env_for_process(path, argv, envp) {
        maybe_log_adjustment(
            &label,
            b"posix_spawn\0",
            filtered.removed,
            filtered.modified,
            filtered.injected,
        );
        return orig(pid, path, file_actions, attrp, argv, filtered.ptrs.as_ptr());
    }

    orig(pid, path, file_actions, attrp, argv, envp)
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `posix_spawnp`. Callers must obey libc
/// preconditions for the provided pointers.
pub unsafe extern "C" fn posix_spawnp(
    pid: *mut libc::pid_t,
    file: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attrp: *const libc::posix_spawnattr_t,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let Some(orig) = read_posix_spawnp() else {
        return libc::ENOSYS;
    };

    if let Some((label, filtered)) = adjusted_env_for_process(file, argv, envp) {
        maybe_log_adjustment(
            &label,
            b"posix_spawnp\0",
            filtered.removed,
            filtered.modified,
            filtered.injected,
        );
        return orig(pid, file, file_actions, attrp, argv, filtered.ptrs.as_ptr());
    }

    orig(pid, file, file_actions, attrp, argv, envp)
}

#[cfg(test)]
mod tests {
    use super::{
        configure_from_env, inject_ld_preload_entry, process_label_from_parts, record_loader_pid,
        rewrite_env_entry, should_adjust_child_env, should_keep_env_for_process, LOADER_PID,
    };
    use std::sync::atomic::Ordering;

    #[test]
    fn the_scanner_is_told_by_its_paths_last_component() {
        use super::path_is_scanner;
        use std::ffi::CString;
        let is = |p: &str| {
            let c = CString::new(p).unwrap();
            unsafe { path_is_scanner(c.as_ptr()) }
        };
        assert!(is("/usr/lib/plexmediaserver/Plex Media Scanner"));
        assert!(is("Plex Media Scanner"));
        assert!(!is("/usr/lib/plexmediaserver/Plex Media Server"));
        assert!(!is("/usr/lib/plexmediaserver/Plex Media Scanner Helper"));
        assert!(!is("/usr/lib/plexmediaserver/Plex Media Scanner/ffmpeg"));
        assert!(!is("/usr/lib/plexmediaserver/Plex Media Scan"));
        assert!(!is("/usr/lib/plexmediaserver/Plex Transcoder"));
        assert!(!is(""));
        assert!(!unsafe { path_is_scanner(std::ptr::null()) });
    }

    #[test]
    fn a_vfork_childs_scanner_environment_gains_the_preload_and_nothing_else() {
        use super::{env_names_preload, env_with_entry};
        use std::ffi::{CStr, CString};
        use std::os::raw::c_char;
        use std::ptr;

        let owned: Vec<CString> = ["PATH=/usr/bin", "X_PLEX_TOKEN=t", "LD_LIBRARY_PATH=/a:/b"]
            .iter()
            .map(|e| CString::new(*e).unwrap())
            .collect();
        let mut envp: Vec<*const c_char> = owned.iter().map(|e| e.as_ptr()).collect();
        envp.push(ptr::null());
        let entry =
            CString::new("LD_PRELOAD=/usr/local/lib/plex-postgresql/db_interpose_pg.so").unwrap();

        assert!(!unsafe { env_names_preload(envp.as_ptr()) });
        assert!(!unsafe { env_names_preload(ptr::null()) });

        let mut out: [*const c_char; 8] = [ptr::null(); 8];
        assert!(unsafe { env_with_entry(envp.as_ptr(), entry.as_ptr(), &mut out) });
        let got: Vec<String> = out
            .iter()
            .take_while(|p| !p.is_null())
            .map(|p| unsafe { CStr::from_ptr(*p) }.to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            got,
            [
                "PATH=/usr/bin",
                "X_PLEX_TOKEN=t",
                "LD_LIBRARY_PATH=/a:/b",
                "LD_PRELOAD=/usr/local/lib/plex-postgresql/db_interpose_pg.so"
            ]
        );
        // The entries are Plex's own strings, pointed at and never copied.
        assert_eq!(out[0], owned[0].as_ptr());
        assert!(unsafe { env_names_preload(out.as_ptr()) });

        // An environment that already names a preload is told apart, and a
        // look-alike key is not one.
        let other = CString::new("LD_PRELOAD_EXTRA=1").unwrap();
        let look_alike = [other.as_ptr(), ptr::null()];
        assert!(!unsafe { env_names_preload(look_alike.as_ptr()) });

        // No room for the entry and the terminator: refused, so the caller
        // passes Plex's environment through as it came.
        let mut exact: [*const c_char; 4] = [ptr::null(); 4];
        assert!(!unsafe { env_with_entry(envp.as_ptr(), entry.as_ptr(), &mut exact) });
        let mut fits: [*const c_char; 5] = [ptr::null(); 5];
        assert!(unsafe { env_with_entry(envp.as_ptr(), entry.as_ptr(), &mut fits) });
        assert!(fits[4].is_null());
        // An empty environment still gets the entry.
        let mut small: [*const c_char; 2] = [ptr::null(); 2];
        assert!(unsafe { env_with_entry(ptr::null(), entry.as_ptr(), &mut small) });
        assert_eq!(small[0], entry.as_ptr());
    }

    #[test]
    fn keep_process_whitelist_is_narrow() {
        assert!(should_keep_env_for_process("Plex Media Scanner"));
        assert!(!should_keep_env_for_process("Plex Media Server"));
        assert!(!should_keep_env_for_process("Shared"));
        assert!(!should_keep_env_for_process("System.bundle"));
    }

    #[test]
    fn process_label_prefers_path_basename() {
        assert_eq!(
            process_label_from_parts(Some("/usr/lib/plexmediaserver/Plex Media Server"), None),
            "Plex Media Server"
        );
        assert_eq!(
            process_label_from_parts(None, Some("/usr/lib/plexmediaserver/Shared")),
            "Shared"
        );
    }

    #[test]
    fn rewrite_env_entry_removes_shim_preload_and_private_env() {
        assert_eq!(
            rewrite_env_entry("LD_PRELOAD=/foo/db_interpose_pg.so:/tmp/other.so")
                .map(|v| v.into_owned()),
            Some("LD_PRELOAD=/tmp/other.so".to_string())
        );
        assert_eq!(rewrite_env_entry("PLEX_PG_LOG_LEVEL=DEBUG"), None);
        assert_eq!(rewrite_env_entry("LANG=en_US.UTF-8"), None);
        assert_eq!(rewrite_env_entry("LANGUAGE=en_US.UTF-8"), None);
        assert_eq!(rewrite_env_entry("LC_ALL=en_US.UTF-8"), None);
        assert_eq!(rewrite_env_entry("CHARSET=UTF-8"), None);
        assert_eq!(
            rewrite_env_entry("PATH=/usr/bin").map(|v| v.into_owned()),
            Some("PATH=/usr/bin".to_string())
        );
    }

    #[test]
    fn rewrite_env_entry_strips_private_library_path_only() {
        assert_eq!(
            rewrite_env_entry(
                "LD_LIBRARY_PATH=/usr/local/lib/plex-postgresql:/usr/lib/plexmediaserver/lib"
            )
            .map(|v| v.into_owned()),
            None
        );
        assert_eq!(
            rewrite_env_entry("LD_LIBRARY_PATH=/usr/lib/plexmediaserver/lib")
                .map(|v| v.into_owned()),
            None
        );
    }

    #[test]
    fn inject_preload_only_when_missing() {
        let base = vec![
            String::from("LANG=en_US.UTF-8"),
            String::from("PATH=/usr/bin"),
        ];
        assert_eq!(
            inject_ld_preload_entry(&base, "/shim/db_interpose_pg.so"),
            Some(vec![
                String::from("LANG=en_US.UTF-8"),
                String::from("PATH=/usr/bin"),
                String::from("LD_PRELOAD=/shim/db_interpose_pg.so")
            ])
        );

        let with_preload = vec![String::from("LD_PRELOAD=/already/set.so")];
        assert_eq!(
            inject_ld_preload_entry(&with_preload, "/shim/db_interpose_pg.so"),
            None
        );
    }

    /// Plex starts its plug-ins with `posix_spawn`, which glibc implements as
    /// `clone(CLONE_VM | CLONE_VFORK)` followed by `execve` in the child. That
    /// child shares the parent's address space and runs while the calling
    /// thread is suspended, so only async-signal-safe work is allowed in it:
    /// allocating, or taking the stdio lock to log, can deadlock against a
    /// lock another thread in the parent already holds, and corrupts the
    /// parent when it resumes.
    ///
    /// Our interposed `execve` did both -- it built a filtered environment and
    /// logged a line about it. Plex's plug-in manager thread then died in the
    /// middle of every spawn: the plug-in process itself appeared and ran, Plex
    /// logged nothing after "Plugin: setting environment variable:
    /// 'PYTHONPATH=...'", no plug-in ever reported its port, and the server
    /// answered 503 for ever with a crash report written at that moment.
    ///
    /// So the wrapper has to notice that it is no longer the process the shim
    /// was loaded into, and get out of the way.
    #[test]
    fn env_adjustment_is_skipped_once_the_pid_is_not_the_one_the_shim_loaded_into() {
        configure_from_env();
        record_loader_pid();

        assert!(
            should_adjust_child_env(),
            "the loading process itself must still scrub its children"
        );

        // Stands in for the vfork child: same memory, different pid.
        let real = LOADER_PID.swap(unsafe { libc::getpid() } + 1, Ordering::AcqRel);

        assert!(
            !should_adjust_child_env(),
            "a process that is not the one the shim loaded into may be sharing \
             its parent's memory, and must do nothing but exec"
        );

        LOADER_PID.store(real, Ordering::Release);
        assert!(should_adjust_child_env());
    }
}
