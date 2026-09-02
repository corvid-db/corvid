#!/usr/bin/env bash
# bump.sh — one-command engine-pin fan-out to every registered binding repo.
#
# When corvid-db/corvid cuts a new release tag, every binding repo still pins
# the old one; updating them by hand is N-repo work. This tool (it orchestrates
# the bindings but lives in the engine repo, next to the coordinator) makes it
# one command: for each repo registered in registry.tsv it clones the base
# branch, verifies the repo's engine pins are internally consistent (refuses
# the repo otherwise), rewrites the old tag -> new tag across the registered
# pin globs, pushes branch bump/<NEW_TAG>, and opens a PR titled
# "Bump engine pin to <NEW_TAG>".
#
# The per-binding golden CI on that PR IS the compatibility verdict — that is
# the design. --merge-when-green polls each PR's checks and squash-merges on
# green; failures are reported with the failing job URL.
#
# The release cascade: after the engine tag exists and every bump PR has
# merged, --release-after-merge tags each binding repo at the SAME vX.Y.Z
# and pushes the tags — which fires the per-repo release workflows that
# publish to the registries (npm / PyPI / pub.dev / crates.io-on-the-engine)
# and pins the tag in the registry-less repos (c/go/cpp/zig/jvm; php's tag
# itself is the Packagist release). One engine tag -> every registry.
#
# Modes:
#   bump.sh --check                    read-only drift audit: current pin per
#                                      registered repo vs the engine's latest tag
#   bump.sh --dry-run NEW_TAG          clone + substitute, show the diff it
#                                      would make; pushes nothing
#   bump.sh NEW_TAG                    the bump: branch, commit, push, PRs,
#                                      table of PR URLs (tag must exist on the
#                                      engine remote)
#   bump.sh --merge-when-green [TAG]   poll open bump PRs' checks until the
#                                      golden CI completes; squash-merge on green
#   bump.sh --release-after-merge NEW_TAG
#                                      the cascade: after every bump PR merged,
#                                      verify each base branch pins NEW_TAG (and
#                                      the package version matches), tag vX.Y.Z,
#                                      push, and print the fired workflow run
#                                      URLs (or tag permalinks where no release
#                                      workflow exists). NEVER runs implicitly —
#                                      this exact flag is the only trigger.
#   bump.sh --dry-run --release-after-merge NEW_TAG
#                                      the cascade's dry form: every verification
#                                      (open PRs, pins, package versions) runs and
#                                      the table shows exactly which repos WOULD
#                                      be tagged — but nothing is tagged/pushed
#
# Options:
#   --repo ORG/NAME   operate on a subset of the registry (repeatable)
#   --timeout MIN    --merge-when-green polling deadline (default 60)
#   --interval SEC   --merge-when-green poll interval (default 30)
#
# Requirements: bash, git, gh (authenticated), ssh push access to the bindings.
# Exit status is non-zero when any repo fails its mode.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENGINE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
REGISTRY="$SCRIPT_DIR/registry.tsv"

readonly TAG_RE='^v[0-9]+\.[0-9]+\.[0-9]+$'
readonly TAG_TOKEN_RE='v[0-9]+\.[0-9]+\.[0-9]+'
# A tag in one of these contexts VOTES on what a repo's engine pin is (exact
# pin assignments). Bare prose mentions do not vote — they are substituted.
readonly PIN_CONTEXT_RE='(tag[[:space:]]*=[[:space:]]*"?|\?tag=|version[[:space:]]*[=:][[:space:]]*"?|pin[[:space:]]*[=:][[:space:]]*"?)'

MODE_CHECK=0
MODE_DRY=0
MODE_MERGE=0
MODE_RELEASE=0
NEW_TAG=""
REPO_FILTER=()
POLL_TIMEOUT_MIN=60
POLL_INTERVAL=30

die() { echo "bump.sh: error: $*" >&2; exit 1; }
log()  { echo "bump.sh: $*" >&2; }

usage() {
    awk 'NR==1 {next} /^set -e/ {exit} {sub(/^#( |$)/, ""); print}' "${BASH_SOURCE[0]}"
}

# ---------------------------------------------------------------------------
# helpers

# ver_key v0.2.10 -> zero-padded key (06d06d06d) for numeric comparison
ver_key() {
    printf '%s' "$1" | awk -F. '{ v=$1; sub(/^v/, "", v); printf "%06d%06d%06d", v, $2, $3 }'
}

engine_latest_tag() {
    git -C "$ENGINE_DIR" ls-remote --tags --refs origin 'v*' 2>/dev/null \
        | sed 's#.*refs/tags/##' \
        | awk -F. '{ v=$1; sub(/^v/, "", v); printf "%06d%06d%06d %s\n", v, $2, $3, $0 }' \
        | sort -n | tail -1 | cut -d' ' -f2-
}

