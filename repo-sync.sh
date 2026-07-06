#!/usr/bin/env bash
set -u

DEFAULT_STRATEGY="rebase"
LIST_NAME_WIDTH=24
LIST_STRATEGY_WIDTH=8
LIST_SUBMODULES_WIDTH=10
LIST_UPDATES_WIDTH=10
LIST_UPDATES_COLUMN=$((LIST_NAME_WIDTH + 2 + LIST_STRATEGY_WIDTH + 2 + LIST_SUBMODULES_WIDTH + 2 + 1))
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG_DIR="${REPO_SYNC_HOME:-"$SCRIPT_DIR/repo-sync-data"}"
CONFIG_FILE="$CONFIG_DIR/repos.tsv"

usage() {
  cat <<'EOF'
Usage:
  repo-sync.sh add <path> [--name <name>] [--strategy rebase|merge] [--submodules|--no-submodules]
  repo-sync.sh list [--fetch|--no-fetch]
  repo-sync.sh remove <name-or-path>
  repo-sync.sh set <name-or-path> [--name <name>] [--path <path>] [--strategy rebase|merge] [--submodules|--no-submodules]
  repo-sync.sh sync [name-or-path ...] [--strategy rebase|merge] [--allow-dirty]
  repo-sync.sh config

Config:
  repo-sync-data/repos.tsv beside this script, or $REPO_SYNC_HOME/repos.tsv if REPO_SYNC_HOME is set.
EOF
}

die() {
  echo "Error: $*" >&2
  exit 1
}

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  C_RESET=$'\033[0m'
  C_BOLD=$'\033[1m'
  C_DIM=$'\033[2m'
  C_BLUE=$'\033[34m'
  C_GREEN=$'\033[32m'
  C_YELLOW=$'\033[33m'
  C_RED=$'\033[31m'
  C_CYAN=$'\033[36m'
else
  C_RESET=""
  C_BOLD=""
  C_DIM=""
  C_BLUE=""
  C_GREEN=""
  C_YELLOW=""
  C_RED=""
  C_CYAN=""
fi

status() {
  local level="$1"
  shift

  local color="$C_RESET"
  case "$level" in
    INFO) color="$C_BLUE" ;;
    SKIP) color="$C_YELLOW" ;;
    FAIL) color="$C_RED" ;;
    OK) color="$C_GREEN" ;;
  esac

  printf '%b[%s]%b %s\n' "$color" "$level" "$C_RESET" "$*"
}

ensure_config() {
  mkdir -p "$CONFIG_DIR" || die "cannot create config dir: $CONFIG_DIR"
  touch "$CONFIG_FILE" || die "cannot write config file: $CONFIG_FILE"
}

abs_path() {
  local path="$1"
  if [[ -d "$path" ]]; then
    (cd "$path" && pwd -P)
    return
  fi

  local parent
  parent="$(dirname "$path")"
  local base
  base="$(basename "$path")"
  (cd "$parent" && printf '%s/%s\n' "$(pwd -P)" "$base")
}

validate_strategy() {
  case "$1" in
    rebase|merge) ;;
    *) die "strategy must be rebase or merge" ;;
  esac
}

is_git_repo() {
  git -C "$1" rev-parse --is-inside-work-tree >/dev/null 2>&1
}

repo_basename() {
  basename "$1"
}

bool_from_flag() {
  case "$1" in
    true|false) printf '%s\n' "$1" ;;
    *) die "internal bool error: $1" ;;
  esac
}

repo_line_by_target() {
  local target="$1"
  local target_abs=""
  if [[ -e "$target" ]]; then
    target_abs="$(abs_path "$target")"
  fi

  local line name path strategy submodules
  while IFS=$'\t' read -r name path strategy submodules; do
    [[ -z "${name:-}" ]] && continue
    if [[ "$name" == "$target" || "$path" == "$target" || "$path" == "$target_abs" ]]; then
      printf '%s\t%s\t%s\t%s\n' "$name" "$path" "$strategy" "$submodules"
      return 0
    fi
  done < "$CONFIG_FILE"

  return 1
}

target_exists() {
  repo_line_by_target "$1" >/dev/null
}

