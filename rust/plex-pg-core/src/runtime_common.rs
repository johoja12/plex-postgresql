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

/// The name the process check reads from `/proc/self/cmdline`: the text
/// after the command line's last `/`, up to the next argument. That is
/// argv[0]'s base name unless an argument holds a `/`, which then names the
/// process: Plex Transcoder's manifest URL ends in
/// `manifest?X-Plex-Http-Pipeline=infinite`, and the wrapper Plex runs under,
/// `subreaper "/usr/lib/plexmediaserver/Plex Media Server"`, reads as Plex
/// Media Server (talos's is a static binary, which the shim never loads in).
/// Which processes argv[0] alone would change has not been looked into on
/// PMS, so the check reads what it always has.
pub(crate) fn process_label(cmdline: &[u8]) -> &[u8] {
    let mut label = cmdline;
    if let Some(pos) = cmdline.iter().rposition(|&b| b == b'/') {
        label = &cmdline[pos + 1..];
    }
    match label.iter().position(|&b| b == 0) {
        Some(pos) => &label[..pos],
        None => label,
    }
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
}