# expand_globs REPO_DIR GLOBS -> relative file paths on stdout (deduped).
# Globs that match nothing are logged — an optional pin file (e.g. .engine-pin)
# may legitimately be absent; a typo'd glob should be visible either way.
expand_globs() {
    local repo_dir="$1" globs="$2" g m found
    local -a glob_list
    read -r -a glob_list <<< "$globs"
    shopt -s nullglob
    for g in "${glob_list[@]}"; do
        found=0
        matches=("$repo_dir/$g")
        for m in "${matches[@]}"; do
            [ -f "$m" ] || continue
            printf '%s\n' "${m#"$repo_dir"/}"
            found=1
        done
        [ "$found" -eq 1 ] || log "  note: glob '$g' matched no files (absent or moved?)"
    done
}

# collect_votes REPO_DIR FILES... -> "relative/file<TAB>tag" lines, one per
# (file, voted tag): every pin-shaped occurrence of a tag in a matched file,
# plus the whole trimmed content of a file named .engine-pin.
collect_votes() {
    local repo_dir="$1"; shift
    local f tag
    for f in "$@"; do
        if [ "$(basename "$f")" = ".engine-pin" ]; then
            tag="$(tr -d '[:space:]' < "$repo_dir/$f" | grep -oxE "$TAG_TOKEN_RE" || true)"
            if [ -n "$tag" ]; then printf '%s\t%s\n' "$f" "$tag"; fi
        else
            { grep -ioE "${PIN_CONTEXT_RE}${TAG_TOKEN_RE}" "$repo_dir/$f" 2>/dev/null || true; } \
                | { grep -oE "$TAG_TOKEN_RE" || true; } \
                | awk -v f="$f" '!seen[$0]++ { print f "\t" $0 }'
        fi
    done
}

# other_tags FILE PIN -> distinct tags in FILE other than PIN (informational:
# historical prose references must NOT be substituted)
other_tags() {
    grep -oE "$TAG_TOKEN_RE" "$1" 2>/dev/null | sort -u | grep -vxF -- "$2" || true
}

# substitute REPO_DIR OLD NEW FILES... -> rewrites every textual occurrence of
# OLD with NEW; echoes "file<TAB>count" per file with count>0.
substitute() {
    local repo_dir="$1" old="$2" new="$3"; shift 3
    local f c
    for f in "$@"; do
        c="$({ grep -oF -- "$old" "$repo_dir/$f" || true; } | wc -l | tr -d ' ')"
        [ "$c" -eq 0 ] && continue
        perl -pi -e "s/\Q$old\E/$new/g" "$repo_dir/$f"
        printf '%s\t%s\n' "$f" "$c"
    done
}

# read_package_version FILE -> the binding's OWN package version on stdout
# (bare X.Y.Z, no v), format-detected from the file: package.json /
# composer.json ("version": "x"), pubspec.yaml (version: x), anything TOMLish
# (version = "x"). Empty output = not found.
read_package_version() {
    local f="$1" base
    base="$(basename "$f")"
    case "$base" in
        package.json|composer.json)
            sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$f" | head -1 ;;
        pubspec.yaml)
            sed -n 's/^version:[[:space:]]*\([^[:space:]#]*\).*/\1/p' "$f" | head -1 ;;
        *)
            sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$f" | head -1 ;;
    esac
}

# set_package_version REPO_DIR FILE NEWVER -> rewrites the FIRST version
# declaration in FILE to NEWVER (in every registered version file the
# top-level package version is the first declaration; dependency tables
# come later and are never touched). Format-aware per read_package_version.
# NEWVER is always validated [0-9.]+ by the caller, so the perl snippets
# are injection-safe.
set_package_version() {
    local repo_dir="$1" f="$2" newver="$3"
    local file="$repo_dir/$f" base
    [ -f "$file" ] || { log "  note: version file '$f' not found (absent or moved?)"; return 1; }
    base="$(basename "$f")"
    case "$base" in
        package.json|composer.json)
            # ${1} (not $1): the version digits would otherwise be read as
            # $10, $11, ... — perl group interpolation swallows the digits.
            perl -pi -e 'BEGIN{$d=0} if (!$d && s/("version"\s*:\s*")[^"]+/${1}'"$newver"'/) {$d=1}' "$file" ;;
        pubspec.yaml)
            perl -pi -e 'BEGIN{$d=0} if (!$d && s/^version:[[:space:]]*\S+/version: '"$newver"'/) {$d=1}' "$file" ;;
        *)
            perl -pi -e 'BEGIN{$d=0} if (!$d && s/^version[[:space:]]*=[[:space:]]*"[^"]+"/version = "'"$newver"'"/) {$d=1}' "$file" ;;
    esac
}

# ---------------------------------------------------------------------------
# CLI

