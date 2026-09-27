use std::os::raw::{c_char, c_int, c_uchar, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, Ordering};

use crate::db_interpose_conn_utils::{log_error, log_info, PthreadMutexGuard};
use crate::env_utils;
use crate::ffi_types::{sqlite3, sqlite3_stmt};

mod common_helpers;
mod exception_context;
mod exception_runtime;
mod exception_support;
mod exception_tracker;
mod fake_values;
mod ffi_exports;
mod lifecycle;
#[cfg(target_os = "linux")]
mod process_policy;
mod sqlite_abi;
mod sqlite_fn_accessors;
mod sqlite_symbols;
mod state;
mod tls_support;
mod worker_runtime;

pub use common_helpers::{
    get_type_name, rewrite_blobs_schema_migrations, rust_get_type_name,
    rust_rewrite_blobs_schema_migrations, rust_simple_str_replace, simple_str_replace,
};
pub(crate) use exception_context::{note_column_phase, copy_context, full_column_trace_enabled};
pub use exception_context::{
    rust_pg_exception_dump_recent_phases, rust_pg_exception_dump_recent_queries,
    rust_pg_exception_note_phase, rust_pg_exception_note_query,
};
pub use exception_runtime::{
    common_handle_exception, common_signal_handler, pg_exception_get_last_column,
    pg_exception_get_last_query, print_exception_info, rust_common_handle_exception,
    rust_common_signal_handler, rust_pg_exception_get_last_column,
    rust_pg_exception_get_last_query, rust_print_exception_info,
};
#[allow(unused_imports)]
use exception_support::{
    env_usize, log_exception_object_dump, log_exception_string_scan, trace_last_query_enabled,
    write_box_line, BOX_BL, BOX_BR, BOX_H, BOX_INNER_WIDTH, BOX_ML, BOX_MR, BOX_TL, BOX_TR,
    TRACE_LAST_QUERY_DEFAULT,
};
use exception_tracker::{
    get_exception_tracker_impl, reset_exception_tracking_impl, ExceptionTypeTracker,
};
pub use fake_values::{rust_pg_check_fake_value, PgFakeValue};
pub(crate) use fake_values::{FAKE_VALUES, MAX_FAKE_VALUES, PG_FAKE_VALUE_MAGIC};
pub use ffi_exports::{
    common_load_sqlite_symbols, delegate_prepare_to_worker, get_exception_tracker,
    is_blobs_db_path, is_library_db_path, pg_check_fake_value, pg_exception_dump_recent_phases,
    pg_exception_dump_recent_queries, pg_exception_note_phase, pg_exception_note_query,
    reset_exception_tracking, reset_symbol_verification, rust_get_exception_tracker,
    rust_reset_exception_tracking, shim_ensure_ready, worker_cleanup, worker_init,
};
pub use lifecycle::{
    common_atfork_child, common_atfork_parent, common_atfork_prepare, common_check_fork,
    common_shim_cleanup, common_shim_init_modules, rust_common_atfork_child,
    rust_common_atfork_parent, rust_common_atfork_prepare, rust_common_check_fork,
    rust_common_shim_cleanup, rust_common_shim_init_modules,
};
#[cfg(target_os = "linux")]
pub use process_policy::{
    linux_apply_current_process_role_policy, linux_apply_process_role_policy,
    linux_handle_fork_child,
};
#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub(crate) use process_policy::{
    linux_process_name_is_primary, linux_process_name_requires_passthrough,
};
pub(crate) use sqlite_abi::*;
pub(crate) use sqlite_fn_accessors::*;
pub use sqlite_symbols::{
    rust_common_load_sqlite_symbols, rust_reset_symbol_verification, rust_shim_ensure_ready,
};
use state::*;
pub use state::{
    orig_sqlite3_bind_blob, orig_sqlite3_bind_blob64, orig_sqlite3_bind_double,
    orig_sqlite3_bind_int, orig_sqlite3_bind_int64, orig_sqlite3_bind_null,
    orig_sqlite3_bind_parameter_count, orig_sqlite3_bind_parameter_index,
    orig_sqlite3_bind_parameter_name, orig_sqlite3_bind_text, orig_sqlite3_bind_text64,
    orig_sqlite3_bind_value, orig_sqlite3_changes, orig_sqlite3_changes64,
    orig_sqlite3_clear_bindings, orig_sqlite3_close, orig_sqlite3_close_v2,
    orig_sqlite3_column_blob, orig_sqlite3_column_bytes, orig_sqlite3_column_count,
    orig_sqlite3_column_decltype, orig_sqlite3_column_double, orig_sqlite3_column_int,
    orig_sqlite3_column_int64, orig_sqlite3_column_name, orig_sqlite3_column_text,
    orig_sqlite3_column_type, orig_sqlite3_column_value, orig_sqlite3_create_collation,
    orig_sqlite3_create_collation_v2, orig_sqlite3_data_count, orig_sqlite3_db_handle,
    orig_sqlite3_errcode, orig_sqlite3_errmsg, orig_sqlite3_exec, orig_sqlite3_expanded_sql,
    orig_sqlite3_extended_errcode, orig_sqlite3_finalize, orig_sqlite3_free,
    orig_sqlite3_get_table, orig_sqlite3_last_insert_rowid, orig_sqlite3_malloc, orig_sqlite3_open,
    orig_sqlite3_open_v2, orig_sqlite3_prepare, orig_sqlite3_prepare16_v2, orig_sqlite3_prepare_v2,
    orig_sqlite3_prepare_v3, orig_sqlite3_reset, orig_sqlite3_sql, orig_sqlite3_step,
    orig_sqlite3_stmt_busy, orig_sqlite3_stmt_readonly, orig_sqlite3_stmt_status,
    orig_sqlite3_value_blob, orig_sqlite3_value_bytes, orig_sqlite3_value_double,
    orig_sqlite3_value_int, orig_sqlite3_value_int64, orig_sqlite3_value_text,
    orig_sqlite3_value_type, shim_sqlite3_errcode, shim_sqlite3_errmsg, shim_sqlite3_prepare_v2,
    sqlite_handle, CxaDemangleFn, CXA_DEMANGLE_FN, SHIM_INITIALIZED, SHIM_PASSTHROUGH_ONLY,
};
pub(crate) use state::{
    CRASH_COLUMN_MAX_LEN as CRASH_LAST_COLUMN_MAX_LEN, CRASH_LAST_COLUMN, CRASH_LAST_COLUMN_LEN,
    CRASH_LAST_COLUMN_SEQ,
};
pub(crate) use state::{GLOBAL_COLUMN_TYPE_CALLS, GLOBAL_VALUE_TYPE_CALLS};
pub(crate) use tls_support::{
    stderr_ptr, tls_column_type_calls_ptr, tls_in_interpose_call_ptr, tls_in_resolve_tables_ptr,
    tls_last_query_ptr, tls_prepare_v2_depth_ptr, tls_value_type_calls_ptr,
};
#[cfg(target_os = "linux")]
pub(crate) use worker_runtime::fast_mark_fork_child_passthrough;
pub use worker_runtime::{rust_delegate_prepare_to_worker, rust_worker_cleanup, rust_worker_init};

#[cfg(test)]
mod tests;
