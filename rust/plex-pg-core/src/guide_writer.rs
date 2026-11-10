//! The guide writer gate: the shim half of plan A in cluster-plex's
//! `docs/superpowers/specs/2026-11-10-guide-writer-and-built-cloud-guide-design.md`
//! (owner, 2026-11-10).
//!
//! Every PMS writes the Live TV guides on its own: it refreshes a cloud
//! guide at its start and on a timer no setting turns off, and it purges
//! ended airings on each guide's own connection. The guides live once, in
//! PostgreSQL (`epg_<dvr uuid>`), so every pod's PMS writes the one guide,
//! and a refresh is not atomic: two at once leave holes or doubled airings.
//! PMS does this work inside its own process, where only this shim sees it.
//!
//! With `PLEX_PG_GUIDE_WRITER_FILE` set, a write on a guide's own connection
//! runs only when the file the manager keeps allows the guide's schema. Each
//! line of the file, surrounding whitespace and blank lines ignored, is one
//! of:
//!
//! - `*`: every guide may be written;
//! - `epg_<uuid>`: this guide may be written;
//! - `-epg_<uuid>`: this guide may not, whatever else the file says.
//!
//! An absent, empty or unreadable file allows nothing. The file is looked at
//! no more than once a second and read again only when its modification
//! time, size or inode changed, so the manager is to replace it whole (write
//! a temporary file and rename it over): a reader that catches a rewrite
//! half done holds that second's writes.
//!
//! A write the file does not allow is held, not refused: it is not sent to
//! PostgreSQL, and PMS is told it ran and changed nothing -- the step answers
//! SQLITE_DONE (a RETURNING of PMS's own answers no row), `changes()` is 0,
//! and an INSERT's `last_insert_rowid` is a synthetic id, unique in the
//! process, so PMS's in-memory maps keep distinct keys for the rows it
//! believes it inserted. A refused statement kills PMS's guide loader: the
//! statement timeout of 2026-11-04 did, left a ghost "Refreshing guide data"
//! activity until PMS restarted, and PMS retried every five minutes.
//!
//! Transaction control and reads always pass, so PMS's transactions open
//! and close as they did. A guide load (`<guide>-loading.db`) is not gated
//! here: the manager gates its stage and swap. With the variable unset or
//! empty nothing here runs and every write goes through, as before.

use std::collections::{HashMap, HashSet};
use std::os::raw::c_char;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::db_interpose_conn_utils::{log_error, log_info};
use crate::db_interpose_helpers::{cstr_to_str_or_empty, epg_schema_for_path, is_guide_load_path};
use crate::env_utils;
use crate::ffi_types::PgConnection;
use crate::sync_utils::mutex_lock;

/// Names the file that says which guides this PMS may write. cluster-plex's
/// manager sets it on the PMS it starts and keeps the file.
pub const GUIDE_WRITER_FILE_ENV: &str = "PLEX_PG_GUIDE_WRITER_FILE";

/// How often the file is looked at, at most.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The first synthetic row id a held INSERT answers; each next one is one
/// lower. Far above any id a guide's sequences will reach, so a later read
/// or write by such an id matches nothing.
pub const FIRST_HELD_ROWID: i64 = 1 << 62;

/// What the file allows, parsed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Allowance {
    every: bool,
    allowed: HashSet<String>,
    denied: HashSet<String>,
}

impl Allowance {
    /// Parses the file's contents (see the module's note). Names are
    /// compared lowercased, as PostgreSQL folds an unquoted schema name and
    /// as `epg_schema_for_path` makes them.
    pub(crate) fn parse(contents: &str) -> Self {
        let mut allowance = Allowance::default();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line == "*" {
                allowance.every = true;
            } else if let Some(name) = line.strip_prefix('-') {
                let name = name.trim();
                if !name.is_empty() {
                    allowance.denied.insert(name.to_ascii_lowercase());
                }
            } else {
                allowance.allowed.insert(line.to_ascii_lowercase());
            }
        }
        allowance
    }

    /// Whether `schema` (lowercase, as `epg_schema_for_path` gives it) may be
    /// written: a deny line wins over `*` and over its own allow line.
    pub(crate) fn allows(&self, schema: &str) -> bool {
        !self.denied.contains(schema) && (self.every || self.allowed.contains(schema))
    }
}