while [ $# -gt 0 ]; do
    case "$1" in
        --check)             MODE_CHECK=1 ;;
        --dry-run)           MODE_DRY=1 ;;
        --merge-when-green)  MODE_MERGE=1 ;;
        --release-after-merge) MODE_RELEASE=1 ;;
        --repo)
            shift
            [ $# -gt 0 ] || die "--repo needs an ORG/NAME argument"
            case "$1" in */*) ;; *) die "--repo expects ORG/NAME, got '$1'" ;; esac
            REPO_FILTER+=("$1") ;;
        --timeout)
            shift
            [ $# -gt 0 ] || die "--timeout needs a minutes argument"
            POLL_TIMEOUT_MIN="$1" ;;
        --interval)
            shift
            [ $# -gt 0 ] || die "--interval needs a seconds argument"
            POLL_INTERVAL="$1" ;;
        -h|--help)  usage; exit 0 ;;
        -*)         die "unknown option: $1 (see --help)" ;;
        *)
            [ -z "$NEW_TAG" ] || die "unexpected second positional argument: $1"
            NEW_TAG="$1" ;;
    esac
    shift
done

[ -f "$REGISTRY" ] || die "registry not found: $REGISTRY"

# --check / --merge-when-green / --release-after-merge are mutually exclusive;
# --dry-run stands alone (the bump dry-run) or rides with --release-after-merge
# (the cascade dry form).
n_modes=$(( MODE_CHECK + MODE_MERGE + MODE_RELEASE ))
[ "$n_modes" -le 1 ] || die "--check, --merge-when-green and --release-after-merge are mutually exclusive"
[ "$MODE_DRY" -eq 0 ] || [ "$n_modes" -eq 0 ] || [ "$MODE_RELEASE" -eq 1 ] \
    || die "--dry-run combines only with --release-after-merge"
if [ "$n_modes" -eq 0 ] && [ "$MODE_DRY" -eq 0 ]; then
    [ -n "$NEW_TAG" ] || die "missing NEW_TAG (usage: bump.sh NEW_TAG | --check | --dry-run NEW_TAG | --merge-when-green [TAG] | --release-after-merge NEW_TAG)"
fi
if [ "$MODE_RELEASE" -eq 1 ]; then
    [ -n "$NEW_TAG" ] || die "--release-after-merge needs a NEW_TAG (usage: bump.sh --release-after-merge vX.Y.Z)"
fi
if [ -n "$NEW_TAG" ]; then
    printf '%s' "$NEW_TAG" | grep -qE "$TAG_RE" \
        || die "NEW_TAG must look like vX.Y.Z, got '$NEW_TAG'"
fi
case "$MODE_CHECK$MODE_DRY" in
    10) [ -z "$NEW_TAG" ] || die "--check takes no NEW_TAG" ;;
    01) [ -n "$NEW_TAG" ] || die "--dry-run needs a NEW_TAG" ;;
esac
case "$POLL_TIMEOUT_MIN$POLL_INTERVAL" in
    *[!0-9]*) die "--timeout/--interval expect integers" ;;
esac

command -v git >/dev/null 2>&1 || die "git not found"
command -v gh  >/dev/null 2>&1 || die "gh not found (authenticate with: gh auth login)"

# ---------------------------------------------------------------------------
# registry

REPOS=() GLOBS=() BRANCHES=() VERSIONS=()
line_no=0
while IFS= read -r raw; do
    line_no=$(( line_no + 1 ))
    case "$raw" in ''|'#'*) continue ;; esac
    tabs="$(printf '%s' "$raw" | tr -cd '\t' | wc -c | tr -d ' ')"
    [ "$tabs" -eq 2 ] || [ "$tabs" -eq 3 ] \
        || die "registry.tsv:$line_no: expected 3 or 4 TAB-separated fields"
    repo="${raw%%$'\t'*}"
    rest="${raw#*$'\t'}"
    globs="${rest%%$'\t'*}"
    rest="${rest#*$'\t'}"
    if [ "$tabs" -eq 3 ]; then
        branch="${rest%%$'\t'*}"
        version_file="${rest#*$'\t'}"
    else
        branch="$rest"
        version_file=""
    fi
    case "$repo" in */*) ;; *) die "registry.tsv:$line_no: repo must be ORG/NAME, got '$repo'" ;; esac
    [ -n "$globs" ]  || die "registry.tsv:$line_no: empty pin-file-globs"
    [ -n "$branch" ] || die "registry.tsv:$line_no: empty base-branch"
    keep=1
    if [ "${#REPO_FILTER[@]}" -gt 0 ]; then
        keep=0
        for f in "${REPO_FILTER[@]}"; do
            if [ "$f" = "$repo" ]; then keep=1; fi
        done
    fi
    if [ "$keep" -eq 1 ]; then
        REPOS+=("$repo"); GLOBS+=("$globs"); BRANCHES+=("$branch"); VERSIONS+=("$version_file")
    fi
done < "$REGISTRY"
[ "${#REPOS[@]}" -gt 0 ] || die "no registered repos selected (registry: $REGISTRY, filter: ${REPO_FILTER[*]+"${REPO_FILTER[*]}"})"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/corvid-bump.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