name_exists() {
  local wanted="$1"
  local skip="${2:-}"
  local name path strategy submodules
  while IFS=$'\t' read -r name path strategy submodules; do
    [[ -z "${name:-}" ]] && continue
    [[ "$name" == "$skip" ]] && continue
    [[ "$name" == "$wanted" ]] && return 0
  done < "$CONFIG_FILE"
  return 1
}

path_exists() {
  local wanted="$1"
  local skip="${2:-}"
  local name path strategy submodules
  while IFS=$'\t' read -r name path strategy submodules; do
    [[ -z "${name:-}" ]] && continue
    [[ "$name" == "$skip" ]] && continue
    [[ "$path" == "$wanted" ]] && return 0
  done < "$CONFIG_FILE"
  return 1
}

sort_config() {
  local tmp
  tmp="$(mktemp "$CONFIG_DIR/repos.XXXXXX")" || die "cannot create temp file"
  LC_ALL=C sort -t $'\t' -k1,1 "$CONFIG_FILE" > "$tmp"
  mv "$tmp" "$CONFIG_FILE"
}

cmd_add() {
  [[ $# -ge 1 ]] || die "add requires <path>"

  local path
  path="$(abs_path "$1")"
  shift

  local name
  name="$(repo_basename "$path")"
  local strategy="$DEFAULT_STRATEGY"
  local submodules="auto"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --name)
        [[ $# -ge 2 ]] || die "--name requires a value"
        name="$2"
        shift 2
        ;;
      --strategy)
        [[ $# -ge 2 ]] || die "--strategy requires a value"
        validate_strategy "$2"
        strategy="$2"
        shift 2
        ;;
      --submodules)
        submodules="true"
        shift
        ;;
      --no-submodules)
        submodules="false"
        shift
        ;;
      *)
        die "unknown add option: $1"
        ;;
    esac
  done

  is_git_repo "$path" || die "not a git repository: $path"
  ensure_config
  name_exists "$name" && die "repository name already exists: $name"
  path_exists "$path" && die "repository path already exists: $path"

  if [[ "$submodules" == "auto" ]]; then
    if [[ -f "$path/.gitmodules" ]]; then
      submodules="true"
    else
      submodules="false"
    fi
  fi

  printf '%s\t%s\t%s\t%s\n' "$name" "$path" "$strategy" "$submodules" >> "$CONFIG_FILE"
  sort_config
  echo "Added: $name -> $path ($strategy, submodules=$submodules)"
}

print_list_header() {
  printf "%b%-${LIST_NAME_WIDTH}s  %-${LIST_STRATEGY_WIDTH}s  %-${LIST_SUBMODULES_WIDTH}s  %-${LIST_UPDATES_WIDTH}s  %s%b\n" \
    "$C_BOLD" "name" "strategy" "submodules" "updates" "path" "$C_RESET"
  printf "%b%-${LIST_NAME_WIDTH}s  %-${LIST_STRATEGY_WIDTH}s  %-${LIST_SUBMODULES_WIDTH}s  %-${LIST_UPDATES_WIDTH}s  %s%b\n" \
    "$C_DIM" "----" "--------" "----------" "-------" "----" "$C_RESET"
}

print_colored_field() {
  local width="$1"
  local text="$2"
  local color="$3"
  local padded
  printf -v padded "%-${width}s" "$text"
  printf '%b%s%b' "$color" "$padded" "$C_RESET"
}

list_submodules_color() {
  case "$1" in
    true) printf '%s' "$C_CYAN" ;;
    false) printf '%s' "$C_DIM" ;;
    *) printf '%s' "$C_YELLOW" ;;
  esac
}

list_updates_color() {
  case "$1" in
    yes) printf '%s' "$C_GREEN" ;;
    no) printf '%s' "$C_DIM" ;;
    unknown) printf '%s' "$C_YELLOW" ;;
    pending) printf '%s' "$C_DIM" ;;
    fetching*) printf '%s' "$C_BLUE" ;;
    *) printf '%s' "$C_RESET" ;;
  esac
}

print_list_row_no_newline() {
  local name="$1"
  local strategy="$2"
  local submodules="$3"
  local updates="$4"
  local path="$5"
  print_colored_field "$LIST_NAME_WIDTH" "$name" "$C_BOLD"
  printf '  '
  print_colored_field "$LIST_STRATEGY_WIDTH" "$strategy" "$C_CYAN"
  printf '  '
  print_colored_field "$LIST_SUBMODULES_WIDTH" "$submodules" "$(list_submodules_color "$submodules")"
  printf '  '
  print_colored_field "$LIST_UPDATES_WIDTH" "$updates" "$(list_updates_color "$updates")"
  printf '  %b%s%b' "$C_DIM" "$path" "$C_RESET"
}

