//! One answer to "what does the shim do with this PostgreSQL error?", for
//! every path that executes a statement: the step's streaming and eager
//! reads, the cached read, both write paths and `sqlite3_exec`.
//!
//! Until 2026-10-16 each path decided for itself, by a different rule. The
//! streaming read retried every error five times over ten seconds before it
//! reported SQLITE_ERROR; the eager read logged "EAGER FALLBACK" and answered
//! SQLITE_DONE with no rows, so a permanent error read as an empty table; and
//! the writes reported SQLITE_DONE whatever PostgreSQL said, so a constraint
//! violation was a success with the previous statement's `changes()`.
//!
//! The rule is now: retry only what a retry can mend -- the connection is
//! gone, the server is shutting down or asked us to, a serialization or
//! deadlock failure -- and a stale prepared statement after the local cache is
//! cleared so the re-prepare happens; everything else (bad data, a constraint,
//! a syntax or type error) fails at once, with PostgreSQL's message as the
//! connection's `sqlite3_errmsg`, exactly as SQLite would have refused it.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

use crate::libpq_helpers::{
    rust_pq_error_message, rust_pq_result_error_field, rust_pq_result_error_message,
    rust_pq_status, PGconn, PGresult,
};

const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;
const CONNECTION_OK: c_int = 0;

/// What to do with a failed statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PgErrorClass {
    /// Run the statement again after the existing backoff: the connection
    /// was lost or the server cannot serve it right now.
    Retry,
    /// The server-side prepared statement no longer matches: clear the local
    /// statement cache (so it is prepared again) and retry.
    StalePlan,
    /// A permanent refusal. Report it to Plex now; a retry gets the same
    /// answer and only delays the 500.
    Fail,
}

/// Whether this SQLSTATE names a prepared statement the server no longer
/// has, or has but can no longer run as planned.
///
/// `26000` is "prepared statement does not exist" (the backend was reset
/// under us). `0A000` is "feature not supported", which covers many things;
/// the one that matters here is "cached plan must not change result type",
/// raised when a table a prepared SELECT reads gained a column -- Plex's
/// migrations do exactly that -- and a DEALLOCATE plus re-prepare is the
/// whole fix.
pub(crate) fn is_stale_plan(sqlstate: &str, message: &str) -> bool {
    sqlstate == "26000"
        || (sqlstate == "0A000" && message.contains("cached plan must not change result type"))
}

/// Classify an error from its parts, so it can be tested without a server.
///
/// `conn_ok` is `PQstatus(conn) == CONNECTION_OK`; `sqlstate` is the
/// result's `PG_DIAG_SQLSTATE` field, or empty when there is no result (libpq
/// gives none for a lost connection).
pub(crate) fn classify(conn_ok: bool, sqlstate: &str, message: &str) -> PgErrorClass {
    if !conn_ok {
        return PgErrorClass::Retry;
    }
    if is_stale_plan(sqlstate, message) {
        return PgErrorClass::StalePlan;
    }
    // Class 08: connection exceptions. 57P01-57P03: admin shutdown, crash
    // shutdown, cannot connect now. 40001 serialization failure and 40P01
    // deadlock detected: PostgreSQL's own advice is to retry both.
    if sqlstate.starts_with("08")
        || matches!(sqlstate, "57P01" | "57P02" | "57P03" | "40001" | "40P01")
    {
        return PgErrorClass::Retry;
    }
    // A result with no SQLSTATE at all on a healthy connection is libpq's
    // own complaint (out of memory, a protocol error); nothing to retry.
    PgErrorClass::Fail
}

fn cstr_or_empty(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// The message Plex will read through `sqlite3_errmsg`: the result's own
/// error, else the connection's, trimmed of libpq's trailing newline.
pub(crate) fn result_message(conn: *mut PGconn, res: *const PGresult) -> String {
    let mut msg = if res.is_null() {
        String::new()
    } else {
        cstr_or_empty(rust_pq_result_error_message(res))
    };
    if msg.trim().is_empty() {
        msg = cstr_or_empty(rust_pq_error_message(conn));
    }
    if msg.trim().is_empty() {
        msg = "PostgreSQL error (no message)".to_string();
    }
    msg.trim_end().to_string()
}

/// Classify a failed result on `conn`, and hand back the message to report.
pub(crate) fn classify_result(conn: *mut PGconn, res: *const PGresult) -> (PgErrorClass, String) {
    let conn_ok = !conn.is_null() && rust_pq_status(conn) == CONNECTION_OK;
    let sqlstate = if res.is_null() {
        String::new()
    } else {
        cstr_or_empty(rust_pq_result_error_field(res, PG_DIAG_SQLSTATE))
    };
    let message = result_message(conn, res);
    (classify(conn_ok, &sqlstate, &message), message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_connection_is_retried_whatever_the_result_says() {
        assert_eq!(classify(false, "", ""), PgErrorClass::Retry);
        assert_eq!(
            classify(false, "23505", "duplicate key"),
            PgErrorClass::Retry
        );
    }

    #[test]
    fn connection_and_operator_intervention_states_are_retried() {
        for state in [
            "08000", "08003", "08006", "57P01", "57P02", "57P03", "40001", "40P01",
        ] {
            assert_eq!(classify(true, state, ""), PgErrorClass::Retry, "{state}");
        }
    }

    #[test]
    fn a_stale_prepared_statement_is_re_prepared() {
        assert_eq!(
            classify(true, "26000", "prepared statement \"s_1\" does not exist"),
            PgErrorClass::StalePlan
        );
        assert_eq!(
            classify(
                true,
                "0A000",
                "ERROR:  cached plan must not change result type"
            ),
            PgErrorClass::StalePlan
        );
    }

    // Every other 0A000 is a real "not supported" and must not loop.
    #[test]
    fn an_unsupported_feature_that_is_not_a_cached_plan_fails_at_once() {
        assert_eq!(
            classify(true, "0A000", "LIMIT #,# syntax is not supported"),
            PgErrorClass::Fail
        );
        assert!(!is_stale_plan("0A000", "something else"));
    }

    // A constraint, a bad value, a missing column and a syntax error: the
    // audit's live case was an integer column compared with '' answered 500
    // after five retries and ten seconds; it is one failure now.
    #[test]
    fn data_constraint_and_syntax_errors_fail_fast() {
        for state in [
            "22P02", "22001", "23505", "23503", "23502", "42703", "42P01", "42601", "22003",
            "25P02",
        ] {
            assert_eq!(classify(true, state, "x"), PgErrorClass::Fail, "{state}");
        }
    }

    #[test]
    fn a_result_without_a_sqlstate_on_a_healthy_connection_fails() {
        assert_eq!(classify(true, "", "out of memory"), PgErrorClass::Fail);
    }

    #[test]
    fn a_null_result_on_a_null_connection_reads_as_a_lost_connection() {
        let (class, msg) = classify_result(std::ptr::null_mut(), std::ptr::null());
        assert_eq!(class, PgErrorClass::Retry);
        assert_eq!(msg, "PostgreSQL error (no message)");
    }
}
