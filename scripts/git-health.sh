#!/usr/bin/env bash
# git-health.sh — read-only health/diagnostics sweep for this repository.
#
# Reports working-tree state, branch hygiene, remote sync, worktrees, object
# integrity, and open PRs, then prints suggested cleanup commands for anything
# it flags. It makes NO changes to your local branches, working tree, or
# history — the only side effect is a `git fetch` to compare against the remote
# (skip it with --no-fetch).
#
#   scripts/git-health.sh              # full sweep (fetches remote state first)
#   scripts/git-health.sh --no-fetch   # offline; compare against last-known refs
#
# Exit status: 0 when healthy; 1 when a *real* problem is found — object-database
# corruption or a stuck in-progress operation. Cosmetic findings (stale refs,
# behind the default branch, uncommitted changes) are reported with a suggested
# fix but do not fail the run, so this is safe to wire into CI as a soft gate.

set -u

# --- args -----------------------------------------------------------------
fetch=1
for arg in "$@"; do
  case "$arg" in
    --no-fetch | --offline) fetch=0 ;;
    -h | --help)
      cat <<'EOF'
git-health.sh — read-only git health/diagnostics sweep.

Usage: scripts/git-health.sh [--no-fetch]
  --no-fetch, --offline   skip the network fetch; compare against last-known refs

Reports working tree, branches, remote sync, worktrees, integrity, and open PRs,
and prints suggested cleanups. Changes nothing local. Exit 0 = healthy,
1 = real problem (corruption or a stuck operation).
EOF
      exit 0
      ;;
    *)
      echo "git-health: unknown argument '$arg' (try --help)" >&2
      exit 2
      ;;
  esac
done

git rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
  echo "git-health: not inside a git repository" >&2
  exit 2
}
cd "$(git rev-parse --show-toplevel)" || exit 2

issues=0
hdr() { printf '\n== %s ==\n' "$*"; }
note() { printf '  %s\n' "$*"; }
problem() {
  printf '  [!] %s\n' "$*"
  issues=$((issues + 1))
}
suggest() { printf '      fix: %s\n' "$*"; }

# Default branch = whatever origin/HEAD points at, else "main".
default_branch=$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null | sed 's#^origin/##')
[ -z "$default_branch" ] && default_branch=main

# --- 1. working tree ------------------------------------------------------
hdr "Working tree"
note "branch: $(git branch --show-current 2>/dev/null || echo '(detached HEAD)')   HEAD: $(git rev-parse --short HEAD)"
dirty=$(git status --porcelain | wc -l | tr -d ' ')
if [ "$dirty" -gt 0 ]; then
  note "$dirty uncommitted path(s) — normal mid-work; commit or stash before switching branches"
else
  note "clean (no uncommitted changes)"
fi
gitdir=$(git rev-parse --git-dir)
for m in MERGE_HEAD rebase-merge rebase-apply CHERRY_PICK_HEAD REVERT_HEAD BISECT_LOG; do
  [ -e "$gitdir/$m" ] && problem "stuck in-progress operation: $m (finish it or run the matching --abort)"
done
stashes=$(git stash list 2>/dev/null | wc -l | tr -d ' ')
if [ "$stashes" -gt 0 ]; then
  note "$stashes stash(es) parked — 'git stash list' to review"
else
  note "no stashes"
fi

# --- 2. remote sync -------------------------------------------------------
hdr "Remote sync"
if [ "$fetch" = 1 ] && git remote get-url origin >/dev/null 2>&1; then
  if git fetch --quiet origin 2>/dev/null; then
    note "fetched origin"
  else
    note "fetch failed (offline?) — comparisons below may be stale"
  fi
elif [ "$fetch" = 0 ]; then
  note "--no-fetch: comparing against last-known remote-tracking refs"
