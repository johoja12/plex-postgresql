#!/bin/bash
# Migrate Plex data from PostgreSQL back to SQLite
#
# Use this to switch back to the standard SQLite-based Plex setup.
# Creates a fresh SQLite database from your PostgreSQL data.
#
# Prerequisites:
#   - psql (PostgreSQL client) installed
#   - sqlite3 installed
#   - Access to the PostgreSQL database
#
# Usage:
#   ./scripts/migrate_pg_to_sqlite.sh
#
# The script will create:
#   - com.plexapp.plugins.library.db       (main library)
#   - com.plexapp.plugins.library.blobs.db  (subtitles, detection and analysis data)
#
# After migration:
#   1. Stop Plex
#   2. Remove/disable the PostgreSQL shim (LD_PRELOAD / DYLD_INSERT_LIBRARIES)
#   3. Copy the generated .db files to your Plex database directory
#   4. Start Plex normally

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Configuration (from environment or defaults)
PG_HOST="${PLEX_PG_HOST:-localhost}"
PG_PORT="${PLEX_PG_PORT:-5432}"
PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
PG_USER="${PLEX_PG_USER:-plex}"
PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"

export PGHOST="$PG_HOST"
export PGPORT="$PG_PORT"
export PGDATABASE="$PG_DATABASE"
export PGUSER="$PG_USER"
export PGPASSWORD="${PLEX_PG_PASSWORD:-plex}"

PSQL="psql -t -A"

# Output directory
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUTPUT_DIR="${OUTPUT_DIR:-$(pwd)}"
SQLITE_DB="$OUTPUT_DIR/com.plexapp.plugins.library.db"
BLOBS_DB="$OUTPUT_DIR/com.plexapp.plugins.library.blobs.db"
SQLITE_SCHEMA="${SCRIPT_DIR}/../schema/sqlite_schema.sql"

# Detect Plex database directory
detect_plex_db_dir() {
    local uname_s=$(uname -s)
    if [[ "$uname_s" == "Darwin" ]]; then
        echo "$HOME/Library/Application Support/Plex Media Server/Plug-in Support/Databases"
    elif [[ -d "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases" ]]; then
        # Docker
        echo "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases"
    else
        echo "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases"
    fi
}

echo "=== PostgreSQL to SQLite Migration ==="
echo ""
echo "PostgreSQL: $PG_USER@$PG_HOST:$PG_PORT/$PG_DATABASE (schema: $PG_SCHEMA)"
echo "Output:     $OUTPUT_DIR"
echo ""

# Check dependencies
for cmd in psql sqlite3; do
    if ! command -v "$cmd" &>/dev/null; then
        echo -e "${RED}ERROR: '$cmd' not found. Install it first.${NC}"
        exit 1
    fi
done

# Check PostgreSQL connection
if ! psql -c "SELECT 1" >/dev/null 2>&1; then
    echo -e "${RED}ERROR: Cannot connect to PostgreSQL at $PG_HOST:$PG_PORT${NC}"
    echo "Set PLEX_PG_HOST, PLEX_PG_PORT, PLEX_PG_USER, PLEX_PG_PASSWORD environment variables."
    exit 1
fi

# Check if PostgreSQL has data
PG_COUNT=$($PSQL -c "SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items;" 2>/dev/null || echo "0")
if [[ "$PG_COUNT" -eq 0 ]]; then
    echo -e "${YELLOW}PostgreSQL database is empty. Nothing to migrate.${NC}"
    exit 0
fi

echo "Found $PG_COUNT items in PostgreSQL."
echo ""

# Warn if output files exist
if [[ -f "$SQLITE_DB" ]]; then
    echo -e "${YELLOW}WARNING: $SQLITE_DB already exists.${NC}"
    read -p "Overwrite? [y/N]: " overwrite
    if [[ ! "$overwrite" =~ ^[Yy] ]]; then
        echo "Cancelled."
        exit 0
    fi
    rm -f "$SQLITE_DB" "${SQLITE_DB}-wal" "${SQLITE_DB}-journal" "${SQLITE_DB}-shm"
fi

# ============================================================================
# Step 1: Create SQLite database with schema
# ============================================================================
echo "Step 1: Creating SQLite database with schema..."

if [[ -f "$SQLITE_SCHEMA" ]]; then
    # Filter out virtual table and spellfix definitions (they need special extensions)
    # Keep only CREATE TABLE, CREATE INDEX, and CREATE UNIQUE INDEX
    sqlite3 "$SQLITE_DB" < <(grep -E '^(CREATE TABLE|CREATE INDEX|CREATE UNIQUE INDEX|INSERT)' "$SQLITE_SCHEMA" | grep -v 'VIRTUAL TABLE' | grep -v 'spellfix' | grep -v 'fts4' | grep -v 'rtree')
    echo -e "  ${GREEN}Schema loaded${NC}"
