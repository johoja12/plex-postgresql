//! The guide-load swap hook: a Live TV guide load is copied into its
//! PostgreSQL schema before PMS's rename makes it the guide (cluster-plex's
//! `docs/superpowers/specs/2026-10-21-guide-loads-copied-at-rename-design.md`,
//! whose shim half this is; built 2026-11-04).
//!
//! PMS builds every guide load in `<guide>-loading.db`, renames it onto the
//! guide's file (`boost::filesystem::rename`, libc's `rename`) and reopens
//! it. The shim routes a guide to PostgreSQL only when its schema exists at
//! that open, and cluster-plex's manager copies a load into the schema once
//! it sees the renamed file, so without this hook a new DVR's first guide was
//! served from the SQLite file until PMS next reopened it, and every later
//! load was served stale, the previous copy, for the minute the poll took.
//! On a Plex cloud lineup (DIRECTV Mobile: 167 channels, 14 days, 354,116
//! rows, 35 s to copy) the window was the larger.
//!
//! `rename(old, new)` is exported like `sigaction`. Unless `old` is a guide
//! load and `new` its guide (`guide_swap_paths`) and `PLEX_PG_GUIDE_SOCKET`
//! names the manager's socket, it is libc's rename. Then it asks the manager
//! over the socket -- `swap <old>\n`, one line back: `ok`, `skip <why>` or
//! `fail <why>` -- waits up to `SWAP_DEADLINE`, logs the answer, and renames
//! whatever it was: a refused connection, a timeout or a malformed answer
//! leaves the rename to pass and the manager's poll to copy the load within
//! 30 s, as before. The hook never fails PMS's rename.

use std::ffi::CStr;
use std::io::{BufRead, BufReader, Write};
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use crate::db_interpose_conn_utils::{log_error, log_info};

/// The environment variable naming the manager's guide socket, set by
/// cluster-plex on the PMS it starts.
pub const GUIDE_SOCKET_ENV: &str = "PLEX_PG_GUIDE_SOCKET";

/// How long the hook waits for the manager's answer: a little longer than
/// the manager's own deadline on the request (230 s), so a slow copy ends in
/// the manager's reason rather than the hook's timeout. A cloud guide's copy
/// took 35 s on talos; an XMLTV guide's about half a second.
pub const SWAP_DEADLINE: Duration = Duration::from_secs(240);

type RenameFn = unsafe extern "C" fn(*const c_char, *const c_char) -> c_int;

static ORIG_RENAME: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// libc's `rename`, resolved once through `RTLD_NEXT`.
unsafe fn orig_rename() -> Option<RenameFn> {
    let mut p = ORIG_RENAME.load(Ordering::Acquire);
    if p.is_null() {
        p = libc::dlsym(libc::RTLD_NEXT, c"rename".as_ptr());
        if p.is_null() {
            return None;
        }
        ORIG_RENAME.store(p, Ordering::Release);
    }
    Some(std::mem::transmute::<*mut c_void, RenameFn>(p))
}

/// Whether a rename from `old` to `new` swaps a Live TV guide load onto its
/// guide: `old` is `<dir>/tv.plex.providers.epg.<provider>-<uuid>-loading.db`
/// and `new` is `<dir>/tv.plex.providers.epg.<provider>-<uuid>.db`. A
/// `-wal`, `-shm` or `-journal` of either, or a load renamed anywhere else,
/// is not a swap.
pub fn guide_swap_paths(old: &str, new: &str) -> bool {
    let Some(stem) = old.strip_suffix("-loading.db") else {
        return false;
    };
    if new.len() != stem.len() + 3 || !new.starts_with(stem) || !new.ends_with(".db") {
        return false;
    }
    crate::db_interpose_helpers::epg_schema_for_path(new).is_some()
}

/// Asks the manager at `socket` to copy the load `old` into its schema, and
/// returns its one-line answer (`ok`, `skip <why>` or `fail <why>`), or why
/// no answer came.
pub fn swap_request(socket: &str, old: &str, deadline: Duration) -> Result<String, String> {
    let start = Instant::now();
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("connect {socket}: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("write timeout: {e}"))?;
    stream
        .set_read_timeout(Some(deadline))
        .map_err(|e| format!("read timeout: {e}"))?;
    stream
        .write_all(format!("swap {old}\n").as_bytes())
        .map_err(|e| format!("send: {e}"))?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|e| format!("no answer after {:.1?}: {e}", start.elapsed()))?;
    let line = line.trim_end_matches(['\r', '\n']).to_string();
    match line.split(' ').next() {
        Some("ok") | Some("skip") | Some("fail") => Ok(line),
        _ if line.is_empty() => Err(format!("empty answer after {:.1?}", start.elapsed())),
        _ => Err(format!("malformed answer {line:?}")),
    }
}

/// Asks the manager to copy the load before it is renamed, and logs what
/// came of it. Whatever the answer, the rename goes ahead.
fn swap_before_rename(socket: &str, old: &str) {
    let start = Instant::now();
    match swap_request(socket, old, SWAP_DEADLINE) {
        Ok(answer) => log_info(&format!(
            "GUIDE_SWAP: the manager answered {:?} for {} in {:.1?}; renaming",
            answer,
            old,
            start.elapsed()
        )),
        Err(err) => log_error(&format!(
            "GUIDE_SWAP: {} for {} after {:.1?}; renaming, the manager's poll copies the load",
            err,
            old,
            start.elapsed()
        )),
    }
}

