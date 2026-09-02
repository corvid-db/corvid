# bindings — engine-pin bump automation

When the engine (corvid-db/corvid) cuts a new release tag, every binding repo
still pins the old one. `bump.sh` makes the fan-out one command, and the
verdict per binding comes from that binding's **own golden-suite CI** on the
bump PR — that is the design: green golden CI means the new pin is compatible;
red means don't merge, and the failing job is the evidence.

The tool lives in the engine repo (it orchestrates the siblings but belongs to
the coordinator). It operates on the repos registered in `registry.tsv`.

## Toolchain policy (2026-09, correcting the stale-defaults mistake)

**New bindings with zero users pin MODERN minimums, and CI tests latest +
previous — there is no compat base to protect.** Defaulting a fresh binding
to an older floor "to be safe" is the mistake this section exists to prevent:
every floor we ship becomes a compat promise, and nothing has users yet.

- **Language floors: latest-minus-one, not oldest-supported.** The `go.mod`
  `go` directive and `engines`/`requires-python` floors track the newest
  toolchain line a mainstream distro or LTS ships (e.g. Go 1.26 with the
  1.27 toolchain auto-forwarding, Node ≥ 20, Python ≥ 3.11) — not the
  oldest line that happens to compile.
- **CI matrices test latest + previous** for every language: e.g. Go
  `['1.27.x', '1.26.x']`, Node `[24, 22, 20]`, Python `[3.14, 3.13, 3.12,
  3.11]` (intermediate Python lines may ride a single leg if wall-time
  demands — keep latest + floor on the wide platform matrix).
- **No EOL lines anywhere** (CI, engines fields, docs): Node 18, Python
  ≤ 3.10, Go ≤ 1.24 must not appear as supported or tested.
- **Build/tool floors follow the same rule**: e.g. corvid-c's
  `cmake_minimum_required(3.28)` = Ubuntu 24.04 LTS system CMake, the
  oldest any supported platform ships.
- **CI actions and lint tooling stay current-major** (currently the
  `actions/checkout@v7` era, `setup-*@v7`, `golangci-lint-action@v9` with
  golangci-lint v2); bump them in the same sweep as language floors.
- The engine's own MSRV is set deliberately by the workspace and is
  **out of scope** here; bindings inherit it via the engine dependency.

When a language line goes EOL, the floor moves up and the matrix drops the
oldest line — in the binding repos, one PR per repo, golden CI as the gate.

## The workflow

```
engine tag vX.Y.Z published
        │
        ▼
scripts/bindings/bump.sh --check            # (optional) drift audit first
scripts/bindings/bump.sh vX.Y.Z             # one PR per registered binding:
        │                                   #   "Bump engine pin to vX.Y.Z"
        ▼
scripts/bindings/bump.sh --merge-when-green vX.Y.Z
        │                                   # polls each PR's checks
        ▼                                   # (gh pr checks) until the golden
merged on green; failures reported          # suite completes; squash-merges
with the failing job URL                    # on green (--auto, then plain)
```

## Usage

```
bump.sh NEW_TAG                    the bump: clone, verify pins, substitute,
                                   branch bump/NEW_TAG, commit, push, PR,
                                   print a table of PR URLs
bump.sh --check                    read-only audit: current pin per repo vs the
                                   engine's latest tag (clones, changes nothing)
bump.sh --dry-run NEW_TAG          full substitution in a temp clone; prints the
                                   exact diff it would make; pushes nothing
bump.sh --merge-when-green [TAG]   poll open bump/* PRs' checks; merge on green
bump.sh --repo ORG/NAME [...]      restrict any mode to a subset of the registry
bump.sh --timeout M --interval S   merge-when-green polling bounds (60m / 30s)
```

All modes refuse a repo whose pins are **inconsistent within the repo** (e.g.
`fetch.sh` at v0.2.1 but `fetch.ps1` at v0.2.0) and report it instead of
bumping; other repos continue. Bumps never go backwards (downgrades are
refused), re-running against a tag a repo already pins is a no-op, and the
plain bump mode requires the tag to exist on the engine remote.

## Registry format

`registry.tsv`, one line per binding (TAB-separated, exactly three fields):

```
repo<TAB>pin-file-globs<TAB>base-branch
```

- **repo** — GitHub `ORG/NAME` of the binding (ssh clone/push target).
- **pin-file-globs** — space-separated globs, relative to the binding repo
  root, matching every file that pins or references the engine tag.
