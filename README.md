# yooooo-repo-sync

Shell helper for batch-syncing local Git repositories.

## Check list

<img width="800" alt="image" src="https://github.com/user-attachments/assets/d1889b12-8b75-4359-9608-c0e4aa9588d5" />

## Sync result

<img width="800" height="677" alt="image" src="https://github.com/user-attachments/assets/825539ef-b96f-453f-8ef4-ad65d22066f7" />

## Config is stored beside the script:

```text
repo-sync-data/repos.tsv
```

You can override it:

```bash
export REPO_SYNC_HOME=/path/to/repo-sync-data
```

## Install

```bash
chmod +x repo-sync.sh
```

Optional:

```bash
ln -s /absolute/path/to/repo-sync.sh /usr/local/bin/repo-sync
```

## Commands

```bash
./repo-sync.sh add ~/code/project-a
./repo-sync.sh add ~/code/project-b --strategy merge
./repo-sync.sh add ~/code/project-c --submodules

./repo-sync.sh list
./repo-sync.sh list --fetch
./repo-sync.sh list --no-fetch

./repo-sync.sh set project-a --strategy merge
./repo-sync.sh set project-a --path ~/new-code/project-a
./repo-sync.sh set project-a --name new-project-a
./repo-sync.sh set project-a --submodules

./repo-sync.sh remove project-a

./repo-sync.sh sync
./repo-sync.sh sync project-a project-b
./repo-sync.sh sync --strategy rebase
./repo-sync.sh sync --strategy merge
```

Dirty working trees are marked in `list` and in the final `sync` result:

```bash
./repo-sync.sh list
./repo-sync.sh sync
```

`sync` still attempts the update when local changes are present. If Git cannot update that repository cleanly, it appears in `Failed` with the local-dirty marker and the Git error.

`--allow-dirty` is still accepted for compatibility.

## Sync behavior

- Default strategy is `rebase`.
- `list` fetches repository metadata in parallel, then shows whether the current branch has updates from its configured upstream, or from `origin/<current-branch>` when no upstream is configured.
- `list` shows `dirty` when a repository has staged, unstaged, or untracked local changes.
- `list` fetches branch refs only; it does not fetch tags, so tag conflicts do not block branch update checks.
- In interactive terminals, `list` prints rows immediately, shows a smooth spinner while each repository fetches, then updates each row in place.
- Interactive `list` output uses color to distinguish update states and lower-emphasis metadata.
- `list --no-fetch` skips network fetches and checks local tracking refs only.
- `sync` first fetches branches and tags for every selected repository in parallel, forcing local tags to match remote tags when names collide.
- During the fetch pass, `sync` shows compact progress instead of full fetch output.
- After the fetch pass, `sync` immediately skips repositories with no remote updates, then serially updates only repositories that do have updates.
- The final `sync` output groups results into `Updated`, `Skipped`, and `Failed` sections with counts. Each item includes the local path; updated items include code diff stats, failed items include error reasons, and dirty repositories are marked with `local dirty`.
- `rebase` runs `git pull --no-tags --rebase --recurse-submodules=on-demand`.
- `merge` runs `git pull --no-tags --no-rebase --recurse-submodules=on-demand`.
- Repositories registered with `--submodules` also run:

```bash
git submodule sync --recursive
git submodule update --init --recursive
```

## License

GNU Affero General Public License v3.0.
