# Linux scanner identity and child completion

The scanner needs both a preloaded shim and successful process identification.
A mapped `db_interpose_pg.so` alone does not prove PostgreSQL routing.

## Process identification regression

Linux `/proc/self/cmdline` contains NUL-separated arguments. Previously,
`runtime_linux.rs` searched for the last slash in the entire buffer before
isolating argv[0]. A normal scanner invocation such as:

```text
/usr/lib/plexmediaserver/Plex Media Scanner.real --match --type 2 --item 123 --content-type=application/json
```

was classified as `json`. The constructor reported
`Not Plex Server/Scanner ('json'), skipping entirely`, enabled passthrough-only
mode, and the scanner read the SQLite shadow instead of PostgreSQL. Directory
arguments containing slashes could cause the same problem.

Split at the first NUL before taking the executable basename. Regression tests
cover the JSON MIME argument, media paths, helpers whose arguments mention Plex,
relative executable names, and empty input.

## Child completion

The Docker image sets `PLEX_PG_DISABLE_SIGCHLD_IGNORE=1`. This preserves Plex's
own SIGCHLD handler instead of forcing SIG_IGN, which auto-reaps children and
suppresses normal completion notifications. The existing explicit
`PLEX_PG_FORCE_SIGCHLD_IGNORE=1` override remains available. No other signal
interposition or process supervision settings are changed.

## Verification

```sh
cargo test --manifest-path rust/plex-pg-core/Cargo.toml \
  --features interpose --lib process_name_tests
```

Operational checks should confirm that a real scanner `--match` returns the
expected item, the shim does not classify it as `json`, and Plex logs scanner
exit status. For a running Linux server, SIGCHLD should be caught rather than
ignored in `/proc/<pid>/status`.

A restored isolated database reproduced the incorrect empty scanner result.
The fixed parser returned the expected episode/media and eliminated match-result
errors. With Plex's signal handler restored, scanner exit code 0 was logged.
The combined corrections passed 240 concurrent metadata requests over 15 forced
refresh rounds, plus full-catalogue ID comparisons, search and media range reads.
The same corrections were deployed in a composite image retaining existing Plex
patches and previously validated shim fixes; this is not a claim that all those
unrelated fixes are included here or that the stock upstream image was tested.

The investigation followed an outage with 50 request workers waiting on metadata
refreshes. The complete permanent worker-exhaustion condition was not
reproduced deterministically; the exact original race remains unproven. Early
trial codec-download delays also mean timing improvements cannot be attributed
solely to SIGCHLD handling.
