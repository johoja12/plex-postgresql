/* Native shim regression: run on an isolated PostgreSQL-backed Plex handle.
 * Pass its SQLite shadow path as argv[1], with the shim enabled for argv[0].
 * Every test write is enclosed in a transaction and rolled back.
 */
#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static sqlite3 *db;
#define CHECK(cond,code) do{if(!(cond)){fprintf(stderr,"FAIL line %d code %d: %s\n",__LINE__,code,sqlite3_errmsg(db));exit(code);}}while(0)
static void exec(const char *q) { char *e=0;int rc=sqlite3_exec(db,q,0,0,&e);CHECK(rc==SQLITE_OK,2); }
static sqlite3_stmt *prep(const char *q){sqlite3_stmt *s=0;CHECK(sqlite3_prepare_v2(db,q,-1,&s,0)==SQLITE_OK,3);return s;}
int main(int argc, char **argv){
 if(argc!=2){fprintf(stderr,"usage: returning_write_regression ISOLATED_SHADOW_DB\n");return 64;}
 CHECK(sqlite3_open(argv[1],&db)==SQLITE_OK,1);
 exec("BEGIN");
 sqlite3_stmt *s=prep("INSERT INTO preferences(name,value) VALUES(:U1,:U2) ON CONFLICT(name) DO UPDATE SET value=excluded.value RETURNING id");
 CHECK(sqlite3_column_count(s)==1,4);
 CHECK(sqlite3_bind_text(s,1,"codex_private_returning_probe",-1,SQLITE_STATIC)==SQLITE_OK,5);
 CHECK(sqlite3_bind_text(s,2,"1",-1,SQLITE_STATIC)==SQLITE_OK,6);
 int rc=sqlite3_step(s);printf("checkpoint_upsert_first_step=%d expected_ROW=%d\n",rc,SQLITE_ROW);CHECK(rc==SQLITE_ROW,7);
 CHECK(sqlite3_column_int64(s,0)>0,8);sqlite3_int64 first_id=sqlite3_column_int64(s,0);CHECK(sqlite3_step(s)==SQLITE_DONE,9);
 CHECK(sqlite3_reset(s)==SQLITE_OK,24);CHECK(sqlite3_bind_text(s,2,"2",-1,SQLITE_STATIC)==SQLITE_OK,25);
 CHECK(sqlite3_step(s)==SQLITE_ROW && sqlite3_column_int64(s,0)==first_id,26);CHECK(sqlite3_step(s)==SQLITE_DONE,27);
 sqlite3_finalize(s);exec("ROLLBACK");exec("BEGIN");
 s=prep("INSERT INTO preferences(name,value) VALUES('codex_private_returning_a','a'),('codex_private_returning_b','b') RETURNING name,id");
 CHECK(sqlite3_column_count(s)==2,10);
 sqlite3_stmt *check=prep("SELECT count(*) FROM preferences WHERE name IN ('codex_private_returning_a','codex_private_returning_b')");
 CHECK(sqlite3_step(check)==SQLITE_ROW && sqlite3_column_int(check,0)==0,11);sqlite3_finalize(check);
 CHECK(sqlite3_step(s)==SQLITE_ROW,12);CHECK(strcmp((const char*)sqlite3_column_text(s,0),"codex_private_returning_a")==0,13);
 CHECK(sqlite3_step(s)==SQLITE_ROW,14);CHECK(strcmp((const char*)sqlite3_column_text(s,0),"codex_private_returning_b")==0,15);
 CHECK(sqlite3_step(s)==SQLITE_DONE,16);sqlite3_finalize(s);
 s=prep("UPDATE preferences SET value='changed' WHERE name='codex_private_returning_a' RETURNING id,value");
 CHECK(sqlite3_step(s)==SQLITE_ROW,17);CHECK(sqlite3_column_int64(s,0)>0 && strcmp((const char*)sqlite3_column_text(s,1),"changed")==0,18);
 CHECK(sqlite3_step(s)==SQLITE_DONE,19);sqlite3_finalize(s);
 s=prep("DELETE FROM preferences WHERE name IN ('codex_private_returning_a','codex_private_returning_b') RETURNING id");
 int rows=0;while((rc=sqlite3_step(s))==SQLITE_ROW){CHECK(sqlite3_column_int64(s,0)>0,20);rows++;}
 CHECK(rows==2 && rc==SQLITE_DONE,21);sqlite3_finalize(s);
 s=prep("UPDATE preferences SET value='none' WHERE name='codex_private_returning_absent' RETURNING id");
 CHECK(sqlite3_step(s)==SQLITE_DONE,22);sqlite3_finalize(s);
 s=prep("INSERT INTO preferences(name,value) VALUES('codex_private_returning_literal','RETURNING') /* RETURNING id */");
 CHECK(sqlite3_step(s)==SQLITE_DONE,23);sqlite3_finalize(s);
 exec("ROLLBACK");sqlite3_close(db);
 puts("PASS checkpoint upsert, multi-row INSERT/DELETE RETURNING, UPDATE RETURNING, zero-row DONE, literal/comment handling and metadata without early writes; all changes rolled back");return 0;
}