print_list_row() {
  print_list_row_no_newline "$@"
  printf '\n'
}

can_live_update_list() {
  [[ -t 1 && "${TERM:-}" != "dumb" ]]
}

update_list_updates_field() {
  local row_index="$1"
  local row_count="$2"
  local updates="$3"

  local lines_up=$((row_count - row_index + 1))
  printf '\033[%sA' "$lines_up"
  printf '\033[%sG' "$LIST_UPDATES_COLUMN"
  print_colored_field "$LIST_UPDATES_WIDTH" "$updates" "$(list_updates_color "$updates")"
  printf '\033[%sB' "$lines_up"
  printf '\r'
}

supports_unicode_spinner() {
  case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
    *UTF-8*|*utf8*|*UTF8*) return 0 ;;
    *) return 1 ;;
  esac
}

list_spinner_label() {
  local frame_index="$1"

  if supports_unicode_spinner; then
    local frames=("⠋" "⠙" "⠹" "⠸" "⠼" "⠴" "⠦" "⠧" "⠇" "⠏")
    printf 'fetching %s' "${frames[$((frame_index % ${#frames[@]}))]}"
  else
    local frames=("-" "\\" "|" "/")
    printf 'fetching %s' "${frames[$((frame_index % ${#frames[@]}))]}"
  fi
}

list_update_worker() {
  local path="$1"
  local refresh="$2"
  local result_file="$3"

  if ! repo_update_label "$path" "$refresh" > "$result_file"; then
    printf 'unknown\n' > "$result_file"
  fi
  if [[ ! -s "$result_file" ]]; then
    printf 'unknown\n' > "$result_file"
  fi
}

list_result_label() {
  local result_file="$1"
  local updates
  updates="$(sed -n '1p' "$result_file" 2>/dev/null)"
  case "$updates" in
    yes|no|unknown) printf '%s\n' "$updates" ;;
    *) printf 'unknown\n' ;;
  esac
}

cmd_list_live_fetch() {
  local names=()
  local paths=()
  local strategies=()
  local submodules_values=()
  local result_files=()
  local pids=()
  local done_flags=()

  local name path strategy submodules
  while IFS=$'\t' read -r name path strategy submodules; do
    [[ -z "${name:-}" ]] && continue
    names+=("$name")
    paths+=("$path")
    strategies+=("$strategy")
    submodules_values+=("$submodules")
  done < "$CONFIG_FILE"

  local tmp_dir
  tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/repo-sync-list.XXXXXX")" || die "cannot create temp dir"

  echo "Config: $CONFIG_FILE"
  print_list_header

  local row_count="${#names[@]}"
  local index
  for ((index = 0; index < row_count; index++)); do
    print_list_row "${names[$index]}" "${strategies[$index]}" "${submodules_values[$index]}" "pending" "${paths[$index]}"
    result_files+=("$tmp_dir/$index.status")
    done_flags+=("false")
    list_update_worker "${paths[$index]}" "true" "${result_files[$index]}" &
    pids+=("$!")
  done

  local remaining="$row_count"
  local frame_index=0
  local updates
  while [[ "$remaining" -gt 0 ]]; do
    for ((index = 0; index < row_count; index++)); do
      [[ "${done_flags[$index]}" == "true" ]] && continue

      if [[ -s "${result_files[$index]}" ]]; then
        updates="$(list_result_label "${result_files[$index]}")"
        update_list_updates_field "$((index + 1))" "$row_count" "$updates"
        done_flags[$index]="true"
        remaining=$((remaining - 1))
      else
        update_list_updates_field "$((index + 1))" "$row_count" "$(list_spinner_label "$frame_index")"
      fi
    done

    if [[ "$remaining" -gt 0 ]]; then
      sleep 0.08
      frame_index=$((frame_index + 1))
    fi
  done

  for ((index = 0; index < row_count; index++)); do
    wait "${pids[$index]}" >/dev/null 2>&1 || true
  done
  rm -rf "$tmp_dir"
}