/// Whether a file whose contents are `contents` (None: no file) allows
/// writes to `schema`: the decision alone, for the tests.
#[cfg(test)]
pub(crate) fn allows(contents: Option<&str>, schema: &str) -> bool {
    contents.is_some_and(|c| Allowance::parse(c).allows(schema))
}

/// What says the file changed: its modification time, size and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    ino: u64,
}

fn stamp_of(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
        ino: meta.ino(),
    })
}

/// The file's allowance, looked at no more than once a `RECHECK_INTERVAL`
/// and read again only when its stamp moved.
#[derive(Debug, Default)]
pub(crate) struct FileCache {
    checked_at: Option<Instant>,
    /// None until the first look; Some(None) while there is no file.
    stamp: Option<Option<Stamp>>,
    allowance: Allowance,
    /// How many times the file was read, for the tests.
    reads: u64,
}

impl FileCache {
    /// The allowance of the file at `path` as of `now`.
    pub(crate) fn allowance(&mut self, path: &Path, now: Instant) -> &Allowance {
        let due = self
            .checked_at
            .is_none_or(|at| now.saturating_duration_since(at) >= RECHECK_INTERVAL);
        if !due {
            return &self.allowance;
        }
        self.checked_at = Some(now);
        let stamp = stamp_of(path);
        if self.stamp == Some(stamp) {
            return &self.allowance;
        }
        self.stamp = Some(stamp);
        self.allowance = match stamp {
            None => Allowance::default(),
            Some(_) => match std::fs::read(path) {
                Ok(bytes) => {
                    self.reads += 1;
                    Allowance::parse(&String::from_utf8_lossy(&bytes))
                }
                Err(err) => {
                    log_error(&format!(
                        "GUIDE_WRITER: reading {} failed: {err}; no guide may be written",
                        path.display()
                    ));
                    Allowance::default()
                }
            },
        };
        &self.allowance
    }
}

/// One guide's gate state: whether its writes are being held, and how many
/// have been held since the process started (the shim keeps no stats
/// facility of its own; the count is logged when writing resumes).
#[derive(Debug, Default)]
struct SchemaGate {
    holding: bool,
    held: u64,
}

/// The gate: the file, its cache, and each guide's state.
#[derive(Debug)]
pub(crate) struct Gate {
    file: PathBuf,
    cache: FileCache,
    schemas: HashMap<String, SchemaGate>,
}

impl Gate {
    pub(crate) fn new(file: PathBuf) -> Self {
        Gate {
            file,
            cache: FileCache::default(),
            schemas: HashMap::new(),
        }
    }

    /// Whether a write to `schema` at `now` is held, and the line to log at
    /// INFO when it changes the guide's state: its first held write since
    /// it was last allowed (or since the start), and its first allowed write
    /// after being held. Nothing for the rest.
    pub(crate) fn holds(&mut self, schema: &str, now: Instant) -> (bool, Option<String>) {
        let allowed = self.cache.allowance(&self.file, now).allows(schema);
        if !self.schemas.contains_key(schema) {
            self.schemas
                .insert(schema.to_string(), SchemaGate::default());
        }
        let state = self.schemas.get_mut(schema).expect("inserted above");
        if allowed {
            if !state.holding {
                return (false, None);
            }
            state.holding = false;
            let line = format!(
                "GUIDE_WRITER: writing {schema} again: {} allows it ({} writes held so far)",
                self.file.display(),
                state.held
            );
            return (false, Some(line));
        }
        state.held += 1;
        if state.holding {
            return (true, None);
        }
        state.holding = true;
        let line = format!(
            "GUIDE_WRITER: holding writes to {schema}: {} does not allow it; each is reported \
             to PMS as run, changing nothing",
            self.file.display()
        );
        (true, Some(line))
    }

