# Session handoff — the durable state of everything (2026-09-03, post mobile-platform program)

Written as the compaction anchor. The repos are the primary record;
this file is the map onto them. Session conventions: SDD machine
(implementer → reviewer → fix round; spec review gates before code)
per superpowers; gates = fmt, clippy `--all-targets --workspace -- -D
warnings`, `cargo test --workspace`, rustdoc `-D warnings`; RED-first
bugs; docs-stay-true (DESIGN decision rows, CHANGELOG, docs site);
user rulings override mine; mimosa hook notes are known noise
(incomplete scans + the target/doc false positive; it also blocks
bash heredocs/echo/sed/chmod touching source-shaped paths — Write/
Edit for files, no script filenames in commit messages) — never
blocks, never claim project safety.

## The org (github.com/corvid-db) — 13 repos, all green

| Repo | Role | State |
|---|---|---|
| corvid | engine + corvid-ffi (typed C ABI, 124 symbols, FFI.md contract) + corvid-mcp | **v0.4.1**: mobile platform artifact sets joined every release — Android cdylib tarballs (aarch64/x86_64-linux-android, NDK r28b API 24, scripts/release/build-android.sh) + `corvid-swift-<tag>.zip` (CorvidEngine.xcframework: iOS device + fat sim + fat macOS staticlib slices, umbrella + MODULE.MAP — SwiftPM forms the clang module ONLY with an explicit module.modulemap; scripts/release/build-apple.sh). corvid-ffi crate-type now cdylib+staticlib. CI mobile jobs cargo-check corvid-ffi for all 7 mobile targets. crates.io `corvid-db` 0.4.1 live. Release dry-run (workflow_dispatch) validates the mobile legs. |
| corvid-swift | Swift/Apple binding — NEW (the mobile program's second half) | **v0.4.1 LIVE on SPM** (the tag is the release; consumer-verified: scratch package resolving the git tag fetched the xcframework and ran a query). SPM product Corvid over binary target CorvidEngine (URL tag + sha256 = the pin; cascade refreshes the checksum from the new zip's bytes — refresh_swift_checksum in bump.sh, proven against v0.4.1). No shim — Swift calls the 124 frozen C symbols via the clang module. Golden suite ported IN FULL (8 fixtures, 267/267 through the DOWNLOADED xcframework) + DeepCheck (admin/graph/geo/scan-aborts/schema/TTL/aggregates) + frozen error table + surface gate (331 constructs: 172 mapped, 159 N/A — same shape as Kotlin). CI: swift test (macos), iOS-Simulator compile leg, surface gate. Release gate: tag == .engine-pin == manifest URL version AND checksum == zip bytes == engine checksums.txt. Swift traps learned: Foundation exports Predicate<…> (public typealias CorvidPredicate is the escape hatch); [Float] casts covariantly to [Any?] (vectors need `as [Float]` in document literals — README documents); Rows/GeoHits/Strs/GroupIter are Sequence+IteratorProtocol (Array(x) ambiguous — materialize via for-loop; single-pass); wrapper Value owns the free (manual corvid_value_free on a wrapped handle = double-free abort). |
| corvid-jvm | Kotlin/JVM + **Android** | **v0.4.1 on Central: corvid-jvm (jars+classifiers) AND corvid-android (the AAR — same Kotlin sources via srcDir against android.jar, AGP singleVariant(withSourcesJar/withJavadocJar) — hand-rolled Jar tasks attached via artifact() COLLIDE with the component; jni/<abi>/ pairs from scripts/build-native-android.sh (engine android tarballs sha256+golden-verified + NDK shim; ART is JNI 1.6 — the shim requests 1_6 under __ANDROID__)). Corvid.load() Android branch: System.loadLibrary("corvid")+"corvidjni" over nativeLibraryDir (NativeLoading.onAndroid via java.vm.name — pure java.*). minSdk 26. Consumer-verified FROM CENTRAL on the arm64 ATD emulator (insert/vector/phrase 1/0); the emulator recipe (aosp_atd arm64-v8a AVD) is PLAN.md's local device gate — deliberately NOT a CI leg (flake factories). android/ is a separate Gradle build (root stays pure-JVM); staging shared (one Central bundle: deployment corvid-jvm-android-vTAG). |
| corvid-node | Node.js binding | **v0.4.1 LIVE on npm** (all six packages: the facade + every corvid-node-<platform>; dist-tag latest=0.4.1; optionalDependencies wired). The four-cycle silent failure (every tag since v0.3.4, E404-masked on the first platform package) was npm-side trusted-publisher config — the user refreshed all six packages' settings on npmjs.com and a rerun of the v0.4.1 publish went green (the diagnosis held: the workflow at the tag was already the fixed explicit-npm-publish shape with the verify-every-package step). |
| corvid-dart | Dart binding | v0.4.1 LIVE on pub.dev as **`corvid`** (the pubspec name IS the product name — NOT corvid_dart). The release verify step queried corvid_dart (always 404, failing runs whose publish HAD landed); fixed to query `corvid` with a plain jq select — verified against the live registry. |
| docs | Starlight site | Current through v0.4.1 incl. the **v0.4.1 snapshot** (releases/vX.Y.Z branch + snapshot.yml; the branch must be created first — `git push origin master:releases/vX.Y.Z`). New corvid-swift page; jvm page carries the AAR section; overview twelve live; banner lists v0.4.1; reference pages re-stamped (no engine API change). |
| corvid-c / corvid-node / corvid-python / corvid-go / corvid-cpp / corvid-zig / corvid-php / corvid-js | the ABI bindings | all at v0.4.1 via the cascade; npm corvid-js / PyPI / pub.dev / Packagist live |

