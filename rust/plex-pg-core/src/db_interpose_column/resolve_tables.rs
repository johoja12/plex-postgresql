use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(super) fn resolve_column_tables_impl(
    pg_stmt: *mut PgStmt,
    pg_conn: *mut PgConnection,
) -> c_int {
    struct ResolveGuard(*mut c_int);
    impl Drop for ResolveGuard {
        fn drop(&mut self) {
            unsafe {
                *self.0 = 0;
            }
        }
    }
    let flag = tls_in_resolve_tables_ptr();
    unsafe {
        if *flag != 0 {
            log_debug("RESOLVE_TABLES: Recursion detected, aborting");
            if !pg_stmt.is_null() {
                (&mut *pg_stmt).col_tables_resolved = 1;
            }
            return -1;
        }
        *flag = 1;
    }
    let _guard = ResolveGuard(flag);

    if pg_stmt.is_null() {
        return 0;
    }

    let pg_stmt_ref = unsafe { &mut *pg_stmt };

    if pg_stmt_ref.result.is_null() || pg_stmt_ref.col_tables_resolved != 0 {
        return 0;
    }

    pg_stmt_ref.column_decltypes.clear();
    let num_cols = pg_stmt_ref.num_cols;
    if num_cols <= 0 {
        pg_stmt_ref.col_tables_resolved = 1;
        return 0;
    }

    let nc = num_cols as usize;
    let mut table_oids = vec![0u32; nc];
    let mut uncached_oids = vec![0usize; nc];
    let mut num_unique_tables = 0usize;
    let mut num_uncached = 0usize;
    let mut cache_hits = 0usize;

    for i in 0..nc {
        let table_oid = crate::db_interpose_helpers::rust_pg_result_col_table_oid(
            helpers_result_ptr(pg_stmt_ref.result),
            i as c_int,
        );
        if table_oid == INVALID_OID {
            continue;
        }

        let cached_name = crate::db_interpose_helpers::rust_oid_table_cache_lookup(table_oid);
        if !cached_name.is_null() {
            let dup = unsafe { libc::strdup(cached_name) };
            if !dup.is_null() {
                pg_stmt_ref.col_table_names[i] = dup;
                cache_hits += 1;
            }
            continue;
        }

        let mut found = false;
        for j in 0..num_unique_tables {
            if table_oids[j] == table_oid {
                found = true;
                break;
            }
        }
        if !found && num_unique_tables < nc {
            table_oids[num_unique_tables] = table_oid;
            uncached_oids[num_uncached] = num_unique_tables;
            num_unique_tables += 1;
            num_uncached += 1;
        }
    }

    if num_uncached == 0 {
        pg_stmt_ref.col_tables_resolved = 1;
        if cache_hits > 0 {
            log_debug_lazy!(
                "RESOLVE_TABLES: All {} columns resolved from cache (0 queries)",
                cache_hits
            );
        }
        return 0;
    }

    if pg_conn.is_null() {
        log_debug("RESOLVE_TABLES: No connection available");
        pg_stmt_ref.col_tables_resolved = 1;
        return -1;
    }
    let pc = unsafe { &*pg_conn };
    if pc.conn.is_null() {
        log_debug("RESOLVE_TABLES: No connection available");
        pg_stmt_ref.col_tables_resolved = 1;
        return -1;
    }

    let mut resolve_conn = pg_conn;
    if pc.streaming_active.load(Ordering::SeqCst) != 0 {
        log_debug_lazy!(
            "RESOLVE_TABLES: Connection {:p} is streaming_active",
            pg_conn
        );
        if !env_utils::env_truthy_str("PLEX_PG_ENABLE_RESOLVE_TABLES_ALT_CONN") {
            log_debug(
                "RESOLVE_TABLES: skipping OID lookup while streaming to avoid reentrant alternate connection acquisition",
            );
            pg_stmt_ref.col_tables_resolved = 1;
            return 0;
        }

        log_debug("RESOLVE_TABLES: alternate connection lookup explicitly enabled");
        let alt_conn = unsafe {
            pg_get_thread_connection_excluding(pc.db_path.as_ptr(), pg_conn as *const c_void)
        };
        if alt_conn.is_null()
            || unsafe { (&*alt_conn).conn.is_null() }
            || alt_conn == pg_conn
            || unsafe { (&*alt_conn).streaming_active.load(Ordering::SeqCst) != 0 }
        {
            log_debug(
                "RESOLVE_TABLES: No alternate connection, skipping OID lookup to protect streaming",
            );
            pg_stmt_ref.col_tables_resolved = 1;
            return 0;
        }

        resolve_conn = alt_conn;
        log_debug_lazy!(
            "RESOLVE_TABLES: Using alternate connection {:p} for OID lookup",
            resolve_conn
        );
    }

    let mut query = String::from("SELECT oid, relname FROM pg_class WHERE oid IN (");
    for i in 0..num_unique_tables {
        if i > 0 {
            query.push(',');
        }
        query.push_str(&format!("{}", table_oids[i]));
    }
    query.push(')');
    let query_cs = match CString::new(query) {
        Ok(cs) => cs,
        Err(_) => {
            log_error("RESOLVE_TABLES: failed to build OID query");
            pg_stmt_ref.col_tables_resolved = 1;
            return -1;
        }
    };

    let rc = unsafe { &mut *resolve_conn };
    let _conn_guard = unsafe { PthreadMutexGuard::lock(&mut rc.mutex as *mut _) };
    let res = crate::libpq_helpers::rust_pq_exec(rc.conn, query_cs.as_ptr());

    if res.is_null() || crate::libpq_helpers::rust_pq_result_status(res) != PGRES_TUPLES_OK {
        log_error(&format!(
            "RESOLVE_TABLES: Query failed: {}",
            if res.is_null() {
                "NULL result".to_string()
            } else {
                cstr_to_string_or(crate::libpq_helpers::rust_pq_error_message(rc.conn), "?")
            }
        ));
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
        pg_stmt_ref.col_tables_resolved = 1;
        return -1;
    }

    let num_results = crate::libpq_helpers::rust_pq_ntuples(res);
    let nr = num_results.max(0) as usize;
    let mut result_oids = vec![0u32; nr];
    let mut result_names: Vec<[c_char; 64]> = vec![[0 as c_char; 64]; nr];

    for i in 0..nr {
        let mut oid_buf = [0 as c_char; 64];
        let ok_oid = crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(res),
            i as c_int,
            0,
            oid_buf.as_mut_ptr(),
            oid_buf.len(),
        );
        if ok_oid < 0 {
            continue;
        }
        let oid_val = unsafe { libc::atol(oid_buf.as_ptr()) } as u32;
        result_oids[i] = oid_val;

        let ok_name = crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(res),
            i as c_int,
            1,
            result_names[i].as_mut_ptr(),
            result_names[i].len(),
        );
        if ok_name < 0 {
            result_names[i][0] = 0;
            continue;
        }

        crate::db_interpose_helpers::rust_oid_table_cache_insert(oid_val, result_names[i].as_ptr());
    }

    crate::libpq_helpers::rust_pq_clear(res);

    for i in 0..nc {
        if pg_stmt_ref.col_table_names[i].is_null() {
            let table_oid = crate::db_interpose_helpers::rust_pg_result_col_table_oid(
                helpers_result_ptr(pg_stmt_ref.result),
                i as c_int,
            );
            if table_oid == INVALID_OID {
                continue;
            }
            for j in 0..nr {
                if result_oids[j] == table_oid {
                    let dup = unsafe { libc::strdup(result_names[j].as_ptr()) };
                    if !dup.is_null() {
                        pg_stmt_ref.col_table_names[i] = dup;
                        log_debug_lazy!(
                            "RESOLVE_TABLES: col[{}] '{}' -> table '{}'",
                            i,
                            cstr_to_string_or(
                                crate::db_interpose_helpers::rust_pg_result_col_name(
                                    helpers_result_ptr(pg_stmt_ref.result),
                                    i as c_int,
                                ),
                                "?",
                            ),
                            cstr_to_string_or(result_names[j].as_ptr(), "?")
                        );
                    }
                    break;
                }
            }
        }
    }

    pg_stmt_ref.col_tables_resolved = 1;
    log_info_lazy!(
        "RESOLVE_TABLES: Resolved {} columns ({} from cache, {} from query)",
        num_cols,
        cache_hits,
        num_unique_tables
    );
    0
}
