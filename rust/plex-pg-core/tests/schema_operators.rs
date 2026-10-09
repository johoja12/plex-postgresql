//! The operators the schema files create live in the schema they name, and
//! nowhere else.
//!
//! An unqualified `COMMUTATOR = =` (or `NEGATOR`) names an operator to be
//! looked up on the search path, and when none is found PostgreSQL creates a
//! shell for it in the first schema of the search path. pg_compat_functions.sql
//! created public.= for (boolean, integer) and (integer, boolean) that way, so
//! a fresh database loaded with `plex` first on its search path, as the
//! entrypoint's (PGUSER=plex) and cluster-plex's are, got shell operators in
//! `plex`. A shell sits ahead of public's real operator, and every comparison
//! of an integer with a boolean failed with "operator is only a shell".

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use postgres::NoTls;
use regex::Regex;

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema")
}

/// Every COMMUTATOR and NEGATOR in schema/*.sql names its operator with its
/// schema, as `OPERATOR(schema.op)`. This runs without a server.
#[test]
fn every_commutator_and_negator_names_its_schema() {
    let clause = Regex::new(r"(?i)\b(COMMUTATOR|NEGATOR)\s*=\s*(\S+)").unwrap();
    let qualified = Regex::new(r"(?i)^OPERATOR\(\s*[a-z_][a-z0-9_]*\.").unwrap();

    let mut files: Vec<PathBuf> = fs::read_dir(schema_dir())
        .expect("read schema/")
        .map(|e| e.expect("schema/ entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .sql files in {:?}", schema_dir());

    let mut seen = 0;
    let mut bad = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read schema file");
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("--") {
                continue;
            }
            for c in clause.captures_iter(line) {
                seen += 1;
                if !qualified.is_match(&c[2]) {
                    bad.push(format!(
                        "{}:{}: {}",
                        file.file_name().unwrap().to_string_lossy(),
                        n + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(seen > 0, "found no COMMUTATOR or NEGATOR at all");
    assert!(
        bad.is_empty(),
        "unqualified operator names create shells in the search path's first schema:\n{}",
        bad.join("\n")
    );
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

struct Server {
    host: String,
    port: String,
    user: String,
    password: String,
    admin_db: String,
}

impl Server {
    fn from_env() -> Self {
        Server {
            host: env_or("PLEX_PG_HOST", "/tmp"),
            port: env_or("PLEX_PG_PORT", "5432"),
            user: env_or("PLEX_PG_USER", "plex"),
            password: env_or("PLEX_PG_PASSWORD", ""),
            admin_db: env_or("PLEX_PG_DATABASE", "plex"),
        }
    }

    fn connect(&self, db: &str, search_path: Option<&str>) -> postgres::Client {
        let mut cfg = postgres::Config::new();
        cfg.host(&self.host)
            .port(self.port.parse().expect("PLEX_PG_PORT"))
            .user(&self.user)
            .dbname(db);
        if !self.password.is_empty() {
            cfg.password(&self.password);
        }
        if let Some(sp) = search_path {
            cfg.options(&format!("-c search_path={sp}"));
        }
        cfg.connect(NoTls).expect("connect to postgres")
    }

    /// psql -f, as the entrypoint loads each file: no ON_ERROR_STOP, so a
    /// failing statement is skipped.
    fn psql_file(&self, db: &str, search_path: &str, file: &Path) {
        let out = Command::new("psql")
            .arg("-q")
            .arg("-f")
            .arg(file)
            .env("PGHOST", &self.host)
            .env("PGPORT", &self.port)
            .env("PGUSER", &self.user)
            .env("PGPASSWORD", &self.password)
            .env("PGDATABASE", db)
            .env("PGOPTIONS", format!("-c search_path={search_path}"))
            .output()
            .expect("run psql");
        assert!(out.status.success(), "psql -f {file:?}: {out:?}");
    }
}

struct FreshDatabase<'a> {
    server: &'a Server,
    name: String,
}

impl<'a> FreshDatabase<'a> {
    fn create(server: &'a Server) -> Self {
        let name = format!(
            "schema_ops_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let mut admin = server.connect(&server.admin_db, None);
        admin
            .simple_query(&format!("CREATE DATABASE {name}"))
            .expect("create database");
        FreshDatabase { server, name }
    }
}

impl Drop for FreshDatabase<'_> {
    fn drop(&mut self) {
        let mut admin = self.server.connect(&self.server.admin_db, None);
        let _ = admin.simple_query(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        ));
    }
}

/// Loads the schema files into a fresh database the way the entrypoint's
/// init_schema does, under each search path a loader uses, and checks that no
/// operator on boolean and integer lands in `plex`.
///
/// Needs a PostgreSQL server (13 or later) and psql on PATH:
///
///   PLEX_PG_HOST=127.0.0.1 PLEX_PG_PORT=5432 PLEX_PG_USER=plex \
///   PLEX_PG_PASSWORD=plex PLEX_PG_DATABASE=plex \
///     cargo test --test schema_operators -- --ignored
#[test]
#[ignore = "needs a PostgreSQL server and psql; see the doc comment"]
fn a_fresh_database_gets_no_operator_shells_in_plex() {
    let server = Server::from_env();
    // "plex,public" is what the entrypoint gets from the default
    // "$user", public with PGUSER=plex; cluster-plex sets "plex" alone.
    for search_path in ["plex,public", "plex"] {
        let db = FreshDatabase::create(&server);
        let mut conn = server.connect(&db.name, Some(search_path));
        conn.batch_execute("CREATE SCHEMA plex; CREATE EXTENSION pg_trgm;")
            .expect("create schema and extension");

        // The compatibility functions run again at every start.
        for file in [
            "plex_schema.sql",
            "seed_data.sql",
            "sqlite_column_types.sql",
            "pg_compat_functions.sql",
            "pg_compat_functions.sql",
        ] {
            server.psql_file(&db.name, search_path, &schema_dir().join(file));
        }

        let rows = conn
            .query(
                "SELECT format('%s.%s(%s, %s)', n.nspname, o.oprname,
                               o.oprleft::regtype, o.oprright::regtype)
                   FROM pg_operator o JOIN pg_namespace n ON n.oid = o.oprnamespace
                  WHERE n.nspname = 'plex'
                    AND (o.oprleft, o.oprright) IN (('boolean'::regtype, 'integer'::regtype),
                                                    ('integer'::regtype, 'boolean'::regtype))
                  ORDER BY 1",
                &[],
            )
            .expect("list operators");
        let stray: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        assert!(
            stray.is_empty(),
            "search_path {search_path:?}: operators in plex: {stray:?}"
        );

        // What the shells broke: both comparisons resolve to public's real
        // operators, and each names the other as its commutator.
        let mut shim = server.connect(&db.name, Some("plex,public"));
        let row = shim
            .query_one("SELECT 1 = true, true = 1, 0 = true, false = 0", &[])
            .expect("compare integer with boolean");
        assert_eq!(
            (
                row.get::<_, bool>(0),
                row.get::<_, bool>(1),
                row.get::<_, bool>(2),
                row.get::<_, bool>(3)
            ),
            (true, true, false, true),
            "search_path {search_path:?}"
        );
        let rows = shim
            .query(
                "SELECT format('%s.%s(%s, %s)', cn.nspname, c.oprname,
                               c.oprleft::regtype, c.oprright::regtype)
                   FROM pg_operator o JOIN pg_namespace n ON n.oid = o.oprnamespace
                   JOIN pg_operator c ON c.oid = o.oprcom
                   JOIN pg_namespace cn ON cn.oid = c.oprnamespace
                  WHERE n.nspname = 'public' AND o.oprname = '='
                    AND (o.oprleft, o.oprright) IN (('boolean'::regtype, 'integer'::regtype),
                                                    ('integer'::regtype, 'boolean'::regtype))
                  ORDER BY o.oprleft::regtype::text",
                &[],
            )
            .expect("list commutators");
        let commutators: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        assert_eq!(
            commutators,
            ["public.=(integer, boolean)", "public.=(boolean, integer)"],
            "search_path {search_path:?}"
        );
    }
}
