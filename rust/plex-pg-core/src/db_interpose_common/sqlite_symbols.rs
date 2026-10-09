use super::*;

macro_rules! load_sym {
    ($slot:ident, $handle:expr, $name:expr, $ty:ty) => {{
        let slot = ptr::addr_of_mut!($slot);
        if (*slot).is_none() {
            let sym = libc::dlsym($handle, $name.as_ptr() as *const c_char);
            if !sym.is_null() {
                *slot = Some(std::mem::transmute::<*mut libc::c_void, $ty>(sym));
            }
        }
    }};
}

pub(super) unsafe fn read_option<T: Copy>(slot: *const Option<T>) -> Option<T> {
    ptr::read(slot)
}

pub fn rust_common_load_sqlite_symbols(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }

    unsafe {
        load_sym!(orig_sqlite3_open, handle, b"sqlite3_open\0", Sqlite3OpenFn);
        load_sym!(
            orig_sqlite3_open_v2,
            handle,
            b"sqlite3_open_v2\0",
            Sqlite3OpenV2Fn
        );
        load_sym!(
            orig_sqlite3_close,
            handle,
            b"sqlite3_close\0",
            Sqlite3DbToIntFn
        );
        load_sym!(
            orig_sqlite3_close_v2,
            handle,
            b"sqlite3_close_v2\0",
            Sqlite3DbToIntFn
        );

        load_sym!(orig_sqlite3_exec, handle, b"sqlite3_exec\0", Sqlite3ExecFn);
        load_sym!(
            orig_sqlite3_get_table,
            handle,
            b"sqlite3_get_table\0",
            Sqlite3GetTableFn
        );

        load_sym!(
            orig_sqlite3_changes,
            handle,
            b"sqlite3_changes\0",
            Sqlite3DbToIntFn
        );
        load_sym!(
            orig_sqlite3_changes64,
            handle,
            b"sqlite3_changes64\0",
            Sqlite3DbToI64Fn
        );
        load_sym!(
            orig_sqlite3_last_insert_rowid,
            handle,
            b"sqlite3_last_insert_rowid\0",
            Sqlite3DbToI64Fn
        );

        load_sym!(
            orig_sqlite3_errmsg,
            handle,
            b"sqlite3_errmsg\0",
            Sqlite3DbToCStrFn
        );
        load_sym!(
            orig_sqlite3_errcode,
            handle,
            b"sqlite3_errcode\0",
            Sqlite3DbToIntFn
        );
        load_sym!(
            orig_sqlite3_extended_errcode,
            handle,
            b"sqlite3_extended_errcode\0",
            Sqlite3DbToIntFn
        );

        load_sym!(
            orig_sqlite3_prepare,
            handle,
            b"sqlite3_prepare\0",
            Sqlite3PrepareFn
        );
        load_sym!(
            orig_sqlite3_prepare_v2,
            handle,
            b"sqlite3_prepare_v2\0",
            Sqlite3PrepareFn
        );
        load_sym!(
            orig_sqlite3_prepare_v3,
            handle,
            b"sqlite3_prepare_v3\0",
            Sqlite3PrepareV3Fn
        );
        load_sym!(
            orig_sqlite3_prepare16_v2,
            handle,
            b"sqlite3_prepare16_v2\0",
            Sqlite3Prepare16Fn
        );

        load_sym!(
            orig_sqlite3_bind_int,
            handle,
            b"sqlite3_bind_int\0",
            Sqlite3BindIntFn
        );
        load_sym!(
            orig_sqlite3_bind_int64,
            handle,
            b"sqlite3_bind_int64\0",
            Sqlite3BindInt64Fn
        );
        load_sym!(
            orig_sqlite3_bind_double,
            handle,
            b"sqlite3_bind_double\0",
            Sqlite3BindDoubleFn
        );
        load_sym!(
            orig_sqlite3_bind_text,
            handle,
            b"sqlite3_bind_text\0",
            Sqlite3BindTextFn
        );
        load_sym!(
            orig_sqlite3_bind_text64,
            handle,
            b"sqlite3_bind_text64\0",
            Sqlite3BindText64Fn
        );
        load_sym!(
            orig_sqlite3_bind_blob,
            handle,
            b"sqlite3_bind_blob\0",
            Sqlite3BindBlobFn
        );
        load_sym!(
            orig_sqlite3_bind_blob64,
            handle,
            b"sqlite3_bind_blob64\0",
            Sqlite3BindBlob64Fn
        );
        load_sym!(
            orig_sqlite3_bind_value,
            handle,
            b"sqlite3_bind_value\0",
            Sqlite3BindValueFn
        );
        load_sym!(
            orig_sqlite3_bind_null,
            handle,
            b"sqlite3_bind_null\0",
            Sqlite3BindNullFn
        );

        load_sym!(
            orig_sqlite3_step,
            handle,
            b"sqlite3_step\0",
            Sqlite3StmtToIntFn
        );
        load_sym!(
            orig_sqlite3_reset,
            handle,
            b"sqlite3_reset\0",
            Sqlite3StmtToIntFn
        );
        load_sym!(
            orig_sqlite3_finalize,
            handle,
            b"sqlite3_finalize\0",
            Sqlite3StmtToIntFn
        );
        load_sym!(
            orig_sqlite3_clear_bindings,
            handle,
            b"sqlite3_clear_bindings\0",
            Sqlite3StmtToIntFn
        );

        load_sym!(
            orig_sqlite3_column_count,
            handle,
            b"sqlite3_column_count\0",
            Sqlite3StmtToIntFn
        );
        load_sym!(
            orig_sqlite3_column_type,
            handle,
            b"sqlite3_column_type\0",
            Sqlite3StmtIndexToIntFn
        );
        load_sym!(
            orig_sqlite3_column_int,
            handle,
            b"sqlite3_column_int\0",
            Sqlite3StmtIndexToIntFn
        );
        load_sym!(
            orig_sqlite3_column_int64,
            handle,
            b"sqlite3_column_int64\0",
            Sqlite3StmtIndexToI64Fn
        );
        load_sym!(
            orig_sqlite3_column_double,
            handle,
            b"sqlite3_column_double\0",
            Sqlite3StmtIndexToDoubleFn
        );
        load_sym!(
            orig_sqlite3_column_text,
            handle,
            b"sqlite3_column_text\0",
            Sqlite3StmtIndexToTextFn
        );
        load_sym!(
            orig_sqlite3_column_blob,
            handle,
            b"sqlite3_column_blob\0",
            Sqlite3StmtIndexToBlobFn
        );
        load_sym!(
            orig_sqlite3_column_bytes,
            handle,
            b"sqlite3_column_bytes\0",
            Sqlite3StmtIndexToIntFn
        );
        load_sym!(
            orig_sqlite3_column_name,
            handle,
            b"sqlite3_column_name\0",
            Sqlite3StmtIndexToNameFn
        );
        load_sym!(
            orig_sqlite3_column_decltype,
            handle,
            b"sqlite3_column_decltype\0",
            Sqlite3StmtIndexToNameFn
        );
        load_sym!(
            orig_sqlite3_column_value,
            handle,
            b"sqlite3_column_value\0",
            Sqlite3StmtIndexToValueFn
        );
        load_sym!(
            orig_sqlite3_data_count,
            handle,
            b"sqlite3_data_count\0",
            Sqlite3StmtToIntFn
        );

        load_sym!(
            orig_sqlite3_value_type,
            handle,
            b"sqlite3_value_type\0",
            Sqlite3ValueToIntFn
        );
        load_sym!(
            orig_sqlite3_value_text,
            handle,
            b"sqlite3_value_text\0",
            Sqlite3ValueToTextFn
        );
        load_sym!(
            orig_sqlite3_value_int,
            handle,
            b"sqlite3_value_int\0",
            Sqlite3ValueToIntFn
        );
        load_sym!(
            orig_sqlite3_value_int64,
            handle,
            b"sqlite3_value_int64\0",
            Sqlite3ValueToI64Fn
        );
        load_sym!(
            orig_sqlite3_value_double,
            handle,
            b"sqlite3_value_double\0",
            Sqlite3ValueToDoubleFn
        );
        load_sym!(
            orig_sqlite3_value_bytes,
            handle,
            b"sqlite3_value_bytes\0",
            Sqlite3ValueToIntFn
        );
        load_sym!(
            orig_sqlite3_value_blob,
            handle,
            b"sqlite3_value_blob\0",
            Sqlite3ValueToBlobFn
        );

        load_sym!(
            orig_sqlite3_create_collation,
            handle,
            b"sqlite3_create_collation\0",
            Sqlite3CreateCollationFn
        );
        load_sym!(
            orig_sqlite3_create_collation_v2,
            handle,
            b"sqlite3_create_collation_v2\0",
            Sqlite3CreateCollationV2Fn
        );

        load_sym!(orig_sqlite3_free, handle, b"sqlite3_free\0", Sqlite3FreeFn);
        load_sym!(
            orig_sqlite3_malloc,
            handle,
            b"sqlite3_malloc\0",
            Sqlite3MallocFn
        );
        load_sym!(
            orig_sqlite3_db_handle,
            handle,
            b"sqlite3_db_handle\0",
            Sqlite3StmtToDbFn
        );
        load_sym!(
            orig_sqlite3_next_stmt,
            handle,
            b"sqlite3_next_stmt\0",
            Sqlite3NextStmtFn
        );
        load_sym!(
            orig_sqlite3_sql,
            handle,
            b"sqlite3_sql\0",
            Sqlite3StmtToCStrFn
        );
        load_sym!(
            orig_sqlite3_expanded_sql,
            handle,
            b"sqlite3_expanded_sql\0",
            Sqlite3StmtToMutCStrFn
        );
        load_sym!(
            orig_sqlite3_bind_parameter_count,
            handle,
            b"sqlite3_bind_parameter_count\0",
            Sqlite3StmtToIntFn
        );
        load_sym!(
            orig_sqlite3_bind_parameter_index,
            handle,
            b"sqlite3_bind_parameter_index\0",
            Sqlite3StmtNameToIntFn
        );
        load_sym!(
            orig_sqlite3_bind_parameter_name,
            handle,
            b"sqlite3_bind_parameter_name\0",
            Sqlite3StmtIndexToNameFn
        );
        load_sym!(
            orig_sqlite3_stmt_readonly,
            handle,
            b"sqlite3_stmt_readonly\0",
            Sqlite3StmtToIntFn
        );
        load_sym!(
            orig_sqlite3_stmt_busy,
            handle,
            b"sqlite3_stmt_busy\0",
            Sqlite3StmtToIntFn
        );
        load_sym!(
            orig_sqlite3_stmt_status,
            handle,
            b"sqlite3_stmt_status\0",
            Sqlite3StmtIdx2ToIntFn
        );

        if read_option(ptr::addr_of!(shim_sqlite3_prepare_v2)).is_none() {
            *ptr::addr_of_mut!(shim_sqlite3_prepare_v2) =
                read_option(ptr::addr_of!(orig_sqlite3_prepare_v2));
        }
        if read_option(ptr::addr_of!(shim_sqlite3_errmsg)).is_none() {
            *ptr::addr_of_mut!(shim_sqlite3_errmsg) =
                read_option(ptr::addr_of!(orig_sqlite3_errmsg));
        }
        if read_option(ptr::addr_of!(shim_sqlite3_errcode)).is_none() {
            *ptr::addr_of_mut!(shim_sqlite3_errcode) =
                read_option(ptr::addr_of!(orig_sqlite3_errcode));
        }

        let open_fn = read_option(ptr::addr_of!(orig_sqlite3_open));
        if let Some(f) = open_fn {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] orig_sqlite3_open = %p\n\0".as_ptr() as *const c_char,
                f as *const c_void,
            );
        } else {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] WARNING: orig_sqlite3_open is NULL!\n\0".as_ptr() as *const c_char,
            );
        }
        let prep_fn = read_option(ptr::addr_of!(orig_sqlite3_prepare_v2));
        if let Some(f) = prep_fn {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] orig_sqlite3_prepare_v2 = %p\n\0".as_ptr() as *const c_char,
                f as *const c_void,
            );
        } else {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] WARNING: orig_sqlite3_prepare_v2 is NULL!\n\0".as_ptr()
                    as *const c_char,
            );
        }
    }
}

