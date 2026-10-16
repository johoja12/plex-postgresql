use super::*;
use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

#[path = "tests/db_to_pool.rs"]
mod db_to_pool;
#[path = "tests/fork.rs"]
mod fork;
#[path = "tests/globals.rs"]
mod globals;
#[path = "tests/hash_sqlstate.rs"]
mod hash_sqlstate;
#[path = "tests/path_selection.rs"]
mod path_selection;
#[path = "tests/pool_manager.rs"]
mod pool_manager;
#[path = "tests/pool_shrink.rs"]
mod pool_shrink;
#[path = "tests/pool_slot.rs"]
mod pool_slot;
#[path = "tests/reaper.rs"]
mod reaper;
#[path = "tests/registry.rs"]
mod registry;
#[path = "tests/slot_leaks.rs"]
mod slot_leaks;
#[path = "tests/sqlstate.rs"]
mod sqlstate;
#[path = "tests/tls_cache.rs"]
mod tls_cache;
