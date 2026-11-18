#!/usr/bin/env bash
# rig.sh: the containers of the Plex upgrade rig (docs/maintenance-checklist.md,
# rule 17; scripts/upgrade-rig/README.md).
#
#   rig.sh db <A|B>            a PostgreSQL for one side, on a fresh copy of $RIG_DB_VOLUME
#   rig.sh plex <A|B> <image>  the shimmed Plex of one side (a Dockerfile.standalone build)
#   rig.sh stock <version>     an unshimmed plexinc/pms-docker:<version>, the reference (side C)
#   rig.sh tuner <dir>         an HTTP server for an HDHomeRun stub's files, as http://tuner/
#   rig.sh wait <A|B|C>        until the side answers /identity
#   rig.sh down                remove the rig's containers (volumes stay)
#
# A is the Plex release in use, B the one to move to. Ports 8091 (A), 8092 (B), 8093 (C).
# RIG_DB_VOLUME names a docker volume holding a PostgreSQL 15 data directory to copy,
# for instance a restored pg_dump of the live database; unset, each side starts empty.
# RIG_MEDIA is a directory of sample media, mounted at /media. No side is claimed: the
# preferences carry no account, so the rig is nobody's server.
set -euo pipefail
RIG_MEDIA=${RIG_MEDIA:-$PWD/fixtures/media}
port() { case "$1" in A) echo 8091 ;; B) echo 8092 ;; *) echo 8093 ;; esac; }
mid() { case "$1" in A) echo aaaaaaaa11114111a111aaaaaaaaaaaa ;; B) echo bbbbbbbb22224222b222bbbbbbbbbbbb ;; *) echo cccccccc33334333c333cccccccccccc ;; esac; }
docker network create pgrig >/dev/null 2>&1 || true

# Production's preferences (cluster-plex's): no butler task and no scan runs on its own,
# so both sides do only what the mix asks of them. No ProcessedMachineIdentifier: PMS
# derives it.
prefs() {
  local X=$1
  docker volume create "pgrig${X}_cfg" >/dev/null
  docker run --rm -v "pgrig${X}_cfg:/config" alpine sh -c '
    d="/config/Library/Application Support/Plex Media Server"; mkdir -p "$d"
    [ -f "$d/Preferences.xml" ] || cat > "$d/Preferences.xml" <<XML
<?xml version="1.0" encoding="utf-8"?>
<Preferences OldestPreviousVersion="1.43.0.10492-121068a07" MachineIdentifier="'"$(mid "$X")"'" AnonymousMachineIdentifier="'"$(mid "$X")"'" AcceptedEULA="1" PublishServerOnPlexOnline="0" FriendlyName="pgrig" ButlerTaskAnalyzeMedia="0" ButlerTaskBackupDatabase="0" ButlerTaskCleanOldBundles="0" ButlerTaskCleanOldCacheFiles="0" ButlerTaskDeepMediaAnalysis="0" ButlerTaskGarbageCollectBlobs="0" ButlerTaskGenerateAutoTags="0" ButlerTaskGenerateChapterThumbs="0" ButlerTaskGenerateMediaIndexFiles="0" ButlerTaskOptimizeDatabase="0" ButlerTaskRefreshEpgGuides="0" ButlerTaskRefreshLibraries="0" ButlerTaskRefreshLocalMedia="0" ButlerTaskRefreshPeriodicMetadata="0" ButlerTaskUpgradeMediaAnalysis="0" FSEventLibraryPartialScanEnabled="0" FSEventLibraryUpdatesEnabled="0" GdmEnabled="0" GenerateAdMarkerBehavior="never" GenerateBIFBehavior="never" GenerateChapterThumbBehavior="never" GenerateCreditsMarkerBehavior="never" GenerateIntroMarkerBehavior="never" GenerateVADBehavior="never" LoudnessAnalysisBehavior="never" ManualPortMappingMode="1" MarkerSource="cloud" MusicAnalysisBehavior="never" RelayEnabled="0" ScheduledLibraryUpdatesEnabled="0" allowMediaDeletion="1" autoEmptyTrash="0" logDebug="1" DlnaEnabled="0" sendCrashReports="0" DvrIncrementalEpgLoader="1"/>
XML
    chown -R 1000:1000 /config'
}