clone_repo() { # ORG/NAME BRANCH DEST
    git clone --quiet --branch "$2" "git@github.com:$1.git" "$3"
}

# ---------------------------------------------------------------------------
# --check: read-only drift audit

if [ "$MODE_CHECK" -eq 1 ]; then
    latest="$(engine_latest_tag)"
    echo "engine: $(basename "$ENGINE_DIR") — latest release tag: ${latest:-unknown}"
    echo
    fail=0
    printf '%-24s %-9s %-9s %s\n' REPO PIN ENGINE VERDICT
    for i in "${!REPOS[@]}"; do
        repo="${REPOS[$i]}" branch="${BRANCHES[$i]}"
        name="${repo#*/}"
        log "cloning $repo ($branch)..."
        clone_repo "$repo" "$branch" "$WORK/$name" || { printf '%-24s %-9s %-9s %s\n' "$repo" - - 'CLONE-FAILED'; fail=1; continue; }

        files=()
        while IFS= read -r f; do files+=("$f"); done < <(expand_globs "$WORK/$name" "${GLOBS[$i]}" | awk '!seen[$0]++')
        if [ "${#files[@]}" -eq 0 ]; then
            printf '%-24s %-9s %-9s %s\n' "$repo" - - 'NO-PIN-FILES (glob(s) matched nothing)'
            fail=1; continue
        fi

        votes="$(collect_votes "$WORK/$name" ${files[@]+"${files[@]}"} | cut -f2 | awk '!seen[$0]++')"
        n_votes="$(printf '%s\n' "$votes" | grep -cxE "$TAG_TOKEN_RE" || true)"
        if [ "$n_votes" -ne 1 ]; then
            printf '%-24s %-9s %-9s %s\n' "$repo" - "${latest:-?}" 'INCONSISTENT — pins disagree:'
            collect_votes "$WORK/$name" ${files[@]+"${files[@]}"} | sed 's/^/    /'
            fail=1; continue
        fi

        pin="$votes"
        if [ "$pin" = "$latest" ]; then verdict='current'
        elif [ "$(ver_key "$pin")" -lt "$(ver_key "$latest")" ]; then verdict="DRIFTED (engine latest $latest)"
        else verdict="AHEAD of engine latest ${latest:-?}"; fi
        printf '%-24s %-9s %-9s %s\n' "$repo" "$pin" "${latest:-?}" "$verdict"
        for f in "${files[@]}"; do
            n="$({ grep -oF -- "$pin" "$WORK/$name/$f" || true; } | wc -l | tr -d ' ')"
            extra="$(other_tags "$WORK/$name/$f" "$pin" | tr '\n' ' ')"
            detail=""
            [ "$n" -gt 0 ] && detail="$n ref(s)"
            voted="$(collect_votes "$WORK/$name" "$f" | cut -f2 | awk '!seen[$0]++' | tr '\n' ' ')"
            [ -n "$voted" ] && detail="${detail:+$detail, }pin assignment: $(printf '%s' "$voted" | sed 's/ $//')"
            [ -n "$extra" ] && detail="${detail:+$detail; }historical: $(printf '%s' "$extra" | sed 's/ $//')"
            printf '    %-32s %s\n' "$f" "${detail:--}"
        done
        vfile="${VERSIONS[$i]}"
        if [ -n "$vfile" ]; then
            if [ -f "$WORK/$name/$vfile" ]; then
                pkgver="$(read_package_version "$WORK/$name/$vfile")"
                if [ "$pkgver" = "${latest#v}" ]; then
                    printf '    %-32s %s\n' "$vfile" "package version ${pkgver:-?} (release parity with engine)"
                else
                    printf '    %-32s %s\n' "$vfile" "package version ${pkgver:-<unreadable>} — engine latest ${latest#v:-?} (parity lands with the next bump)"
                fi
            else
                printf '    %-32s %s\n' "$vfile" "MISSING (registered version file not found)"
            fi
        fi
    done
    [ "$fail" -eq 0 ] || exit 1
    exit 0
fi

# ---------------------------------------------------------------------------
# --merge-when-green: poll bump PRs' golden CI; squash-merge on green

if [ "$MODE_MERGE" -eq 1 ]; then
    pr_repos=() pr_urls=() pr_states=() pr_details=()
    for repo in "${REPOS[@]}"; do
    # NB: the regex's \. must be DOUBLE-backslashed here — jq unwraps
    # \\ to \ in its string literals, so "\\\\." would arrive at the
    # regex engine as a literal-backslash match and select nothing
    # (the exact bug the first real --merge-when-green run hit).
    list="$(gh pr list --repo "$repo" --state open --limit 100 \
                 --json headRefName,url --jq '.[] | select(.headRefName | test("^bump/v[0-9]+\\.[0-9]+\\.[0-9]+$")) | .headRefName + "\t" + .url' 2>/dev/null || true)"
        while IFS=$'\t' read -r head url; do
            [ -n "$head" ] || continue
            if [ -n "$NEW_TAG" ] && [ "$head" != "bump/$NEW_TAG" ]; then continue; fi
            pr_repos+=("$repo"); pr_urls+=("$url"); pr_states+=("pending"); pr_details+=("")
            log "tracking $repo $head $url"
        done <<EOF