## The mobile-platform program — COMPLETE (2026-09-03)

Engine v0.4.1 (additive: android cdylib sets + xcframework zip,
FFI_VERSION still 1) → corvid-android AAR (rides corvid-jvm's repo,
Central, consumer-verified on emulator) + corvid-swift (own repo, SPM,
golden 267/267, consumer-verified from the git tag). Everything proven
locally first (this machine: Xcode 26.6, rustup mobile targets, NDK
r28b at ~/Library/Android/sdk/ndk, ATD arm64 emulator image), then in
CI (release dry-run validated the mobile legs before the tag).

## The docs-depth program — COMPLETE (2026-09-03)

The binding pages are now full per-language guides without hand-written
redundancy: every page embeds ALL SIX tour programs (extended
sync-binding-examples — 66 files across 11 repos, each carrying
docs:begin/end markers, drift-gated by verify-sync), an API-at-a-glance
table folded from each repo's docs/SURFACE.tsv (new sync-api-glance.sh
+ gen-api-glance.mjs, gated the same way — 71–86 API groups per
binding), and an API-reference section pointing at the ecosystem-native
docs (pub.dev/PyPI/pkg.go.dev/SPI/.d.ts/headers; corvid-jvm got a Dokka
site — corvid-db.github.io/corvid-jvm, docs.yml workflow, Pages enabled
via API with build_type=workflow). corvid-swift gained its own
six-example tour (executable targets, CI leg) so the org invariant
holds for all 11. Gotchas encoded: pkg.go.dev is bot-gated — the module
is in proxy.golang.org but the pkg.go.dev page indexes on the first
HUMAN visit of the link; SPI likewise indexes on first visit; macOS
checkout exec-bits drift out of the git index on `git add -A` — CI
steps bash-invoke scripts rather than exec them.

## Org homepage + site truth pass — COMPLETE (2026-09-03)

The `.github` repo (local clone: /Users/rocky/www/org-dot-github) now
carries the org profile README at **profile/README.md** — NOT the repo
root (that's the user-profile convention; ORG profiles read
`.github/profile/README.md`, which is why the first attempt at the root
never rendered). Redesigned modern: centered hero + for-the-badge row,
live registry version badges (flat-square), 13 language logo chips,
features table, collapsible Proof/Where-to-go details; render verified
in-browser. Site pointers now truthful: org website + corvid repo
homepage → corvid-db.github.io/docs/; the stale v0.1-era landing page in
engine `site/index.html` (it still pointed at the pre-org personal repo
i-rocky and git-dependency install) is now a redirect to the docs site —
engine pages.yml keeps rebuilding **rustdoc at /corvid/api/** (READMEs
and docs pages deep-link it; that URL is load-bearing). Docs truth pass:
corvid-c's file-inventory section ("What's inside") removed; all
present-tense artifact pins bumped v0.3.2→v0.4.1 (c, go, dart, cpp, php
+ "current is" wording); SURFACE prose 327→331 rows (dart/php/jvm/zig +
cpp's mapped/N-A split now 180/151); cpp raii "145 checks"→157 (verified
by rebuilding test/raii.cpp at master). All SURFACE.tsv files are 331
rows at v0.4.1; per-repo MAPPED/N-A splits differ legitimately
(cpp/c/zig 180/151, php 179/152, js 176/155, dart 173/158, others
172/159). corvid-js's golden self-asserts 230 lines (its own subset —
not drift; its CI is green on it).

## Release pipeline — now with verdict verification

Engine tag → release.yml (desktop + android + apple legs; dry_run
dispatch for validation) → `bump.sh vX.Y.Z` (waits for assets; PRs to
all 11 registered bindings incl. corvid-swift — Package.swift URL +
checksum + .engine-pin + README together; bare Maven/Gradle README
coordinates for jvm+android) → `--merge-when-green` →
`--release-after-merge` **now polls every fired release run to
completion and FAILS on red verdicts** (the corvid-node lesson: four
cycles of silently-failed npm publishes hid behind green tag pushes —
"fired" is not "published"). Known-manual per cycle: NEW engine
constructs need SURFACE.tsv rows in EVERY binding (swift's is in
place). Docs snapshot per release WITH docs changes.

## Small truths worth keeping

- The frozen error table: 20 codes 0..19, BUSY(19) is FFI-only;
  CorvidErrorCode is CaseIterable; ErrCodesTest pins it in jvm+swift.
- AAR jniLibs land under `jni/<abi>/` inside the archive (the jniLibs
  SOURCE dir name differs); package manager installs them into
  nativeLibraryDir; DT_NEEDED libcorvid.so + SONAME make the pair
  resolve there.
- AGP timing: publications live in afterEvaluate (the release
  component + metadata tasks are born there); `tasks.named` on them
  at configuration time fails — matching+configureEach if ordering is
  ever needed.
- Maven Central: same io.github.corvid-db namespace serves jvm and
  android from one bundle/upload (deployment corvid-jvm-android-vTAG);
  fail-on-existing-checksums=false and maven-metadata stripping
  unchanged.
- pub.dev's package is `corvid`; PyPI's is `corvid-python`; npm's are
  `corvid-js` + (blocked) `corvid-node` + platform packages; Packagist
  is `corvid/php-corvid`.
- The user: decisive, wants full completion not deference, catches
  wrong claims — verify primary sources before instructing them.