case "${1:-}" in
db)
  X=$2
  docker rm -f "pgrig$X-pg" >/dev/null 2>&1 || true
  docker volume rm "pgrig${X}_pg" >/dev/null 2>&1 || true
  docker volume create "pgrig${X}_pg" >/dev/null
  if [ -n "${RIG_DB_VOLUME:-}" ]; then
    docker run --rm -v "$RIG_DB_VOLUME:/src:ro" -v "pgrig${X}_pg:/dst" alpine \
      sh -c 'cp -a /src/. /dst/ && rm -f /dst/postmaster.pid'
  fi
  # Every statement is logged: the two sides' logs are compared by statement shape.
  docker run -d --name "pgrig$X-pg" --network pgrig --network-alias "pg$X" \
    -e POSTGRES_USER=plex -e POSTGRES_PASSWORD=plex -e POSTGRES_DB=plex \
    -v "pgrig${X}_pg:/var/lib/postgresql/data" postgres:15-alpine postgres \
    -c shared_buffers=256MB -c work_mem=16MB -c max_connections=300 \
    -c log_statement=all -c log_min_error_statement=error -c log_min_messages=warning \
    -c "log_line_prefix=%m [%p] %a " >/dev/null
  ;;
plex)
  X=$2 IMG=$3
  docker rm -f "pgrig$X-plex" >/dev/null 2>&1 || true
  prefs "$X"
  # ERROR is what production logs at; DEBUG makes every column read a log line, and a
  # whole-library request minutes long.
  docker run -d --name "pgrig$X-plex" --network pgrig --network-alias "plex$X" \
    -p "127.0.0.1:$(port "$X"):32400" -v "pgrig${X}_cfg:/config" -v "$RIG_MEDIA:/media:ro" -e TZ=UTC \
    -e "PLEX_PG_HOST=pg$X" -e PLEX_PG_PORT=5432 -e PLEX_PG_DATABASE=plex -e PLEX_PG_USER=plex \
    -e PLEX_PG_PASSWORD=plex -e PLEX_PG_SCHEMA=plex -e PLEX_PG_POOL_SIZE=50 -e PLEX_PG_POOL_MAX=100 \
    -e "PLEX_PG_LOG_LEVEL=${PLEX_PG_LOG_LEVEL:-ERROR}" -e PLEX_PG_LOG_MAX_SIZE=2G \
    -e "PLEX_PG_LOG_FILE=/config/Library/Application Support/Plex Media Server/Logs/plex_redirect_pg.log" \
    "$IMG" >/dev/null
  ;;
stock)
  docker rm -f pgrigC-plex >/dev/null 2>&1 || true
  prefs C
  docker run -d --name pgrigC-plex --network pgrig --network-alias plexC \
    -p "127.0.0.1:$(port C):32400" -v pgrigC_cfg:/config -v "$RIG_MEDIA:/media:ro" -e TZ=UTC \
    "plexinc/pms-docker:$2" >/dev/null
  ;;
tuner)
  docker rm -f pgrig-tuner >/dev/null 2>&1 || true
  docker run -d --name pgrig-tuner --network pgrig --network-alias tuner \
    -v "$2:/usr/share/nginx/html:ro" nginx:alpine >/dev/null
  ;;
wait)
  X=$2
  for i in $(seq 1 120); do
    if curl -fsS -m 3 -H 'Accept: application/json' "http://127.0.0.1:$(port "$X")/identity" 2>/dev/null | grep -q machineIdentifier; then
      echo "$X up after ~$((i * 3))s"
      exit 0
    fi
    sleep 3
  done
  echo "$X did not come up" >&2
  exit 1
  ;;
down)
  docker rm -f pgrigA-plex pgrigB-plex pgrigC-plex pgrigA-pg pgrigB-pg pgrig-tuner >/dev/null 2>&1 || true
  ;;
*)
  sed -n '2,16p' "$0"
  exit 2
  ;;
esac