    /// How many writes to `schema` have been held.
    #[cfg(test)]
    pub(crate) fn held_writes(&self, schema: &str) -> u64 {
        self.schemas.get(schema).map_or(0, |s| s.held)
    }
}

/// -1: the variable not looked at yet; 0: unset, nothing is gated; 1: set,
/// and `GATE` holds the gate. A load on every write, so the variable unset
/// costs no lock and no syscall.
static CONFIGURED: AtomicI32 = AtomicI32::new(-1);
static GATE: Mutex<Option<Gate>> = Mutex::new(None);
static NEXT_HELD_ROWID: AtomicI64 = AtomicI64::new(FIRST_HELD_ROWID);

fn gated() -> bool {
    match CONFIGURED.load(Ordering::Acquire) {
        0 => false,
        1 => true,
        _ => configure(),
    }
}

fn configure() -> bool {
    let mut gate = mutex_lock(&GATE);
    let state = CONFIGURED.load(Ordering::Acquire);
    if state != -1 {
        return state == 1;
    }
    let file = env_utils::env_string(GUIDE_WRITER_FILE_ENV)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let on = file.is_some();
    if let Some(file) = file {
        log_info(&format!(
            "GUIDE_WRITER: a guide's writes run only when {file} allows its schema"
        ));
        *gate = Some(Gate::new(PathBuf::from(file)));
    }
    CONFIGURED.store(i32::from(on), Ordering::Release);
    on
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut gate = mutex_lock(&GATE);
    *gate = None;
    CONFIGURED.store(-1, Ordering::Release);
}

/// The schema of the guide whose own database is `path`; None for a guide's
/// load (the manager gates those), the library, and anything else.
pub(crate) fn guide_schema(path: &str) -> Option<String> {
    if is_guide_load_path(path) {
        return None;
    }
    epg_schema_for_path(path)
}

/// Whether `sql` is a statement the gate may hold: a write as the shim's
/// write path takes one (`is_write_operation_str`: INSERT, UPDATE, DELETE,
/// REPLACE and DDL), never transaction control, never a read.
pub(crate) fn held_kind(sql: &str) -> bool {
    crate::pg_config::is_write_operation_str(sql)
        && crate::pg_client::txn::classify_sql(sql).is_none()
}

/// Whether `sql` makes a row SQLite would number: an INSERT or a REPLACE
/// (upserts included). An UPDATE or a DELETE leaves `last_insert_rowid`
/// alone, in SQLite and for a held write alike.
pub(crate) fn inserts_row(sql: &str) -> bool {
    let head = sql.trim_start().as_bytes();
    let starts =
        |word: &[u8]| head.len() >= word.len() && head[..word.len()].eq_ignore_ascii_case(word);
    starts(b"insert") || starts(b"replace")
}

/// The next synthetic row id: unique in the process, counting down from
/// `FIRST_HELD_ROWID`.
pub(crate) fn next_held_rowid() -> i64 {
    NEXT_HELD_ROWID.fetch_sub(1, Ordering::Relaxed)
}

/// Whether the write `sql` about to run on `conn` is held: the variable is
/// set, `conn` is a guide's own connection, `sql` is a write and the file
/// does not allow the guide.
///
/// # Safety
/// `conn` is null or a live `PgConnection`; `sql` is null or NUL-terminated.
pub(crate) unsafe fn holds(conn: *const PgConnection, sql: *const c_char) -> bool {
    if !gated() || conn.is_null() || sql.is_null() {
        return false;
    }
    let Some(schema) = guide_schema(cstr_to_str_or_empty((*conn).db_path.as_ptr())) else {
        return false;
    };
    if !held_kind(cstr_to_str_or_empty(sql)) {
        return false;
    }
    // Logged after the lock is let go, so a write to the log never holds up
    // another guide write's decision.
    let (hold, line) = {
        let mut gate = mutex_lock(&GATE);
        match gate.as_mut() {
            Some(gate) => gate.holds(&schema, Instant::now()),
            None => (false, None),
        }
    };
    if let Some(line) = line {
        log_info(&line);
    }
    hold
}

