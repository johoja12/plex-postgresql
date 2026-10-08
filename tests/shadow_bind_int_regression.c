/* Use the real Plex SQLite library and preloaded shim in a disposable,
 * network-disabled process named "Plex Media Scanner". No database server
 * or production files are accessed. The old shim deadlocks on the first
 * cached bind_int; run with an external timeout.
 */
#include <dlfcn.h>
#include <limits.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
typedef struct sqlite3 sqlite3;
typedef struct sqlite3_stmt sqlite3_stmt;
extern int sqlite3_open(const char *, sqlite3 **);
extern int sqlite3_prepare_v2(sqlite3 *, const char *, int, sqlite3_stmt **, const char **);
extern int sqlite3_bind_int(sqlite3_stmt *, int, int);
extern int sqlite3_step(sqlite3_stmt *);
extern int sqlite3_reset(sqlite3_stmt *);
static void *(*cache_stmt)(void *, void *, const char *, sqlite3_stmt *, const char *);
static int (*column_int)(sqlite3_stmt *, int);
static void check(int actual, int expected, const char *label) {
    if (actual != expected) {
        fprintf(stderr, "%s: got %d expected %d\n", label, actual, expected);
        exit(20);
    }
}
static void *worker(void *unused) {
    (void)unused;
    const int values[] = {0, 42, -42, INT_MIN, INT_MAX};
    for (int cached = 0; cached < 2; ++cached) {
        sqlite3 *db = NULL;
        sqlite3_stmt *stmt = NULL;
        check(sqlite3_open(":memory:", &db), 0, "open");
        check(sqlite3_prepare_v2(db, "SELECT ?", -1, &stmt, NULL), 0, "prepare");
        if (cached) {
            void *conn = calloc(1, 8192);
            if (!cache_stmt(NULL, conn, "SELECT ?", stmt, "SELECT $1")) exit(21);
        }
        for (int round = 0; round < 100; ++round) {
            int value = values[round % 5];
            check(sqlite3_bind_int(stmt, 1, value), 0, "bind int");
            check(sqlite3_step(stmt), 100, "row");
            check(column_int(stmt, 0), value, "signed value");
            check(sqlite3_step(stmt), 101, "done");
            check(sqlite3_reset(stmt), 0, "reset");
        }
    }
    return NULL;
}
int main(void) {
    void *sql = dlopen("/usr/lib/plexmediaserver/lib/libsqlite3.so", RTLD_NOW);
    if (!sql) return 22;
    cache_stmt = dlsym(RTLD_DEFAULT, "rust_step_cached_read_prepare_stmt");
    column_int = dlsym(sql, "sqlite3_column_int");
    if (!cache_stmt || !column_int) return 23;
    worker(NULL);
    pthread_t threads[4];
    for (int i = 0; i < 4; ++i) pthread_create(&threads[i], NULL, worker, NULL);
    for (int i = 0; i < 4; ++i) pthread_join(threads[i], NULL);
    puts("PASS: 1000 passthrough/cached integer bindings, signed limits, statement reuse and four threads");
    return 0;
}
