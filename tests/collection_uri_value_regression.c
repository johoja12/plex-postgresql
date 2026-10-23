/* Run only against an isolated PostgreSQL-backed Plex handle. All writes roll back. */
#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static sqlite3 *db;
#define CHECK(x,n) do{if(!(x)){fprintf(stderr,"FAIL %d: %s\n",n,sqlite3_errmsg(db));exit(n);}}while(0)
int main(int argc,char **argv){
 if(argc!=2)return 64;
 CHECK(sqlite3_open(argv[1],&db)==SQLITE_OK,1);
 CHECK(sqlite3_exec(db,"BEGIN",0,0,0)==SQLITE_OK,2);
 const char *input="server://m/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=titleSort%3Adesc";
 const char *expected="library:///directory/%2Flibrary%2Fsections%2F2%2Fall%3Ftype%3D2%26sort%3DtitleSort%253Adesc";
 sqlite3_stmt *s=0;
 CHECK(sqlite3_prepare_v2(db,"INSERT INTO preferences(name,value) VALUES('codex_private_uri_value_probe',?)",-1,&s,0)==SQLITE_OK,3);
 CHECK(sqlite3_bind_text(s,1,input,-1,SQLITE_STATIC)==SQLITE_OK,4);CHECK(sqlite3_step(s)==SQLITE_DONE,5);sqlite3_finalize(s);
 CHECK(sqlite3_prepare_v2(db,"SELECT value AS extra_data FROM preferences WHERE name='codex_private_uri_value_probe'",-1,&s,0)==SQLITE_OK,6);
 CHECK(sqlite3_step(s)==SQLITE_ROW,7);
 sqlite3_value *v=sqlite3_column_value(s,0);CHECK(v!=0,8);
 /* This is the native PMS ORM path, before any column_text/blob call. */
 CHECK(sqlite3_value_bytes(v)==(int)strlen(expected),9);
 const unsigned char *text=sqlite3_value_text(v);CHECK(text && !strcmp((const char*)text,expected),10);
 const void *blob=sqlite3_value_blob(v);CHECK(blob && !memcmp(blob,expected,strlen(expected)),11);
 CHECK(sqlite3_column_bytes(s,0)==(int)strlen(expected),12);
 CHECK(!strcmp((const char*)sqlite3_column_text(s,0),expected),13);
 CHECK(!memcmp(sqlite3_column_blob(s,0),expected,strlen(expected)),14);
 sqlite3_finalize(s);CHECK(sqlite3_exec(db,"ROLLBACK",0,0,0)==SQLITE_OK,15);sqlite3_close(db);
 puts("PASS native value_* and column_* URI text/blob/length consistency; transaction rolled back");return 0;
}
