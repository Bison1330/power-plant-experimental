# Generating real weights for pallet-vitreus-dex, pallet-launchpad and pallet-launch-treasury

The pallets in this workspace ship `weights.rs` files in the
frame-weight-template layout; the DEX's and the launchpad's are still the
placeholder constants, the treasury's were measured on a `c-16` on
2026-09-16 (its header says where and how). This is the runbook for
replacing any of them with measured weights. It lives here, beside the
pallets, because everything it checks — the benchmark counts, the function
signatures, the storage each call touches — is a fact about these crates;
the one thing it needs from elsewhere is a runtime that wires them, which
§2 parameterises. Do not run it on the development box:
2 vCPU shared with a live node produces numbers that are wrong by an unknown
factor, and wrong weights are worse than placeholders because they look
authoritative.

## 1. Hardware

Weights are only meaningful relative to the hardware the chain is expected
to run on. Polkadot's reference hardware is 8 physical cores at ≥ 3.4 GHz,
32 GB RAM, NVMe; `benchmark machine` (step 4) scores a box against it.

DigitalOcean **CPU-Optimized, dedicated vCPU**, Ubuntu 24.04 x64:

| Size | Use |
|---|---|
| `c-16` (16 vCPU, 32 GB) | recommended — the release build with `runtime-benchmarks` is the slow part (~25–35 min here vs 90+ on c-8); benchmarks themselves are single-threaded |
| `c-8` (8 vCPU, 16 GB) | works; roughly 2× the build time |

Do not use shared-CPU (Basic / Premium) droplets; steal time shows up
directly in the numbers. Nothing else should run on the box while
`benchmark pallet` runs. Budget: ~2 hours on `c-16`.

## 2. Provision and build

```bash
# as root on the droplet
apt update && apt install -y build-essential clang libclang-dev llvm protobuf-compiler \
  pkg-config libssl-dev git curl cmake
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"

# The consuming runtime: whichever power-plant branch wires these pallets,
# at the commit whose Cargo.lock pins the experimental SHA you are
# measuring. Today that is the fork's `design/launch-treasury`
# (git@github.com:Bison1330/power-plant.git); once #100 consumes a pinned
# SHA it is the Foundation's branch that carries the pin. Never a branch
# that vendors its own copy of a pallet — the weights would describe code
# this repo does not have.
git clone <consumer> power-plant && cd power-plant
git checkout <branch>
git rev-parse HEAD > /tmp/bench-commit           # the consumer commit measured
grep -A1 'pallet-launch-treasury' Cargo.lock | grep -o 'rev=[0-9a-f]*' | head -1 >> /tmp/bench-commit   # the pallet SHA measured
rustup show                                       # picks up rust-toolchain.toml (1.83 + wasm32)

# These pallets are testnet-only, so the benchmarking node must carry the
# testnet native runtime. All three are measured from this one binary.
cargo build --release --locked --features testnet-native,runtime-benchmarks
ls -la target/release/vitreus-power-plant-node
```

`--locked` matters: the checked-in `Cargo.lock` is what the runtime was
verified against, and it is what names the experimental SHA the weights
belong to.

### 2.1 The runtime's two benchmark lists — read this before rebasing

The runtime defines its benchmark set twice, one `mod benches` per network
feature (`runtime/vitreus/src/lib.rs`, the two `define_benchmarks!` blocks
near the end). Upstream `bench/repairs` (daa14f2) gates them explicitly:

```rust
#[cfg(all(feature = "runtime-benchmarks", feature = "mainnet-runtime"))]
mod benches { define_benchmarks!( [frame_system, …] [pallet_evm, EVM] [pallet_treasury_extension, …] ); }

#[cfg(all(feature = "runtime-benchmarks", feature = "testnet-runtime"))]
mod benches { define_benchmarks!( …the same three… ); }
```

Upstream the two lists are identical, so which block a build takes does
not matter and the commit that changed the gates was safe. **The wiring
for these pallets is exactly what makes them diverge**: the fork adds
`[pallet_vitreus_dex, VitreusDex]`, `[pallet_launchpad, Launchpad]` and
`[pallet_launch_treasury, LaunchTreasury]` to the testnet block only,
because the pallets are not in the mainnet runtime. Three things follow
for whoever rebases that wiring onto `bench/repairs` or its descendants:

