#!/bin/sh
# Plex may clear LD_PRELOAD for scanner children. Restore only the PostgreSQL
# shim so scanner database access uses the same backend as the server.
export LD_PRELOAD=/usr/local/lib/plex-postgresql/db_interpose_pg.so
exec "/usr/lib/plexmediaserver/Plex Media Scanner.real" "$@"
