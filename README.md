# drvjs

A Rust CLI that turns a pnpm lockfile and project manifests into a Nix-built
`node_modules` tree. Supports workspace dependency subsets. Early prototype,
licensed CC0.

Each npm tarball is a separate `fetchurl` derivation with the lockfile's SRI
hash. Nix reuses unchanged downloads locally or through a binary cache.
Assembly is one derivation per exported selection; it is not yet a derivation
per installed package or workspace.

## Try it

Run directly from GitHub with `nix run github:ryantm/drvjs -- --help`, or
use the commands below from a checkout.

```sh
# Whole project or workspace
nix run . -- export -C /path/to/project -o /tmp/project-deps
nix-build /tmp/project-deps -o result

# One workspace package, including its workspace and registry dependencies
nix run . -- export -C /path/to/vue --filter @vue/compiler-sfc -o /tmp/vue-deps
nix-build /tmp/vue-deps -o result

# Select a union; explicitly add root tools when needed
nix run . -- export -C /path/to/vue \
  --filter packages/compiler-sfc --filter . -o /tmp/vue-with-tools
```

Filters match exact package names or root-relative importer paths. Repeat
`--filter` to select multiple packages. Dependency closure is always included;
`--filter '@vue/compiler-sfc...'` is also accepted. Other pnpm filter syntax,
globs, dependents, and exclusions are not implemented. A missing or ambiguous
selector is an error. Omitting filters selects the whole workspace.

Closure includes dependencies, devDependencies and optionalDependencies of
selected workspaces, plus registry snapshot dependencies (including peer
contexts), optional dependencies, and workspace links. Cycles are supported.
Selecting a child does **not** implicitly install root development tools; add
`--filter .` to request them.

The output contains the selected workspace sources/manifests at their original
relative paths, root and per-workspace `node_modules`, a projected frozen
lockfile, and workspace configuration. The build-time `.pnpm-store` is discarded
by default; installed package files remain usable without it.
Relative workspace links point to sources inside the immutable output. A
synthetic root with no dependencies is used when the root is not selected;
root pnpm settings, resolutions, and engine requirements are retained.
Selected workspace source changes invalidate assembly; excluded workspace
sources and excluded lockfile/catalog entries do not. The project root's sources
are included only when the root is selected. `.git`, existing `node_modules`,
and `.pnpm-store` are excluded from source snapshots.

Use the output tree in a downstream Nix build. Simply moving one workspace's
`node_modules` out of that tree can break its relative workspace links. Lifecycle
scripts and workspace compilation are not run, so TypeScript workspaces may
need a subsequent build before they can be imported as JavaScript.

Exports include the Rust helper's source, Cargo.lock, the projected project,
and pinned Nix expressions. They do not reference the original checkout. No
IFD, dynamic derivations, or recursive Nix are required.

## Nix flake distribution

