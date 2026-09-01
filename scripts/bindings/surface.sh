#!/usr/bin/env bash
# surface.sh — emit the engine's binding-surface manifest as TSV.
#
# The engine's public surface is catalogued by the radar-enforced MANIFEST in
# crates/corvid/tests/surface/mod.rs (the same source docs/SYNTAX.md
# regenerates from): one row per public construct with its statement class.
# This script parses those rows DIRECTLY out of the Rust source (never the
# generated markdown) and writes scripts/bindings/surface.tsv:
#
#     item<TAB>class<TAB>exposure
#
# one line per construct, in manifest order. `exposure` starts UNMAPPED for
# every row: it is the fill-me-in slot a binding overwrites in its own
# docs/SURFACE.tsv (MAPPED or N/A — see the binding's surface gate).
#
# Bindings fetch this file from the raw URL at their pinned engine tag:
#   https://raw.githubusercontent.com/corvid-db/corvid/<tag>/scripts/bindings/surface.tsv
# so a tagged release must contain a current copy (the TSV drift test in
# crates/corvid/tests/surface_tsv.rs fails CI when they diverge).
#
# Usage: scripts/bindings/surface.sh [--check]
#   (no args)   write scripts/bindings/surface.tsv
#   --check     verify the committed TSV matches the manifest; exit 1 on
#               drift (the same check the Rust drift test runs in CI)
#
# Requirements: bash (3.2+), awk. shellcheck-clean.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENGINE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
MANIFEST="$ENGINE_DIR/crates/corvid/tests/surface/mod.rs"
TSV="$SCRIPT_DIR/surface.tsv"

[ -f "$MANIFEST" ] || { echo "surface.sh: manifest not found: $MANIFEST" >&2; exit 1; }

# Extract the MANIFEST block and pull the first two string literals out of
# every row(...) call — the item path and its statement class. Handles both
# shapes rustfmt emits: the multi-line call and the single-line exempt row
# `row("item", "class", &[]),`. String literals never span lines in the
# manifest, so a line-oriented state machine is exact: start collecting at
# `row(`, stop at the second literal (the covering-test names that follow
# are not extracted).
generate() {
    awk '
        /^static MANIFEST: &\[Row\] = &\[/ { in_manifest = 1; next }
        in_manifest && /^\];/ { in_manifest = 0 }
        in_manifest {
            line = $0
            if (collecting == 0) {
                if (index(line, "row(") == 0) next
                collecting = 1
                fields = 0
                item = class = ""
                line = substr(line, index(line, "row(") + 4)
            }
            while (match(line, /"[^"]*"/)) {
                lit = substr(line, RSTART + 1, RLENGTH - 2)
                line = substr(line, RSTART + RLENGTH)
                if (fields == 0) { item = lit; fields = 1 }
                else if (fields == 1) { class = lit; fields = 2 }
            }
            if (fields == 2) {
                print item "\t" class "\tUNMAPPED"
                collecting = 0
            }
        }
    ' "$MANIFEST"
}

case "${1:-}" in
    "")
        tmp="$TSV.tmp"
        generate >"$tmp"
        mv "$tmp" "$TSV"
        echo "surface.sh: wrote $TSV ($(wc -l <"$TSV" | tr -d ' ') constructs)"
        ;;
    --check)
        generate >"$TSV.expected"
        if [ ! -f "$TSV" ]; then
            echo "surface.sh: DRIFT — $TSV is missing; run scripts/bindings/surface.sh" >&2
            rm -f "$TSV.expected"
            exit 1
        fi
        if ! diff -u "$TSV" "$TSV.expected" >"$TSV.diff"; then
            echo "surface.sh: DRIFT — $TSV does not match the MANIFEST." >&2
            echo "  Regenerate with: scripts/bindings/surface.sh" >&2
            sed -n '1,20p' "$TSV.diff" >&2
            rm -f "$TSV.expected" "$TSV.diff"
            exit 1
        fi
        rm -f "$TSV.expected" "$TSV.diff"
        echo "surface.sh: ok — $TSV matches the manifest ($(wc -l <"$TSV" | tr -d ' ') constructs)"
        ;;
    *)
        echo "usage: surface.sh [--check]" >&2
        exit 2
        ;;
esac
