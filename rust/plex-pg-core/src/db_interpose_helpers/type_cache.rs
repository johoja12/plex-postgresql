use super::{
    cstr_to_str, has_boundary, starts_with_icase, SQLITE_BLOB_CONST, SQLITE_FLOAT_CONST,
    SQLITE_INTEGER_CONST, SQLITE_TEXT_CONST,
};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::{LazyLock, RwLock};

#[derive(Default)]
struct DecltypeCache {
    entries: HashMap<String, CString>,
    // Cache misses too: expression aliases often have no schema entry.
    aliases: HashMap<String, Option<String>>,
}

impl DecltypeCache {
    fn insert(&mut self, key: String, value: CString) {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.entries.entry(key) {
            entry.insert(value);
            // A newly loaded schema entry can resolve a previous miss or provide
            // a longer match. Published CStrings themselves are never replaced.
            self.aliases.clear();
            crate::libpq_helpers::invalidate_result_metadata();
        }
    }

    fn cached_alias(&self, alias: &str) -> Option<*const c_char> {
        if let Some(value) = self.entries.get(alias) {
            return Some(value.as_ptr());
        }
        self.aliases.get(alias).map(|key| {
            key.as_ref()
                .and_then(|key| self.entries.get(key))
                .map_or(std::ptr::null(), |value| value.as_ptr())
        })
    }

    fn resolve_alias(&mut self, alias: &str) -> *const c_char {
        if let Some(value) = self.cached_alias(alias) {
            return value;
        }
        let mut best: Option<&String> = None;
        for key in self.entries.keys() {
            if alias_matches_cache_key(alias, key)
                && best.is_none_or(|previous| key.len() > previous.len())
            {
                best = Some(key);
            }
        }
        let resolved = best.cloned();
        let result = resolved
            .as_ref()
            .and_then(|key| self.entries.get(key))
            .map_or(std::ptr::null(), |value| value.as_ptr());
        // Bound query-generated alias memory without touching published values.
        if self.aliases.len() >= 4096 {
            self.aliases.clear();
        }
        self.aliases.insert(alias.to_owned(), resolved);
        result
    }
}

static DECLTYPE_CACHE: LazyLock<RwLock<DecltypeCache>> =
    LazyLock::new(|| RwLock::new(DecltypeCache::default()));
static OID_TABLE_CACHE: LazyLock<RwLock<HashMap<u32, CString>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn rust_decltype_hash(ptr: *const c_char) -> u32 {
    let mut hash: u32 = 5381;
    let s = cstr_to_str(ptr).unwrap_or("");
    for b in s.as_bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(*b as u32);
    }
    hash
}

pub fn rust_decltype_cache_insert(key: *const c_char, decltype_val: *const c_char) -> c_int {
    let key_str = match cstr_to_str(key) {
        Some(s) if !s.is_empty() => s,
        _ => return 0,
    };

    let decltype_str = match cstr_to_str(decltype_val) {
        Some(s) if !s.is_empty() => s,
        _ => return 0,
    };
    let normalized_owned = match CString::new(decltype_str) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let mut cache = match DECLTYPE_CACHE.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    // Never replace a published entry. `rust_decltype_cache_lookup` returns a
    // pointer into the stored CString and then drops the read guard; those
    // pointers reach Plex as `sqlite3_column_decltype()` results, which stay
    // valid until the statement is finalized. Replacing would drop the old
    // CString and free it underneath every holder. Re-inserting a key is the
    // normal case, not an edge one -- the decltype preload re-runs its whole
    // pass whenever a previous attempt failed -- so first publication wins.
    cache.insert(key_str.to_string(), normalized_owned);
    1
}

pub fn rust_decltype_cache_lookup(key: *const c_char) -> *const c_char {
    let key_str = match cstr_to_str(key) {
        Some(s) if !s.is_empty() => s,
        _ => return std::ptr::null(),
    };
    let cache = match DECLTYPE_CACHE.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    cache
        .entries
        .get(key_str)
        .map(|s| s.as_ptr())
        .unwrap_or(std::ptr::null())
}

pub fn rust_oid_table_cache_insert(oid: c_uint, name: *const c_char) -> c_int {
    let name_str = match cstr_to_str(name) {
        Some(s) if !s.is_empty() => s,
        _ => return 0,
    };
    let cstr = match CString::new(name_str) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    let mut cache = match OID_TABLE_CACHE.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    // `or_insert`, not `insert`: see `rust_decltype_cache_insert`. Lookups hand
    // out pointers into these CStrings, so a published entry must never be
    // dropped.
    cache.entry(oid).or_insert(cstr);
    1
}

pub fn rust_oid_table_cache_lookup(oid: c_uint) -> *const c_char {
    let cache = match OID_TABLE_CACHE.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    cache
        .get(&oid)
        .map(|s| s.as_ptr())
        .unwrap_or(std::ptr::null())
}