cmd_list_parallel_plain() {
  local refresh="$1"
  local names=()
  local paths=()
  local strategies=()
  local submodules_values=()
  local result_files=()
  local pids=()

  local name path strategy submodules
  while IFS=$'\t' read -r name path strategy submodules; do
    [[ -z "${name:-}" ]] && continue
    names+=("$name")
    paths+=("$path")
    strategies+=("$strategy")
    submodules_values+=("$submodules")
  done < "$CONFIG_FILE"

  local tmp_dir
  tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/repo-sync-list.XXXXXX")" || die "cannot create temp dir"

  local row_count="${#names[@]}"
  local index
  for ((index = 0; index < row_count; index++)); do
    result_files+=("$tmp_dir/$index.status")
    list_update_worker "${paths[$index]}" "$refresh" "${result_files[$index]}" &
    pids+=("$!")
  done

  for ((index = 0; index < row_count; index++)); do
    wait "${pids[$index]}" >/dev/null 2>&1 || true
  done

  echo "Config: $CONFIG_FILE"
  print_list_header

  local updates
  for ((index = 0; index < row_count; index++)); do
    updates="$(list_result_label "${result_files[$index]}")"
    print_list_row "${names[$index]}" "${strategies[$index]}" "${submodules_values[$index]}" "$updates" "${paths[$index]}"
  done

  rm -rf "$tmp_dir"
}

cmd_list() {
  ensure_config

  local refresh="true"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --fetch)
        refresh="true"
        shift
        ;;
      --no-fetch)
        refresh="false"
        shift
        ;;
      *)
        die "unknown list option: $1"
        ;;
    esac
  done

  if [[ ! -s "$CONFIG_FILE" ]]; then
    echo "No repositories registered. Config: $CONFIG_FILE"
    return
  fi

  if [[ "$refresh" == "true" ]] && can_live_update_list; then
    cmd_list_live_fetch
    return
  fi

  cmd_list_parallel_plain "$refresh"
}

