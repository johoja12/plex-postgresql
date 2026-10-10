/* Run in a disposable process with Plex's libsqlite3 and the shim preloaded.
 * Name the executable "Plex Media Scanner" to activate the shim constructor.
 * All queries use :memory:, with a synthetic cached-PG association only;
 * no PostgreSQL server or production files are accessed. Bound execution with
 * timeout: the old shim spins on the third step, while the fixed shim completes.
 * Example: LD_PRELOAD=/usr/local/lib/plex-postgresql/db_interpose_pg.so
 *          LD_LIBRARY_PATH=/usr/lib/plexmediaserver/lib:/usr/local/lib/plex-postgresql
 *          timeout 10 /usr/lib/plexmediaserver/lib/ld-musl-x86_64.so.1 './Plex Media Scanner'
 */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <pthread.h>
typedef struct sqlite3 sqlite3;
typedef struct sqlite3_stmt sqlite3_stmt;
extern int sqlite3_open(const char*,sqlite3**);
extern int sqlite3_prepare_v2(sqlite3*,const char*,int,sqlite3_stmt**,const char**);
extern int sqlite3_step(sqlite3_stmt*);
extern int sqlite3_reset(sqlite3_stmt*);
static void *(*cache_stmt)(void*,void*,const char*,sqlite3_stmt*,const char*);
static int (*bind_int)(sqlite3_stmt*,int,int);
static int (*column_int)(sqlite3_stmt*,int);
static void check(int got,int want,const char *label) {
 if(got!=want){fprintf(stderr,"%s got=%d expected=%d\n",label,got,want);exit(20);}
}
static void *worker(void *unused) {
 (void)unused;sqlite3 *db=0;sqlite3_stmt *s=0;
 check(sqlite3_open(":memory:",&db),0,"open");
 check(sqlite3_prepare_v2(db,"SELECT ?",-1,&s,0),0,"prepare");
 check(bind_int(s,1,42),0,"bind");
 void *conn=calloc(1,8192);
 if(!cache_stmt(NULL,conn,"SELECT ?",s,"SELECT $1"))exit(21);
 for(int i=0;i<100;i++) {
  check(sqlite3_step(s),100,"row");check(column_int(s,0),42,"bound value");
  check(sqlite3_step(s),101,"done");
  check(sqlite3_step(s),100,"auto reset row");check(column_int(s,0),42,"auto reset bound value");
  check(sqlite3_reset(s),0,"explicit reset");
 }
 return NULL;
}
int main(void) {
 void *sql=dlopen("/usr/lib/plexmediaserver/lib/libsqlite3.so",RTLD_NOW);
 if(!sql){fprintf(stderr,"dlopen failed: %s\n",dlerror());return 22;}
 cache_stmt=dlsym(RTLD_DEFAULT,"rust_step_cached_read_prepare_stmt");
 bind_int=dlsym(sql,"sqlite3_bind_int");column_int=dlsym(sql,"sqlite3_column_int");
 if(!cache_stmt||!bind_int||!column_int)return 23;
 fprintf(stderr,"Starting cached shadow reset regression\n");
 worker(NULL);
 pthread_t threads[4];for(int i=0;i<4;i++)pthread_create(&threads[i],NULL,worker,NULL);
 for(int i=0;i<4;i++)pthread_join(threads[i],NULL);
 fprintf(stderr,"PASS: 500 cached statement reuse cycles, auto/explicit reset, bindings, four threads\n");
 return 0;
}
