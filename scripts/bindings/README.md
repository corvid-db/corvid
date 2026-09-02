# bindings — engine-pin bump automation + the release cascade

When the engine (corvid-db/corvid) cuts a new release tag, every binding repo
still pins the old one. `bump.sh` makes the fan-out one command, and the
verdict per binding comes from that binding's **own golden-suite CI** on the
bump PR — that is the design: green golden CI means the new pin is compatible;
red means don't merge, and the failing job is the evidence.

The same tool then finishes the job: `--release-after-merge` tags every
binding at the engine's version, which fires the per-repo release workflows
that publish to the registries. **One engine tag cascades to every registry.**

The tool lives in the engine repo (it orchestrates the siblings but belongs to
the coordinator). It operates on the repos registered in `registry.tsv`.

## USER INSTRUCTIONS — one-time registry wiring

Run each step ONCE; after that the cascade (bottom of this file) is the whole
publish pipeline. Nothing below is needed for CI to stay green — only for the
registry-publishing steps of a release.

1. **crates.io — the engine crate `corvid-db`** (token secret):
   - create a token at <https://crates.io/settings/tokens>
     (scope: *publish-update*), then:
   - `gh secret set CARGO_REGISTRY_TOKEN -R corvid-db/corvid`
     (paste the token at the prompt)
2. **npm — corvid-js** (token secret):
   - create an **Automation** token at
     <https://www.npmjs.com/settings/&lt;user&gt;/tokens>, then:
   - `gh secret set NPM_TOKEN -R corvid-db/corvid-js`
   - (corvid-node's `NPM_TOKEN` is ALREADY set — 0.3.2 was published with it;
     no action needed there.)
3. **PyPI — corvid-python** (trusted publishing, NO token):
   - sign in at <https://pypi.org>, then **Account settings → Publishing →
     Add a new pending publisher**, with exactly:
     - PyPI project name: `corvid-python`
     - Owner: `corvid-db`
     - Repository: `corvid-python`
     - Workflow name: `release.yml`
     - Environment name: *(leave blank)*
4. **pub.dev — corvid_dart** (trusted publishing, NO token):
   - pub.dev automates only EXISTING packages, so publish the first version
     manually once: remove `publish_to: none` from `pubspec.yaml`, commit,
     `dart pub publish` (browser login);
   - then on pub.dev open the package's **Admin → Automated publishing**
     page and set:
     - Repository: `corvid-db/corvid-dart`
     - Tag pattern: `v{{version}}`
     - Workflow: `release.yml`
     - Environment: *(leave unset)*
5. **Packagist — corvid-php**: already linked (its GitHub integration syncs
   each pushed tag; nothing to build, nothing to configure).

## The release cascade (one engine tag → every registry)

```
engine tag vX.Y.Z pushed
        │
        ▼
engine .github/workflows/release.yml
        platform binaries + FFI artifacts on the GitHub release,
        then `cargo publish -p corvid-db` to crates.io
        (CARGO_REGISTRY_TOKEN; idempotence guard via the crates.io API)
        │
        ▼
scripts/bindings/bump.sh --check            # (optional) drift audit first
scripts/bindings/bump.sh vX.Y.Z             # one PR per binding: pin bump +
        │                                   # package-version parity (registry
        ▼                                   # repos) — golden CI is the verdict
scripts/bindings/bump.sh --merge-when-green vX.Y.Z
        │                                   # polls each PR's checks (gh pr
        ▼                                   # checks), squash-merges on green
scripts/bindings/bump.sh --release-after-merge vX.Y.Z
        │                                   # verifies each base branch pins
        ▼                                   # the tag, tags every repo vX.Y.Z,
                                            # pushes, prints run URLs
registries live:
  corvid-db (crates.io)   published by the engine release workflow above
  corvid-node  → npm      release.yml (5-target napi matrix; NPM_TOKEN)
  corvid-js    → npm      release.yml (wasm-pack build; NPM_TOKEN; npm view verify)
  corvid-python→ PyPI     release.yml (maturin wheels+sdist; trusted publishing)
  corvid_dart  → pub.dev  release.yml (dart pub publish; OIDC trusted publishing)
  corvid-php   → Packagist (the tag itself; auto-synced, nothing builds)
  c/go/cpp/zig/jvm → tagged for pinning (git/artifact consumers, no registry)
```

`--release-after-merge` is deliberately the ONLY thing that creates those
tags — nothing in CI or the bump PRs ever tags a binding. It refuses (and
reports, without touching the repo) any binding whose bump PR is still open,
whose base branch doesn't pin the tag yet, or whose registered package
version doesn't match the tag. Re-running it is safe: existing tags are kept
(ALREADY-TAGGED) and every publish workflow carries its own idempotence
(crates.io API guard, `--skip-existing`/`npm view`, pub.dev tag check).

Every release workflow also has a manual **dry_run** dispatch (Actions tab →
Release → Run workflow → `dry_run`): the full build + verification runs and
only the publish/upload step is skipped — the way to test workflow edits
without publishing anything.

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

## Usage

