#![allow(non_snake_case)]

use super::*;
use std::ffi::{CStr, CString};

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn take_cstring(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CString::from_raw(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn normalize_decltype(input: Option<&str>) -> String {
    let ptr = normalize_sqlite_decltype_impl(input);
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn djb2(s: &str) -> u32 {
    let mut hash: u32 = 5381;
    for b in s.as_bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(*b as u32);
    }
    hash
}

#[test]
fn validate_utf8_accepts_valid_input() {
    let s = "Plex \u{1F4FA}";
    assert_eq!(rust_validate_utf8(s.as_ptr() as *const c_char, s.len()), 1);
}

#[test]
fn validate_utf8_rejects_invalid_input() {
    let invalid = [0xffu8, 0xfeu8];
    assert_eq!(
        rust_validate_utf8(invalid.as_ptr() as *const c_char, invalid.len()),
        0
    );
}

#[test]
fn rewrite_server_uri_rewrites_expected_prefix() {
    let input = CString::new("server://machine/com.plexapp.plugins.library/library/metadata/123")
        .expect("valid c string");
    let mut out = [0 as c_char; 256];

    assert_eq!(
        rust_rewrite_server_library_uri(input.as_ptr(), out.as_mut_ptr(), out.len()),
        1
    );

    let rewritten = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_str()
        .expect("utf8 output");
    assert_eq!(rewritten, "library://metadata/123");
}

#[test]
fn rewrite_server_uri_handles_multiple_matches() {
    let input = CString::new(
        "a=server://m1/com.plexapp.plugins.library/library/one;b=server://m2/com.plexapp.plugins.library/library/two",
    )
    .expect("valid c string");
    let mut out = [0 as c_char; 256];

    assert_eq!(
        rust_rewrite_server_library_uri(input.as_ptr(), out.as_mut_ptr(), out.len()),
        1
    );

    let rewritten = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_str()
        .expect("utf8 output");
    assert_eq!(rewritten, "a=library://one;b=library://two");
}

fn rewrite_with_buf(input: &str, out_len: usize) -> (i32, String) {
    let input = CString::new(input).expect("valid c string");
    let mut out = vec![0 as c_char; out_len];
    let ok = rust_rewrite_server_library_uri(input.as_ptr(), out.as_mut_ptr(), out.len());
    let rewritten = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_str()
        .unwrap_or("")
        .to_string();
    (ok, rewritten)
}

#[test]
fn rewrite_server_uri_standalone() {
    let (ok, out) = rewrite_with_buf(
        "server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all?type=2",
        512,
    );
    assert_eq!(ok, 1);
    assert_eq!(out, "library:///directory/%2Flibrary%2Fsections%2F2%2Fall%3Ftype%3D2");
}

#[test]
fn rewrite_server_uri_json_embedded() {
    let (ok, out) = rewrite_with_buf(
        "{\"at:childCount\":\"1\",\"at:smart\":\"1\",\"pv:uri\":\"server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=date\"}",
        1024,
    );
    assert_eq!(ok, 1);
    assert_eq!(
        out,
        "{\"at:childCount\":\"1\",\"at:smart\":\"1\",\"pv:uri\":\"library:///directory/%2Flibrary%2Fsections%2F2%2Fall%3Ftype%3D2%26sort%3Ddate\"}"
    );
}

#[test]
fn rewrite_server_uri_no_server_prefix() {
    let (ok, out) = rewrite_with_buf("library://sections/1/all", 256);
    assert_eq!(ok, 0);
    assert!(out.is_empty());
}

#[test]
fn rewrite_server_uri_no_plugin_path() {
    let (ok, out) = rewrite_with_buf("server://abc123/some/other/path", 256);
    assert_eq!(ok, 0);
    assert!(out.is_empty());
}

#[test]
fn rewrite_server_uri_empty_string() {
    let (ok, out) = rewrite_with_buf("", 64);
    assert_eq!(ok, 0);
    assert!(out.is_empty());
}

#[test]
fn rewrite_server_uri_null_input() {
    let mut out = [0 as c_char; 64];
    let ok = rust_rewrite_server_library_uri(std::ptr::null(), out.as_mut_ptr(), out.len());
    assert_eq!(ok, 0);
}

#[test]
fn rewrite_server_uri_plain_text() {
    let (ok, out) = rewrite_with_buf(
        "{\"at:childCount\":\"5\",\"pv:thumbBlurHash\":\"abc123\"}",
        256,
    );
    assert_eq!(ok, 0);
    assert!(out.is_empty());
}

#[test]
fn rewrite_server_uri_multiple_in_json() {
    let (ok, out) = rewrite_with_buf(
        "{\"uri1\":\"server://aaa/com.plexapp.plugins.library/library/sections/1/all\",\"uri2\":\"server://aaa/com.plexapp.plugins.library/library/sections/2/all\"}",
        2048,
    );
    assert_eq!(ok, 1);
    assert_eq!(
        out,
        "{\"uri1\":\"library:///directory/%2Flibrary%2Fsections%2F1%2Fall\",\"uri2\":\"library:///directory/%2Flibrary%2Fsections%2F2%2Fall\"}"
    );
}

#[test]
fn rewrite_server_uri_output_shorter() {
    let input = "server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all";
    let (ok, out) = rewrite_with_buf(input, 512);
    assert_eq!(ok, 1);
    assert!(out.len() < input.len());
}

#[test]
fn rewrite_server_uri_encoded_params() {
    let (ok, out) = rewrite_with_buf(
        "server://71b287/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=originallyAvailableAt%3Adesc&push=1&show.network=248684&pop=1",
        1024,
    );
    assert_eq!(ok, 1);
    assert_eq!(
        out,
        "library:///directory/%2Flibrary%2Fsections%2F2%2Fall%3Ftype%3D2%26sort%3DoriginallyAvailableAt%253Adesc%26push%3D1%26show.network%3D248684%26pop%3D1"
    );
}

#[test]
fn rewrite_server_uri_small_buffer() {
    let (ok, _out) = rewrite_with_buf(
        "server://abc/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=date",
        32,
    );
    assert_eq!(ok, 0, "never return a truncated encoded URI");
}

#[test]
fn rewrite_server_uri_tiny_buffer() {
    let (ok, _) = rewrite_with_buf("server://x", 8);
    assert_eq!(ok, 0);
}

#[test]
fn rewrite_server_uri_real_plex_blob() {
    let (ok, out) = rewrite_with_buf(
        "{\"at:childCount\":\"1\",\"at:smart\":\"1\",\"pv:blurHashesChangedAt\":\"277470\",\"pv:thumbBlurHash\":\"LJC?YqM{IVoz\",\"pv:uri\":\"server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=originallyAvailableAt%3Adesc&push=1&show.genre=8966&pop=1\"}",
        2048,
    );
    assert_eq!(ok, 1);
    assert!(out.contains("library:///directory/%2Flibrary%2Fsections%2F2%2Fall"));
    assert!(!out.contains("server://"));
}

#[test]
fn normalize_sql_literals_extracts_two_params() {
    let sql = "SELECT * FROM t WHERE id = 123 AND score >= -4.5";
    let (normalized, params) =
        normalize_sql_literals_impl(sql).expect("expected normalized result");
    assert_eq!(normalized, "SELECT * FROM t WHERE id = $1 AND score >= $2");
    assert_eq!(params, vec!["123".to_string(), "-4.5".to_string()]);
}

#[test]
fn normalize_sql_literals_keeps_non_ascii_text() {
    let sql = "SELECT * FROM t WHERE path = 'La Jetée (1962)' AND id = 123";
    let (normalized, params) =
        normalize_sql_literals_impl(sql).expect("expected normalized result");
    assert_eq!(
        normalized,
        "SELECT * FROM t WHERE path = 'La Jetée (1962)' AND id = $1"
    );
    assert_eq!(params, vec!["123".to_string()]);
}

#[test]
fn normalize_sql_literals_skips_insert() {
    assert!(normalize_sql_literals_impl("INSERT INTO t VALUES (1)").is_none());
}

#[test]
fn prepare_simple_hash_is_deterministic() {
    let a = prepare_simple_hash("SELECT * FROM t", 200);
    let b = prepare_simple_hash("SELECT * FROM t", 200);
    assert_eq!(a, b);
}

#[test]
fn compat_aliases__alias_collection_sync_aggregates_rewrites_select_list() {
    let sqlite =
        "select count(*), min(year), max(year) from tags join taggings on 1=1 group by tags.id";
    let pg =
        "SELECT count(*), min(year), max(year) FROM tags JOIN taggings ON true GROUP BY tags.id";
    let out = alias_collection_sync_aggregates(sqlite, pg).expect("should rewrite");
    assert!(out.contains("count(*) AS \"count(*)\""));
    assert!(out.contains("min(year) AS \"min(year)\""));
    assert!(out.contains("max(year) AS \"max(year)\""));
}

#[test]
fn compat_aliases__alias_collection_sync_aggregates_noop_for_other_queries() {
    let sqlite = "select id from tags";
    let pg = "SELECT id FROM tags";
    assert!(alias_collection_sync_aggregates(sqlite, pg).is_none());
}

#[test]
fn strip_collate_icu_root_removes_both_forms() {
    let sql = "SELECT * FROM t COLLATE icu_root WHERE x=1";
    let out = strip_collate_icu_root(sql).expect("should strip");
    assert!(!out.to_ascii_lowercase().contains("collate icu_root"));
}

#[test]
fn is_library_db_path_matches_suffix() {
    assert!(is_library_db_path_impl(
        "/x/y/com.plexapp.plugins.library.db"
    ));
    assert!(!is_library_db_path_impl("/x/y/other.db"));
}

#[test]
fn is_library_or_blobs_path_matches_both() {
    assert!(is_library_or_blobs_db_path_impl(
        "/x/y/com.plexapp.plugins.library.db"
    ));
    assert!(is_library_or_blobs_db_path_impl(
        "/x/y/com.plexapp.plugins.library.blobs.db"
    ));
    assert!(!is_library_or_blobs_db_path_impl("/x/y/other.db"));
}

#[test]
fn junk_metadata_insert_detects_null_pair() {
    let sql = "INSERT INTO metadata_items (library_section_id, metadata_type, title) VALUES (NULL, NULL, 'x')";
    assert!(is_junk_metadata_insert_impl(sql));
}

#[test]
fn junk_metadata_insert_ignores_non_null() {
    let sql = "INSERT INTO metadata_items (library_section_id, metadata_type) VALUES (1, NULL)";
    assert!(!is_junk_metadata_insert_impl(sql));
}

#[test]
fn trace_list_contains_idx_matches_values() {
    assert!(list_contains_idx("5,6; 7", 6));
    assert!(!list_contains_idx("5,6; 7", 4));
    assert!(list_contains_idx("all", 999));
}

#[test]
fn trace_list_any_token_in_haystack_matches_token() {
    assert!(list_any_token_in_haystack(
        "tags,collections",
        "from tags join x"
    ));
    assert!(!list_any_token_in_haystack("abc,def", "from tags join x"));
}

#[test]
fn subset_fts__simplify_fts_for_sqlite_rewrites_match_and_join() {
    let sql = "SELECT * FROM a JOIN fts4_metadata_titles t ON t.rowid=a.id WHERE fts4_metadata_titles.title MATCH 'foo''bar'";
    let out = simplify_fts_for_sqlite(sql).expect("should simplify");
    assert!(!out
        .to_ascii_lowercase()
        .contains("join fts4_metadata_titles"));
    assert!(out.contains("1=0"));
}

#[test]
fn subset_fts__simplify_fts_for_sqlite_noop_without_fts() {
    assert!(simplify_fts_for_sqlite("SELECT * FROM t").is_none());
}

#[test]
fn subset_ddl_lite__add_if_not_exists_for_sqlite_ddl_rewrites_create_table() {
    let sql = "CREATE TABLE tags (id INTEGER)";
    let out = add_if_not_exists_for_sqlite_ddl(sql).expect("should rewrite");
    assert!(out.contains("CREATE TABLE IF NOT EXISTS tags"));
}

#[test]
fn subset_ddl_lite__add_if_not_exists_for_sqlite_ddl_rewrites_create_unique_index() {
    let sql = "CREATE UNIQUE INDEX idx_tags ON tags(id)";
    let out = add_if_not_exists_for_sqlite_ddl(sql).expect("should rewrite");
    assert!(out.contains("CREATE UNIQUE INDEX IF NOT EXISTS idx_tags"));
}

#[test]
fn subset_ddl_lite__add_if_not_exists_for_sqlite_ddl_noop_if_already_present() {
    let sql = "CREATE INDEX IF NOT EXISTS idx_tags ON tags(id)";
    assert!(add_if_not_exists_for_sqlite_ddl(sql).is_none());
}

#[test]
fn binary_detection_and_hex_encoding_work() {
    assert!(contains_binary_bytes_impl(&[0x1f, 0x8b, 0x08]));
    assert!(!contains_binary_bytes_impl(b"hello"));
    assert_eq!(bytes_to_pg_hex_impl(&[0x41, 0x42, 0xff]), "\\x4142ff");
}

#[test]
fn related_items_and_mask_predicates_work() {
    let sql = "select * from taggings as related join x";
    assert!(is_related_items_query_impl(sql));
    assert!(should_mask_collection_metadata_type_impl(
        sql,
        "metadata_type",
        18
    ));
    assert!(!should_mask_collection_metadata_type_impl(
        sql,
        "other_col",
        18
    ));
    assert!(!should_mask_collection_metadata_type_impl(
        "select * from x",
        "metadata_type",
        18
    ));
}

#[test]
fn find_insert_column_index_handles_quoted_columns() {
    let sql = "INSERT INTO metadata_items (\"id\", `library_section_id`, metadata_type, title) VALUES ($1,$2,$3,$4)";
    assert_eq!(find_insert_column_index_impl(sql, "library_section_id"), 1);
    assert_eq!(find_insert_column_index_impl(sql, "metadata_type"), 2);
    assert_eq!(find_insert_column_index_impl(sql, "missing_col"), -1);
}

#[test]
fn pg_oid_to_sqlite_type_mapping_matches_expectations() {
    assert_eq!(
        pg_oid_to_sqlite_type_impl(20),
        crate::db_interpose_value_helpers::SQLITE_INTEGER_CONST
    );
    assert_eq!(
        pg_oid_to_sqlite_type_impl(701),
        crate::db_interpose_value_helpers::SQLITE_FLOAT_CONST
    );
    assert_eq!(
        pg_oid_to_sqlite_type_impl(17),
        crate::db_interpose_value_helpers::SQLITE_BLOB_CONST
    );
    assert_eq!(
        pg_oid_to_sqlite_type_impl(25),
        crate::db_interpose_value_helpers::SQLITE_TEXT_CONST
    );
}

#[test]
fn trim_first_line_trims_ws_and_newline() {
    assert_eq!(trim_first_line("  abc \r\n").as_deref(), Some("abc"));
    assert_eq!(trim_first_line("   \n"), None);
}

#[test]
fn common_helpers_is_library_db_path_null_returns_false() {
    assert_eq!(rust_is_library_db_path(std::ptr::null()), 0);
}

#[test]
fn common_helpers_is_library_db_path_empty_returns_false() {
    assert!(!is_library_db_path_impl(""));
}

#[test]
fn common_helpers_is_library_db_path_matches_known_paths() {
    assert!(is_library_db_path_impl(
        "/data/Databases/com.plexapp.plugins.library.db"
    ));
    assert!(is_library_db_path_impl(
        "/data/Databases/com.plexapp.plugins.library.blobs.db"
    ));
    assert!(is_library_db_path_impl("com.plexapp.plugins.library.db"));
    assert!(is_library_db_path_impl(
        "com.plexapp.plugins.library.blobs.db"
    ));
    assert!(is_library_db_path_impl("/Users/plex/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"));
    assert!(is_library_db_path_impl("/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.blobs.db"));
}

#[test]
fn common_helpers_is_library_db_path_rejects_non_library_paths() {
    assert!(!is_library_db_path_impl(
        "com.plexapp.plugins.preferences.db"
    ));
    assert!(!is_library_db_path_impl("/tmp/test.db"));
    assert!(!is_library_db_path_impl("library.db"));
}

const GUIDE: &str = "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/tv.plex.providers.epg.xmltv-adfe4a6f-79d0-4135-93d5-edf0235ec02b.db";

#[test]
fn a_live_tv_guide_is_served_from_a_schema_named_for_its_dvr() {
    // The file name talos's DVR 2 gave its guide (2026-10-20).
    assert_eq!(
        epg_schema_for_path_impl(GUIDE).as_deref(),
        Some("epg_adfe4a6f79d0413593d5edf0235ec02b")
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        assert_eq!(
            epg_schema_for_path_impl(&format!("{GUIDE}{suffix}")).as_deref(),
            Some("epg_adfe4a6f79d0413593d5edf0235ec02b"),
            "{suffix}"
        );
    }
    assert_eq!(
        epg_schema_for_path_impl(
            "tv.plex.providers.epg.cloud-ADFE4A6F-79D0-4135-93D5-EDF0235EC02B.db"
        )
        .as_deref(),
        Some("epg_adfe4a6f79d0413593d5edf0235ec02b"),
        "lower-cased"
    );
}

#[test]
fn only_a_guide_with_a_real_uuid_has_a_schema() {
    for path in [
        "/x/com.plexapp.plugins.library.db",
        "/x/tv.plex.providers.epg.xmltv.db",
        "/x/tv.plex.providers.epg.xmltv-not-a-uuid.db",
        "/x/tv.plex.providers.epg.xmltv-adfe4a6f-79d0-4135-93d5-edf0235ec02z.db",
        "/x/tv.plex.providers.epg.xmltv-adfe4a6f79d0413593d5edf0235ec02b0000.db",
        "/x/tv.plex.providers.epg.-adfe4a6f-79d0-4135-93d5-edf0235ec02b.db",
        "/x/tv.plex.providers.epg.x;y-adfe4a6f-79d0-4135-93d5-edf0235ec02b.db",
        "/x/tv.plex.providers.epg.xmltv-adfe4a6f-79d0-4135-93d5-edf0235ec02b.db.bak",
        "/x/other.tv.plex.providers.epg.xmltv-adfe4a6f-79d0-4135-93d5-edf0235ec02b.db",
        "",
    ] {
        assert_eq!(epg_schema_for_path_impl(path), None, "{path}");
    }
}

#[test]
fn a_live_tv_guide_is_served_from_postgresql_like_the_library() {
    assert!(is_library_db_path_impl(GUIDE));
    assert!(is_library_or_blobs_db_path_impl(GUIDE));
    assert!(!is_library_db_path_impl(
        "/x/tv.plex.providers.epg.xmltv-nope.db"
    ));
}

#[test]
fn common_helpers_is_library_db_path_accepts_wal_suffix() {
    assert!(is_library_db_path_impl(
        "com.plexapp.plugins.library.db-wal"
    ));
}

#[test]
fn bind_helpers_contains_binary_bytes_null_and_empty() {
    assert_eq!(rust_contains_binary_bytes(std::ptr::null(), 0), 0);
    assert_eq!(rust_contains_binary_bytes(b"".as_ptr(), 0), 0);
}

#[test]
fn bind_helpers_contains_binary_bytes_text_cases() {
    assert!(!contains_binary_bytes_impl(b"Hello, World!"));
    let utf8 = b"H\xc3\xa9llo W\xc3\xb6rld";
    assert!(!contains_binary_bytes_impl(utf8));
    assert!(!contains_binary_bytes_impl(b"col1\tcol2"));
    assert!(!contains_binary_bytes_impl(b"line1\nline2"));
    assert!(!contains_binary_bytes_impl(b"line1\r\nline2"));
}

#[test]
fn bind_helpers_contains_binary_bytes_detects_nul_gzip_and_invalid_utf8() {
    assert!(contains_binary_bytes_impl(b"\x00"));
    assert!(contains_binary_bytes_impl(b"abc\x00def"));
    assert!(contains_binary_bytes_impl(b"\xC0"));
    assert!(contains_binary_bytes_impl(b"\xC1"));
    assert!(contains_binary_bytes_impl(b"\xF5"));
    assert!(contains_binary_bytes_impl(b"\xFF\xFE"));
    assert!(contains_binary_bytes_impl(&[0x1f, 0x8b, 0x08, 0x00]));
}

// A control byte is text to PostgreSQL and to SQLite; sent as bytea hex, a
// summary with a \x01 in it reached the column as the literal `\x5c78...`.
#[test]
fn bind_helpers_control_bytes_in_text_stay_text() {
    assert!(!contains_binary_bytes_impl(b"\x01"));
    assert!(!contains_binary_bytes_impl(b"\x07test"));
    assert!(!contains_binary_bytes_impl(b"\x7F"));
    assert!(!contains_binary_bytes_impl(b"\x1b[0m"));
    let mixed = [b'H', b'e', b'l', b'l', b'o', 0x01, b'W'];
    assert!(!contains_binary_bytes_impl(&mixed));
    let late_control = [b'A', b'B', b'C', b'D', b'E', 0x02];
    assert!(!contains_binary_bytes_impl(&late_control));
}

#[test]
fn bind_helpers_bytes_to_pg_hex_null_and_empty() {
    let out = take_cstring(rust_bytes_to_pg_hex(std::ptr::null(), 0));
    assert_eq!(out, "");
    let out = take_cstring(rust_bytes_to_pg_hex(b"".as_ptr(), 0));
    assert_eq!(out, "");
}

#[test]
fn bind_helpers_bytes_to_pg_hex_known_values() {
    let out = take_cstring(rust_bytes_to_pg_hex([0xAB].as_ptr(), 1));
    assert_eq!(out, "\\xab");
    let out = take_cstring(rust_bytes_to_pg_hex([0xDE, 0xAD, 0xBE, 0xEF].as_ptr(), 4));
    assert_eq!(out, "\\xdeadbeef");
    let out = take_cstring(rust_bytes_to_pg_hex([0x00, 0x00, 0x00].as_ptr(), 3));
    assert_eq!(out, "\\x000000");
    let out = take_cstring(rust_bytes_to_pg_hex([0xFF, 0xFF].as_ptr(), 2));
    assert_eq!(out, "\\xffff");
    let out = take_cstring(rust_bytes_to_pg_hex(b"AB".as_ptr(), 2));
    assert_eq!(out, "\\x4142");
}

#[test]
fn bind_helpers_bytes_to_pg_hex_png_header() {
    let blob = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let out = take_cstring(rust_bytes_to_pg_hex(blob.as_ptr(), blob.len()));
    assert_eq!(out, "\\x89504e470d0a1a0a");
}

#[test]
fn decltype_preserves_original_sqlite_strings() {
    assert_eq!(normalize_decltype(Some("DT_INTEGER(8)")), "DT_INTEGER(8)");
    assert_eq!(normalize_decltype(Some("dt_integer(8)")), "dt_integer(8)");
    assert_eq!(normalize_decltype(Some("varchar(255)")), "varchar(255)");
    assert_eq!(normalize_decltype(Some("boolean")), "boolean");
    assert_eq!(normalize_decltype(Some("INTEGER")), "INTEGER");
}

#[test]
fn decltype_preserve_empty_returns_null() {
    assert_eq!(normalize_decltype(Some("")), "");
    assert_eq!(normalize_decltype(None), "");
}

#[test]
fn pg_udt_fallback_mapping_uses_sqlite_like_decltypes() {
    let cases = [
        ("int4", "integer"),
        ("int8", "dt_integer(8)"),
        ("timestamp", "dt_integer(8)"),
        ("timestamptz", "dt_integer(8)"),
        ("bool", "boolean"),
        ("varchar", "varchar(255)"),
        ("text", "varchar(255)"),
        ("float8", "float"),
        ("bytea", "blob"),
    ];

    for (input, expected) in cases {
        let ptr = pg_udt_to_sqlite_decltype_impl(Some(input));
        let actual = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap();
        assert_eq!(actual, expected);
    }
}

#[test]
fn decltype_cache_preserves_exact_decltype_string() {
    let key = c("accounts_auto_select_subtitle");
    let decl = c("varchar(255)");

    assert_eq!(rust_decltype_cache_insert(key.as_ptr(), decl.as_ptr()), 1);
    let cached = rust_decltype_cache_lookup(key.as_ptr());
    let cached = unsafe { CStr::from_ptr(cached) }.to_str().unwrap();
    assert_eq!(cached, "varchar(255)");
}

#[test]
fn decltype_cache_alias_lookup_accepts_full_table_column_key() {
    let key = c("metadata_item_settings_last_viewed_at");
    let decl = c("dt_integer(8)");

    assert_eq!(rust_decltype_cache_insert(key.as_ptr(), decl.as_ptr()), 1);
    let cached = rust_decltype_cache_lookup_alias(key.as_ptr());
    let cached = unsafe { CStr::from_ptr(cached) }.to_str().unwrap();
    assert_eq!(cached, "dt_integer(8)");
}

#[test]
fn decltype_cache_alias_lookup_resolves_self_join_parent_alias() {
    let key = c("metadata_items_added_at");
    let decl = c("dt_integer(8)");
    let alias = c("metadata_items_parents_added_at");

    assert_eq!(rust_decltype_cache_insert(key.as_ptr(), decl.as_ptr()), 1);
    let cached = rust_decltype_cache_lookup_alias(alias.as_ptr());
    let cached = unsafe { CStr::from_ptr(cached) }.to_str().unwrap();
    assert_eq!(cached, "dt_integer(8)");
}

#[test]
fn decltype_cache_alias_lookup_resolves_self_join_text_alias() {
    let key = c("metadata_items_user_square_art_url");
    let decl = c("varchar(255)");
    let alias = c("metadata_items_parents_user_square_art_url");

    assert_eq!(rust_decltype_cache_insert(key.as_ptr(), decl.as_ptr()), 1);
    let cached = rust_decltype_cache_lookup_alias(alias.as_ptr());
    let cached = unsafe { CStr::from_ptr(cached) }.to_str().unwrap();
    assert_eq!(cached, "varchar(255)");
}

#[test]
fn decltype_cache_never_invalidates_a_published_entry() {
    // `rust_decltype_cache_lookup` hands back a pointer into the CString held
    // in the shared map and then drops the read guard. Those pointers reach
    // Plex as `sqlite3_column_decltype()` results, which SQLite says stay
    // valid until the statement is finalized. A replacing `insert` would drop
    // the old CString and free it underneath every holder -- and the preload
    // re-runs its whole pass whenever a previous attempt failed, so every key
    // gets re-inserted.
    let key = c("racecheck_published_col");
    let first = c("dt_integer(8)");
    let second = c("TEXT");

    assert_eq!(rust_decltype_cache_insert(key.as_ptr(), first.as_ptr()), 1);
    let published = rust_decltype_cache_lookup(key.as_ptr());
    assert!(!published.is_null());
    assert_eq!(
        unsafe { CStr::from_ptr(published) }.to_str().unwrap(),
        "dt_integer(8)"
    );

    // A second load pass over the same key must leave the published
    // allocation exactly where it is.
    rust_decltype_cache_insert(key.as_ptr(), second.as_ptr());

    assert_eq!(
        rust_decltype_cache_lookup(key.as_ptr()),
        published,
        "decltype entry was replaced, freeing a pointer already handed to the caller"
    );
    assert_eq!(
        unsafe { CStr::from_ptr(published) }.to_str().unwrap(),
        "dt_integer(8)"
    );
}

#[test]
fn type_normalization_decltype_hash_matches_djb2() {
    let s = "dt_integer(8)";
    let cs = c(s);
    assert_eq!(rust_decltype_hash(cs.as_ptr()), djb2(s));
}

#[test]
fn type_normalization_decltype_hash_null_matches_empty() {
    assert_eq!(rust_decltype_hash(std::ptr::null()), djb2(""));
}

#[test]
fn type_normalization_decltype_hash_differs_for_different_strings() {
    let a = c("INTEGER");
    let b = c("TEXT");
    assert_ne!(
        rust_decltype_hash(a.as_ptr()),
        rust_decltype_hash(b.as_ptr())
    );
}

#[test]
fn subset_fts__fts_quotes_simple_query_rewrites() {
    let sql = "SELECT * FROM metadata_items \
               JOIN fts4_metadata_titles ON metadata_items.id = fts4_metadata_titles.id \
               WHERE fts4_metadata_titles.title match 'test'";
    let out = simplify_fts_for_sqlite(sql).expect("should simplify");
    assert!(!out.to_ascii_lowercase().contains("fts4_metadata_titles"));
    assert!(out.contains("1=0"));
}

#[test]
fn subset_fts__fts_quotes_handles_apostrophes() {
    let sql = "SELECT * FROM metadata_items \
               JOIN fts4_metadata_titles ON metadata_items.id = fts4_metadata_titles.id \
               WHERE fts4_metadata_titles.title match 'it''s a test'";
    let out = simplify_fts_for_sqlite(sql).expect("should simplify");
    let out_lower = out.to_ascii_lowercase();
    assert!(out.contains("1=0"));
    assert!(!out_lower.contains(" match "));
}

#[test]
fn subset_fts__fts_quotes_handles_escaped_quote_pairs() {
    let sql = "SELECT * FROM items \
               JOIN fts4_metadata_titles ON items.id = fts4_metadata_titles.id \
               WHERE fts4_metadata_titles.title match 'can''t stop'";
    let out = simplify_fts_for_sqlite(sql).expect("should simplify");
    assert!(out.contains("1=0"));
    assert!(!out.to_ascii_lowercase().contains(" match "));
}

#[test]
fn expected_sqlite_type_for_decltype_maps_basic_types() {
    let int = c("INTEGER");
    let real = c("REAL");
    let text = c("TEXT");
    let blob = c("BLOB");
    let dt_int8 = c("DT_INTEGER(8)");

    assert_eq!(
        rust_expected_sqlite_type_for_decltype(int.as_ptr()),
        SQLITE_INTEGER_CONST
    );
    assert_eq!(
        rust_expected_sqlite_type_for_decltype(real.as_ptr()),
        SQLITE_FLOAT_CONST
    );
    assert_eq!(
        rust_expected_sqlite_type_for_decltype(text.as_ptr()),
        SQLITE_TEXT_CONST
    );
    assert_eq!(
        rust_expected_sqlite_type_for_decltype(blob.as_ptr()),
        SQLITE_BLOB_CONST
    );
    assert_eq!(
        rust_expected_sqlite_type_for_decltype(dt_int8.as_ptr()),
        SQLITE_INTEGER_CONST
    );
}

#[test]
fn expected_sqlite_type_for_decltype_unknown_returns_negative() {
    let numeric = c("NUMERIC");
    let unknown = c("WHAT");
    assert!(rust_expected_sqlite_type_for_decltype(numeric.as_ptr()) < 0);
    assert!(rust_expected_sqlite_type_for_decltype(unknown.as_ptr()) < 0);
    assert!(rust_expected_sqlite_type_for_decltype(std::ptr::null()) < 0);
}

#[test]
fn column_text_reformat_aggregate_int8() {
    let col = c("count");
    let sql = c("select count(*) from t");
    let src = c("123");
    let mut out = [0 as c_char; 32];

    let rc = rust_column_text_reformat_aggregate(
        col.as_ptr(),
        20,
        sql.as_ptr(),
        src.as_ptr(),
        out.as_mut_ptr(),
        out.len(),
    );

    assert_eq!(rc, 1);
    let out_s = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(out_s, "123");
}

#[test]
fn column_text_reformat_aggregate_alias_expression_int8() {
    let col = c("max(year)");
    let sql = c("select max(year) from t");
    let src = c("2026");
    let mut out = [0 as c_char; 32];

    let rc = rust_column_text_reformat_aggregate(
        col.as_ptr(),
        20,
        sql.as_ptr(),
        src.as_ptr(),
        out.as_mut_ptr(),
        out.len(),
    );

    assert_eq!(rc, 1);
    let out_s = unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(out_s, "2026");
}

#[test]
fn column_text_reformat_aggregate_non_match_returns_zero() {
    let col = c("id");
    let sql = c("select id from t");
    let src = c("456");
    let mut out = [0 as c_char; 32];

    let rc = rust_column_text_reformat_aggregate(
        col.as_ptr(),
        20,
        sql.as_ptr(),
        src.as_ptr(),
        out.as_mut_ptr(),
        out.len(),
    );
    assert_eq!(rc, 0);
}

// Plex lists a library's Categories (/library/sections/N/categories) by
// asking `select exists(select 1 ...)` per genre and keeping a genre whose
// answer is 1. PostgreSQL's boolean reads as the text "t"; SQLite has no
// booleans and answers 1, so every category was dropped (2026-10-08).
#[test]
fn column_text_transform_reads_a_boolean_as_sqlite_does() {
    for (src, want) in [("t", "1"), ("f", "0")] {
        let col = c("exists");
        let sql = c("SELECT EXISTS (SELECT 1 FROM taggings WHERE taggings.tag_id = $1)");
        let value = c(src);
        let mut out = [0 as c_char; 32];
        let rc = rust_column_text_transform(
            col.as_ptr(),
            16,
            sql.as_ptr(),
            value.as_ptr(),
            1,
            out.as_mut_ptr(),
            out.len(),
        );
        assert_eq!(rc, 1, "{src} is rewritten");
        let got = unsafe { CStr::from_ptr(out.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(got, want, "{src}");
    }
}

#[test]
fn column_text_transform_leaves_a_text_t_alone() {
    let col = c("tag");
    let sql = c("SELECT tags.tag FROM tags");
    let value = c("t");
    let mut out = [0 as c_char; 32];
    let rc = rust_column_text_transform(
        col.as_ptr(),
        25,
        sql.as_ptr(),
        value.as_ptr(),
        1,
        out.as_mut_ptr(),
        out.len(),
    );
    assert_eq!(rc, 0, "text is passed through untouched");
}

#[test]
fn owned_boolean_transform_matches_upstream_and_borrows_ordinary_text() {
    let name = c("exists");
    for (source, expected) in [("t", b"1\0"), ("f", b"0\0")] {
        let value = c(source);
        let owned = column_text_transform_owned(name.as_ptr(), 16, std::ptr::null(),
            value.as_ptr(), value.as_bytes()).unwrap();
        assert_eq!(owned, expected);
        let mut legacy = [0 as c_char; 8];
        assert_eq!(rust_column_text_transform(name.as_ptr(), 16, std::ptr::null(),
            value.as_ptr(), value.as_bytes().len(), legacy.as_mut_ptr(), legacy.len()), 1);
        assert_eq!(owned, unsafe { CStr::from_ptr(legacy.as_ptr()) }.to_bytes_with_nul());
        assert!(column_text_transform_owned(name.as_ptr(), 25, std::ptr::null(),
            value.as_ptr(), value.as_bytes()).is_none());
    }
}

#[test]
fn rewrite_server_uri_preserves_nested_escapes_unicode_and_json_fields() {
    let input = r#"{"pv:uri":"server://m/com.plexapp.plugins.library/library/sections/2/all?title=Pokémon%20A%26B&sort=titleSort%3Adesc","label":"unchanged"}"#;
    let (ok, out) = rewrite_with_buf(input, 2048);
    assert_eq!(ok, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["pv:uri"], "library:///directory/%2Flibrary%2Fsections%2F2%2Fall%3Ftitle%3DPok%C3%A9mon%2520A%2526B%26sort%3DtitleSort%253Adesc");
    assert_eq!(v["label"], "unchanged");
    let (again, _) = rewrite_with_buf(&out, 2048);
    assert_eq!(again, 0, "canonical URIs are already normalized");
}

#[test]
fn rewrite_server_uri_does_not_join_separate_fields_or_other_plugins() {
    for input in [
        r#"{"a":"server://m","b":"/com.plexapp.plugins.library/library/sections/2/all"}"#,
        "server://m/other/com.plexapp.plugins.library/library/sections/2/all",
    ] {
        let (ok, _) = rewrite_with_buf(input, 2048);
        assert_eq!(ok, 0);
    }
}

#[test]
fn owned_collection_uri_can_expand_past_input_and_fixed_buffers_fail_closed() {
    use crate::db_interpose_helpers::column_text_transform_owned;
    let uri = format!("server://m/com.plexapp.plugins.library/library/sections/2/all?{}", "x=%26&".repeat(200));
    let src = CString::new(uri.clone()).unwrap();
    let name = CString::new("extra_data").unwrap();
    let out = column_text_transform_owned(name.as_ptr(), 25, std::ptr::null(), src.as_ptr(), uri.as_bytes()).unwrap();
    assert_eq!(out.last(), Some(&0));
    let decoded = CStr::from_bytes_with_nul(&out).unwrap().to_str().unwrap();
    assert!(decoded.starts_with("library:///directory/%2Flibrary%2Fsections%2F2%2Fall%3F"));
    assert_eq!(decoded.matches("x%3D%2526%26").count(), 200);
    assert!(decoded.len() > uri.len());
    let (ok, _) = rewrite_with_buf(&uri, uri.len());
    assert_eq!(ok, 0, "a fixed buffer must not report a partial URI as success");
}


#[test]
fn rewrite_server_uri_updates_both_imported_extra_data_representations() {
    let input = r#"{"pv:uri":"server://m/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=titleSort%3Adesc","url":"keep=unchanged%2Fvalue&pv%3Auri=server%3A%2F%2Fm%2Fcom%2Eplexapp%2Eplugins%2Elibrary%2Flibrary%2Fsections%2F2%2Fall%3Ftype%3D2%26sort%3DtitleSort%253Adesc&after=still%2Bsame"}"#;
    let (ok, out) = rewrite_with_buf(input, 4096);
    assert_eq!(ok, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["pv:uri"], "library:///directory/%2Flibrary%2Fsections%2F2%2Fall%3Ftype%3D2%26sort%3DtitleSort%253Adesc");
    assert_eq!(v["url"], "keep=unchanged%2Fvalue&pv%3Auri=library%3A%2F%2F%2Fdirectory%2F%252Flibrary%252Fsections%252F2%252Fall%253Ftype%253D2%2526sort%253DtitleSort%25253Adesc&after=still%2Bsame");
    let (ok, _) = rewrite_with_buf(&out, 4096);
    assert_eq!(ok, 0);
}

#[test]
fn rewrite_server_uri_handles_encoded_only_and_invalid_percent_escapes() {
    let (ok, out) = rewrite_with_buf("pv%3Auri=server%3a%2f%2fm%2fcom.plexapp.plugins.library%2flibrary%2fsections%2f2%2fall", 4096);
    assert_eq!(ok, 1);
    assert_eq!(out, "pv%3Auri=library%3A%2F%2F%2Fdirectory%2F%252Flibrary%252Fsections%252F2%252Fall");
    assert_eq!(rewrite_with_buf("pv%3Auri=server%3A%2F%2Fm%zz", 4096).0, 0);
}