pub fn rust_expected_sqlite_type_for_decltype(decl: *const c_char) -> c_int {
    let t = match cstr_to_str(decl) {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => return -1,
    };
    let bytes = t.as_bytes();

    if starts_with_icase(bytes, b"DT_INTEGER") {
        return SQLITE_INTEGER_CONST;
    }
    if starts_with_icase(bytes, b"INTEGER") && has_boundary(bytes, 7) {
        return SQLITE_INTEGER_CONST;
    }
    if starts_with_icase(bytes, b"BIGINT") && has_boundary(bytes, 6) {
        return SQLITE_INTEGER_CONST;
    }
    if t.eq_ignore_ascii_case("INT8")
        || t.eq_ignore_ascii_case("INT64")
        || t.eq_ignore_ascii_case("LONG")
        || t.eq_ignore_ascii_case("BOOLEAN")
        || t.eq_ignore_ascii_case("TIMESTAMP")
    {
        return SQLITE_INTEGER_CONST;
    }

    if t.eq_ignore_ascii_case("FLOAT")
        || t.eq_ignore_ascii_case("DOUBLE")
        || t.eq_ignore_ascii_case("REAL")
    {
        return SQLITE_FLOAT_CONST;
    }

    if starts_with_icase(bytes, b"VARCHAR") && has_boundary(bytes, 7) {
        return SQLITE_TEXT_CONST;
    }
    if t.eq_ignore_ascii_case("STRING")
        || t.eq_ignore_ascii_case("CHAR")
        || t.eq_ignore_ascii_case("TEXT")
    {
        return SQLITE_TEXT_CONST;
    }

    if t.eq_ignore_ascii_case("BLOB") {
        return SQLITE_BLOB_CONST;
    }

    -1
}

pub fn rust_decltype_cache_lookup_alias(alias: *const c_char) -> *const c_char {
    let alias_str = match cstr_to_str(alias) {
        Some(s) if !s.is_empty() => s,
        _ => return std::ptr::null(),
    };
    let cache = match DECLTYPE_CACHE.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if let Some(value) = cache.cached_alias(alias_str) {
        return value;
    }
    drop(cache);
    let mut cache = match DECLTYPE_CACHE.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    cache.resolve_alias(alias_str)
}

fn alias_matches_cache_key(alias: &str, cache_key: &str) -> bool {
    if alias == cache_key {
        return true;
    }

    for (idx, b) in cache_key.as_bytes().iter().enumerate() {
        if *b != b'_' {
            continue;
        }

        let table = &cache_key[..idx];
        let column = &cache_key[idx + 1..];
        if table.is_empty() || column.is_empty() {
            continue;
        }

        if !alias.starts_with(table) {
            continue;
        }
        if alias.len() <= table.len() || alias.as_bytes()[table.len()] != b'_' {
            continue;
        }

        if alias.ends_with(&cache_key[idx..]) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_cache_preserves_matches_misses_and_published_pointers() {
        let mut cache = DecltypeCache::default();
        cache.insert("media_items_id".into(), CString::new("INTEGER").unwrap());
        let pointer = cache.resolve_alias("media_items_42_id");
        assert!(!pointer.is_null());
        assert_eq!(cache.cached_alias("media_items_42_id"), Some(pointer));
        assert!(cache.resolve_alias("new_table_1_title").is_null());
        assert_eq!(
            cache.cached_alias("new_table_1_title"),
            Some(std::ptr::null())
        );
        cache.insert("new_table_title".into(), CString::new("TEXT").unwrap());
        assert!(cache.cached_alias("new_table_1_title").is_none());
        assert!(!cache.resolve_alias("new_table_1_title").is_null());
        cache.insert("media_items_id".into(), CString::new("TEXT").unwrap());
        assert_eq!(cache.resolve_alias("media_items_42_id"), pointer);
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(pointer) }.to_bytes(),
            b"INTEGER"
        );
        for n in 0..5000 {
            assert!(cache.resolve_alias(&format!("unknown_{n}")).is_null());
        }
        assert!(cache.aliases.len() <= 4096);
        assert_eq!(cache.resolve_alias("media_items_42_id"), pointer);
    }

    #[test]
    fn new_longer_alias_match_invalidates_previous_resolution() {
        let mut cache = DecltypeCache::default();
        cache.insert("a_id".into(), CString::new("INTEGER").unwrap());
        let short = cache.resolve_alias("a_b_1_id");
        cache.insert("a_b_id".into(), CString::new("TEXT").unwrap());
        let longer = cache.resolve_alias("a_b_1_id");
        assert_ne!(short, longer);
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(longer) }.to_bytes(),
            b"TEXT"
        );
        assert_eq!(cache.resolve_alias("a_id"), short);
    }
}