else
    echo -e "${RED}ERROR: SQLite schema not found at $SQLITE_SCHEMA${NC}"
    echo "Make sure you're running this from the plex-postgresql directory."
    exit 1
fi

# ============================================================================
# Step 2: Get list of tables from PostgreSQL
# ============================================================================
echo ""
echo "Step 2: Migrating tables from PostgreSQL..."

# Get tables that exist in both PostgreSQL and SQLite
PG_TABLES=$($PSQL -c "
    SELECT table_name FROM information_schema.tables
    WHERE table_schema = '$PG_SCHEMA'
    AND table_type = 'BASE TABLE'
    ORDER BY table_name;
")

SQLITE_TABLES=$(sqlite3 "$SQLITE_DB" ".tables" | tr -s ' ' '\n' | grep -v '^$' | sort)

migrated=0
skipped=0
failed=0

for table in $PG_TABLES; do
    # Skip blobs table (handled separately)
    if [[ "$table" == "blobs" ]]; then
        continue
    fi

    # Check if table exists in SQLite schema
    if ! echo "$SQLITE_TABLES" | grep -qw "$table"; then
        continue
    fi

    # Get row count
    count=$($PSQL -c "SELECT COUNT(*) FROM $PG_SCHEMA.\"$table\";" 2>/dev/null || echo "0")

    if [[ "$count" -eq 0 ]]; then
        continue
    fi

    printf "  %-40s %8s rows... " "$table" "$count"

    # Get PostgreSQL columns for this table
    pg_cols=$($PSQL -c "
        SELECT string_agg(column_name, ',' ORDER BY ordinal_position)
        FROM information_schema.columns
        WHERE table_schema = '$PG_SCHEMA' AND table_name = '$table';
    ")

    # Get SQLite columns
    sqlite_cols=$(sqlite3 "$SQLITE_DB" "PRAGMA table_info(\"$table\");" | cut -d'|' -f2 | tr '\n' ',' | sed 's/,$//')

    if [[ -z "$pg_cols" ]] || [[ -z "$sqlite_cols" ]]; then
        echo -e "${YELLOW}SKIP (no columns)${NC}"
        ((skipped++))
        continue
    fi

    # Find common columns
    common_cols=""
    IFS=',' read -ra SQLITE_COL_ARRAY <<< "$sqlite_cols"
    for col in "${SQLITE_COL_ARRAY[@]}"; do
        col=$(echo "$col" | tr -d ' ')
        if echo ",$pg_cols," | grep -q ",$col,"; then
            if [[ -z "$common_cols" ]]; then
                common_cols="\"$col\""
            else
                common_cols="$common_cols,\"$col\""
            fi
        fi
    done

    if [[ -z "$common_cols" ]]; then
        echo -e "${RED}SKIP (no common cols)${NC}"
        ((skipped++))
        continue
    fi

    # Export from PostgreSQL to CSV
    tmpfile="/tmp/plex_pg2sqlite_${table}.csv"
    psql -c "\\copy (SELECT $common_cols FROM $PG_SCHEMA.\"$table\") TO '$tmpfile' WITH CSV HEADER" 2>/dev/null

    if [[ -s "$tmpfile" ]]; then
        # Import into SQLite
        if sqlite3 "$SQLITE_DB" ".mode csv" ".import --skip 1 $tmpfile $table" 2>/dev/null; then
            echo -e "${GREEN}OK${NC}"
            ((migrated++))
        else
            echo -e "${RED}FAIL${NC}"
            ((failed++))
        fi
    else
        echo -e "${YELLOW}EMPTY${NC}"
    fi

    rm -f "$tmpfile"
done

echo ""
echo "Migration summary (library.db):"
echo "  Tables migrated: $migrated"
echo "  Tables skipped:  $skipped"
echo "  Tables failed:   $failed"

sqlite_count=$(sqlite3 "$SQLITE_DB" "SELECT COUNT(*) FROM metadata_items;" 2>/dev/null || echo "0")
echo "  Total items in SQLite: $sqlite_count"

# ============================================================================
# Step 3: Migrate blobs (subtitles, detection and analysis data)
# ============================================================================
echo ""
echo "Step 3: Migrating blobs (subtitles, detection and analysis data)..."

blob_count=$($PSQL -c "SELECT COUNT(*) FROM $PG_SCHEMA.blobs;" 2>/dev/null || echo "0")

if [[ "$blob_count" -gt 0 ]]; then
    echo "  Found $blob_count blobs in PostgreSQL"

    # Remove existing blobs.db
    rm -f "$BLOBS_DB" "${BLOBS_DB}-wal" "${BLOBS_DB}-journal" "${BLOBS_DB}-shm"

    # Create blobs.db schema
    sqlite3 "$BLOBS_DB" "
        CREATE TABLE IF NOT EXISTS blobs (
            id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
            blob blob,
            linked_type varchar(255),
            linked_id integer,
            linked_guid varchar(255),
            created_at integer(8),
            blob_type integer
        );
        CREATE INDEX IF NOT EXISTS index_blobs_on_linked_type ON blobs (linked_type);
        CREATE UNIQUE INDEX IF NOT EXISTS index_blobs_on_linked_type_linked_id_and_blob_type ON blobs (linked_type, linked_id, blob_type);
        CREATE UNIQUE INDEX IF NOT EXISTS index_blobs_on_linked_type_linked_guid_and_blob_type ON blobs (linked_type, linked_guid, blob_type);
    "

    # Export blob metadata via CSV (no binary data)
    psql -c "\\copy (SELECT id, linked_type, linked_id, linked_guid, EXTRACT(EPOCH FROM created_at)::bigint AS created_at, blob_type FROM $PG_SCHEMA.blobs) TO '/tmp/plex_blobs_meta.csv' WITH CSV HEADER" 2>/dev/null || \
    psql -c "\\copy (SELECT id, linked_type, linked_id, linked_guid, created_at, blob_type FROM $PG_SCHEMA.blobs) TO '/tmp/plex_blobs_meta.csv' WITH CSV HEADER" 2>/dev/null

    if [[ -s "/tmp/plex_blobs_meta.csv" ]]; then
        sqlite3 "$BLOBS_DB" ".mode csv" ".import --skip 1 /tmp/plex_blobs_meta.csv blobs" 2>/dev/null
    fi

    # Migrate blob binary data in batches using hex encoding
    echo "  Migrating blob data (this may take a while)..."
    batch=100
    offset=0
    blob_migrated=0

    while true; do
        # Export batch of blobs as hex-encoded SQL UPDATE statements
        $PSQL -c "
            SELECT 'UPDATE blobs SET blob = x''' || encode(blob, 'hex') || ''' WHERE id = ' || id || ';'
            FROM $PG_SCHEMA.blobs
            WHERE blob IS NOT NULL
            ORDER BY id
            LIMIT $batch OFFSET $offset;
        " > /tmp/plex_blob_batch.sql 2>/dev/null

        # Check if batch is empty
        if [[ ! -s /tmp/plex_blob_batch.sql ]]; then
            break
        fi

        # Execute batch in SQLite
        sqlite3 "$BLOBS_DB" < /tmp/plex_blob_batch.sql 2>/dev/null

        offset=$((offset + batch))
        blob_migrated=$((blob_migrated + batch))
        if [[ $blob_migrated -le $blob_count ]]; then
            printf "    Progress: %d / %d\r" "$blob_migrated" "$blob_count"
        fi
    done
    echo ""

    sqlite_blob_count=$(sqlite3 "$BLOBS_DB" "SELECT COUNT(*) FROM blobs;" 2>/dev/null || echo "0")
    echo -e "  ${GREEN}Done: $sqlite_blob_count blobs in SQLite${NC}"

    rm -f /tmp/plex_blobs_meta.csv /tmp/plex_blob_batch.sql
else
    echo "  No blobs found in PostgreSQL, skipping."
fi

# ============================================================================
# Step 4: Verify
# ============================================================================
echo ""
echo "=== Migration Complete ==="
echo ""
echo "Files created:"
echo "  $SQLITE_DB"
if [[ -f "$BLOBS_DB" ]]; then
    echo "  $BLOBS_DB"
fi
echo ""

# Show counts
echo "Verification:"
echo "  PostgreSQL metadata_items: $PG_COUNT"
echo "  SQLite metadata_items:     $sqlite_count"
if [[ "$blob_count" -gt 0 ]]; then
    echo "  PostgreSQL blobs:          $blob_count"
    echo "  SQLite blobs:              $sqlite_blob_count"
fi
echo ""

# Instructions
PLEX_DB_DIR=$(detect_plex_db_dir)
echo "To switch back to SQLite:"
echo ""
echo "  1. Stop Plex Media Server"
echo ""
echo "  2. Remove the PostgreSQL shim:"
echo "     # macOS: remove DYLD_INSERT_LIBRARIES from your start script"
echo "     # Linux: remove LD_PRELOAD from systemd config or run:"
echo "     #   sudo ./scripts/uninstall_wrappers_linux.sh"
echo "     # Docker: remove LD_PRELOAD from docker-compose.yml"
echo ""
echo "  3. Back up your current SQLite database (if any):"
echo "     cp \"$PLEX_DB_DIR/com.plexapp.plugins.library.db\" \\"
echo "        \"$PLEX_DB_DIR/com.plexapp.plugins.library.db.bak\""
echo ""
echo "  4. Copy the new files:"
echo "     cp \"$SQLITE_DB\" \"$PLEX_DB_DIR/\""
if [[ -f "$BLOBS_DB" ]]; then
    echo "     cp \"$BLOBS_DB\" \"$PLEX_DB_DIR/\""
fi
echo ""
echo "  5. Start Plex normally"