$list
EOF
    done
    n_prs="${#pr_urls[@]}"
    if [ "$n_prs" -eq 0 ]; then
        log "no open bump/* PRs found in the selected repos — nothing to do"
        exit 0
    fi

    deadline=$(( $(date +%s) + POLL_TIMEOUT_MIN * 60 ))
    while :; do
        all_done=1
        for i in "${!pr_urls[@]}"; do
            [ "${pr_states[i]}" = "pending" ] || continue
            state_json="$(gh pr view "${pr_urls[i]}" --repo "${pr_repos[i]}" --json state --jq .state 2>/dev/null || true)"
            if [ "$state_json" = "MERGED" ]; then
                pr_states[i]="merged"; pr_details[i]="merged out-of-band"; log "${pr_repos[i]}: PR already merged"; continue
            fi
            checks="$(gh pr checks "${pr_urls[i]}" --repo "${pr_repos[i]}" \
                          --json name,bucket,link \
                          --jq '.[] | .bucket + "\t" + .name + "\t" + .link' 2>/dev/null || true)"
            if [ -z "$checks" ]; then
                all_done=0   # no checks reported yet — keep waiting
                continue
            fi
            if printf '%s\n' "$checks" | cut -f1 | grep -qx 'pending'; then
                all_done=0; continue
            fi
            bad="$(printf '%s\n' "$checks" | awk -F'\t' '$1=="fail" || $1=="cancel"' | head -1)"
            if [ -n "$bad" ]; then
                job="$(printf '%s' "$bad" | cut -f2)"; link="$(printf '%s' "$bad" | cut -f3)"
                pr_states[i]="FAILED"; pr_details[i]="$job: $link"
                log "${pr_repos[i]}: golden CI failed — $job $link"
                continue
            fi
            if gh pr merge "${pr_urls[i]}" --repo "${pr_repos[i]}" --squash --auto --delete-branch >/dev/null 2>&1; then
                pr_states[i]="merging"; pr_details[i]="checks green — squash auto-merge armed"
            elif gh pr merge "${pr_urls[i]}" --repo "${pr_repos[i]}" --squash --delete-branch >/dev/null 2>&1; then
                pr_states[i]="merging"; pr_details[i]="checks green — squash-merged"
            else
                pr_states[i]="FAILED"; pr_details[i]="checks green but merge refused (branch protection? review required?) — merge manually"
            fi
            log "${pr_repos[i]}: ${pr_details[i]}"
        done
        if [ "$all_done" -eq 1 ]; then break; fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            for i in "${!pr_urls[@]}"; do
                [ "${pr_states[i]}" = "pending" ] && { pr_states[i]="TIMEOUT"; pr_details[i]="checks still pending after ${POLL_TIMEOUT_MIN}min"; }
            done
            break
        fi
        sleep "$POLL_INTERVAL"
    done

    echo
    printf '%-24s %-8s %s\n' REPO RESULT DETAIL
    fail=0
    for i in "${!pr_urls[@]}"; do
        printf '%-24s %-8s %s\n' "${pr_repos[i]}" "${pr_states[i]}" "${pr_details[i]} ${pr_urls[i]}"
        case "${pr_states[i]}" in FAILED|TIMEOUT) fail=1 ;; esac
    done
    exit "$fail"
fi

# ---------------------------------------------------------------------------
# bump / --dry-run: clone, verify consistency, substitute, (branch, push, PR)

tag_exists="$(git -C "$ENGINE_DIR" ls-remote --tags --refs origin "refs/tags/$NEW_TAG" 2>/dev/null | wc -l | tr -d ' ')"
if [ "$MODE_DRY" -eq 0 ] && [ "$tag_exists" -eq 0 ]; then
    die "tag $NEW_TAG does not exist on the engine remote — cut the release first (or use --dry-run)"
fi
[ "$tag_exists" -eq 1 ] || log "note: $NEW_TAG is not (yet) an engine tag — dry-run only"

# ---------------------------------------------------------------------------
# --release-after-merge: the cascade — tag every binding at NEW_TAG and let
# the per-repo release workflows publish. Explicit opt-in only: this flag is
# the ONLY path that creates these tags, and it refuses any repo whose bump
# PR is still open, whose base branch does not pin NEW_TAG yet, or whose
# registered package version does not match the tag (release parity).