- **base-branch** — the branch `bump/vX.Y.Z` is cut from and the PR base.

Current rows: `corvid-db/corvid-c` (`fetch.sh`, `fetch.ps1`, optional
`.engine-pin`, `README.md`, `docs/PLAN.md`), `corvid-db/corvid-node`
(`Cargo.toml`, `Cargo.lock`, `README.md`, `docs/PLAN.md`),
`corvid-db/corvid-python` (same shape as corvid-node),
`corvid-db/corvid-go` (same shape as corvid-c), and
`corvid-db/corvid-js` (same shape as corvid-node).

## The binding-surface manifest: `surface.sh` / `surface.tsv`

`surface.sh` parses the radar-enforced MANIFEST in
`crates/corvid/tests/surface/mod.rs` (the same source `docs/SYNTAX.md`
regenerates from) and emits `surface.tsv`: one `item<TAB>class<TAB>exposure`
line per public construct, exposure starting `UNMAPPED`. The file is
committed and drift-gated in engine CI (`crates/corvid/tests/surface_tsv.rs`
re-derives it and fails on any difference).

Every binding repo fetches `surface.tsv` from the raw URL at its pinned
engine tag and resolves every line in its own `docs/SURFACE.tsv`
(binding-api + proving test, or `N/A` + reason). This is the
"how do we know a binding isn't missing engine surface?" gate — each
binding's `surface-gate` CI job enforces it, so a tag that changes the
engine surface lands in the binding gates the moment the pin is bumped.

**A new binding registers itself by adding ONE line to `registry.tsv`** and
being pushable by whoever runs the tool. Nothing else to wire up.

## Substitution semantics (read this before your first bump)

- **Pin detection is by pin-shaped context.** A `vX.Y.Z` token votes on what
  the repo's pin is when it appears as `tag = "vX.Y.Z"` (Cargo.toml),
  `?tag=vX.Y.Z` (Cargo.lock source), or `<something>VERSION/PIN = vX.Y.Z`
  (case-insensitive, e.g. `CORVID_VERSION=`, `$CorvidVersion =`,
  `ENGINE_PIN=`), or as the whole content of a `.engine-pin` file. All votes
  in a repo must agree on exactly one tag.
- **The substitution is purely textual**: every occurrence of the old pin tag
  across the registered globs becomes the new tag — including prose references
  in README/PLAN ("the current pin", "vendored from the vX.Y.Z release").
  Occurrences of *other* (historical) tags are never touched, so
  defect-write-up history like "the v0.2.0 darwin dylib defect" survives a
  bump verbatim.
- **Caveat, by design:** prose that mentions the *old pin* in a historical
  sense ("resolved in v0.2.1") does get rewritten to the new tag. That is the
  cost of pure-textual substitution; review the `--dry-run` diff before a real
  bump — it prints exactly what will land in the PR.
- **Cargo.lock:** the `?tag=vX.Y.Z` in the git source line is substituted; the
  `version = "X.Y.Z"` line and the `#rev` hash are not (no `v`, and a rev
  cannot be computed textually). Cargo re-resolves the lock on the first build
  — the golden CI builds without `--locked`, so this is fine — and a
  maintainer's first local `cargo update -p corvid` refreshes the committed
  lock.
- **Pin globs that match nothing are reported, not fatal** (an optional
  `.engine-pin` may legitimately be absent); a typo'd glob is visible in every
  mode's output.

## Example: the live drift audit (2026-09-01)

```
$ bump.sh --check
engine: corvid — latest release tag: v0.2.1

REPO                     PIN       ENGINE    VERDICT
corvid-db/corvid-c       v0.2.1    v0.2.1    current
    fetch.sh                         1 ref(s), pin assignment: v0.2.1
    fetch.ps1                        1 ref(s), pin assignment: v0.2.1
    README.md             5 ref(s), pin assignment: v0.2.1; historical: v0.2.0
    docs/PLAN.md                     7 ref(s); historical: v0.2.0
corvid-db/corvid-node    v0.2.1    v0.2.1    current
    Cargo.toml                       1 ref(s), pin assignment: v0.2.1
    Cargo.lock                       1 ref(s), pin assignment: v0.2.1
    README.md                        -
    docs/PLAN.md          3 ref(s), pin assignment: v0.2.1; historical: v0.1.0
```

Requirements: bash, `git`, `gh` (authenticated), ssh push access to the
binding repos. `shellcheck`-clean; POSIX-bash compatible (runs on stock
macOS bash 3.2).
