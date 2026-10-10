# QNAP maintained fork

Repository: https://github.com/johoja12/plex-postgresql

Maintained branch: `main`. The previous `qnap/mediactl-patched` branch is retained as an integration snapshot. Local independent checkout: `/opt/projects/plex-postgresql-fork`.

`origin` is our GitHub fork. `upstream` is https://github.com/mediactl/plex-postgresql. GitHub records the fork-network parent as cgnl/plex-postgresql; mediactl/main is the source used for current integration.

## Source baseline

The complete previously deployed integration is retained at `938e2116045171554f76040d11e9e8fe4e791eef`, merging mediactl/main through `c180e43` and retaining 37 local commits. All 37 are ancestors of the maintained branch; no patches were squashed or dropped. Existing worktrees and individual upstream PR branches are preserved.

Included fixes cover shadow migration batching and failure handling, sequence alignment, scanner preload and identity, large-result and column-access performance, UTF-8 and FTS compatibility, aggregate metadata, query/index improvements, smart TV collection paging, shadow reset lifecycle, and the cached integer-binding reentry deadlock (`4a8cd89`). Regression tests and benchmark documentation remain in their original commits.

The maintained source already includes `scripts/scanner-preload.sh` and its Docker integration. Do not replace the reviewed Plex base with Dockerfile's default latest upstream base when building for QNAP. Production builds combine this shim with the complete matching patched Plex tree from `/opt/projects/plex-patch`, preserving the matching plugin bundle, NVIDIA/audio/4K binary fixes and scanner preload. The operational release runbook is `/opt/projects/home-mon/docs/plex-setup.md`.

## Updating

```sh
cd /opt/projects/plex-postgresql-fork
git fetch upstream main
git merge upstream/main
# Review differences and run targeted shim regressions before publishing.
git push origin main
```

An upstream merge and a GitHub push do not deploy Plex. Production image building, restored-database rehearsal, scoped rollout and live acceptance remain separate steps. Never migrate the live PostgreSQL database again or change production configuration merely to synchronize this source repository.
