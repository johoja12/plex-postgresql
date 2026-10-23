use super::*;
use crate::log_debug_lazy;

#[no_mangle]
pub extern "C" fn rust_step_write_execute_and_finalize(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    unsafe {
        if !pg_conn_error_out.is_null() {
            *pg_conn_error_out = 0;
        }
        if pg_stmt.is_null() || exec_conn.is_null() || (&*exec_conn).conn.is_null() {
            if !pg_stmt.is_null() {
                let s = &mut *pg_stmt;
                s.write_executed = 1;
            }
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }
        let stmt = &mut *pg_stmt;
        let ec = &mut *exec_conn;
        let mut conn_guard = PthreadMutexGuard::lock(&mut ec.mutex as *mut _);

        if skip_stats_resources_update()
            && !stmt.pg_sql.is_null()
            && starts_with_icase_bytes(cstr_bytes(stmt.pg_sql), b"UPDATE")
            && contains_icase_bytes(cstr_bytes(stmt.pg_sql), b"statistics_resources")
        {
            log_error("STEP WRITE: skipping statistics_resources UPDATE via PLEX_PG_SKIP_STATS_RESOURCES_UPDATE");
            conn_guard.unlock();
            stmt.write_executed = 1;
            return STEP_RESULT_DONE;
        }

        let res: *mut PGresult = if stmt.use_prepared != 0 && stmt.stmt_name[0] != 0 {
            let mut cached_name: *const c_char = std::ptr::null();
            let mut is_cached = crate::pg_client::rust_stmt_cache_lookup(
                exec_conn as *mut c_void,
                stmt.sql_hash,
                &mut cached_name,
            ) != 0;

            if !is_cached {
                let prep_res = crate::libpq_helpers::rust_pq_prepare(
                    ec.conn,
                    stmt.stmt_name.as_ptr(),
                    stmt.pg_sql,
                    stmt.param_count,
                    std::ptr::null(),
                );
                if crate::libpq_helpers::rust_pq_result_status(prep_res) == PGRES_COMMAND_OK {
                    crate::pg_client::rust_stmt_cache_add(
                        exec_conn as *mut c_void,
                        stmt.sql_hash,
                        stmt.stmt_name.as_ptr(),
                        stmt.param_count,
                    );
                    cached_name = stmt.stmt_name.as_ptr();
                    is_cached = true;
                } else if is_duplicate_prepared_stmt(prep_res) {
                    crate::pg_client::rust_stmt_cache_add(
                        exec_conn as *mut c_void,
                        stmt.sql_hash,
                        stmt.stmt_name.as_ptr(),
                        stmt.param_count,
                    );
                    cached_name = stmt.stmt_name.as_ptr();
                    is_cached = true;
                } else {
                    log_debug_lazy!(
                        "PQprepare (write) failed for {}: {}",
                        cstr_to_string_or(stmt.stmt_name.as_ptr(), ""),
                        cstr_to_string_or(
                            crate::libpq_helpers::rust_pq_error_message(ec.conn),
                            "(null)"
                        )
                    );
                }
                crate::libpq_helpers::rust_pq_clear(prep_res);
            }

            if is_cached && !cached_name.is_null() {
                crate::libpq_helpers::rust_pq_exec_prepared(
                    ec.conn,
                    cached_name,
                    stmt.param_count,
                    param_values,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            } else {
                crate::libpq_helpers::rust_pq_exec_params(
                    ec.conn,
                    stmt.pg_sql,
                    stmt.param_count,
                    std::ptr::null(),
                    param_values,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            }
        } else {
            crate::libpq_helpers::rust_pq_exec_params(
                ec.conn,
                stmt.pg_sql,
                stmt.param_count,
                std::ptr::null(),
                param_values,
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        };

        conn_guard.unlock();

        let status = crate::libpq_helpers::rust_pq_result_status(res);
        if status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK {
            let cmd_tuples = crate::libpq_helpers::rust_pq_cmd_tuples(res);
            let tuples_ptr = if cmd_tuples.is_null() {
                b"1\0".as_ptr() as *const c_char
            } else {
                cmd_tuples
            };
            ec.last_changes = crate::db_interpose_helpers::rust_pg_text_to_int(tuples_ptr);

            if status == PGRES_TUPLES_OK && crate::libpq_helpers::rust_pq_ntuples(res) > 0 {
                let mut id_buf = [0 as c_char; 64];
                let mut id_str: *const c_char = std::ptr::null();
                if crate::db_interpose_helpers::rust_pg_result_text_copy(
                    res as *const crate::db_interpose_helpers::PGresult,
                    0,
                    0,
                    id_buf.as_mut_ptr(),
                    id_buf.len(),
                ) >= 0
                {
                    id_str = id_buf.as_ptr();
                }
                if !id_str.is_null() && !CStr::from_ptr(id_str).to_bytes().is_empty() {
                    let rowid = crate::db_interpose_helpers::rust_pg_text_to_int64(id_str);
                    if rowid > 0 {
                        ec.last_insert_rowid = rowid;
                        ec.last_insert_failed.store(0, Ordering::Release);
                        crate::pg_client::rust_set_global_last_insert_rowid(rowid);
                    }

                    if !stmt.pg_sql.is_null()
                        && contains_bytes(cstr_bytes(stmt.pg_sql), b"play_queue_generators")
                    {
                        log_debug_lazy!(
                            "STEP play_queue_generators: RETURNING id = {} on thread {:p} conn {:p}",
                            cstr_to_string_or(id_str, "?"),
                            libc::pthread_self() as *mut c_void,
                            exec_conn
                        );
                    }
                    let meta_id = crate::pg_statement::rust_extract_metadata_id(stmt.sql);
                    if meta_id > 0 {
                        crate::pg_client::rust_set_global_metadata_id(meta_id);
                    }
                }
            }
        } else {
            // A write PostgreSQL refused used to read as SQLITE_DONE, with the
            // previous statement's changes() and last_insert_rowid: a
            // constraint violation was a success to Plex. It is an error now,
            // reported the way SQLite reports one.
            let (class, message) = crate::pg_client::classify_result(ec.conn, res);
            log_error(&format!("STEP PG write error ({:?}): {}", class, message));
            log_error(&format!(
                "  Original SQL: {}",
                cstr_prefix(stmt.sql, 300, "(null)")
            ));
            log_error(&format!(
                "  Translated SQL: {}",
                cstr_prefix(stmt.pg_sql, 300, "(null)")
            ));
            if !res.is_null() {
                crate::libpq_helpers::rust_pq_clear(res);
            }
            stmt.write_executed = 1;
            match class {
                PgErrorClass::StalePlan => {
                    crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                }
                PgErrorClass::Retry => {
                    crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                }
                PgErrorClass::Fail => {
                    note_failed_write(ec, stmt.sql, &message);
                }
            }
            return STEP_RESULT_ERROR;
        }

        stmt.write_executed = 1;
        if plex_reads_returning_rows(
            cstr_bytes(stmt.sql),
            status,
            crate::libpq_helpers::rust_pq_ntuples(res),
        ) {
            // Plex steps a RETURNING write as SQLite does: SQLITE_ROW with
            // the row, then SQLITE_DONE (write_executed, above). The result
            // replaces the metadata-only one a column call may have made.
            let _stmt_guard = PgStmt::lock_mutex(pg_stmt);
            if !stmt.result.is_null() {
                crate::libpq_helpers::rust_pq_clear(stmt.result);
            }
            stmt.result = res;
            stmt.metadata_only_result = 0;
            stmt.num_rows = crate::libpq_helpers::rust_pq_ntuples(res);
            stmt.num_cols = crate::libpq_helpers::rust_pq_nfields(res);
            stmt.ensure_column_capacity(stmt.num_cols.max(0) as usize);
            stmt.current_row = 0;
            stmt.set_result_conn(exec_conn);
            return STEP_RESULT_ROW;
        }
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
        STEP_RESULT_DONE
    }
}

/// Whether the rows a write came back with are Plex's to read: its own SQL
/// named RETURNING, and PostgreSQL answered rows. The `RETURNING id` the
/// shim appends to an INSERT of its own accord feeds `last_insert_rowid`
/// alone, and such a step stays SQLITE_DONE, as it was. PMS 1.43.5's
/// migration 202609141200 upserts a preference with `... ON CONFLICT(name)
/// DO UPDATE SET value = excluded.value RETURNING id` and reads the id from
/// the row: given SQLITE_DONE and no row it threw, rolled the migration
/// back and the server never set up (2026-10-22).
pub(crate) fn plex_reads_returning_rows(orig_sql: &[u8], status: c_int, ntuples: c_int) -> bool {
    if status != PGRES_TUPLES_OK || ntuples <= 0 {
        return false;
    }
    let Ok(sql) = std::str::from_utf8(orig_sql) else { return false; };
    let Ok(tokens) = sqlparser::tokenizer::Tokenizer::new(
        &sqlparser::dialect::SQLiteDialect {}, sql,
    ).tokenize() else { return false; };
    tokens.iter().any(|t| matches!(t, sqlparser::tokenizer::Token::Word(w)
        if w.quote_style.is_none() && w.keyword == sqlparser::keywords::Keyword::RETURNING))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPSERT: &[u8] = b"INSERT INTO preferences (`name`,`value`) VALUES (:U1,:U2) ON CONFLICT(`name`) DO UPDATE SET `value`=excluded.`value` RETURNING `id`";

    #[test]
    fn a_returning_write_of_plexs_own_is_a_row() {
        assert!(plex_reads_returning_rows(UPSERT, PGRES_TUPLES_OK, 1));
        assert!(plex_reads_returning_rows(
            b"UPDATE metadata_items SET title = ? WHERE id = ? returning id, title",
            PGRES_TUPLES_OK,
            3
        ));
    }

    #[test]
    fn the_shims_own_returning_id_stays_done() {
        // Plex's SQL named no RETURNING; the shim appended one for last_insert_rowid.
        assert!(!plex_reads_returning_rows(
            b"INSERT INTO media_parts (directory, file) VALUES (?, ?)",
            PGRES_TUPLES_OK,
            1
        ));
    }

    #[test]
    fn returning_in_values_identifiers_or_comments_is_not_a_clause() {
        for sql in [
            b"INSERT INTO preferences(name,value) VALUES('RETURNING','x')".as_slice(),
            b"INSERT INTO preferences(name,value) VALUES('x','y') /* RETURNING */",
            b"INSERT INTO preferences(name,value) VALUES('x','y') -- RETURNING id",
            b"UPDATE metadata_items SET `RETURNING` = 1",
        ] {
            assert!(!plex_reads_returning_rows(sql, PGRES_TUPLES_OK, 1));
        }
    }

    #[test]
    fn a_returning_write_that_touched_no_row_is_done() {
        assert!(!plex_reads_returning_rows(
            b"INSERT INTO schema_migrations (version) VALUES (?) ON CONFLICT DO NOTHING RETURNING version",
            PGRES_TUPLES_OK,
            0
        ));
        assert!(!plex_reads_returning_rows(UPSERT, PGRES_COMMAND_OK, 0));
    }
}

/// Record a write that PostgreSQL refused on the connection Plex will ask:
/// the message for `sqlite3_errmsg`, `changes()` of 0, and -- for an INSERT
/// -- the flag that makes `sqlite3_last_insert_rowid` answer 0 rather than
/// the previous row's id (see `PgConnection::last_insert_failed`).
pub(crate) unsafe fn note_failed_write(conn: &mut PgConnection, sql: *const c_char, message: &str) {
    conn.set_last_error(SQLITE_ERROR, message);
    conn.last_changes = 0;
    if starts_with_icase_bytes(cstr_bytes(sql), b"INSERT") {
        conn.last_insert_failed.store(1, Ordering::Release);
    }
}