pub fn rust_shim_ensure_ready() -> c_int {
    if SYMBOLS_VERIFIED.load(Ordering::Acquire) != 0 {
        return 1;
    }

    std::sync::atomic::fence(Ordering::SeqCst);

    unsafe {
        if SHIM_INITIALIZED.load(Ordering::Acquire) == 0 {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM] WARNING: shim_ensure_ready called before shim_initialized!\n\0".as_ptr()
                    as *const c_char,
            );
            libc::fflush(stderr_ptr());
            return 0;
        }

        let open_missing = read_option(ptr::addr_of!(orig_sqlite3_open)).is_none();
        let prep_missing = read_option(ptr::addr_of!(orig_sqlite3_prepare_v2)).is_none();
        let step_missing = read_option(ptr::addr_of!(orig_sqlite3_step)).is_none();
        if open_missing || prep_missing || step_missing {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM] WARNING: Critical symbols NULL, attempting fallback...\n\0".as_ptr()
                    as *const c_char,
            );
            libc::fflush(stderr_ptr());

            if cfg!(target_os = "macos") {
                let sh = ptr::read(ptr::addr_of!(sqlite_handle));
                if !sh.is_null() {
                    load_sym!(orig_sqlite3_open, sh, b"sqlite3_open\0", Sqlite3OpenFn);
                    load_sym!(
                        orig_sqlite3_prepare_v2,
                        sh,
                        b"sqlite3_prepare_v2\0",
                        Sqlite3PrepareFn
                    );
                    load_sym!(orig_sqlite3_step, sh, b"sqlite3_step\0", Sqlite3StmtToIntFn);
                }
            } else {
                load_sym!(
                    orig_sqlite3_open,
                    libc::RTLD_NEXT,
                    b"sqlite3_open\0",
                    Sqlite3OpenFn
                );
                load_sym!(
                    orig_sqlite3_prepare_v2,
                    libc::RTLD_NEXT,
                    b"sqlite3_prepare_v2\0",
                    Sqlite3PrepareFn
                );
                load_sym!(
                    orig_sqlite3_step,
                    libc::RTLD_NEXT,
                    b"sqlite3_step\0",
                    Sqlite3StmtToIntFn
                );
            }

            let open_missing = read_option(ptr::addr_of!(orig_sqlite3_open)).is_none();
            let prep_missing = read_option(ptr::addr_of!(orig_sqlite3_prepare_v2)).is_none();
            let step_missing = read_option(ptr::addr_of!(orig_sqlite3_step)).is_none();
            if open_missing || prep_missing || step_missing {
                libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM] FATAL: Cannot resolve critical SQLite symbols!\n\0".as_ptr()
                        as *const c_char,
                );
                libc::fflush(stderr_ptr());
                return 0;
            }
        }
    }

    SYMBOLS_VERIFIED.store(1, Ordering::Release);
    1
}

pub fn rust_reset_symbol_verification() {
    SYMBOLS_VERIFIED.store(0, Ordering::SeqCst);
}