if [ "$MODE_RELEASE" -eq 1 ]; then
    bare="${NEW_TAG#v}"
    rel_repo=() rel_action=() rel_detail=() rel_url=()
    rel_fail=0
    for i in "${!REPOS[@]}"; do
        repo="${REPOS[$i]}" branch="${BRANCHES[$i]}" vfile="${VERSIONS[$i]}"
        name="${repo#*/}"
        log "cloning $repo ($branch)..."
        clone_repo "$repo" "$branch" "$WORK/$name" || {
            rel_repo+=("$repo"); rel_action+=("CLONE-FAILED"); rel_detail+=("-"); rel_url+=("-"); rel_fail=1; continue; }

        # 1. the bump PR for NEW_TAG must be merged (or closed) out of the way
        open_url="$(gh pr list --repo "$repo" --head "bump/$NEW_TAG" --state open --json url --jq '.[0].url' 2>/dev/null || true)"
        if [ -n "$open_url" ]; then
            rel_repo+=("$repo"); rel_action+=("SKIPPED"); rel_detail+=("bump PR still open"); rel_url+=("$open_url"); rel_fail=1
            log "$repo: bump/$NEW_TAG PR is still open — merge (or close) it first"; continue
        fi

        # 2. the merged base branch must pin NEW_TAG (the bump landed)
        files=()
        while IFS= read -r f; do files+=("$f"); done < <(expand_globs "$WORK/$name" "${GLOBS[$i]}" | awk '!seen[$0]++')
        pin=""
        [ "${#files[@]}" -gt 0 ] && pin="$(collect_votes "$WORK/$name" ${files[@]+"${files[@]}"} | cut -f2 | awk '!seen[$0]++')"
        n_votes="$(printf '%s\n' "$pin" | grep -cxE "$TAG_TOKEN_RE" || true)"
        if [ "$n_votes" -ne 1 ] || [ "$pin" != "$NEW_TAG" ]; then
            rel_repo+=("$repo"); rel_action+=("SKIPPED"); rel_detail+=("pins '${pin:-nothing}', not $NEW_TAG"); rel_url+=("-"); rel_fail=1
            log "$repo: base branch pins '${pin:-nothing}' — expected $NEW_TAG (bump PR merged?)"; continue
        fi

        # 3. registry-publishing bindings: package version must equal the tag
        #    (release parity — the binding publishes at the engine's version)
        if [ -n "$vfile" ]; then
            if [ ! -f "$WORK/$name/$vfile" ]; then
                rel_repo+=("$repo"); rel_action+=("SKIPPED"); rel_detail+=("version file '$vfile' missing"); rel_url+=("-"); rel_fail=1
                log "$repo: registered version file '$vfile' not found"; continue
            fi
            pkgver="$(read_package_version "$WORK/$name/$vfile")"
            if [ "$pkgver" != "$bare" ]; then
                rel_repo+=("$repo"); rel_action+=("SKIPPED"); rel_detail+=("$vfile at ${pkgver:-<unreadable>}, tag wants $bare"); rel_url+=("-"); rel_fail=1
                log "$repo: package version '${pkgver:-unset}' != $bare — land the version bump, then re-run"; continue
            fi
        fi

        # dry-run of the cascade: every check above ran; tag NOTHING
        if [ "$MODE_DRY" -eq 1 ]; then
            rel_repo+=("$repo"); rel_action+=("WOULD-TAG"); rel_detail+=("$NEW_TAG (all checks passed)"); rel_url+=("-")
            log "$repo: would tag $NEW_TAG and fire its release (dry-run — nothing tagged)"
            continue
        fi

        # 4. tag HEAD at NEW_TAG (idempotent: an existing remote tag is kept)
        have="$(git -C "$WORK/$name" ls-remote --tags --refs origin "refs/tags/$NEW_TAG" | wc -l | tr -d ' ')"
        if [ "$have" -ge 1 ]; then
            action="ALREADY-TAGGED"
            log "$repo: $NEW_TAG already exists on the remote — keeping it"
        else
            git -C "$WORK/$name" tag "$NEW_TAG" || {
                rel_repo+=("$repo"); rel_action+=("TAG-FAILED"); rel_detail+=("$NEW_TAG"); rel_url+=("-"); rel_fail=1; continue; }
            if ! git -C "$WORK/$name" push --quiet origin "refs/tags/$NEW_TAG"; then
                rel_repo+=("$repo"); rel_action+=("PUSH-FAILED"); rel_detail+=("tag $NEW_TAG"); rel_url+=("-"); rel_fail=1; continue
            fi
            action="TAGGED"
        fi

        # 5. surface the fired workflow run (repos with a release workflow)
        #    or the tag permalink (pin-only repos; php: Packagist syncs)
        url="-"; detail="$NEW_TAG"
        if [ -f "$WORK/$name/.github/workflows/release.yml" ]; then
            url=""
            for _ in 1 2 3 4 5 6 7 8 9; do
                url="$(gh run list --repo "$repo" --branch "$NEW_TAG" --event push --limit 1 --json url --jq '.[0].url' 2>/dev/null || true)"
                [ -n "$url" ] && break
                sleep 5
            done
            if [ -n "$url" ]; then
                detail="$NEW_TAG (release workflow fired)"
            else
                url="-"; detail="$NEW_TAG (tag pushed; no run URL after 45s — watch Actions)"
            fi
        else
            url="https://github.com/$repo/tree/$NEW_TAG"
            if [ -f "$WORK/$name/composer.json" ]; then
                detail="$NEW_TAG (Packagist auto-syncs from the tag)"
            else
                detail="$NEW_TAG (pin tag; no release workflow)"
            fi
        fi
        rel_repo+=("$repo"); rel_action+=("$action"); rel_detail+=("$detail"); rel_url+=("$url")
        log "$repo: $action — $detail $url"
    done

    echo
    dry_suffix=""
    [ "$MODE_DRY" -eq 1 ] && dry_suffix=" (dry-run — nothing tagged, nothing pushed)"
    echo "=== --release-after-merge $NEW_TAG$dry_suffix — summary ==="
    printf '%-24s %-14s %-44s %s\n' REPO ACTION DETAIL URL
    for i in "${!rel_repo[@]}"; do
        printf '%-24s %-14s %-44s %s\n' "${rel_repo[$i]}" "${rel_action[$i]}" "${rel_detail[$i]}" "${rel_url[$i]}"
    done
    [ "$rel_fail" -eq 0 ] || exit 1
    exit 0