Versioned releases are published to
[FlakeHub](https://flakehub.com/flake/ryantm/drvjs) and
[Flakestry](https://flakestry.dev/flake/github/ryantm/drvjs).
To consume a release as an input:

```nix
inputs.drvjs.url = "https://flakehub.com/f/ryantm/drvjs/0.1.0.tar.gz";
# GitHub is also supported:
# inputs.drvjs.url = "github:ryantm/drvjs/v0.1.0";
```

Use `drvjs.packages.${system}.default` or `drvjs.packages.${system}.drvjs`.
Outputs are declared for `x86_64-linux` and `aarch64-linux`; ARM64 has not yet
been validated by the integration suite.

Maintainers publish a new version by pushing a `v*.*.*` tag. The
[publication workflow](.github/workflows/publish.yml) checks the flake before
publishing to both registries. It can also retry an existing tag through
`workflow_dispatch`.

## Experimental native assembly

`--native` selects the Rust assembler. It reads the projected lockfile, verifies
and decompresses Nix-fetched tarballs, applies locked patches with `git apply`,
and creates the isolated package layout, workspace links, hoists, executable
shims, and pnpm metadata. **Assembly does not launch pnpm.** Lifecycle scripts
remain disabled; a downstream build must produce any required generated files.

```sh
drvjs export --native -C /path/to/project -o /tmp/native-deps
nix-build /tmp/native-deps -o result

# Independent compatibility check, only when requested:
nix-build /tmp/native-deps --arg keepVerificationStore true -o result-check
drvjs verify result-check
```

An existing export can select this with `--arg nativeAssembler true`.
`--arg unpackedStore /nix/store/…/.pnpm-store` also works: drvjs reads and
integrity-checks cached content without modifying the supplied store. Nix makes
the completed output immutable. Retaining the verification store is optional
and off by default; pnpm writes only to the temporary copy created by `verify`.
Dependency changes require exporting/building a new tree, rather than running
`pnpm add` or `pnpm install` inside the immutable result.

You can verify an output that has no retained store using a compatible existing
store; verification copies that store to its temporary directory:

```sh
drvjs verify result --store /nix/store/…/.pnpm-store
```

Native assembly runs independent package work in a bounded worker pool. It uses
up to four workers by default, bounded by `NIX_BUILD_CORES` when set or
available parallelism otherwise; `DRVJS_JOBS` changes the requested worker count
within that budget. Cached files are integrity-checked in
the same pass that copies them. With tarball inputs and no retained store,
verified archives extract directly into package directories; peer instances
reuse hardlinks. Patches use independent copies, and changing a bin's executable
permissions first detaches its file from shared content.

This is experimental compatibility with pnpm 10.34.5's isolated layout on
Linux, not a replacement dependency resolver. It consumes the locked graph;
hooks are checksum-checked rather than executed, and pnpm remains the authority
for generating/updating lockfiles. Native frozen-configuration validation is
not yet equivalent to every pnpm check (notably package-extension configuration
checksums and engine requirements). Use independent verification before adopting
it for a new project. The pnpm-backed assembler remains the default.

Root `allowedDeprecatedVersions` and `neverBuiltDependencies` are accepted and
preserved: native assembly emits no deprecation warnings and runs no lifecycle
scripts. Frozen validation follows pnpm's dependency-section precedence:
`optionalDependencies` overrides `dependencies`, which overrides
`devDependencies`. Shadowed declarations need no separate importer entry; the
winning declaration must still match the lockfile. Focused regressions for these
behaviors and store reuse are tested against pnpm 10.6.1 as well as the pinned
version; native output metadata and independent verification still target the
pinned pnpm version.

`tests/native.sh` covers tarballs, retained caches, patches, independent offline
verification, Nix reproducibility, and a failing pnpm stub during native assembly.

Native output passed independent pnpm verification for the public Vue compiler-sfc
subset (98 package instances, 6 importers).

The current native path uses libdeflate for gzip archives up to 256 MiB expanded
per worker, with streaming fallback for larger or unusual streams. It traverses
memory-backed tar data with seeks and writes borrowed slices, retaining the tar
parser's support for extended paths. Direct outputs avoid unused per-file store
metadata and reuse parsed manifests. SRI checks precede extraction; retained
verification stores still contain complete pnpm metadata.

### Optional zstd archive cache

For repeated native reconstruction, Nix can normalize downloaded gzip tarballs
into separately cached zstd archives:

```sh
drvjs export --native --zstd-archives -C ./project -o ./export
nix-build ./export -o result
```

The Nix argument `--arg zstdArchives true` also enables this for a native export.
`nix-build ./export -A archiveSources --no-out-link` realizes just its archive
manifest and package cache. The original download is checked by `fetchurl`;
normalization runs in Nix, and assembly checks the normalized payload's SHA-512
before decoding. Prepared manifests are trusted cache metadata, like supplied
pnpm stores. Retained verification stores keep the original pnpm integrity keys.
This option supports both retained stores and external-store verification.

Normalization adds a one-time preparation step and can change the cache size.
Whether it improves repeated reconstruction depends on the packages, CPU and
filesystem. Measure cache preparation separately from assembly. The
[cache preparation prototype](benchmarks/prepare-zstd.py) and
[alternating experiment harness](benchmarks/experiment.py) support comparisons
using your own exported project and already-fetched inputs.

## How it works

1. Rust reads the v9 lockfile, matches selectors, and walks importer links and
   package snapshots. It retains only reachable importers, package resolutions,
   peer-context snapshots, and used catalog entries. Version specifiers,
   integrity hashes and selected dependency edges are preserved. Declared patches
   are pruned with the selection; root settings and the pnpm hook are retained.
2. Nix fetches only retained tarballs and verifies their SRI hashes. Unrelated
   workspace packages are absent from the derivation graph, not merely skipped
   by a later pnpm command.
3. Inside the Nix sandbox, a Rust loopback server serves only Nix-fetched
   tarballs. `pnpm fetch` fills a fresh content-addressed store and materializes
   packages using hardlinks on the output filesystem. The server cannot forward
   requests to the public registry.
4. After the server stops, drvjs moves the prepared virtual store into place.
   `pnpm install --offline --frozen-lockfile --ignore-scripts` reuses those files
   and finishes workspace links, hoisting and executable shims.
5. Nix verifies tarball hashes and pnpm validates integrity during fetching.
   pnpm must leave the projected lockfile byte-for-byte unchanged. The additional
   full installed-file scan (`pnpm store status`) runs only when retaining the
   verification store, and during explicit `drvjs verify`. Volatile timestamps and temporary project
   registrations are normalized or removed for reproducible outputs.

For a single non-workspace project with no filters, the original lockfile bytes
are preserved unless a URL-only resolution needs an integrity hash. drvjs fetches
those URLs during export and records SHA-512 in the exported lockfile. Review and
retain that export to pin the bytes of URLs that have no upstream integrity hash. Workspace exports deliberately contain a **projection**, not a
byte-identical copy of the complete original lockfile. The original checkout
is never edited.

## Optional offline verification

Retaining the verification store is **opt-in** because it keeps another copy of
package contents. To enable it when exporting:

```sh
drvjs export -C /path/to/project -o /tmp/project-deps --keep-verification-store
nix-build /tmp/project-deps -o result
drvjs verify ./result
```

Or override the choice for an existing export without exporting again:

```sh
nix-build /tmp/project-deps --arg keepVerificationStore true -o result-with-store
drvjs verify ./result-with-store
```

`--arg keepVerificationStore false` produces the smaller output even if the
export opted in. Both variants reuse the same Nix tarball fetches. Default builds
still verify fetched tarball integrity and perform frozen installation before
removing the cache. The additional installed-file scan is reserved for the
verification mode. `verify` on a default output explains how to rebuild with the cache.
The optional cache is retained inside that output, not a separate runtime dependency.

`drvjs verify RESULT` copies sources and package-store contents to a temporary
writable directory, reconstructs node_modules offline with pnpm, checks the
store, and compares every root and workspace installation. Comparison includes
file hashes, executable flags, symlink targets and pnpm metadata; only timestamps,
store location, and generated root paths are normalized. The original result
is never modified. This detects installed-file changes that `pnpm store status`
alone would not detect. pnpm 10's `store status` compares installed files with
the unpatched tarball index and reports intentional patches as mutations. drvjs
runs that command against a temporary view excluding patched snapshots; its
offline reconstruction comparison still covers **every patched package**.

## Registries, hooks, and patches

Root `.npmrc` registry mappings and supported install settings are preserved.
Authentication settings are stripped from exports. For private registries,
preload the integrity-checked tarballs into Nix on the host:

```sh
drvjs export -C /path/to/project --filter pkg/app -o /tmp/app-deps
drvjs fetch /tmp/app-deps --npmrc ~/.npmrc
nix-build /tmp/app-deps -o result
```

`fetch` defaults to custom registries; `--all` includes npmjs. Credentials default
to `~/.npmrc`, supporting scoped `:_authToken` and `:_auth`, including `${VAR}`
values. Credentials go to curl through stdin, never into Nix expressions, process
arguments, or store files. Each redirect is checked against credential scopes.
Already imported tarballs are reused without a download. Binary caches containing
private package bytes need the same access restrictions as the packages. For
URL-only packages requiring auth during export, also pass `export --npmrc PATH`.

Overrides (including root `resolutions`), package extensions, peer rules,
architecture settings, hoist settings, and declared patches are retained. Only
patches reachable in a workspace selection are copied. pnpm applies patches and
validates hook/config checksums during frozen installation. The root
`.pnpmfile.cjs` is copied and executes in pnpm; export itself does not execute it.
Hooks must be self-contained or use files included in the selection. Build hooks
run in the Nix sandbox; `verify` runs them locally. Lifecycle scripts remain
disabled. Hook environments are cleared, with `CI=true` and
`PNPM_SKIP_AUTH_CHECK=true`; private-package stub flags are not inherited.

## Real-world validation: Vue

Tested with [Vue v3.5.22](https://github.com/vuejs/core/tree/5a8aa0b2ba575e098cbb63b396e9bcb751eb3a0f),
whose repository declares pnpm 10.17.1. drvjs uses pinned pnpm 10.34.5 with
automatic package-manager switching disabled. Vue's newer main branch uses
pnpm 12 split lockfiles, which are not supported yet.

| Selection | Selected importers | Nix tarball fetches | Installed registry packages on Linux |
|---|---:|---:|---:|
| Whole Vue workspace | 18 | 741 | Not realized in this test |
| `@vue/compiler-sfc` and dependencies | 5 | 108 | 98 |
| `@vue/reactivity` and dependencies | 2 | 0 | 0 |

The compiler subset eliminates 633 of 741 tarball fetches (85.4%). pnpm also
sees an empty synthetic root, so it reports six projects for the five selected
compiler workspaces. These are dependency-count measurements, not timing claims.

Validation includes actual sandboxed builds of both subsets, fresh offline pnpm
reconstruction, working local links, Babel parser/PostCSS imports, Nix `--check`
rebuilds, stale-manifest rejection, and unchanged derivation identity after
excluded source/manifest/lock/catalog edits. A selected-source edit changes
assembly while preserving all tarball identities. Vue's own compilation and test
suite were not run.

The test is reproducible:

```sh
# In the project development environment, with drvjs on PATH:
bash tests/vue-workspace.sh
# Or reuse an existing checkout at the exact pinned revision:
bash tests/vue-workspace.sh /path/to/vue-v3.5.22
```

## Materialization performance

The assembler reuses the virtual store prepared by `pnpm fetch`, uses hardlinks
on the output filesystem, and reserves the additional installed-file integrity
scan for verification mode. It removes fetch's provisional current-lock and hoisting metadata before
installation so all executable shims match a fresh pnpm install. Tarball
integrity and frozen-lockfile checks remain enabled. Nix prefers local assembly
to avoid transferring a large completed tree back from a remote builder;
binary-cache substitution remains available.

`benchmarks/materialize.py` runs alternating assembler-only comparisons with
existing project and tarball-manifest inputs. Downloads, export and Nix
realization overhead should be measured separately.

### Experimental unpacked-store reuse

A retained pnpm store can seed another assembly without fetching or
decompressing tarballs. This is opt-in:

```sh
# Prepare the reusable store once; this also builds the initial node_modules.
cache=$(nix-build ./export --no-out-link --arg keepVerificationStore true)
# Reuse it for this export or another export with compatible dependencies.
nix-build ./export --no-out-link --arg unpackedStore "$cache/.pnpm-store"
```

The new output still omits its verification store unless
`--arg keepVerificationStore true` is also supplied. The seed remains a separate,
large Nix output; retain a GC root if you want to keep it. This prototype reuses
an entire retained store, rather than producing independent unpack derivations
for each package.

Nix store files stay immutable. drvjs creates writable directories and copies
the indexes, referencing cached contents through private symlinks. Only contents
belonging to patched packages get writable copies before installation. pnpm
imports regular files using copy mode, preserving Node module resolution, and
runs its frozen offline install with integrity checks enabled. The input cache
remains unchanged. Opt-in retained verification stores materialize the temporary
links so that they remain self-contained. Missing
packages fail offline rather than triggering downloads; regenerate the seed
when dependencies require it. Use trusted caches produced with the pinned pnpm
version and compatible project settings.

Native store reuse reads the base index's `files` map and ignores `sideEffects`
overlays. The base files must be pristine so that each locked patch can be
applied once. Before patching cached content, drvjs checks the patch preimage.
Already-patched or otherwise incompatible content is rejected with the package,
patch and recovery instructions; patch failures are never treated as success.
These checks leave the supplied store unchanged. They do not establish why an
external store's base files differ from the original tarball.

To regenerate a compatible store from the locked downloads:

```sh
nix-build ./export --arg unpackedStore null --arg keepVerificationStore true -o fresh
# A later build can use fresh/.pnpm-store as its unpackedStore.
```

`benchmarks/unpacked-store.py` compares this path with cached tarballs using
fresh outputs and alternating trials. `tests/unpacked.sh` covers independent
pnpm verification, patches/hooks, Nix reproducibility, optional retention, and
incomplete-cache rejection. This mode remains opt-in.
The final Nix output is immutable: read-only pnpm commands can inspect it,
but commands that modify dependencies need a writable project/install directory.

## Current limits

- Linux; pinned pnpm 10.34.5, lockfile format 9.0. ARM64 is declared but not tested.
- Registry packages and explicit HTTP(S) tarballs with SHA-256/384/512 integrity.
  HTTPS is required except for loopback test registries. URL-only resolutions are
  hashed during export; Git dependencies remain unsupported.
- Workspace `link:` dependencies must point to importers inside the project.
- Supports workspace catalogs, root pnpm settings, hooks, patches and scoped
  private registries as described above. Unknown settings and repeated array `.npmrc`
  keys fail explicitly; scalar keys use the last value; nested `.npmrc`/hooks in selected workspaces are not supported.
  Configuration in excluded workspaces does not block subset export.
- Git/local-file dependencies, injected workspaces and source symlinks are rejected.
- Registry URLs containing credentials, query strings or fragments are rejected.
  Authenticated fetches require host access before a sandboxed build.
- All retained tarballs are fetched; pnpm filters platform-specific packages
  during installation. Native modules needing postinstall may not work yet.
- Large graphs can exhibit pnpm private-hoisting nondeterminism. Exact offline
  comparison and Nix `--check` can report differences between otherwise equivalent
  installs; drvjs does not resolve upstream hoisting nondeterminism.
- Root tools are opt-in for subsets. Full pnpm filter grammar and production-only
  selection are future work.

## Development

`devenv.nix` declares Rust, Node, pnpm and Nix for this project.

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
nix build
PATH="$PWD/result/bin:$PATH" bash tests/integration.sh
PATH="$PWD/result/bin:$PATH" bash tests/vue-workspace.sh
PATH="$PWD/result/bin:$PATH" bash tests/configured.sh
PATH="$PWD/result/bin:$PATH" bash tests/unpacked.sh
PATH="$PWD/result/bin:$PATH" bash tests/native.sh
PATH="$PWD/result/bin:$PATH" bash tests/native-compat.sh
PATH="$PWD/result/bin:$PATH" bash tests/native-store-patches.sh
PATH="$PWD/result/bin:$PATH" python3 tests/registry.py
```

Set `PNPM_COMPAT_BIN=/path/to/pnpm-10.6.1/bin/pnpm.cjs` for the two native
compatibility scripts to exercise that exact release. The store-patch script
downloads the public `is-number` fixture, checks ordinary pnpm-store reuse, then
deliberately constructs an incompatible base index and verifies safe rejection.

The smaller integration fixture covers aliases, scoped packages, peer contexts,
transitive dependencies, CLI shims, platform-specific optional dependencies,
offline verification, reproducibility, and tamper detection. The Vue integration
test covers real workspace projection and cache isolation.

## License

[CC0 1.0 Universal](LICENSE) (`CC0-1.0`): dedicated to the public domain.
