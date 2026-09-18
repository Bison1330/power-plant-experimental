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
pallets/launch-treasury/         # a slice of every launch-token trade, staked as one cooperator; yield burns the token
pallets/LAUNCHPAD_SPEC.md        # the launchpad's design, invariants and failure modes
pallets/LAUNCH_TREASURY_SPEC.md  # the treasury's — and §10.12, where pallet code lives and how consumers pin it
pallets/REVIEW_2026-09-17.md     # adversarial review of the pallets, with the red tests it produced
pallets/BENCHMARKING.md          # how weights are measured, inside a consuming runtime; §2.1 is for whoever rebases the wiring
.maintain/frame-weight-template.hbs  # the weights.rs shape; identical to power-plant's copy
.github/workflows/ci.yml         # build, test, benchmarks, try-runtime, fmt, clippy — on every commit
# optional later: thin --dev node / runtime, scripts
```

Each pallet carries its tests, its measured `weights.rs`, its benchmarks (which run inside a
consuming runtime — `pallets/BENCHMARKING.md` here is the runbook, and `.maintain/` the weight
template it uses), and its
storage migrations as `VersionedMigration`s, so a crate at any commit is right for a fresh
chain and for one that ran an earlier version.

## Consuming from power-plant

Pin a commit (or a tag once one is cut), never a branch:

```toml
pallet-vitreus-dex = { git = "https://github.com/Vitreus-Foundation/power-plant-experimental", rev = "<sha>", default-features = false }
pallet-launchpad   = { git = "https://github.com/Vitreus-Foundation/power-plant-experimental", rev = "<sha>", default-features = false }
pallet-launch-treasury = { git = "https://github.com/Vitreus-Foundation/power-plant-experimental", rev = "<sha>", default-features = false }
```

The treasury reaches the runtime's staking and energy broker through two pallet-local traits,
`TreasuryStaking` and `TreasuryExchange`; the consumer implements each with a small adapter over its
own pallets (power-plant's are `EnergyGenerationStaking` and `EnergyBrokerExchange` in its runtime).
No crate here depends on a runtime trait crate.

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
PROPTEST_CASES=5000 cargo test --release -p pallet-launch-treasury --lib fuzz   # the property harness; 32 cases in plain `cargo test`
```

CI (`.github/workflows/ci.yml`) runs those plus a `try-runtime` feature check and clippy with warnings
denied, `--locked` throughout. `rustfmt.toml` matches `power-plant`'s; with it, the pinned stable
toolchain and `power-plant`'s nightly `fmt --check` produce the same output, so a file formatted here
passes there.

## License

GPL-3.0 (see [LICENSE](LICENSE)).
