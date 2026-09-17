# power-plant-experimental

Experimental Vitreus Power Plant pallets for testnet

Workspace for FRAME crates (and optional dev tooling) that may run on **Vitreus testnet** before they are ready to live in [`power-plant`](https://github.com/Vitreus-Foundation/power-plant).

## Policy

- **Not for mainnet** until reviewed, audited as needed, and moved into `power-plant`.
- `power-plant` should depend on this repo via a **pinned git tag or commit**, not a floating branch.
- Breaking changes are expected; pin and bump deliberately.

## Layout

```text
Cargo.toml                       # workspace root; polkadot-sdk named exactly as power-plant names it
Cargo.lock                       # resolves polkadot-sdk to the same commit as power-plant's lock
pallets/vitreus-dex/             # constant-product AMM, LP positions, per-pool fee routing, solver marketplace
pallets/vitreus-dex/SECURITY_AUDIT.md
pallets/launchpad/               # bonding-curve token launches graduating into a locked DEX pool
pallets/LAUNCHPAD_SPEC.md        # the launchpad's design, invariants and failure modes
pallets/REVIEW_2026-09-17.md     # adversarial review of the pallets, with the red tests it produced
.github/workflows/ci.yml         # build, test, benchmarks, try-runtime, fmt, clippy — on every commit
# optional later: thin --dev node / runtime, scripts
```

Each pallet carries its tests, its measured `weights.rs`, its benchmarks (which run inside a
consuming runtime — `power-plant`'s `pallets/BENCHMARKING.md` is the runbook), and its
storage migrations as `VersionedMigration`s, so a crate at any commit is right for a fresh
chain and for one that ran an earlier version.

## Consuming from power-plant

Pin a commit (or a tag once one is cut), never a branch:

```toml
pallet-vitreus-dex = { git = "https://github.com/Vitreus-Foundation/power-plant-experimental", rev = "<sha>", default-features = false }
pallet-launchpad   = { git = "https://github.com/Vitreus-Foundation/power-plant-experimental", rev = "<sha>", default-features = false }
```

Wire crates into the testnet runtime only (`testnet-runtime` / equivalent). A fix to a pallet
is a commit here and a pin bump in the consumer; nothing is patched in a consumer.

## Dependency alignment

`[workspace.dependencies]` names polkadot-sdk as `git = "https://github.com/paritytech/polkadot-sdk",
branch = "stable2407"` — the same strings as `power-plant`'s `Cargo.toml` — and `Cargo.lock` resolves
it to the same commit. Cargo unifies a git dependency between this workspace and a consumer only when
the source strings match exactly; a `rev` or `tag` here against a `branch` there would give the
consumer two `frame-support`s and a type error at every `Config` boundary. When `power-plant` moves
its lock, move this one to the same commit.

## Developing

```bash
cargo test --workspace --locked
cargo check --workspace --locked --features runtime-benchmarks
cargo fmt --all -- --check
```

CI (`.github/workflows/ci.yml`) runs those plus a `try-runtime` feature check and clippy with warnings
denied, `--locked` throughout. `rustfmt.toml` matches `power-plant`'s; with it, the pinned stable
toolchain and `power-plant`'s nightly `fmt --check` produce the same output, so a file formatted here
passes there.

## License

GPL-3.0 (see [LICENSE](LICENSE)).
