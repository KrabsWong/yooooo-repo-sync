# yooooo-repo-sync

Shell helper for batch-syncing local Git repositories.

Config is stored beside the script:

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

By default, dirty working trees are skipped and printed:

```bash
./repo-sync.sh sync
```

If you really want to sync with local changes:

```bash
./repo-sync.sh sync --allow-dirty
```

## Sync behavior

- Default strategy is `rebase`.
- `rebase` runs `git pull --rebase --recurse-submodules=on-demand`.
- `merge` runs `git pull --no-rebase --recurse-submodules=on-demand`.
- Repositories registered with `--submodules` also run:

```bash
git submodule sync --recursive
git submodule update --init --recursive
```

## License

GNU Affero General Public License v3.0.
