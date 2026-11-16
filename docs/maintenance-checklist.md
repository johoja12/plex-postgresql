# Shim Maintenance Checklist

Use this checklist when adding or changing step/interpose helpers.

## Safety rules

1. **No `unsafe` in business logic.** All `unsafe` must be at the FFI boundary (`#[no_mangle] pub extern "C" fn`). Internal functions take `&mut PgStmt` / `&mut PgConnection`, not raw pointers.
2. **No logging inside mutex scope.** Use `log_debug_lazy!` / `log_info_lazy!` outside the guard to avoid ABBA deadlock with the LOGGER mutex.
3. **No nested mutex acquisition.** Lock ordering: `stmt.mutex` (Rust Mutex) before `conn.mutex` (pthread). Never hold conn mutex while acquiring stmt mutex.
4. **Vec-based PgStmt arrays.** Call `ensure_param_capacity()` at prepare time and `ensure_column_capacity()` at first step. Never index without ensuring capacity first.
5. **No `Box::new([T; large])`.** Use `vec![T; n].into_boxed_slice()` for allocations >1KB to avoid stack overflow on Plex's 544K worker threads.

## Code conventions

6. Keep `db_interpose_step.rs` orchestration-only where possible.
7. Use `step_result_t` for helper return values (`ROW/DONE/ERROR/FALLBACK`).
8. If helper can fail at connection level, expose `conn_error_out` and set it explicitly.
9. Be explicit about state side effects (`write_executed`, `read_done`, cached stmt updates).
10. Centralize libpq cancel/drain behavior via `rust_step_conn_cancel_and_drain` (fast path skips PQcancel when not busy).
11. Prefer helper modules (`step_read_utils/`, `step_write_utils/`, `step_cached_read_utils`) over inline logic.
12. Keep logging taxonomy consistent (`READ`, `WRITE`, `CACHED_READ`, `CACHED_WRITE`, `LIFECYCLE`).
13. Update `docs/step-flow.md` when module boundaries or contracts change.

## Performance rules

14. Use `log_debug_lazy!` / `log_info_lazy!` instead of `log_debug(&format!(...))` — the macro checks LOG_LEVEL before `format!()`.
15. Transaction control (`BEGIN`/`COMMIT`/`ROLLBACK`) is **skip-SQL**, not routed to PG. Reasons:
    - **Connection pool mismatch**: Plex's `BEGIN` would go to conn A, but the next `INSERT` may get conn B from the pool — the transaction sits on the wrong connection.
    - **No functional effect**: each PG query is already an implicit transaction. Explicit `BEGIN`/`COMMIT` adds roundtrips for zero benefit.
    - **Memory impact**: routing these to PG previously caused 1.8GB memory growth (thousands of dummy shadow statements + PG roundtrips per minute). Skipping them brought memory from 1.8GB to 59MB.
16. `validate_type_consistency` runs on every column accessor at every log level (since 2026-03-29, so a type mismatch is logged at ERROR), which makes everything it calls part of the cost of reading a value: a library listing reads some 170 columns a row. Nothing on that path may do work in proportion to the statement's text or to a cache's size. Reading the SQL (`CStr::to_str` over about 10 KB) and walking the declared-type cache for an alias that is no key made a whole-library read of 7,665 episodes take 47 s instead of 8 (2026-11-16); test the cheap condition first and remember what was matched.
17. Before a Plex upgrade, run the old and the new server side by side on one shim build and two copies of one database (`Dockerfile.standalone`'s `PLEX_VERSION` build argument), with PostgreSQL logging every statement, and compare the answers and the statement shapes. A stock server of the new version on the same media is the reference for anything the shim might do differently.
