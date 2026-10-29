#!/bin/sh
# Publishes a tag's Linux release assets from this fork.
#
# A tag push runs the release workflow itself (since the fork's first manual
# dispatch on 2026-10-27; before it no push had ever started a workflow
# there). This dispatches the same workflow by hand for a tag whose push ran
# nothing, or to rebuild a release's assets: both architectures build
# natively and the GitHub release gets its bundles. Idempotent: a second run
# replaces the assets.
#
#   scripts/publish-release.sh v1.3.17-clusterplex.42
#
# The tag must already be pushed. With --watch the script follows the run.
set -eu
tag=${1:?usage: $0 <tag> [--watch]}
repo=${PLEX_PG_RELEASE_REPO:-johoja12/plex-postgresql}
git ls-remote --exit-code --tags "https://github.com/${repo}.git" "refs/tags/${tag}" >/dev/null \
	|| { echo "tag ${tag} is not on ${repo}; push it first" >&2; exit 1; }
gh workflow run "Release Linux Artifacts" --repo "$repo" -f "tag=${tag}"
echo "dispatched Release Linux Artifacts for ${tag} on ${repo}"
if [ "${2:-}" = "--watch" ]; then
	sleep 5
	id=$(gh run list --repo "$repo" --workflow "Release Linux Artifacts" --limit 1 --json databaseId --jq '.[0].databaseId')
	gh run watch --repo "$repo" --exit-status "$id"
	gh release view --repo "$repo" "$tag" --json assets --jq '.assets[] | "\(.name)\t\(.size)"'
fi
