#!/bin/sh
# Publishes a tag's Linux release assets from this fork.
#
# A tag push does not start a workflow on mediactl/plex-postgresql (no push
# event has ever started one there, 2026-10-27), so the release workflow is
# dispatched by hand for the tag, which builds both architectures natively
# and creates the GitHub release with its bundles. Idempotent: a second run
# replaces the assets.
#
#   scripts/publish-release.sh v1.3.17-clusterplex.42
#
# The tag must already be pushed. With --watch the script follows the run.
set -eu
tag=${1:?usage: $0 <tag> [--watch]}
repo=${PLEX_PG_RELEASE_REPO:-mediactl/plex-postgresql}
git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null \
	|| { echo "tag ${tag} is not on origin; push it first" >&2; exit 1; }
gh workflow run "Release Linux Artifacts" --repo "$repo" -f "tag=${tag}"
echo "dispatched Release Linux Artifacts for ${tag} on ${repo}"
if [ "${2:-}" = "--watch" ]; then
	sleep 5
	id=$(gh run list --repo "$repo" --workflow "Release Linux Artifacts" --limit 1 --json databaseId --jq '.[0].databaseId')
	gh run watch --repo "$repo" --exit-status "$id"
	gh release view --repo "$repo" "$tag" --json assets --jq '.assets[] | "\(.name)\t\(.size)"'
fi
