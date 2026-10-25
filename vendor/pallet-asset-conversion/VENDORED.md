# pallet-asset-conversion, vendored

polkadot-sdk's `pallet-asset-conversion` from `polkadot-stable2609`, built
against this workspace's `polkadot-stable2512-8`. The pallet on 2609 carries
the pool fixes and per-pool fees; the rest of the SDK stays on 2512.

## What is here

| Path | Origin | Checked by CI |
|---|---|---|
| `src/`, `README.md` | upstream at the commit in `UPSTREAM`, plus `../pallet-asset-conversion.genesis.patch` | byte for byte |
| `Cargo.toml` | ours: upstream's manifest restated against this workspace | dependency names only |
| `rustfmt.toml` | ours: turns `cargo fmt` off for upstream code | no |
| `UPSTREAM` | ours: repository, tag, commit, path | no (it is the input) |

`scripts/check-vendored.sh` runs as the first CI step. It fails if the tag no
longer resolves to the recorded commit, if `src/` or `README.md` differ from
that commit's with the patch applied, or if `Cargo.toml` names a dependency
upstream's manifest does not (or misses one it does).

The patch is what it takes to configure the pallet on 2512, and nothing
else. The unpatched source compiles on its own; a runtime (or the pallet's
own mock) does not:

- `lib.rs`: 2609 adds a genesis pool config, and with it a
  `MaybeSerializeDeserialize` bound on `AssetKind`. 2512's `NativeOrWithId`,
  the usual `AssetKind`, implements neither `Serialize` nor `Deserialize`, so
  no 2512 runtime using it could set `type AssetKind`. The patch removes the
  genesis config (`GenesisConfig`, its build, `setup_pool_from_genesis`) and
  the bound. Pools are created by extrinsic, as on 2512.
- `mock.rs`: `type AssetIdAllocator = ()` in the two `pallet_assets` configs,
  an item 2512's `pallet-assets` does not have.
- `tests.rs`: the four genesis tests, their setup and the `BuildStorage`
  import.

`Cargo.toml` keeps upstream's version, 32.0.0, its dependency set and its
features, and copies polkadot-sdk's `[workspace.lints]` table, which upstream
inherits. The source is formatted to polkadot-sdk's rules (hard tabs, nightly
options), so `rustfmt.toml` leaves it alone; `cargo fmt --all` would otherwise
rewrite it.

## How it is wired

The root `Cargo.toml` names the pallet at the same git source as every other
SDK crate and patches it by name:

```toml
pallet-asset-conversion = { git = "https://github.com/paritytech/polkadot-sdk", tag = "polkadot-stable2512-8", default-features = false }

[patch."https://github.com/paritytech/polkadot-sdk"]
pallet-asset-conversion = { path = "vendor/pallet-asset-conversion" }
```

A `[patch]` applies only in the workspace that declares it. power-plant
will declare the same patch, pointing at this repository by git and the rev
it pins, so both resolve the pallet to this crate.

## The patch reaches into the SDK in power-plant

A `[patch]` replaces the crate everywhere in the graph, not only where we
depend on it. Here nothing else depends on it. In power-plant on 2512 two
crates do, and both compile against this 2609 crate instead of 2512's own
27.0.0 after power-plant's 2512 bump:

- `staging-xcm-builder` 2512, a normal dependency of power-plant's runtime.
- `vitreus-runtime-common`, power-plant's own crate. `exchange.rs`
  re-exports `pallet_asset_conversion::Swap` and bounds the energy exchange
  on it.

That is safe, for three reasons:

- **The traits both crates use are unchanged.** Outside its tests,
  xcm-builder uses `SwapCredit` and `QuotePrice`, in
  `asset_exchange/single_asset_adapter/adapter.rs`; `vitreus-runtime-common`
  uses `Swap`. All three are defined in `src/swap.rs`, which is
  byte-identical between `polkadot-stable2512-8` and `polkadot-stable2609`.
- **xcm-builder's only `Config` impl for the pallet is in its test mock**,
  behind `#[cfg(test)]`, and is not built as a dependency.
- **power-plant's runtime does not configure the pallet today.** 2609's
  breaking changes are all in `Config` (`LPFee` becomes `Get<Permill>`; new
  `AdminOrigin` and `MaxSwapFee`; per-pool fees) and reach only a runtime
  that configures the pallet.

The runtime does keep two leftovers from an earlier asset-conversion
pallet, since renamed: the `PoolAssets` instance (`pallet_assets::Instance1`,
index 9) and the `py/ascon` `PalletId` with the `AssetConversionOrigin`
account derived from it. Neither is an `impl pallet_asset_conversion::Config`.

The DEX built on this pallet will configure it in power-plant's runtime, on
purpose, against the 2609 `Config`. That runtime gets its own pallet-assets
instance for LP tokens and its own `PalletId`, so it does not reuse
`PoolAssets` or the `py/ascon` account.

What was run to confirm it, on `polkadot-stable2512-8`:

- A crate depending on both `staging-xcm-builder` (25.0.0 at that tag) and
  `pallet-asset-conversion` by git, with this patch pointing at the vendored
  crate. `Cargo.lock` held a single `pallet-asset-conversion`, the vendored
  32.0.0, as the dependency of both. A `[patch]` may replace a crate with a
  different semver-incompatible version; Cargo does not keep 2512's 27.0.0
  alongside it.
- `cargo check` of that crate, so xcm-builder compiled against the vendored
  pallet.

The 17 other SDK crates that depend on the pallet in 2512 (assets-common,
asset-tx-payment, cumulus-primitives-utility, the asset-hub runtimes and the
like) are not in power-plant's runtime graph. If one is added there, it too
builds against this crate, and the same check applies: does it use more than
`src/swap.rs`, or configure the pallet?

power-plant's SDK entries must also use this workspace's source string,
`tag = "polkadot-stable2512-8"`. A `branch` or `rev` there would give it two
copies of every SDK crate.

## Updating

To move to a later upstream tag: set `tag` and `commit` in `UPSTREAM`, copy
that commit's `src/` and `README.md` here, apply the genesis patch (refresh it
if it no longer applies; it should touch only the genesis config), bring
`Cargo.toml`'s version, dependencies and features and the lint table in line
with upstream's, and run `scripts/check-vendored.sh`.