cmd_remove() {
  [[ $# -eq 1 ]] || die "remove requires <name-or-path>"
  ensure_config

  local target="$1"
  local line
  line="$(repo_line_by_target "$target")" || die "repository not registered: $target"

  local old_name old_path old_strategy old_submodules
  IFS=$'\t' read -r old_name old_path old_strategy old_submodules <<< "$line"

  local tmp
  tmp="$(mktemp "$CONFIG_DIR/repos.XXXXXX")" || die "cannot create temp file"

  local name path strategy submodules
  while IFS=$'\t' read -r name path strategy submodules; do
    [[ -z "${name:-}" ]] && continue
    if [[ "$name" != "$old_name" ]]; then
      printf '%s\t%s\t%s\t%s\n' "$name" "$path" "$strategy" "$submodules" >> "$tmp"
    fi
  done < "$CONFIG_FILE"

  mv "$tmp" "$CONFIG_FILE"
  echo "Removed: $old_name -> $old_path"
}

cmd_set() {
  [[ $# -ge 1 ]] || die "set requires <name-or-path>"
  ensure_config

  local target="$1"
  shift

  local line
  line="$(repo_line_by_target "$target")" || die "repository not registered: $target"

  local old_name old_path old_strategy old_submodules
  IFS=$'\t' read -r old_name old_path old_strategy old_submodules <<< "$line"

  local new_name="$old_name"
  local new_path="$old_path"
  local new_strategy="$old_strategy"
  local new_submodules="$old_submodules"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --name)
        [[ $# -ge 2 ]] || die "--name requires a value"
        new_name="$2"
        shift 2
        ;;
      --path)
        [[ $# -ge 2 ]] || die "--path requires a value"
        new_path="$(abs_path "$2")"
        shift 2
        ;;
      --strategy)
        [[ $# -ge 2 ]] || die "--strategy requires a value"
        validate_strategy "$2"
        new_strategy="$2"
        shift 2
        ;;
      --submodules)
        new_submodules="true"
        shift
        ;;
      --no-submodules)
        new_submodules="false"
        shift
        ;;
      *)
        die "unknown set option: $1"
        ;;
    esac
  done

  is_git_repo "$new_path" || die "not a git repository: $new_path"
  name_exists "$new_name" "$old_name" && die "repository name already exists: $new_name"
  path_exists "$new_path" "$old_name" && die "repository path already exists: $new_path"

  local tmp
  tmp="$(mktemp "$CONFIG_DIR/repos.XXXXXX")" || die "cannot create temp file"

  local name path strategy submodules
  while IFS=$'\t' read -r name path strategy submodules; do
    [[ -z "${name:-}" ]] && continue
    if [[ "$name" == "$old_name" ]]; then
      printf '%s\t%s\t%s\t%s\n' "$new_name" "$new_path" "$new_strategy" "$new_submodules" >> "$tmp"
    else
      printf '%s\t%s\t%s\t%s\n' "$name" "$path" "$strategy" "$submodules" >> "$tmp"
    fi
  done < "$CONFIG_FILE"

  mv "$tmp" "$CONFIG_FILE"
  sort_config
  echo "Updated: $new_name -> $new_path ($new_strategy, submodules=$new_submodules)"
}

git_logged() {
  local repo_path="$1"
  shift
  printf '%b$%b git -C %s %s\n' "$C_DIM" "$C_RESET" "$repo_path" "$*"
  git -C "$repo_path" "$@"
}

git_fetch_quiet() {
  git -C "$1" fetch --quiet --prune >/dev/null 2>&1
}

repo_upstream() {
  git -C "$1" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null
}

repo_current_branch() {
  git -C "$1" branch --show-current 2>/dev/null
}

repo_branch_compare_ref() {
  local repo_path="$1"
  local branch="$2"
  local upstream
  upstream="$(repo_upstream "$repo_path")"
  if [[ -n "$upstream" ]]; then
    printf '%s\n' "$upstream"
    return
  fi

  if git -C "$repo_path" show-ref --verify --quiet "refs/remotes/origin/$branch"; then
    printf 'origin/%s\n' "$branch"
    return
  fi

  return 1
}

repo_remote_update_count() {
  local repo_path="$1"
  local compare_ref="$2"
  local count
  count="$(git -C "$repo_path" rev-list --count "HEAD..$compare_ref" 2>/dev/null)" || return 1
  [[ "$count" =~ ^[0-9]+$ ]] || return 1
  printf '%s\n' "$count"
}

repo_update_label() {
  local path="$1"
  local refresh="${2:-false}"
  if [[ ! -d "$path" ]] || ! is_git_repo "$path"; then
    printf 'unknown\n'
    return
  fi

  local branch
  branch="$(repo_current_branch "$path")"
  if [[ -z "$branch" ]]; then
    printf 'unknown\n'
    return
  fi

  if [[ "$refresh" == "true" ]] && ! git_fetch_quiet "$path"; then
    printf 'unknown\n'
    return
  fi

  local compare_ref
  compare_ref="$(repo_branch_compare_ref "$path" "$branch")" || {
    printf 'unknown\n'
    return
  }

  local update_count
  update_count="$(repo_remote_update_count "$path" "$compare_ref")" || {
    printf 'unknown\n'
    return
  }
  if [[ "$update_count" -gt 0 ]]; then
    printf 'yes\n'
  else
    printf 'no\n'
  fi
}

sync_one() {
  local name="$1"
  local path="$2"
  local strategy="$3"
  local submodules="$4"
  local allow_dirty="$5"

  printf '\n%b==>%b %b%s%b %b(%s)%b\n' "$C_CYAN" "$C_RESET" "$C_BOLD" "$name" "$C_RESET" "$C_DIM" "$path" "$C_RESET"

  if [[ ! -d "$path" ]]; then
    status SKIP "Path does not exist: $path"
    return 10
  fi
  if ! is_git_repo "$path"; then
    status SKIP "Not a git repository: $path"
    return 10
  fi

  local branch
  branch="$(git -C "$path" branch --show-current 2>/dev/null)"
  if [[ -z "$branch" ]]; then
    status SKIP "Detached HEAD or no current branch."
    return 10
  fi

  local upstream
  upstream="$(git -C "$path" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null)"
  if [[ -z "$upstream" ]]; then
    status SKIP "Branch has no upstream: $branch"
    return 10
  fi

  status INFO "branch=$branch, upstream=$upstream, strategy=$strategy"

  git_logged "$path" fetch --prune --tags || {
    status FAIL "fetch failed for $name"
    return 20
  }

  local update_count
  update_count="$(repo_remote_update_count "$path" "$upstream")" || {
    status FAIL "Cannot compare $branch with $upstream."
    return 20
  }
  if [[ "$update_count" -eq 0 ]]; then
    status SKIP "$name: no updates; already up to date with $upstream."
    return 10
  fi

  status INFO "remote updates=$update_count"

  local dirty
  dirty="$(git -C "$path" status --porcelain=v1 --untracked-files=normal 2>&1)"
  if [[ $? -ne 0 ]]; then
    status FAIL "Cannot read git status:"
    echo "$dirty"
    return 20
  fi

  if [[ -n "$dirty" && "$allow_dirty" != "true" ]]; then
    status SKIP "$name: working tree has local changes."
    echo "$dirty" | sed 's/^/       /' | head -n 20
    local dirty_count
    dirty_count="$(echo "$dirty" | wc -l | tr -d ' ')"
    if [[ "$dirty_count" -gt 20 ]]; then
      echo "       ... $((dirty_count - 20)) more lines"
    fi
    return 10
  fi

  case "$strategy" in
    rebase)
      git_logged "$path" pull --rebase --recurse-submodules=on-demand || {
        status FAIL "pull failed for $name"
        return 20
      }
      ;;
    merge)
      git_logged "$path" pull --no-rebase --recurse-submodules=on-demand || {
        status FAIL "pull failed for $name"
        return 20
      }
      ;;
    *)
      status FAIL "Unknown strategy: $strategy"
      return 20
      ;;
  esac

  if [[ "$submodules" == "true" ]]; then
    git_logged "$path" submodule sync --recursive || {
      status FAIL "submodule sync failed for $name"
      return 20
    }
    git_logged "$path" submodule update --init --recursive || {
      status FAIL "submodule update failed for $name"
      return 20
    }
  fi

  status OK "$name"
  return 0
}

cmd_sync() {
  ensure_config

  local override_strategy=""
  local allow_dirty="false"
  local targets=()

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --strategy)
        [[ $# -ge 2 ]] || die "--strategy requires a value"
        validate_strategy "$2"
        override_strategy="$2"
        shift 2
        ;;
      --allow-dirty)
        allow_dirty="true"
        shift
        ;;
      *)
        targets+=("$1")
        shift
        ;;
    esac
  done

  if [[ ! -s "$CONFIG_FILE" ]]; then
    echo "No repositories to sync."
    return
  fi

  local ok=0
  local skipped=0
  local failed=0

  if [[ ${#targets[@]} -gt 0 ]]; then
    local target line
    for target in "${targets[@]}"; do
      line="$(repo_line_by_target "$target")" || die "repository not registered: $target"
      local name path strategy submodules
      IFS=$'\t' read -r name path strategy submodules <<< "$line"
      [[ -n "$override_strategy" ]] && strategy="$override_strategy"
      sync_one "$name" "$path" "$strategy" "$submodules" "$allow_dirty"
      case $? in
        0) ok=$((ok + 1)) ;;
        10) skipped=$((skipped + 1)) ;;
        *) failed=$((failed + 1)) ;;
      esac
    done
  else
    local name path strategy submodules
    while IFS=$'\t' read -r name path strategy submodules; do
      [[ -z "${name:-}" ]] && continue
      [[ -n "$override_strategy" ]] && strategy="$override_strategy"
      sync_one "$name" "$path" "$strategy" "$submodules" "$allow_dirty"
      case $? in
        0) ok=$((ok + 1)) ;;
        10) skipped=$((skipped + 1)) ;;
        *) failed=$((failed + 1)) ;;
      esac
    done < "$CONFIG_FILE"
  fi

  echo ""
  printf 'Summary: %bok=%s%b, %bskipped=%s%b, %bfailed=%s%b\n' \
    "$C_GREEN" "$ok" "$C_RESET" \
    "$C_YELLOW" "$skipped" "$C_RESET" \
    "$C_RED" "$failed" "$C_RESET"
  [[ "$failed" -eq 0 ]]
}

main() {
  [[ $# -ge 1 ]] || {
    usage
    exit 1
  }

  local command="$1"
  shift

  case "$command" in
    add) cmd_add "$@" ;;
    list) cmd_list "$@" ;;
    remove) cmd_remove "$@" ;;
    set) cmd_set "$@" ;;
    sync) cmd_sync "$@" ;;
    config)
      ensure_config
      echo "$CONFIG_FILE"
      ;;
    -h|--help|help)
      usage
      ;;
    *)
      usage
      exit 1
      ;;
  esac
}

main "$@"