fi
if git rev-parse --verify --quiet "origin/$default_branch" >/dev/null; then
  counts=$(git rev-list --left-right --count "$default_branch...origin/$default_branch" 2>/dev/null)
  ahead=$(printf '%s' "$counts" | awk '{print $1}')
  behind=$(printf '%s' "$counts" | awk '{print $2}')
  ahead=${ahead:-0}
  behind=${behind:-0}
  if [ "$behind" -gt 0 ]; then
    note "local $default_branch is behind origin by $behind"
    suggest "git checkout $default_branch && git pull --ff-only"
  fi
  [ "$ahead" -gt 0 ] && note "local $default_branch is AHEAD of origin by $ahead (unpushed)"
  [ "$ahead" = 0 ] && [ "$behind" = 0 ] && note "$default_branch in sync with origin"
else
  note "no origin/$default_branch to compare against"
fi
unpushed=$(git log --branches --not --remotes --oneline 2>/dev/null | wc -l | tr -d ' ')
if [ "$unpushed" -gt 0 ]; then
  note "$unpushed commit(s) on local branches not on any remote — 'git log --branches --not --remotes'"
else
  note "no unpushed commits"
fi
stale=$(git remote prune origin --dry-run 2>/dev/null | grep -c 'would prune')
if [ "$stale" -gt 0 ]; then
  note "$stale stale remote-tracking ref(s) (their upstream was deleted)"
  suggest "git remote prune origin   # or: git fetch --prune"
fi

# --- 3. branches ----------------------------------------------------------
hdr "Branches"
gone=$(git branch -vv | grep ': gone]' | awk '{print $1}' | sed 's/^\*//')
if [ -n "$gone" ]; then
  note "local branch(es) whose upstream is gone (usually merged-and-deleted; safe to remove):"
  printf '%s\n' "$gone" | sed 's/^/      /'
  suggest "git branch -D <branch>"
fi
unmerged=$(git branch --no-merged "origin/$default_branch" 2>/dev/null | sed 's/^[* ] //' | grep -v "^$default_branch\$")
if [ -n "$unmerged" ]; then
  note "branch(es) not merged into origin/$default_branch (unshipped work — confirm each is intentional):"
  printf '%s\n' "$unmerged" | sed 's/^/      /'
else
  note "no unmerged local branches"
fi

# --- 4. worktrees ---------------------------------------------------------
hdr "Worktrees"
git worktree list | sed 's/^/  /'
locked=$(git worktree list --porcelain 2>/dev/null | grep -c '^locked')
if [ "$locked" -gt 0 ]; then
  note "$locked locked worktree(s) — protected from prune; another process/agent may be using one"
  suggest "git worktree remove --force <path>   # only if you're certain it's abandoned"
fi
prunable=$(git worktree list --porcelain 2>/dev/null | grep -c '^prunable')
if [ "$prunable" -gt 0 ]; then
  note "$prunable prunable worktree(s) (their directory is gone)"
  suggest "git worktree prune"
fi

# --- 5. integrity & size --------------------------------------------------
hdr "Integrity & size"
corrupt=$(git fsck --full 2>&1 | grep -iE 'missing|corrupt|broken|^error' | head)
if [ -n "$corrupt" ]; then
  problem "object-database issues detected:"
  printf '%s\n' "$corrupt" | sed 's/^/      /'
else
  note "fsck clean (no corruption; 'dangling' objects are normal and not flagged)"
fi
note ".git size: $(du -sh "$gitdir" 2>/dev/null | cut -f1)"

# --- 6. open PRs (optional; needs gh) -------------------------------------
if command -v gh >/dev/null 2>&1; then
  hdr "Open PRs (gh)"
  gh pr list --state open --limit 30 \
    --json number,title,headRefName,isDraft \
    --template '{{range .}}  #{{.number}} {{.title}} ({{.headRefName}}){{if .isDraft}} DRAFT{{end}}{{"\n"}}{{end}}' 2>/dev/null ||
    note "gh present but not authenticated / no repo access"
fi

# --- summary --------------------------------------------------------------
hdr "Summary"
if [ "$issues" -eq 0 ]; then
  note "healthy — no real problems (act on any suggested cleanups above at your leisure)"
  exit 0
fi
note "$issues real problem(s) found — see the [!] lines above"
exit 1
