use super::*;
use crate::log_debug_lazy;

/// Whether a write is one the shim answers without sending: the junk
/// `metadata_items` row below.
///
/// `statistics_media` inserts used to be skipped here too when their count
/// and duration were "empty" (upstream's "310M empty rows bug", 2025-12-31),
/// and the schema dump's `statistics_media_reject_empty` trigger dropped the
/// same rows in PostgreSQL. PMS opens every statistics bucket empty -- count
/// 0, duration 0 -- and then adds to it with `UPDATE statistics_media SET
/// count=?, duration=? WHERE id=<the inserted row's id>`, so between them no
/// bucket ever existed: talos held 0 rows against a sequence at 16445
/// (2026-11-10). Neither answered an id either, so PMS read back the
/// connection's last insert from another table (its updates then hit a row
/// of nothing), or 0 on a connection that had inserted nothing yet: right
/// after a restart with a session playing, PMS threw `DB::Exception` there
/// and logged "Uncaught exception running async task ... std::exception".
/// (The skip's own check was off by one as well: it read the count from
/// parameter 6, which is the duration in PMS 1.43.5's seven.) The insert now
/// runs as SQLite runs it, and `pg_compat_functions.sql` drops the trigger.
/// On the kind rig a session's statistics then held one row per bucket, each
/// updated in place, with no new row after the first five (2026-11-10).
#[no_mangle]
pub extern "C" fn rust_step_write_should_skip_special_insert(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
) -> c_int {
    if pg_stmt.is_null() {
        return 0;
    }
    let stmt = unsafe { &mut *pg_stmt };
    if stmt.pg_sql.is_null() {
        return 0;
    }

    unsafe {
        let pg_sql_bytes = cstr_bytes(stmt.pg_sql);
        if contains_icase_bytes(pg_sql_bytes, b"INSERT INTO")
            && contains_icase_bytes(pg_sql_bytes, b"metadata_items")
            && !contains_icase_bytes(pg_sql_bytes, b"metadata_item_settings")
            && !contains_icase_bytes(pg_sql_bytes, b"metadata_item_views")
            && !contains_icase_bytes(pg_sql_bytes, b"metadata_item_accounts")
            && !contains_icase_bytes(pg_sql_bytes, b"metadata_item_clusters")
        {
            let lib_col = CString::new("library_section_id").unwrap();
            let type_col = CString::new("metadata_type").unwrap();
            let lib_idx = crate::db_interpose_helpers::rust_find_insert_column_index(
                stmt.pg_sql,
                lib_col.as_ptr(),
            );
            let type_idx = crate::db_interpose_helpers::rust_find_insert_column_index(
                stmt.pg_sql,
                type_col.as_ptr(),
            );

            if lib_idx >= 0
                && type_idx >= 0
                && lib_idx < stmt.param_count
                && type_idx < stmt.param_count
            {
                let lib_val = param_at(param_values, lib_idx as usize);
                let type_val = param_at(param_values, type_idx as usize);
                if lib_val.is_null() && type_val.is_null() {
                    log_error(&format!(
                        "GUARD: Blocked junk INSERT into metadata_items (library_section_id=NULL, metadata_type=NULL) param_count={} lib_idx={} type_idx={}",
                        stmt.param_count, lib_idx, type_idx
                    ));

                    if !exec_conn.is_null() && !(&*exec_conn).conn.is_null() {
                        let ec2 = &mut *exec_conn;
                        let _conn_guard = PthreadMutexGuard::lock(&mut ec2.mutex as *mut _);
                        if !ec2.conn.is_null()
                            && crate::libpq_helpers::rust_pq_status(ec2.conn) == CONNECTION_OK
                        {
                            let seq_res = crate::libpq_helpers::rust_pq_exec(
                                ec2.conn,
                                b"SELECT nextval('plex.metadata_items_id_seq')\0".as_ptr()
                                    as *const c_char,
                            );
                            if crate::libpq_helpers::rust_pq_result_status(seq_res)
                                == PGRES_TUPLES_OK
                                && crate::libpq_helpers::rust_pq_ntuples(seq_res) > 0
                            {
                                let mut seq_buf = [0 as c_char; 64];
                                let mut seq_val: *const c_char = std::ptr::null();
                                if crate::db_interpose_helpers::rust_pg_result_text_copy(
                                    seq_res as *const crate::db_interpose_helpers::PGresult,
                                    0,
                                    0,
                                    seq_buf.as_mut_ptr(),
                                    seq_buf.len(),
                                ) >= 0
                                {
                                    seq_val = seq_buf.as_ptr();
                                }
                                log_debug_lazy!(
                                    "GUARD: Advanced metadata_items sequence to {}",
                                    cstr_to_string_or(seq_val, "?")
                                );
                            }
                            crate::libpq_helpers::rust_pq_clear(seq_res);
                        }
                    }

                    stmt.write_executed = 1;
                    return 1;
                }
            }
        }

        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PMS 1.43.5's statistics bucket insert, as the translator sends it,
    /// with the seven values PMS binds; `count` and `duration` are the last
    /// two. Answers whether the shim would skip it, and that it did not
    /// claim to have run it.
    fn statistics_insert_skipped(count: &str, duration: &str) -> c_int {
        let sql = CString::new(
            "INSERT INTO statistics_media (\"account_id\", \"device_id\", \"timespan\", \"at\", \
             \"metadata_type\", \"count\", \"duration\") VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING id",
        )
        .unwrap();
        let values: Vec<CString> = ["1", "1", "4", "1794320700", "1", count, duration]
            .iter()
            .map(|v| CString::new(*v).unwrap())
            .collect();
        let ptrs: Vec<*const c_char> = values.iter().map(|v| v.as_ptr()).collect();
        let mut stmt = PgStmt::default();
        stmt.pg_sql = sql.as_ptr() as *mut c_char;
        stmt.param_count = 7;
        let rc = rust_step_write_should_skip_special_insert(
            &mut stmt,
            std::ptr::null_mut(),
            ptrs.as_ptr(),
        );
        let executed = stmt.write_executed;
        stmt.pg_sql = std::ptr::null_mut();
        assert_eq!(executed, 0, "a skipped insert is marked executed");
        rc
    }

    /// The bucket PMS opens when a play starts: count 1, duration 0. The old
    /// check read the duration as the count and dropped it.
    #[test]
    fn a_statistics_bucket_pms_opens_is_inserted() {
        assert_eq!(statistics_insert_skipped("1", "0"), 0);
    }

    /// An empty bucket is inserted as SQLite would insert it, so PMS gets a
    /// row id of its own to update.
    #[test]
    fn an_empty_statistics_bucket_is_inserted() {
        assert_eq!(statistics_insert_skipped("0", "0"), 0);
    }
}
