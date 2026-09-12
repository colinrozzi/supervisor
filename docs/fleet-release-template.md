# Fleet release+upgrade template — sketch

Status: SKETCH (2026-09-12). supervisor v0.1.0 is the proven reference; this generalizes
its `.github/workflows/release.yml` + `dist/supervisor-upgrade` into a fleet-reusable pair.
**Lands after** the first real supervised deploy (inbox-under-supervisor), per manager —
so the pattern we template is battle-tested, not just CI-proven. Shape for review now.

## Two artifacts

### 1. A reusable release workflow (`workflow_call`)

Host `fleet-release.yml` once (a shared repo, e.g. `colinrozzi/.github` or `colinrozzi/fleet-ci`).
Each actor repo's `release.yml` becomes a thin caller:

```yaml
# colinrozzi/<actor>/.github/workflows/release.yml
on: { push: { tags: ["v*"] } }
jobs:
  release:
    uses: colinrozzi/fleet-ci/.github/workflows/fleet-release.yml@v1
    with:
      bin: supervisor               # binary name + cargo package (if they differ, add `package:`)
      package_dir: cli              # working-dir of the bin crate (default ".")
      wasm_crate: supervisor        # OPTIONAL: build this wasm32 crate first and embed it
      wasm_embed_env: SUPERVISOR_WASM   # env the bin's build.rs reads to include_bytes! the wasm
      assets: |                     # extra files to attach beyond the bin + SHA256SUMS
        target/wasm32-unknown-unknown/release/supervisor.wasm
    permissions: { contents: write }
```

The reusable workflow does what supervisor's does today, parameterized:
- install rust + `x86_64-unknown-linux-musl` (+ `wasm32-unknown-unknown` if `wasm_crate` set) + `musl-tools`;
- if `wasm_crate`: `cargo build -p <wasm_crate> --release --target wasm32-unknown-unknown`;
- build the bin static-musl (`CC_x86_64_unknown_linux_musl=musl-gcc`, `<wasm_embed_env>` → the wasm path);
- **static-assert**: `file <bin> | grep -q static` or fail (catches static-pie + statically-linked);
- `sha256sum` the bin + assets → `SHA256SUMS`; attach via `action-gh-release` with `fail_on_unmatched_files`.

Actors that ship a plain binary (no wasm) just omit `wasm_crate`/`wasm_embed_env`.

### 2. A generic upgrade wrapper — `fleet-upgrade`

One script, replacing the per-tool `*-upgrade` copies:

```sh
fleet-upgrade <repo> <bin> [extra-asset ...] [--tag TAG]
# e.g.
fleet-upgrade colinrozzi/supervisor supervisor supervisor.wasm
```

Behavior (generalized from `supervisor-upgrade`): gh-download `<bin>` + `SHA256SUMS` (+ extra
assets) from the release (gh-preferred, public-curl fallback), `sha256sum -c --ignore-missing`,
`install -m0755` to `~/.local/bin/<bin>`, spares to `~/.local/share/<bin>/`. Each tool keeps a
thin alias (`supervisor-upgrade` → `fleet-upgrade colinrozzi/supervisor supervisor supervisor.wasm`),
or a tiny per-tool config table the one wrapper reads.

Adopters: website / mesh / pack (new); fold inbox / tickets / theater's ad-hoc upgraders onto it.

## Open questions for review

- Where `fleet-release.yml` lives (`colinrozzi/.github` reusable-workflow repo vs a `fleet-ci` repo).
- Whether `fleet-upgrade` is one script + per-tool aliases, or one script + a config manifest.
- Non-x86_64 / multiple arches — punt until a non-x86_64 fleet container exists.
- Signing beyond SHA256SUMS (cosign?) — probably not for v1; internal fleet, same-owner repos.
