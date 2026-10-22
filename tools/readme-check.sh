#!/usr/bin/env bash
# Runs the README's quick start ("Getting started (dev)") as a new user would, from a fresh checkout
# inside the dev shell (CI's `readme` job; locally `just readme`). The clone, `cd` and `nix develop`
# lines are skipped: the job is already in the checkout, in the shell. The last command, `hostd run`,
# needs an LLM server, and CI has none. So it must get as far as asking one (config, workspace and
# allowlist loaded, component compiled and instantiated, a plan requested) and fail only there.
set -euo pipefail

readme=${1:-README.md}
if [ -e hostd.toml ] && [ -z "${CI:-}" ]; then
  echo "refusing: the quick start copies hostd.toml.example over hostd.toml; move yours aside" >&2
  exit 2
fi

block=$(awk '/^From the repository root:/ { found = 1; next }
             found && /^```bash/ { inside = 1; next }
             inside && /^```/ { exit }
             inside { print }' "$readme")
if [ -z "$block" ]; then
  echo "no quick start block after 'From the repository root:' in $readme" >&2
  exit 1
fi

ran=0
while IFS= read -r line; do
  # The commands carry no '#' of their own, so everything after one is a comment.
  command=$(printf '%s' "${line%%#*}" | sed 's/[[:space:]]*$//')
  [ -n "$command" ] || continue
  case "$command" in
    "git clone "* | "cd "* | "nix develop"*)
      echo "skip: $command"
      continue
      ;;
  esac
  echo "+ $command"
  if [[ "$command" == "cargo run -p hostd -- run"* ]]; then
    set +e
    output=$(bash -c "$command" 2>&1)
    status=$?
    set -e
    printf '%s\n' "$output" | tail -n 15
    if [ "$status" -eq 0 ] || ! grep -q "every LLM provider failed" <<<"$output"; then
      echo "the quick start's run did not get as far as the LLM request (exit $status)" >&2
      exit 1
    fi
    echo "(it reached the LLM request, which has no server here: as expected)"
  else
    bash -c "$command"
  fi
  ran=$((ran + 1))
done <<<"$block"

if [ "$ran" -lt 4 ]; then
  echo "only $ran quick start commands ran; the block changed shape" >&2
  exit 1
fi
echo "the README quick start works: $ran commands"
