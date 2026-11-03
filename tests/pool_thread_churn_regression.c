/* Run only against an isolated PostgreSQL-backed library with POOL_SIZE=4,
 * POOL_MAX=4. Start with argv[0]="Plex Media Scanner" so the real shim loads.
 * One long-lived SQLite handle is reused by successive short-lived workers.
 * Occupy retired pthread IDs with non-DB parking threads: libc is otherwise
 * free to recycle the same ID and accidentally hide stranded ownership.
 * Unfixed p28 refuses prepare_v3 on worker five (SQLITE_BUSY, null statement).
 * The fixed shim must complete every real PostgreSQL SELECT without a reset
 * or a larger pool. No SQL writes are performed.
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>

typedef void DB;
typedef void ST;
static DB *db;
static int (*prepare)(DB *, const char *, int, unsigned, ST **, const char **);
static int (*step)(ST *);
static int (*finalize)(ST *);
static int (*column)(ST *, int);
static const char *(*error)(DB *);
static int failed;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int parked;
static int finished;

static void *park(void *unused) {
    (void)unused;
    pthread_mutex_lock(&lock);
    parked++;
    pthread_cond_broadcast(&cv);
    while (!finished) pthread_cond_wait(&cv, &lock);
    pthread_mutex_unlock(&lock);
    return NULL;
}

static void *query(void *arg) {
    int iteration = (int)(intptr_t)arg;
    char sql[128];
    snprintf(sql, sizeof(sql), "SELECT COUNT(*) FROM library_sections WHERE id <> %d", -iteration-1);
    ST *statement = NULL;
    int rc = prepare(db, sql, -1, 0, &statement, NULL);
    if (rc || !statement) {
        printf("iteration=%d prepare_rc=%d statement_null=%d error=%s\n", iteration, rc, statement == NULL, error(db));
        failed = 1;
        return NULL;
    }
    rc = step(statement);
    if (rc != 100 || column(statement, 0) < 1) {
        printf("iteration=%d step_rc=%d\n", iteration, rc);
        failed = 1;
    }
    finalize(statement);
    printf("iteration=%d prepared_and_read=%d\n", iteration, !failed);
    fflush(stdout);
    return NULL;
}

int main(void) {
    int (*open_db)(const char *, DB **, int, const char *) = dlsym(RTLD_DEFAULT, "sqlite3_open_v2");
    prepare = dlsym(RTLD_DEFAULT, "sqlite3_prepare_v3");
    step = dlsym(RTLD_DEFAULT, "sqlite3_step");
    finalize = dlsym(RTLD_DEFAULT, "sqlite3_finalize");
    column = dlsym(RTLD_DEFAULT, "sqlite3_column_int");
    error = dlsym(RTLD_DEFAULT, "sqlite3_errmsg");
    if (!open_db || !prepare || !step || !finalize || !column || !error) return 90;
    if (open_db("/tmp/pool-churn/com.plexapp.plugins.library.db", &db, 6|0x10000, NULL)) return 91;
    int total = getenv("POOL_CHURN_ITERATIONS") ? atoi(getenv("POOL_CHURN_ITERATIONS")) : 12;
    if (total < 5 || total > 256) return 92;
    pthread_t holders[256];
    for (int i=0; i<total; i++) {
        pthread_attr_t attributes;
        pthread_attr_init(&attributes);
        pthread_attr_setstacksize(&attributes, 2*1024*1024);
        pthread_t worker;
        if (pthread_create(&worker, &attributes, query, (void *)(intptr_t)i)) return 93;
        pthread_join(worker, NULL);
        if (failed) return 20;
        if (pthread_create(&holders[i], &attributes, park, NULL)) return 94;
        pthread_attr_destroy(&attributes);
        pthread_mutex_lock(&lock);
        while (parked < i+1) pthread_cond_wait(&cv, &lock);
        pthread_mutex_unlock(&lock);
    }
    pthread_mutex_lock(&lock);
    finished = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&lock);
    for (int i=0; i<total; i++) pthread_join(holders[i], NULL);
    puts("PASS: all worker generations prepared PostgreSQL reads within the original four-slot cap");
    return 0;
}