```
bump.sh NEW_TAG                    the bump: clone, verify pins, substitute,
                                   set the package version (registry repos),
                                   branch bump/NEW_TAG, commit, push, PR,
                                   print a table of PR URLs
bump.sh --check                    read-only audit: current pin per repo vs the
                                   engine's latest tag (clones, changes nothing)
bump.sh --dry-run NEW_TAG          full substitution + version set in a temp
                                   clone; prints the exact diff it would make;
                                   pushes nothing
bump.sh --merge-when-green [TAG]   poll open bump/* PRs' checks; merge on green
bump.sh --release-after-merge NEW_TAG
                                   the cascade: after every bump PR merged,
                                   verify pins + versions, tag each repo
                                   vX.Y.Z, push, print the fired workflow run
                                   URLs (tag permalinks for pin-only repos)
bump.sh --dry-run --release-after-merge NEW_TAG
                                   the cascade's dry form: every verification
                                   runs and the table shows which repos WOULD
                                   be tagged — nothing is tagged or pushed
bump.sh --repo ORG/NAME [...]      restrict any mode to a subset of the registry
bump.sh --timeout M --interval S   merge-when-green polling bounds (60m / 30s)
```

All modes refuse a repo whose pins are **inconsistent within the repo** (e.g.
`fetch.sh` at v0.2.1 but `fetch.ps1` at v0.2.0) and report it instead of
bumping; other repos continue. Bumps never go backwards (downgrades are
refused — of the pin AND of a registered package version), re-running against
a tag a repo already pins is a no-op, and the plain bump and
`--release-after-merge` modes require the tag to exist on the engine remote.

## Registry format

`registry.tsv`, one line per binding (TAB-separated, three or four fields):

```
repo<TAB>pin-file-globs<TAB>base-branch[<TAB>version-file]
```

- **repo** — GitHub `ORG/NAME` of the binding (ssh clone/push target).
- **pin-file-globs** — space-separated globs, relative to the binding repo
  root, matching every file that pins or references the engine tag.
- **base-branch** — the branch `bump/vX.Y.Z` is cut from and the PR base.
- **version-file** (optional, 4th field) — the binding's OWN package-version
  file (`package.json` for node/js, `pyproject.toml` for python,
  `pubspec.yaml` for dart). The bump PR sets its version to the tag's bare
  `X.Y.Z` (release parity: registry bindings publish at the engine's
  version), and `--release-after-merge` refuses to tag a repo whose version
  file doesn't match the tag. Registry-less repos omit the field.

Current rows: `corvid-db/corvid-c` (`fetch.sh`, `fetch.ps1`, optional
`.engine-pin`, `README.md`, `docs/PLAN.md`),
`corvid-db/corvid-node` (same shape as corvid-c, plus
`Cargo.toml`/`Cargo.lock` pins and `package.json` version),
`corvid-db/corvid-python` (pins like corvid-node, version `pyproject.toml`),
`corvid-db/corvid-go` (same shape as corvid-c),
`corvid-db/corvid-js` (pins like corvid-node, version `package.json`),
`corvid-db/corvid-cpp` (same shape as corvid-c),
`corvid-db/corvid-zig` (same shape as corvid-c),
`corvid-db/corvid-dart` (pins like corvid-c, version `pubspec.yaml`),
`corvid-db/corvid-php` (same shape as corvid-c — no version file:
Packagist derives the version from the pushed tag), and
`corvid-db/corvid-jvm` (same shape as corvid-c; the JNI shim build
scripts `scripts/build-native.sh`/`.ps1` ride along but pin nothing;
Maven publishing stays trigger-deferred).

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
  in a repo must agree on exactly one tag. (A binding's own package-version
  line — `"version": "0.3.2"`, `version = "0.3.2"` — never votes: it has no
  `v`.)
- **The substitution is purely textual**: every occurrence of the old pin tag
  across the registered globs becomes the new tag — including prose references
  in README/PLAN ("the current pin", "vendored from the vX.Y.Z release").
  Occurrences of *other* (historical) tags are never touched, so
  defect-write-up history like "the v0.2.0 darwin dylib defect" survives a
  bump verbatim.
- **The version-file set is surgical**: only the FIRST version declaration in
  the registered file is rewritten (the top-level package version; dependency
  tables and lock files are never touched). For corvid-node, `napi
  pre-publish` re-syncs the platform-package versions and the root's
  `optionalDependencies` at publish time — the bump PR deliberately does not
  chase those.
- **Caveat, by design:** prose that mentions the *old pin* in a historical
  sense ("resolved in v0.2.1") does get rewritten to the new tag. That is the
  cost of pure-textual substitution; review the `--dry-run` diff before a real
  bump — it prints exactly what will land in the PR.
- **Cargo.lock:** the `?tag=vX.Y.Z` in the git source line is substituted; the
  `version = "X.Y.Z"` line and the `#rev` hash are not (no `v`, and a rev
  cannot be computed textually). Cargo re-resolves the lock on the first build
  — the golden CI builds without `--locked`, so this is fine — and a
  maintainer's first local `cargo update -p corvid-db` refreshes the committed
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
    package.json                     package version 0.2.1 (release parity with engine)
```

Requirements: bash, `git`, `gh` (authenticated), ssh push access to the
binding repos. `shellcheck`-clean; POSIX-bash compatible (runs on stock
macOS bash 3.2).
