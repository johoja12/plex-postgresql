use std::ffi::CStr;

const DECLTYPE_INTEGER: &[u8] = b"INTEGER\0";
const DECLTYPE_DT_INTEGER_8: &[u8] = b"dt_integer(8)\0";
const DECLTYPE_REAL: &[u8] = b"REAL\0";
const DECLTYPE_BLOB: &[u8] = b"BLOB\0";
const DECLTYPE_TEXT: &[u8] = b"TEXT\0";

const ON_CONFLICT_CLAUSE: &str = " ON CONFLICT (account_id, guid) DO UPDATE SET \
rating = COALESCE(EXCLUDED.rating, plex.metadata_item_settings.rating), \
view_offset = EXCLUDED.view_offset, \
view_count = CASE WHEN plex.metadata_item_settings.view_count > 0 AND EXCLUDED.view_count = 0 \
THEN 0 ELSE GREATEST(EXCLUDED.view_count, plex.metadata_item_settings.view_count, 1) END, \
last_viewed_at = CASE WHEN plex.metadata_item_settings.view_count > 0 AND EXCLUDED.view_count = 0 \
THEN NULL ELSE COALESCE(EXCLUDED.last_viewed_at, EXTRACT(EPOCH FROM NOW())::bigint) END, \
updated_at = COALESCE(EXCLUDED.updated_at, EXTRACT(EPOCH FROM NOW())::bigint), \
skip_count = EXCLUDED.skip_count, \
last_skipped_at = EXCLUDED.last_skipped_at, \
changed_at = COALESCE(EXCLUDED.changed_at, EXTRACT(EPOCH FROM NOW())::bigint), \
extra_data = COALESCE(EXCLUDED.extra_data, plex.metadata_item_settings.extra_data), \
last_rated_at = COALESCE(EXCLUDED.last_rated_at, plex.metadata_item_settings.last_rated_at) \
RETURNING id";

pub(crate) fn oid_to_sqlite_type(oid: u32) -> i32 {
    match oid {
        16 | 20 | 21 | 23 | 26 => super::SQLITE_INTEGER,
        700 | 701 | 1700 => super::SQLITE_FLOAT,
        17 => super::SQLITE_BLOB,
        _ => super::SQLITE_TEXT,
    }
}

pub(crate) fn oid_to_sqlite_decltype(oid: u32) -> &'static CStr {
    let bytes: &'static [u8] = match oid {
        16 | 21 | 23 | 26 => DECLTYPE_INTEGER,
        20 => DECLTYPE_DT_INTEGER_8, // int8 → SOCI int64
        // timestamp and timestamptz (1114, 1184) are TEXT: their value is
        // "2024-01-02 03:04:05", not an epoch.
        700 | 701 | 1700 => DECLTYPE_REAL,
        17 => DECLTYPE_BLOB,
        _ => DECLTYPE_TEXT,
    };
    unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}

pub(crate) fn convert_metadata_settings_upsert(sql: &str) -> Option<String> {
    let lower = sql.to_lowercase();
    if !lower.contains("insert into") {
        return None;
    }
    if !lower.contains("metadata_item_settings") {
        return None;
    }
    if lower.contains("on conflict") {
        return None;
    }
    if lower.contains("returning") {
        return None;
    }
    Some(format!("{}{}", sql, ON_CONFLICT_CLAUSE))
}

pub(crate) fn extract_metadata_id(sql: &str) -> i64 {
    let lower = sql.to_lowercase();
    if !lower.contains("play_queue_generators") {
        return 0;
    }
    if !lower.contains("insert") {
        return 0;
    }

    let pat_encoded = "%2Fmetadata%2F";
    let pat_plain = "/metadata/";

    let after = if let Some(i) = sql.find(pat_encoded) {
        &sql[i + pat_encoded.len()..]
    } else if let Some(i) = sql.find(pat_plain) {
        &sql[i + pat_plain.len()..]
    } else {
        return 0;
    };

    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return 0;
    }
    digits.parse::<i64>().unwrap_or(0)
}