/// Records a held write the way a write that ran and changed nothing is
/// recorded: `changes()` 0 and, for an INSERT or REPLACE, a synthetic
/// `last_insert_rowid` (on the connection, which is what PMS asks, and as
/// the process's last inserted id, as `write_exec` keeps it) with the
/// failed-insert flag cleared. The connection's last error is left as a
/// write that ran leaves it; the step clears it after SQLITE_DONE.
pub(crate) fn note_held_write(conn: &mut PgConnection, rowid: Option<i64>) {
    conn.last_changes = 0;
    if let Some(rowid) = rowid {
        conn.last_insert_rowid = rowid;
        conn.last_insert_failed.store(0, Ordering::Release);
        crate::pg_client::rust_set_global_last_insert_rowid(rowid);
    }
}

/// Holds the write `sql` about to run on `conn` when the gate says so
/// (`holds`), and reports whether it did. A held write is recorded on `conn`
/// and on `changes_conn` when that is another connection (the one PMS's
/// `changes()` reads on the cached write path), with one synthetic row id
/// for both. The caller then marks its statement executed and answers as
/// for a write that ran, never sending it.
///
/// # Safety
/// `conn` and `changes_conn` are each null or a live `PgConnection`; `sql`
/// is null or NUL-terminated.
pub(crate) unsafe fn hold_write(
    conn: *mut PgConnection,
    changes_conn: *mut PgConnection,
    sql: *const c_char,
) -> bool {
    if !holds(conn, sql) {
        return false;
    }
    let rowid = inserts_row(cstr_to_str_or_empty(sql)).then(next_held_rowid);
    note_held_write(&mut *conn, rowid);
    if !changes_conn.is_null() && changes_conn != conn {
        note_held_write(&mut *changes_conn, rowid);
    }
    true
}

/// How many writes to `schema` this process has held.
#[cfg(test)]
pub(crate) fn held_writes(schema: &str) -> u64 {
    mutex_lock(&GATE)
        .as_ref()
        .map_or(0, |g| g.held_writes(schema))
}

/// Runs `f` with `PLEX_PG_GUIDE_WRITER_FILE` set to `file` (None: unset) and
/// the gate read afresh, under the tests' environment lock.
#[cfg(test)]
pub(crate) fn with_writer_file<T>(file: Option<&Path>, f: impl FnOnce() -> T) -> T {
    let _guard = crate::test_utils::env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let prev = std::env::var(GUIDE_WRITER_FILE_ENV).ok();
    match file {
        Some(file) => std::env::set_var(GUIDE_WRITER_FILE_ENV, file),
        None => std::env::remove_var(GUIDE_WRITER_FILE_ENV),
    }
    reset_for_tests();
    let out = f();
    match prev {
        Some(v) => std::env::set_var(GUIDE_WRITER_FILE_ENV, v),
        None => std::env::remove_var(GUIDE_WRITER_FILE_ENV),
    }
    reset_for_tests();
    out
}

