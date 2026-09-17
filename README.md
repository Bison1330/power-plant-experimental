# power-plant-experimental

Experimental Vitreus Power Plant pallets for testnet

Workspace for FRAME crates (and optional dev tooling) that may run on **Vitreus testnet** before they are ready to live in [`power-plant`](https://github.com/Vitreus-Foundation/power-plant).

## Policy

- **Not for mainnet** until reviewed, audited as needed, and moved into `power-plant`.
- `power-plant` should depend on this repo via a **pinned git tag or commit**, not a floating branch.
- Breaking changes are expected; pin and bump deliberately.

## Layout (planned)

```text
Cargo.toml   # workspace root
pallets/     # experimental FRAME pallets
# optional later: thin --dev node / runtime, scripts
```

## Consuming from power-plant

```toml
pallet-example = {
  git = "https://github.com/Vitreus-Foundation/power-plant-experimental",
  tag = "v0.1.0",
  default-features = false
}
```

Wire crates into the testnet runtime only (`testnet-runtime` / equivalent).

## License

GPL-3.0 (see [LICENSE](LICENSE)).
