//! A parameter must read exactly as it was last bound. Plex reuses one cached
//! `UPDATE metadata_items SET ...,parent_id=:U2,...` for every item it saves:
//! an episode binds its season to `:U2`, the next movie or show binds NULL.
//! When NULL left the integer in place, the movie was written with the
//! episode's season as its parent and PostgreSQL's trigger refused the whole
//! save (kind-cluster-plex, 2026-10-06: "Cross-section parent link
//! prevented", "Show N cannot have non-collection parent (type 3)").

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use rusqlite::{ffi, Connection};

use super::*;
use crate::db_interpose_stmt_lifecycle::rust_my_sqlite3_clear_bindings;
use crate::pg_statement::c_abi::{pg_register_stmt, pg_unregister_stmt};
use crate::pg_statement::rust_stmt_create;

/// A PG-routed write statement with named parameters `U1`..`Un`, registered
/// against a real SQLite statement so the bind path finds it as it does in
/// Plex.
struct Bound {
    _conn: Connection,
    sqlite: *mut ffi::sqlite3_stmt,
    pg: *mut PgStmt,
    _names: Vec<CString>,
    _name_ptrs: Box<[*mut c_char]>,
}

impl Bound {
    fn new(n: usize) -> Self {
        let conn = Connection::open_in_memory().unwrap();
        let placeholders: Vec<String> = (1..=n).map(|i| format!(":U{i}")).collect();
        let sql = CString::new(format!("SELECT {}", placeholders.join(", "))).unwrap();
        let mut sqlite: *mut ffi::sqlite3_stmt = ptr::null_mut();
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(
                conn.handle(),
                sql.as_ptr(),
                -1,
                &mut sqlite,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, ffi::SQLITE_OK);

        let names: Vec<CString> = (1..=n)
            .map(|i| CString::new(format!("U{i}")).unwrap())
            .collect();
        let mut name_ptrs: Box<[*mut c_char]> =
            names.iter().map(|c| c.as_ptr() as *mut c_char).collect();

        let pg = rust_stmt_create(ptr::null_mut(), sql.as_ptr(), ptr::null_mut());
        assert!(!pg.is_null());
        unsafe {
            (*pg).is_pg = 1;
            (*pg).is_cached = 0;
            (*pg).param_count = n as c_int;
            (*pg).param_names = name_ptrs.as_mut_ptr();
            (*pg).ensure_param_capacity(n);
        }
        pg_register_stmt(sqlite as *mut sqlite3_stmt, pg);
        Bound {
            _conn: conn,
            sqlite,
            pg,
            _names: names,
            _name_ptrs: name_ptrs,
        }
    }

    fn stmt(&self) -> *mut sqlite3_stmt {
        self.sqlite as *mut sqlite3_stmt
    }

    /// What the step would send PostgreSQL for parameter `idx` (1-based).
    fn sent(&self, idx: usize) -> Option<String> {
        let v = unsafe { (&(*self.pg).param_values)[idx - 1] };
        if v.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(v) }.to_string_lossy().into_owned())
        }
    }

    fn format(&self, idx: usize) -> c_int {
        unsafe { (&(*self.pg).param_formats)[idx - 1] }
    }
}

impl Drop for Bound {
    fn drop(&mut self) {
        pg_unregister_stmt(self.stmt());
        unsafe {
            // The names belong to this struct, not the PgStmt.
            (*self.pg).param_names = ptr::null_mut();
            (*self.pg).param_count = 0;
            ffi::sqlite3_finalize(self.sqlite);
        }
    }
}

#[test]
fn null_after_an_integer_sends_null() {
    let b = Bound::new(2);
    rust_my_sqlite3_bind_int64(b.stmt(), 2, 8751);
    assert_eq!(b.sent(2).as_deref(), Some("8751"));

    rust_my_sqlite3_bind_null(b.stmt(), 2);
    assert_eq!(
        b.sent(2),
        None,
        "the previous row's parent_id was sent again"
    );
}

