use super::super::connection_lifecycle::handle_session;
use super::super::session::{session_statement_timeout_sql, SESSION_STATEMENT_TIMEOUT_SQL};

const DATABASES: &str =
    "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases";

#[test]
fn a_guide_session_starts_with_no_statement_timeout_and_a_library_one_with_sixty_seconds() {
    // A guide's reads never reset the timeout (.51), so its session's own
    // timeout is the one PMS's whole-guide read runs under: talos's ran about
    // six minutes and was cancelled at sixty seconds under .53.
    assert_eq!(
        session_statement_timeout_sql(true),
        "SET statement_timeout = 0"
    );
    assert_eq!(
        session_statement_timeout_sql(false),
        SESSION_STATEMENT_TIMEOUT_SQL
    );
    assert_eq!(
        SESSION_STATEMENT_TIMEOUT_SQL,
        "SET statement_timeout = '60s'"
    );
}

#[test]
fn a_handle_connection_is_a_guide_s_by_its_path() {
    let guide =
        format!("{DATABASES}/tv.plex.providers.epg.cloud-0ab001b3-182f-4835-897c-43a9ed6797c5.db");
    assert_eq!(
        handle_session(&guide, "plex"),
        ("epg_0ab001b3182f4835897c43a9ed6797c5".to_string(), true)
    );

    let load = format!(
        "{DATABASES}/tv.plex.providers.epg.cloud-0ab001b3-182f-4835-897c-43a9ed6797c5-loading.db"
    );
    assert_eq!(
        handle_session(&load, "plex"),
        (
            "epg_0ab001b3182f4835897c43a9ed6797c5_next".to_string(),
            true
        )
    );

    let library = format!("{DATABASES}/com.plexapp.plugins.library.db");
    assert_eq!(
        handle_session(&library, "plex"),
        ("plex".to_string(), false)
    );
    assert_eq!(handle_session("", "plex"), ("plex".to_string(), false));
}