/// A zeroed `PgConnection` whose database is `path`, for the tests; free it
/// with `libc::free`.
#[cfg(test)]
pub(crate) fn test_connection(path: &str) -> *mut PgConnection {
    let conn = unsafe { libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection };
    unsafe {
        for (i, b) in path.as_bytes().iter().enumerate() {
            (*conn).db_path[i] = *b as c_char;
        }
        (*conn).is_pg_active = 1;
    }
    conn
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::CString;

    pub(crate) const DIR: &str =
        "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases";
    pub(crate) const GUIDE: &str =
        "tv.plex.providers.epg.cloud-0ab001b3-182f-4835-897c-43a9ed6797c5.db";
    pub(crate) const SCHEMA: &str = "epg_0ab001b3182f4835897c43a9ed6797c5";
    const OTHER: &str = "epg_1111111122223333444455556666777f";
    const LOAD: &str =
        "tv.plex.providers.epg.cloud-0ab001b3-182f-4835-897c-43a9ed6797c5-loading.db";

    /// A fresh scratch file path for one test, its directory made.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "plex-pg-guide-writer-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("guide-writer");
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_star_allows_every_guide() {
        assert!(allows(Some("*\n"), SCHEMA));
        assert!(allows(Some("*"), OTHER));
    }

    #[test]
    fn a_name_allows_that_guide_alone() {
        let file = format!("{SCHEMA}\n");
        assert!(allows(Some(&file), SCHEMA));
        assert!(!allows(Some(&file), OTHER));
    }

    #[test]
    fn a_deny_line_wins_over_everything_else() {
        let file = format!("*\n-{SCHEMA}\n{SCHEMA}\n");
        assert!(!allows(Some(&file), SCHEMA), "denied whatever else says");
        assert!(allows(Some(&file), OTHER), "the star still allows the rest");
        let file = format!("-{SCHEMA}\n");
        assert!(!allows(Some(&file), SCHEMA));
        assert!(!allows(Some(&file), OTHER), "a deny alone allows nothing");
    }

    #[test]
    fn an_empty_or_absent_file_allows_nothing() {
        assert!(!allows(None, SCHEMA));
        assert!(!allows(Some(""), SCHEMA));
        assert!(!allows(Some("\n\n  \n\t\n"), SCHEMA));
        assert!(!allows(Some("-\n"), SCHEMA), "a bare dash names nothing");
    }

    #[test]
    fn whitespace_and_blank_lines_are_ignored() {
        let file = format!("\n   \n  {SCHEMA}  \r\n\t-{OTHER}\t\n\n");
        assert!(allows(Some(&file), SCHEMA));
        assert!(!allows(Some(&file), OTHER));
        assert!(allows(Some("  *  \r\n"), OTHER));
        let file = format!("- {OTHER}\n*\n");
        assert!(!allows(Some(&file), OTHER), "space after the dash");
        let upper = SCHEMA.to_ascii_uppercase().replace("EPG_", "epg_");
        assert!(
            allows(Some(&upper), SCHEMA),
            "names compare lowercased, as PostgreSQL folds them"
        );
    }

    #[test]
    fn only_a_guides_own_database_is_gated() {
        assert_eq!(
            guide_schema(&format!("{DIR}/{GUIDE}")).as_deref(),
            Some(SCHEMA)
        );
        assert_eq!(guide_schema(&format!("{DIR}/{LOAD}")), None, "a load");
        assert_eq!(
            guide_schema(&format!("{DIR}/com.plexapp.plugins.library.db")),
            None
        );
        assert_eq!(guide_schema(""), None);
    }

    #[test]
    fn only_writes_are_held_never_transaction_control_or_reads() {
        for sql in [
            "INSERT INTO metadata_items (title) VALUES (?)",
            "  insert or replace into tags (tag) values (?)",
            "REPLACE INTO taggings (tag_id) VALUES (?)",
            "update metadata_items set title=? where id=?",
            "DELETE FROM media_items WHERE begins_at < ?",
            "CREATE INDEX i ON t (c)",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN c",
            "-- a comment\nDELETE FROM t",
        ] {
            assert!(held_kind(sql), "{sql}");
        }
        for sql in [
            "BEGIN",
            "BEGIN IMMEDIATE TRANSACTION",
            "COMMIT",
            "END",
            "ROLLBACK",
            "ROLLBACK TO SAVEPOINT sp1",
            "SAVEPOINT sp1",
            "RELEASE SAVEPOINT sp1",
            "SELECT id FROM metadata_items",
            "PRAGMA journal_mode",
            "",
        ] {
            assert!(!held_kind(sql), "{sql}");
        }
    }

    #[test]
    fn only_an_insert_or_replace_takes_a_row_id() {
        assert!(inserts_row("INSERT INTO t (a) VALUES (?)"));
        assert!(inserts_row("  insert or ignore into t (a) values (?)"));
        assert!(inserts_row("REPLACE INTO t (a) VALUES (?)"));
        assert!(!inserts_row("UPDATE t SET a=?"));
        assert!(!inserts_row("DELETE FROM t"));
        assert!(!inserts_row("ins"));
    }

    #[test]
    fn synthetic_row_ids_are_unique_in_the_process_and_count_down() {
        let a = next_held_rowid();
        let b = next_held_rowid();
        assert!(b < a, "counting down");
        assert!(a <= FIRST_HELD_ROWID);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| (0..1000).map(|_| next_held_rowid()).collect::<Vec<_>>())
            })
            .collect();
        let mut all: Vec<i64> = threads
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect();
        all.push(a);
        all.push(b);
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "no id handed out twice");
        assert!(all.iter().all(|&id| id > 0 && id <= FIRST_HELD_ROWID));
    }

    /// Sets a file's modification time, so two writes of one size inside the
    /// same clock tick still read as a change.
    fn write_at(path: &Path, contents: &str, modified: SystemTime) {
        std::fs::write(path, contents).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn the_file_is_read_again_when_it_changes_and_looked_at_once_a_second() {
        let path = scratch("cache");
        let mut cache = FileCache::default();
        let t0 = Instant::now();
        let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);

        // No file: nothing allowed, nothing read.
        assert!(!cache.allowance(&path, t0).allows(SCHEMA));
        assert_eq!(cache.reads, 0);

        // Written, but looked at again only a second after the last look.
        write_at(&path, "*\n", epoch);
        assert!(
            !cache
                .allowance(&path, t0 + Duration::from_millis(500))
                .allows(SCHEMA),
            "within the second: the last look's answer"
        );
        assert!(cache
            .allowance(&path, t0 + Duration::from_millis(1000))
            .allows(SCHEMA));
        assert_eq!(cache.reads, 1);

        // Unchanged: looked at, never read again.
        assert!(cache
            .allowance(&path, t0 + Duration::from_secs(3))
            .allows(SCHEMA));
        assert_eq!(cache.reads, 1);

        // A new size is a change.
        write_at(&path, &format!("*\n-{SCHEMA}\n"), epoch);
        assert!(!cache
            .allowance(&path, t0 + Duration::from_secs(5))
            .allows(SCHEMA));
        assert_eq!(cache.reads, 2);

        // The same size with a new modification time is a change.
        write_at(
            &path,
            &format!("*\n-{OTHER}\n"),
            epoch + Duration::from_secs(1),
        );
        assert!(cache
            .allowance(&path, t0 + Duration::from_secs(7))
            .allows(SCHEMA));
        assert!(!cache
            .allowance(&path, t0 + Duration::from_secs(7))
            .allows(OTHER));
        assert_eq!(cache.reads, 3);

        // Emptied: allows nothing.
        write_at(&path, "", epoch + Duration::from_secs(2));
        assert!(!cache
            .allowance(&path, t0 + Duration::from_secs(9))
            .allows(SCHEMA));

        // Removed: allows nothing.
        write_at(&path, "*\n", epoch + Duration::from_secs(3));
        assert!(cache
            .allowance(&path, t0 + Duration::from_secs(11))
            .allows(SCHEMA));
        std::fs::remove_file(&path).unwrap();
        assert!(!cache
            .allowance(&path, t0 + Duration::from_secs(13))
            .allows(SCHEMA));
    }

    #[test]
    fn a_guide_logs_its_first_held_write_and_its_first_allowed_one_after() {
        let path = scratch("transitions");
        let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let mut gate = Gate::new(path.clone());
        let t0 = Instant::now();

        write_at(&path, "*\n", epoch);
        assert_eq!(
            gate.holds(SCHEMA, t0),
            (false, None),
            "allowed since start: quiet"
        );

        write_at(&path, "", epoch + Duration::from_secs(1));
        let (hold, line) = gate.holds(SCHEMA, t0 + Duration::from_secs(2));
        assert!(hold);
        let line = line.expect("the first held write is logged");
        assert!(line.contains(SCHEMA) && line.contains(&path.display().to_string()));
        assert_eq!(
            gate.holds(SCHEMA, t0 + Duration::from_secs(2)),
            (true, None)
        );
        assert_eq!(
            gate.holds(SCHEMA, t0 + Duration::from_secs(2)),
            (true, None)
        );
        assert_eq!(gate.held_writes(SCHEMA), 3);
        assert_eq!(gate.held_writes(OTHER), 0);

        write_at(
            &path,
            &format!("{SCHEMA}\n"),
            epoch + Duration::from_secs(2),
        );
        let (hold, line) = gate.holds(SCHEMA, t0 + Duration::from_secs(4));
        assert!(!hold);
        let line = line.expect("the first allowed write after holding is logged");
        assert!(line.contains(SCHEMA) && line.contains("3 writes held"));
        assert_eq!(
            gate.holds(SCHEMA, t0 + Duration::from_secs(4)),
            (false, None)
        );

        // Another guide, never allowed: its first held write is logged too.
        let (hold, line) = gate.holds(OTHER, t0 + Duration::from_secs(4));
        assert!(hold && line.is_some());
    }

    #[test]
    fn unset_holds_nothing() {
        let conn = test_connection(&format!("{DIR}/{GUIDE}"));
        let sql = CString::new("DELETE FROM media_items").unwrap();
        with_writer_file(None, || unsafe {
            assert!(!holds(conn, sql.as_ptr()));
            assert!(!gated());
        });
        unsafe { libc::free(conn as *mut libc::c_void) };
    }

    #[test]
    fn a_set_variable_holds_only_a_guides_writes_the_file_does_not_allow() {
        let path = scratch("holds");
        std::fs::write(&path, format!("*\n-{SCHEMA}\n")).unwrap();
        let guide = test_connection(&format!("{DIR}/{GUIDE}"));
        let load = test_connection(&format!("{DIR}/{LOAD}"));
        let library = test_connection(&format!("{DIR}/com.plexapp.plugins.library.db"));
        let insert = CString::new("INSERT INTO metadata_items (title) VALUES (?)").unwrap();
        let update = CString::new("UPDATE metadata_items SET title=? WHERE id=?").unwrap();
        let begin = CString::new("BEGIN IMMEDIATE").unwrap();
        let select = CString::new("SELECT id FROM metadata_items").unwrap();
        with_writer_file(Some(&path), || unsafe {
            assert!(holds(guide, insert.as_ptr()));
            assert!(!holds(guide, begin.as_ptr()), "transaction control passes");
            assert!(!holds(guide, select.as_ptr()), "reads pass");
            assert!(!holds(load, insert.as_ptr()), "loads are the manager's");
            assert!(
                !holds(library, insert.as_ptr()),
                "the library is never gated"
            );
            assert!(!holds(std::ptr::null(), insert.as_ptr()));

            (*guide).last_changes = 7;
            (*guide).last_insert_rowid = 42;
            (*guide).last_insert_failed.store(1, Ordering::Release);
            assert!(hold_write(guide, std::ptr::null_mut(), insert.as_ptr()));
            assert_eq!((*guide).last_changes, 0);
            let rowid = (*guide).last_insert_rowid;
            assert!(
                rowid > 42 && rowid <= FIRST_HELD_ROWID,
                "synthetic: {rowid}"
            );
            assert_eq!((*guide).last_insert_failed.load(Ordering::Acquire), 0);

            // An UPDATE keeps the last insert's id, as SQLite does.
            (*guide).last_changes = 3;
            (*library).last_changes = 5;
            assert!(hold_write(guide, library, update.as_ptr()));
            assert_eq!((*guide).last_changes, 0);
            assert_eq!((*guide).last_insert_rowid, rowid);
            assert_eq!((*library).last_changes, 0, "and on changes() connection");

            assert_eq!(held_writes(SCHEMA), 3);
        });
        for c in [guide, load, library] {
            unsafe { libc::free(c as *mut libc::c_void) };
        }
    }

    #[test]
    fn an_absent_file_holds_every_guide_write() {
        let path = scratch("absent");
        let guide = test_connection(&format!("{DIR}/{GUIDE}"));
        let delete = CString::new("DELETE FROM media_items WHERE ends_at < ?").unwrap();
        with_writer_file(Some(&path), || unsafe {
            assert!(holds(guide, delete.as_ptr()));
        });
        unsafe { libc::free(guide as *mut libc::c_void) };
    }
}