#[test]
fn null_after_an_int_or_a_double_sends_null() {
    let b = Bound::new(2);
    rust_my_sqlite3_bind_int(b.stmt(), 1, 3);
    rust_my_sqlite3_bind_double(b.stmt(), 2, 7.5);
    rust_my_sqlite3_bind_null(b.stmt(), 1);
    rust_my_sqlite3_bind_null(b.stmt(), 2);
    assert_eq!(b.sent(1), None);
    assert_eq!(b.sent(2), None);
}

#[test]
fn a_null_text_after_an_integer_sends_null() {
    let b = Bound::new(1);
    rust_my_sqlite3_bind_int64(b.stmt(), 1, 8751);
    rust_my_sqlite3_bind_text(b.stmt(), 1, ptr::null(), -1, ptr::null_mut());
    assert_eq!(b.sent(1), None);
}

#[test]
fn a_null_value_after_an_integer_sends_null() {
    let b = Bound::new(1);
    rust_my_sqlite3_bind_int64(b.stmt(), 1, 8751);

    // A real SQLite NULL value, as Plex passes one through sqlite3_bind_value.
    let conn = Connection::open_in_memory().unwrap();
    let sql = CString::new("SELECT NULL").unwrap();
    let mut src: *mut ffi::sqlite3_stmt = ptr::null_mut();
    unsafe {
        assert_eq!(
            ffi::sqlite3_prepare_v2(conn.handle(), sql.as_ptr(), -1, &mut src, ptr::null_mut()),
            ffi::SQLITE_OK
        );
        assert_eq!(ffi::sqlite3_step(src), ffi::SQLITE_ROW);
        let value = ffi::sqlite3_column_value(src, 0);
        rust_my_sqlite3_bind_value(b.stmt(), 1, value as *const sqlite3_value);
        ffi::sqlite3_finalize(src);
    }
    assert_eq!(b.sent(1), None);
}

#[test]
fn clear_bindings_clears_integers_too() {
    let b = Bound::new(2);
    rust_my_sqlite3_bind_int64(b.stmt(), 1, 2);
    rust_my_sqlite3_bind_int64(b.stmt(), 2, 8751);
    rust_my_sqlite3_clear_bindings(b.stmt());
    assert_eq!(b.sent(1), None);
    assert_eq!(b.sent(2), None);
}

#[test]
fn an_integer_after_a_text_is_the_integer_in_text_format() {
    let b = Bound::new(1);
    let t = CString::new("hello").unwrap();
    rust_my_sqlite3_bind_text(b.stmt(), 1, t.as_ptr(), -1, ptr::null_mut());
    unsafe { (&mut (*b.pg).param_formats)[0] = 1 };
    rust_my_sqlite3_bind_int64(b.stmt(), 1, 42);
    assert_eq!(b.sent(1).as_deref(), Some("42"));
    assert_eq!(b.format(1), 0, "an integer is sent as text");
}

#[test]
fn rebinding_an_integer_keeps_the_new_value() {
    let b = Bound::new(1);
    rust_my_sqlite3_bind_int64(b.stmt(), 1, 8751);
    rust_my_sqlite3_bind_int64(b.stmt(), 1, 12);
    assert_eq!(b.sent(1).as_deref(), Some("12"));
}

#[test]
fn a_null_blob_after_an_integer_sends_null() {
    let b = Bound::new(1);
    rust_my_sqlite3_bind_int64(b.stmt(), 1, 8751);
    rust_my_sqlite3_bind_blob(b.stmt(), 1, ptr::null(), 0, ptr::null_mut());
    assert_eq!(b.sent(1), None);
}

#[test]
fn an_empty_blob_after_an_integer_sends_an_empty_bytea() {
    let b = Bound::new(1);
    rust_my_sqlite3_bind_int64(b.stmt(), 1, 8751);
    let empty = [0u8; 0];
    rust_my_sqlite3_bind_blob(
        b.stmt(),
        1,
        empty.as_ptr() as *const c_void,
        0,
        ptr::null_mut(),
    );
    assert_eq!(b.sent(1).as_deref(), Some(""));
}
