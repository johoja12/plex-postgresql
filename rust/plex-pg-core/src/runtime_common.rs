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

/// The name the process check reads from `/proc/self/cmdline`.
///
/// argv[0]'s base name, when that is one of the two programs the shim
/// serves, Plex Media Server and Plex Media Scanner. Otherwise the text
/// after the command line's last `/`, up to the next argument, which is
/// what every release read for every process: argv[0]'s base name unless an
/// argument holds a `/`, which then names the process. Plex Transcoder's
/// manifest URL ends in `manifest?X-Plex-Http-Pipeline=infinite`, and the
/// wrapper Plex runs under, `subreaper "/usr/lib/plexmediaserver/Plex Media
/// Server"`, reads as Plex Media Server, so the wrapper keeps the shim's
/// environment for the server it starts (talos's is a static binary, which
/// the shim never loads in).
///
/// The last-`/` rule alone named the scanner by its arguments. PMS matches
/// a new item with `Plex Media Scanner --match --type 1 --item <id>
/// --content-type=application/json`, which read as `json`: the shim stood
/// aside, the matcher opened the empty shadow SQLite file, answered
/// `{"MediaContainer":{"size":0}}`, and PMS logged "Scanner: error reading
/// match results" and tried again every 30 s, for every item (the upgrade
/// rig, 2026-11-16, PMS 1.43.5.11029 and 1.43.5.11079 alike: nine of nine
/// match jobs, none on a stock server).
pub(crate) fn process_label(cmdline: &[u8]) -> &[u8] {
    let argv0 = match cmdline.iter().position(|&b| b == 0) {
        Some(pos) => &cmdline[..pos],
        None => cmdline,
    };
    let argv0_base = match argv0.iter().rposition(|&b| b == b'/') {
        Some(pos) => &argv0[pos + 1..],
        None => argv0,
    };
    if contains_bytes(argv0_base, b"Plex Media Server")
        || contains_bytes(argv0_base, b"Plex Media Scanner")
    {
        return argv0_base;
    }

    let mut label = cmdline;
    if let Some(pos) = cmdline.iter().rposition(|&b| b == b'/') {
        label = &cmdline[pos + 1..];
    }
    match label.iter().position(|&b| b == 0) {
        Some(pos) => &label[..pos],
        None => label,
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod process_label_tests {
    use super::process_label;

    #[test]
    fn a_process_is_named_by_the_text_after_its_command_lines_last_slash() {
        assert_eq!(
            process_label(
                b"/usr/local/bin/subreaper\0/usr/lib/plexmediaserver/Plex Media Server\0"
            ),
            b"Plex Media Server"
        );
        assert_eq!(
            process_label(b"/usr/lib/plexmediaserver/Plex Media Server\0"),
            b"Plex Media Server"
        );
        assert_eq!(process_label(b"Plex Media Server\0"), b"Plex Media Server");
        assert_eq!(
            process_label(b"/usr/lib/plexmediaserver/Plex Tuner Service\0--port\0"),
            b"Plex Tuner Service"
        );
        assert_eq!(process_label(b""), b"");
    }

    #[test]
    fn the_scanner_and_the_server_are_named_by_argv0_whatever_their_arguments_hold() {
        // PMS's match job: the last "/" is inside --content-type.
        assert_eq!(
            process_label(
                b"/usr/lib/plexmediaserver/Plex Media Scanner\0--match\0--type\x001\0--log-file-suffix\0 Matcher\0--item\x0017092\0--content-type=application/json\0"
            ),
            b"Plex Media Scanner"
        );
        assert_eq!(
            process_label(
                b"/usr/lib/plexmediaserver/Plex Media Scanner\0--scan\0--directory\0/media/tv/Breaking Bad (2008)\0"
            ),
            b"Plex Media Scanner"
        );
        assert_eq!(
            process_label(b"Plex Media Server\0--some-url=http://127.0.0.1:32400/x\0"),
            b"Plex Media Server"
        );
    }

    #[test]
    fn every_other_process_reads_as_it_always_has() {
        // The transcoder is named by its manifest URL, and stays unshimmed.
        assert_eq!(
            process_label(
                b"/usr/lib/plexmediaserver/Plex Transcoder\0-i\0http://127.0.0.1:32400/video/:/transcode/session/x/manifest?X-Plex-Http-Pipeline=infinite\0"
            ),
            b"manifest?X-Plex-Http-Pipeline=infinite"
        );
        // A helper whose argument names the server's path reads as the
        // server: the wrapper keeps the shim's environment for its child.
        assert_eq!(
            process_label(b"subreaper\0/usr/lib/plexmediaserver/Plex Media Server\0"),
            b"Plex Media Server"
        );
    }
}
