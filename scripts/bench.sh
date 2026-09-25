#!/usr/bin/env bash
# Benchmark pando's read-only and dry-run operations against a generated
# sample repository.
#
#   scripts/bench.sh <sample-dir> <pando-binary> [<pando-binary>...]
#
# <sample-dir> is a directory produced by scripts/bench-repo.py. With more than
# one binary, hyperfine compares them command by command. Results are written
# as markdown to $BENCH_OUT (default: ./bench-results.md). Requires hyperfine.
set -euo pipefail

sample=$(cd "$1" && pwd)
shift
repo=$sample/repo
dirty=$sample/worktrees/local-00001
clean=$sample/worktrees/local-00079
out=${BENCH_OUT:-$PWD/bench-results.md}
runs=${BENCH_RUNS:-15}

# Isolate from the invoking user's configuration and trust store.
home=$(mktemp -d)
trap 'rm -rf "$home"' EXIT
export HOME=$home XDG_CONFIG_HOME=$home/.config

binaries=()
for binary in "$@"; do
  binaries+=("$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")")
done

# name|directory|arguments
cases=(
  "list|$repo|list"
  "list json|$repo|--output json list"
  "list branches|$repo|list --branches"
  "get branch|$clean|get branch"
  "switch existing|$repo|switch local/00079"
  "switch remote-only (dry run)|$repo|switch topic/01400 --dry-run"
  "create new (dry run)|$repo|create bench/new --dry-run"
  "remove (dry run)|$repo|remove local/00079 --dry-run"
  "commit (dry run)|$dirty|commit --stage-all -m bench --dry-run"
  "merge (dry run)|$clean|merge --dry-run"
)

: >"$out"
for case in "${cases[@]}"; do
  IFS='|' read -r name dir args <<<"$case"
  commands=()
  names=()
  for binary in "${binaries[@]}"; do
    commands+=("cd '$dir' && '$binary' $args </dev/null >/dev/null 2>&1 || true")
    names+=(-n "$(basename "$binary")")
  done
  echo "## $name" | tee -a "$out"
  hyperfine --shell=bash --warmup 2 --runs "$runs" "${names[@]}" \
    --export-markdown "$sample/.case.md" "${commands[@]}" >/dev/null
  cat "$sample/.case.md" | tee -a "$out"
  echo | tee -a "$out"
done
rm -f "$sample/.case.md"
