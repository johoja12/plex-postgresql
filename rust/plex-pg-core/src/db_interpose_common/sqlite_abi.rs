use super::*;

pub(crate) type CollationCompare =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int>;
pub(crate) type CollationDestroy = Option<unsafe extern "C" fn(*mut c_void)>;

pub(crate) type Sqlite3OpenFn = unsafe extern "C" fn(*const c_char, *mut *mut sqlite3) -> c_int;
pub(crate) type Sqlite3OpenV2Fn =
    unsafe extern "C" fn(*const c_char, *mut *mut sqlite3, c_int, *const c_char) -> c_int;
pub(crate) type Sqlite3DbToIntFn = unsafe extern "C" fn(*mut sqlite3) -> c_int;
pub(crate) type Sqlite3DbToI64Fn = unsafe extern "C" fn(*mut sqlite3) -> i64;
pub(crate) type Sqlite3DbToCStrFn = unsafe extern "C" fn(*mut sqlite3) -> *const c_char;
pub(crate) type Sqlite3ExecCallback =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>;
pub(crate) type Sqlite3ExecFn = unsafe extern "C" fn(
    *mut sqlite3,
    *const c_char,
    Sqlite3ExecCallback,
    *mut c_void,
    *mut *mut c_char,
) -> c_int;
pub(crate) type Sqlite3GetTableFn = unsafe extern "C" fn(
    *mut sqlite3,
    *const c_char,
    *mut *mut *mut c_char,
    *mut c_int,
    *mut c_int,
    *mut *mut c_char,
) -> c_int;
pub(crate) type Sqlite3PrepareFn = unsafe extern "C" fn(
    *mut sqlite3,
    *const c_char,
    c_int,
    *mut *mut sqlite3_stmt,
    *mut *const c_char,
) -> c_int;
pub(crate) type Sqlite3PrepareV3Fn = unsafe extern "C" fn(
    *mut sqlite3,
    *const c_char,
    c_int,
    c_uint,
    *mut *mut sqlite3_stmt,
    *mut *const c_char,
) -> c_int;
pub(crate) type Sqlite3Prepare16Fn = unsafe extern "C" fn(
    *mut sqlite3,
    *const c_void,
    c_int,
    *mut *mut sqlite3_stmt,
    *mut *const c_void,
) -> c_int;
pub(crate) type Sqlite3BindIntFn = unsafe extern "C" fn(*mut sqlite3_stmt, c_int, c_int) -> c_int;
pub(crate) type Sqlite3BindInt64Fn = unsafe extern "C" fn(*mut sqlite3_stmt, c_int, i64) -> c_int;
pub(crate) type Sqlite3BindDoubleFn = unsafe extern "C" fn(*mut sqlite3_stmt, c_int, f64) -> c_int;
pub(crate) type Sqlite3BindTextFn =
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_char, c_int, *mut c_void) -> c_int;
pub(crate) type Sqlite3BindText64Fn = unsafe extern "C" fn(
    *mut sqlite3_stmt,
    c_int,
    *const c_char,
    u64,
    *mut c_void,
    c_uchar,
) -> c_int;
pub(crate) type Sqlite3BindBlobFn =
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_void, c_int, *mut c_void) -> c_int;
pub(crate) type Sqlite3BindBlob64Fn =
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_void, u64, *mut c_void) -> c_int;
pub(crate) type Sqlite3BindValueFn =
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const crate::ffi_types::sqlite3_value) -> c_int;
pub(crate) type Sqlite3BindNullFn = unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int;
pub(crate) type Sqlite3StmtToIntFn = unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int;
pub(crate) type Sqlite3StmtToDbFn = unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut sqlite3;
pub(crate) type Sqlite3NextStmtFn =
    unsafe extern "C" fn(*mut sqlite3, *mut sqlite3_stmt) -> *mut sqlite3_stmt;
pub(crate) type Sqlite3StmtToCStrFn = unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char;
pub(crate) type Sqlite3StmtToMutCStrFn = unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut c_char;
pub(crate) type Sqlite3StmtIndexToIntFn = unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int;
pub(crate) type Sqlite3StmtIndexToI64Fn = unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> i64;
pub(crate) type Sqlite3StmtIndexToDoubleFn = unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> f64;
pub(crate) type Sqlite3StmtIndexToTextFn =
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_uchar;
pub(crate) type Sqlite3StmtIndexToBlobFn =
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_void;
pub(crate) type Sqlite3StmtIndexToNameFn =
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_char;
pub(crate) type Sqlite3StmtIndexToValueFn =
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *mut crate::ffi_types::sqlite3_value;
pub(crate) type Sqlite3StmtIdx2ToIntFn =
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, c_int) -> c_int;
pub(crate) type Sqlite3StmtNameToIntFn =
    unsafe extern "C" fn(*mut sqlite3_stmt, *const c_char) -> c_int;
pub(crate) type Sqlite3ValueToIntFn =
    unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> c_int;
pub(crate) type Sqlite3ValueToI64Fn =
    unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> i64;
pub(crate) type Sqlite3ValueToDoubleFn =
    unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> f64;
pub(crate) type Sqlite3ValueToTextFn =
    unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> *const c_uchar;
pub(crate) type Sqlite3ValueToBlobFn =
    unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> *const c_void;
pub(crate) type Sqlite3CreateCollationFn = unsafe extern "C" fn(
    *mut sqlite3,
    *const c_char,
    c_int,
    *mut c_void,
    CollationCompare,
) -> c_int;
pub(crate) type Sqlite3CreateCollationV2Fn = unsafe extern "C" fn(
    *mut sqlite3,
    *const c_char,
    c_int,
    *mut c_void,
    CollationCompare,
    CollationDestroy,
) -> c_int;
pub(crate) type Sqlite3FreeFn = unsafe extern "C" fn(*mut c_void);
pub(crate) type Sqlite3MallocFn = unsafe extern "C" fn(c_int) -> *mut c_void;