#[no_mangle]
/// # Safety
/// An ABI-level interposition hook for libc's `rename`: `old` and `new` are
/// NUL-terminated paths, as the libc API takes them (or NULL, which libc
/// refuses).
pub unsafe extern "C" fn rename(old: *const c_char, new: *const c_char) -> c_int {
    let Some(orig) = orig_rename() else {
        *libc::__errno_location() = libc::ENOSYS;
        return -1;
    };
    if !old.is_null() && !new.is_null() {
        if let (Ok(o), Ok(n)) = (CStr::from_ptr(old).to_str(), CStr::from_ptr(new).to_str()) {
            if guide_swap_paths(o, n) {
                if let Some(socket) = crate::env_utils::env_string(GUIDE_SOCKET_ENV) {
                    swap_before_rename(&socket, o);
                }
            }
        }
    }
    orig(old, new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    const DIR: &str =
        "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases";
    const LOAD: &str =
        "tv.plex.providers.epg.cloud-0ab001b3-182f-4835-897c-43a9ed6797c5-loading.db";
    const GUIDE: &str = "tv.plex.providers.epg.cloud-0ab001b3-182f-4835-897c-43a9ed6797c5.db";

    #[test]
    fn a_guide_load_renamed_onto_its_guide_is_a_swap() {
        assert!(guide_swap_paths(
            &format!("{DIR}/{LOAD}"),
            &format!("{DIR}/{GUIDE}")
        ));
        assert!(guide_swap_paths(
            &format!(
                "{DIR}/tv.plex.providers.epg.xmltv-9d6bf2a3-fda0-4a81-a1eb-49bd39bf995f-loading.db"
            ),
            &format!("{DIR}/tv.plex.providers.epg.xmltv-9d6bf2a3-fda0-4a81-a1eb-49bd39bf995f.db")
        ));
    }

    #[test]
    fn anything_else_is_libcs_rename() {
        // The load's write-ahead log and shared memory, moved beside it.
        assert!(!guide_swap_paths(
            &format!("{DIR}/{LOAD}-wal"),
            &format!("{DIR}/{GUIDE}-wal")
        ));
        assert!(!guide_swap_paths(
            &format!("{DIR}/{LOAD}-shm"),
            &format!("{DIR}/{GUIDE}-shm")
        ));
        // Another guide's name, another directory, a name no uuid ends.
        assert!(!guide_swap_paths(
            &format!("{DIR}/{LOAD}"),
            &format!("{DIR}/tv.plex.providers.epg.cloud-09108875-81a5-4778-94b0-52a8998adf6f.db")
        ));
        assert!(!guide_swap_paths(
            &format!("{DIR}/{LOAD}"),
            &format!("/tmp/{GUIDE}")
        ));
        assert!(!guide_swap_paths(
            &format!("{DIR}/tv.plex.providers.epg.cloud-loading.db"),
            &format!("{DIR}/tv.plex.providers.epg.cloud.db")
        ));
        // The library's own files, and a backup.
        assert!(!guide_swap_paths(
            &format!("{DIR}/com.plexapp.plugins.library.db-loading.db"),
            &format!("{DIR}/com.plexapp.plugins.library.db")
        ));
        assert!(!guide_swap_paths(
            &format!("{DIR}/com.plexapp.plugins.library.db"),
            &format!("{DIR}/com.plexapp.plugins.library.db-2026-11-04")
        ));
    }

    fn socket_path(name: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("plex-pg-guide-swap-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("g.sock").to_string_lossy().into_owned()
    }

    /// A manager that answers `reply` to the one request it reads, handing
    /// the request back on the channel.
    fn manager(path: &str, reply: &'static str) -> std::sync::mpsc::Receiver<String> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let n = c.read(&mut buf).unwrap();
            tx.send(String::from_utf8_lossy(&buf[..n]).into_owned())
                .unwrap();
            if !reply.is_empty() {
                c.write_all(reply.as_bytes()).unwrap();
            } else {
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        rx
    }

    #[test]
    fn the_managers_answer_comes_back_whole() {
        let path = socket_path("ok");
        let rx = manager(&path, "ok\n");
        let old = format!("{DIR}/{LOAD}");
        assert_eq!(
            swap_request(&path, &old, Duration::from_secs(5)).unwrap(),
            "ok"
        );
        assert_eq!(rx.recv().unwrap(), format!("swap {old}\n"));

        let path = socket_path("skip");
        let _rx = manager(&path, "skip no newer load with airings\r\n");
        assert_eq!(
            swap_request(&path, &old, Duration::from_secs(5)).unwrap(),
            "skip no newer load with airings"
        );

        let path = socket_path("fail");
        let _rx = manager(&path, "fail not a guide load\n");
        assert_eq!(
            swap_request(&path, &old, Duration::from_secs(5)).unwrap(),
            "fail not a guide load"
        );
    }

    #[test]
    fn no_manager_a_silent_one_or_a_malformed_answer_is_an_error() {
        let old = format!("{DIR}/{LOAD}");
        let err =
            swap_request("/nonexistent/guide.sock", &old, Duration::from_secs(1)).unwrap_err();
        assert!(err.starts_with("connect "), "{err}");

        let path = socket_path("silent");
        let _rx = manager(&path, "");
        let start = Instant::now();
        let err = swap_request(&path, &old, Duration::from_millis(300)).unwrap_err();
        assert!(err.starts_with("no answer after "), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the deadline, not the manager's own time"
        );

        let path = socket_path("malformed");
        let _rx = manager(&path, "copied\n");
        let err = swap_request(&path, &old, Duration::from_secs(5)).unwrap_err();
        assert!(err.starts_with("malformed answer"), "{err}");
    }
}