fi

declare -a out_repo=() out_old=() out_new=() out_url=() out_status=()
fail=0
for i in "${!REPOS[@]}"; do
    repo="${REPOS[$i]}" branch="${BRANCHES[$i]}"
    name="${repo#*/}"
    log "cloning $repo ($branch)..."
    clone_repo "$repo" "$branch" "$WORK/$name" || { out_repo+=("$repo"); out_old+=(-); out_new+=("$NEW_TAG"); out_url+=(-); out_status+=("CLONE-FAILED"); fail=1; continue; }

    files=()
    while IFS= read -r f; do files+=("$f"); done < <(expand_globs "$WORK/$name" "${GLOBS[$i]}" | awk '!seen[$0]++')
    if [ "${#files[@]}" -eq 0 ]; then
        out_repo+=("$repo"); out_old+=(-); out_new+=("$NEW_TAG"); out_url+=(-)
        out_status+=("NO-PIN-FILES (glob(s) matched nothing)"); fail=1; continue
    fi

    votes="$(collect_votes "$WORK/$name" ${files[@]+"${files[@]}"} | cut -f2 | awk '!seen[$0]++')"
    n_votes="$(printf '%s\n' "$votes" | grep -cxE "$TAG_TOKEN_RE" || true)"
    if [ "$n_votes" -ne 1 ]; then
        log "$repo: refusing — pins are inconsistent within the repo:"
        collect_votes "$WORK/$name" ${files[@]+"${files[@]}"} | sed 's/^/    /' >&2
        out_repo+=("$repo"); out_old+=(-); out_new+=("$NEW_TAG"); out_url+=(-)
        out_status+=("REFUSED — inconsistent pins"); fail=1; continue
    fi
    old="$votes"

    if [ "$old" = "$NEW_TAG" ]; then
        out_repo+=("$repo"); out_old+=("$old"); out_new+=("$NEW_TAG"); out_url+=(-)
        out_status+=("already at $NEW_TAG — nothing to do"); continue
    fi
    if [ "$(ver_key "$old")" -gt "$(ver_key "$NEW_TAG")" ]; then
        out_repo+=("$repo"); out_old+=("$old"); out_new+=("$NEW_TAG"); out_url+=(-)
        out_status+=("REFUSED — would be a downgrade"); fail=1; continue
    fi

    subs="$(substitute "$WORK/$name" "$old" "$NEW_TAG" ${files[@]+"${files[@]}"})"
    total="$(printf '%s\n' "$subs" | cut -f2 | awk '{s+=$1} END {print s+0}')"

    # Release parity: repos with a registered version-file ALSO get their own
    # package version set to the tag's bare version in the same PR, so the
    # later --release-after-merge tag publishes at the engine's version.
    pkg_old="" pkg_new="" version_note=""
    if [ -n "${VERSIONS[$i]}" ]; then
        if [ -f "$WORK/$name/${VERSIONS[$i]}" ]; then
            pkg_old="$(read_package_version "$WORK/$name/${VERSIONS[$i]}")"
            if [ -n "$pkg_old" ] && [ "$(ver_key "$pkg_old")" -gt "$(ver_key "$NEW_TAG")" ]; then
                out_repo+=("$repo"); out_old+=("$old"); out_new+=("$NEW_TAG"); out_url+=(-)
                out_status+=("REFUSED — package version $pkg_old would be a downgrade"); fail=1; continue
            fi
            if set_package_version "$WORK/$name" "${VERSIONS[$i]}" "${NEW_TAG#v}"; then
                pkg_new="$(read_package_version "$WORK/$name/${VERSIONS[$i]}")"
                if [ "$pkg_new" != "$pkg_old" ]; then
                    version_note="y"
                    log "$repo: package version $pkg_old -> $pkg_new (${VERSIONS[$i]})"
                fi
            fi
        else
            log "$repo: registered version file '${VERSIONS[$i]}' not found — pin bumped, version untouched"
        fi
    fi

    if git -C "$WORK/$name" diff --quiet; then
        out_repo+=("$repo"); out_old+=("$old"); out_new+=("$NEW_TAG"); out_url+=("-")
        out_status+=("NO-OP — substitution produced no diff"); fail=1; continue
    fi

    if [ "$MODE_DRY" -eq 1 ]; then
        echo
        echo "=== $repo: $old -> $NEW_TAG ($total substitution(s)) — diff it would make ==="
        git -C "$WORK/$name" --no-pager diff
        out_repo+=("$repo"); out_old+=("$old"); out_new+=("$NEW_TAG"); out_url+=("(dry-run)")
        out_status+=("$total substitution(s)${version_note:+ + ${VERSIONS[$i]} $pkg_old -> $pkg_new}; would branch bump/$NEW_TAG and open a PR"); continue
    fi

    body="$WORK/pr-body.md"
    {
        echo "Automated engine-pin bump \`$old\` -> \`$NEW_TAG\`, opened by \`scripts/bindings/bump.sh\` in corvid-db/corvid."
        echo
        echo "The substitution is purely textual: every occurrence of \`$old\` in this repo's registered pin globs became \`$NEW_TAG\`:"
        printf '%s\n' "$subs" | awk -F'\t' '{ printf "- \`%s\`: %s substitution(s)\n", $1, $2 }'
        if [ "$version_note" = "y" ]; then
            echo "- \`${VERSIONS[$i]}\`: package version \`$pkg_old\` -> \`$pkg_new\` (release parity — this binding publishes at the engine's version once tagged)"
        fi
        echo
        echo "**The golden suite on this PR is the verdict for $NEW_TAG.** Green means the new pin is compatible with this binding — merge (squash). Red means do not merge; the failing job is the evidence."
        echo
        echo "Tracked by \`scripts/bindings/bump.sh --merge-when-green $NEW_TAG\` in the engine repo."
    } > "$body"

    git -C "$WORK/$name" checkout -q -B "bump/$NEW_TAG"
    git -C "$WORK/$name" add -A
    n_files="$(printf '%s\n' "$subs" | grep -c . || true)"
    if ! git -C "$WORK/$name" commit -q \
        -m "chore: bump engine pin $old -> $NEW_TAG" \
        -m "Automated by scripts/bindings/bump.sh (corvid-db/corvid): $total textual substitution(s) across $n_files file(s)${version_note:+, package version $pkg_old -> $pkg_new for registry parity}. The golden CI on this branch is the verdict for the new pin."; then
        out_repo+=("$repo"); out_old+=("$old"); out_new+=("$NEW_TAG"); out_url+=(-)
        out_status+=("COMMIT-FAILED"); fail=1; continue
    fi
    if ! git -C "$WORK/$name" push --quiet --force-with-lease origin "bump/$NEW_TAG"; then
        out_repo+=("$repo"); out_old+=("$old"); out_new+=("$NEW_TAG"); out_url+=(-)
        out_status+=("PUSH-FAILED"); fail=1; continue
    fi
    url="$(gh pr create --repo "$repo" --base "$branch" --head "bump/$NEW_TAG" \
                --title "Bump engine pin to $NEW_TAG" --body-file "$body" 2>/dev/null || true)"
    if [ -z "$url" ]; then
        # rerun on an existing open PR: gh pr create refuses, so look it up
        url="$(gh pr list --repo "$repo" --head "bump/$NEW_TAG" --state open \
                    --json url --jq '.[0].url' 2>/dev/null || true)"
    fi
    out_repo+=("$repo"); out_old+=("$old"); out_new+=("$NEW_TAG")
    if [ -n "$url" ]; then
        out_url+=("$url"); out_status+=("PR opened ($total substitution(s))"); log "$repo: PR $url"
    else
        out_url+=("-"); out_status+=("PR-FAILED (branch bump/$NEW_TAG was pushed)"); fail=1
        log "$repo: PR creation failed (branch bump/$NEW_TAG was pushed)"
    fi
done

echo
mode_label="bump $NEW_TAG"
if [ "$MODE_DRY" -eq 1 ]; then mode_label="dry-run $NEW_TAG (nothing pushed)"; fi
echo "=== $mode_label — summary ==="
printf '%-24s %-9s %-9s %-12s %s\n' REPO FROM TO STATUS PR
for i in "${!out_repo[@]}"; do
    printf '%-24s %-9s %-9s %-12s %s\n' "${out_repo[$i]}" "${out_old[$i]}" "${out_new[$i]}" "${out_status[$i]}" "${out_url[$i]}"
done
[ "$fail" -eq 0 ] || exit 1
exit 0
