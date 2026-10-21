use std::cell::UnsafeCell;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

use crate::db_interpose_common::stderr_ptr;
use crate::env_utils;
use crate::log_info_lazy;

pub(crate) fn should_skip_shim_init() -> bool {
    if cfg!(test) {
        return true;
    }
    env_utils::env_truthy(b"PLEX_PG_DISABLE_SHIM_INIT\0")
}

pub(crate) fn log_ctor_start(os_label: &str) {
    log_stderr_line(&format!(
        "[SHIM_INIT] Constructor starting ({})...",
        os_label
    ));
}

pub(crate) fn log_ctor_complete(os_label: &str, pid: i32) {
    log_stderr_line(&format!(
        "[SHIM_INIT] Constructor complete ({}, PID {})",
        os_label, pid
    ));
}

pub(crate) fn log_logging_initialized() {
    log_stderr_line("[SHIM_INIT] Logging initialized");
}

pub(crate) fn log_shim_loaded(os_label: &str) {
    log_info_lazy!(
        "=== Plex PostgreSQL Interpose Shim loaded ({}) ===",
        os_label
    );
}

pub(crate) fn log_shim_unloading(os_label: &str) {
    log_info_lazy!(
        "=== Plex PostgreSQL Interpose Shim unloading ({}) ===",
        os_label
    );
}

pub(crate) fn shim_init_common<F, G, H, K>(
    os_label: &str,
    pre: F,
    before_modules: G,
    after_modules: H,
    after_ready: K,
) where
    F: FnOnce() -> bool,
    G: FnOnce(),
    H: FnOnce(),
    K: FnOnce(),
{
    if should_skip_shim_init() {
        return;
    }

    log_ctor_start(os_label);

    if !pre() {
        return;
    }

    crate::pg_logging::pg_logging_init();
    log_shim_loaded(os_label);
    log_logging_initialized();

    before_modules();
    crate::db_interpose_common::common_shim_init_modules();
    after_modules();

    crate::db_interpose_common::SHIM_INITIALIZED.store(1, std::sync::atomic::Ordering::Release);

    after_ready();
    log_ctor_complete(os_label, unsafe { libc::getpid() });
}

fn log_stderr_line(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        unsafe {
            libc::fprintf(
                stderr_ptr(),
                b"%s\n\0".as_ptr() as *const c_char,
                cs.as_ptr(),
            );
            libc::fflush(stderr_ptr());
        }
    }
}

thread_local! {
    static IN_EXCEPTION_HANDLER: UnsafeCell<c_int> = UnsafeCell::new(0);
}

pub(crate) fn handle_exception_with_tls(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
) -> (c_int, c_int) {
    let mut should_call_original: c_int = 1;
    let handled = IN_EXCEPTION_HANDLER.with(|cell| {
        let guard = cell.get();
        crate::db_interpose_common::common_handle_exception(
            thrown_exception,
            tinfo,
            guard,
            &mut should_call_original,
        )
    });
    (handled, should_call_original)
}

/// The program's own name from `/proc/self/cmdline`: argv[0]'s base name.
///
/// Not the text after the last `/` of the whole command line, which is what
/// the shim read until 2026-10-20: an argument holding a slash named the
/// process after itself. Plex Transcoder logged as
/// `manifest?X-Plex-Http-Pipeline=infinite`, its manifest URL's tail, and a
/// Plex Media Scanner given a directory would have lost the shim and read
/// the empty SQLite shadow.
pub(crate) fn process_base_name(cmdline: &[u8]) -> &[u8] {
    let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or(&[]);
    match argv0.iter().rposition(|&b| b == b'/') {
        Some(pos) => &argv0[pos + 1..],
        None => argv0,
    }
}

#[cfg(test)]
mod process_name_tests {
    use super::process_base_name;

    #[test]
    fn a_process_is_named_by_its_own_program_not_an_argument() {
        let transcoder = b"/usr/lib/plexmediaserver/Plex Transcoder\0-codec:0\0h264\0http://127.0.0.1:32400/video/:/transcode/session/x/progress\0/transcode/Transcode/Sessions/plex-transcode-x/manifest?X-Plex-Http-Pipeline=infinite\0";
        assert_eq!(process_base_name(transcoder), b"Plex Transcoder");
        let scanner = b"/usr/lib/plexmediaserver/Plex Media Scanner\0--directory\0/media/movies/Heat (1995)\0";
        assert_eq!(process_base_name(scanner), b"Plex Media Scanner");
        assert_eq!(
            process_base_name(b"/usr/lib/plexmediaserver/Plex Media Server\0"),
            b"Plex Media Server"
        );
        assert_eq!(
            process_base_name(b"Plex Media Server\0"),
            b"Plex Media Server"
        );
        assert_eq!(process_base_name(b""), b"");
    }
}