- The three entries go in the `testnet-runtime` block and nowhere else. A
  merge that lands them in the `mainnet-runtime` block fails to compile
  (no `VitreusDex` in that runtime) — good, it is loud. A merge that
  drops them from the testnet block compiles and silently benchmarks
  nothing: §3's count is the check.
- The fork's gates were `testnet-runtime` / `not(testnet-runtime)`;
  upstream's are `testnet-runtime` / `mainnet-runtime`. Take upstream's.
  The difference is only that a build with *neither* network feature no
  longer has a `benches` module at all (`list_benchmarks!` will not
  resolve); that configuration also gets no wasm binary (lib.rs, the two
  `include!` lines at the top), so nothing runs it.
- `benchmark pallet` measures whatever list the binary was built with.
  If §3 shows fewer than 39 lines for the three pallets, the block is
  wrong, not the pallets.

## 3. Sanity: the benchmark list

```bash
./target/release/vitreus-power-plant-node benchmark pallet --chain dev --list \
  | grep -E '^pallet_(vitreus_dex|launchpad|launch_treasury),'
```

Expect 20 lines for `pallet_vitreus_dex` (create_pool … set_solver_bond_amount,
then set_default_fee_routing, set_protocol_fee_recipient,
claim_pool_creator_fees, withdraw_protocol_fees), 11 for
`pallet_launchpad` (create_launch, buy, buy_crossing, sell, graduate,
claim_creator_fees, set_creator_fee_recipient, set_params,
set_creation_paused, force_seed_into_existing_pool, set_launch_metadata —
ten calls plus `buy_crossing`, the crossing branch of `buy`) and 8 for
`pallet_launch_treasury` (stake, retarget, harvest, compound, retire,
finalize_retirement, set_terms, set_targets): 39 in all. If a pallet is
missing, the binary was built without `testnet-native` or without
`runtime-benchmarks`, or its entry fell out of the testnet block (§2.1).

## 4. Score the machine

```bash
./target/release/vitreus-power-plant-node benchmark machine --chain dev \
  --disk-duration 30 --allow-fail 2>&1 | tee /tmp/benchmark-machine.txt
# (--allow-fail only turns a below-reference score from an error into a
#  warning so the report still prints; a warning still means stop.)
```

Every row should be ≥ the reference score. A CPU row well below reference
means a shared or throttled box — stop and re-provision. Keep this file; it
goes back with the weights.

## 5. Generate

```bash
COMMON="--chain dev --wasm-execution compiled --steps 50 --repeat 20 --heap-pages 4096 \
        --template <experimental>/.maintain/frame-weight-template.hbs"
# The template is this repo's (.maintain/); power-plant carries an
# identical copy on bench/repairs. Either produces committed-shape output;
# if they ever differ, this repo's is the one the pallets' headers match.

./target/release/vitreus-power-plant-node benchmark pallet $COMMON \
  --pallet pallet_vitreus_dex --extrinsic '*' \
  --output pallets/vitreus-dex/src/weights.rs \
  --json-file /tmp/bench-vitreus-dex.json 2>&1 | tee /tmp/bench-vitreus-dex.log

./target/release/vitreus-power-plant-node benchmark pallet $COMMON \
  --pallet pallet_launchpad --extrinsic '*' \
  --output pallets/launchpad/src/weights.rs \
  --json-file /tmp/bench-launchpad.json 2>&1 | tee /tmp/bench-launchpad.log

./target/release/vitreus-power-plant-node benchmark pallet $COMMON \
  --pallet pallet_launch_treasury --extrinsic '*' \
  --output pallets/launch-treasury/src/weights.rs \
  --json-file /tmp/bench-launch-treasury.json 2>&1 | tee /tmp/bench-launch-treasury.log
```

`--output` paths are the pallet crates in *this* repo, wherever cargo put
them for the consumer build (`~/.cargo/git/checkouts/power-plant-experimental-*/<sha>/pallets/…`
is read-only; write to a clone of this repo and commit here).

`--steps 50 --repeat 20` is the Polkadot convention: 50 points per linear
component (`create_launch` has four, `set_launch_metadata` two, the
treasury's `finalize_retirement` one on the matured chunk count), 20
repeats each. Together the three runs take on the order of 30–50 minutes
on `c-16`. `--output` overwrites the file in place; the template writes
the same `WeightInfo` trait, `SubstrateWeight<T>` and `()` impls the
existing files have, so nothing else in the crate changes.

Optional, same session, and recommended by LAUNCHPAD_SPEC §8.2: the
consumer's other pallets' weights were generated under the old
`AssetId = u32` switch and measured a narrower type than production.
Regenerating them is the same command per pallet, with `--output` in the
consumer; it is a separate commit there.

## 6. Verify before committing

On the droplet, all of these must pass:

```bash
# 1. The generated files compile and every test still passes. Tests that
#    reason about weights (`weights_crossing_buy_refunds_when_not_crossing`)
#    use `<() as WeightInfo>` and are unaffected by the numbers.
# In this repo:
cargo test --workspace --locked
cargo test --workspace --locked --features runtime-benchmarks
# In the consumer, with Cargo.lock pointed at the commit that carries the new weights:
cargo check -p vitreus-power-plant-runtime --features testnet-runtime,runtime-benchmarks
cargo check -p vitreus-power-plant-runtime --features mainnet-runtime

# 2. Every function is present with the right signature.
grep -c 'fn .*-> Weight' pallets/vitreus-dex/src/weights.rs        # 60: 20 in the trait and both impls
grep -c 'fn .*-> Weight' pallets/launch-treasury/src/weights.rs    # 24: 8 × 3
grep -n 'fn create_launch(n: u32, s: u32, d: u32, u: u32)\|fn set_launch_metadata(d: u32, u: u32)' \
  pallets/launchpad/src/weights.rs
```

Then read the numbers, not just the diff:

- **No zero `ref_time` and no zero `proof_size`** on any function; a zero
  means the benchmark's `#[extrinsic_call]` did not run the path.
- **Ordering holds:** `buy_crossing() > buy()`; `graduate()` is close to
  `buy_crossing()` less a plain buy; `create_launch` at maximum components
  > at minimum; `swap_exact_tokens_for_tokens` (measured on the routed
  branch: three transfers, two counters) > `lock_liquidity()`;
  `set_launch_metadata` grows with `d` and `u`. Treasury: `stake`,
  `retarget` and `retire` each contain a `cooperate` over the full target
  list, so all three > `harvest`; `compound` > `harvest` (it harvests,
  then sells and buys); `finalize_retirement` grows with `n`.
- **Components are sane:** the per-byte slopes on `n`, `s`, `d`, `u` should
  be small positive numbers (storage write cost per byte), not large or
  negative. A negative slope means noise dominated — re-run that pallet
  with `--repeat 50`.
- **Reads/writes match the code:** the `// Storage:` comments the template
  emits list every storage item each call touched. `buy_crossing` must show the DEX `Pools`, `LiquidityPositions`,
  `TotalLiquidity` writes (the seed) that `buy` does not;
  `claim_pool_creator_fees` must show `CreatorFeesUnclaimed` read and
  written plus two `System::Account` writes. If a call's list is missing a
  storage item you know it touches, the benchmark setup took a cheaper
  branch than intended.
- **Magnitude vs placeholders:** placeholders are in the 10^7–10^8 ref_time
  range. Measured values 2–10× off in either direction are normal; 100× off
  is a benchmark that measured the wrong thing.

## 7. Copy back and commit

Copy to the repo on the box you commit from:

```
pallets/vitreus-dex/src/weights.rs
pallets/launchpad/src/weights.rs
pallets/launch-treasury/src/weights.rs
/tmp/benchmark-machine.txt      -> kept with the raw output (see below)
/tmp/bench-*.json               -> kept with the raw output; hundreds of
                                   thousands of lines, so not in this repo
/tmp/bench-*.log                -> keep locally; not committed
/tmp/bench-commit               -> goes in the commit message
```

The raw `benchmark pallet` JSON and the `benchmark machine` output are the
provenance for a weights.rs; keep them where the PR that ships the weights
can link to them (the contributor's fork, a gist, or a release asset). The
commit that ships weights states the machine, its `benchmark machine`
summary line, steps/repeat, and both lines of `/tmp/bench-commit` — the
consumer commit and the pallet SHA it pinned — in its body; each
weights.rs already carries the per-extrinsic statistics in its header.
The commit lands in this repo; the consumer then bumps its pin.
Regenerated weights for the consumer's other pallets are a commit there,
not here.

After it lands: the consumer's `spec_version` must be bumped for the
weights to reach a live chain (weights are compiled into the runtime), and
`benchmark overhead` (block and extrinsic base weights, in the consumer's
`runtime/vitreus/src/weights/`) is a separate exercise on the same hardware
if those were never measured either.
