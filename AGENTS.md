# AGENTS.md

Guidance for coding agents in this repository. If a request conflicts with a rule here, name the rule, say if it looks wrong for the case, and wait for the requester to confirm before you follow the request. A PR doesn't change a rule it breaks.

Vitreus power-plant-experimental: pallets that power-plant takes as git dependencies, all at one rev. VTRS is the native token; transaction fees are paid in the energy token VNRG.

The rules below are the ones from power-plant's AGENTS.md (`Vitreus-Foundation/power-plant`) that apply here, word for word. Where they mention the runtime, its release PRs or `construct_runtime!`, they mean power-plant's.

## Commands

The CI workflow in `.github/workflows/` is the source of truth. Always pass `--locked`. While iterating, test one crate (`cargo test -p pallet-vitreus-dex --locked`).

## Dependencies

- Polkadot SDK and Frontier crates come from the sources in the root `Cargo.toml`. Take new crates from the same source as the existing entries: cargo unifies a git dependency only when the source strings match.

## Runtime code

- Runtime and pallet code must not panic. Avoid `unwrap`; restructure the code, or use `expect` with a proof that it cannot fail, ending in `; qed`. Tests are exempt.
- Runtime and pallet crates are `no_std`. Add dependencies with `default-features = false` and propagate the `std`, `runtime-benchmarks` and `try-runtime` features to them.
- Work in hooks (`on_initialize`, `on_finalize`, `on_idle`) is bounded per block, and its weight is accounted for.
- Don't turn a failure into "nothing to do": an `Option` or `Result` that means "can't" doesn't become a default (`unwrap_or_default`, `unwrap_or(0)`). A hook that can't return an error reports it in an event.
- Don't mint VTRS in new code, migrations included: that changes issuance on mainnet. A migration that funds an account transfers from an existing one, such as the Treasury.
- Pallet and call indices are never reused once a runtime with them ran on a network (mainnet, testnet or stagenet): a removed pallet or call leaves its index empty, and a removed pallet stays as a comment in `construct_runtime!`. The release PRs (`Runtime version N`) mark the runtimes that shipped. An existing call keeps its purpose: a call that does something else is a new call at a new index. Otherwise calls stored on chain or signed earlier decode as the new ones.
- A bug fix in runtime or pallet code comes with a test that fails without it; changing only a configuration value, such as a constant, needs none. Write that test first and see it fail, then make the fix without changing the test. Put new tests next to the related ones. Name new tests as a short statement of what they check, subject first and without articles (`failing_swap_is_reported`); rename existing tests only in a PR of their own.
- Test a pallet's own logic in the pallet's tests, against its mock. Runtime tests are for what depends on the runtime: config wiring, the real types behind config items (exchanges, fee handlers) and pallets working together.

## Code comments

- Comment what the code can't say: invariants, non-obvious reasons, units of values (base units or whole tokens, basis points, blocks or milliseconds), edge cases. Arguments for a design go in the PR description.
- Don't reference spec, design, review or audit documents (their sections such as `§x.y`, their decision or finding IDs, or the files themselves), files outside the repository, or this repo's PRs and issues, in comments or test names. History lives in git. A link to an issue in a dependency, such as the Polkadot SDK or Frontier, that a workaround depends on is fine.
- No market values, dates or snapshots, whether of a live network (rates, balances, prices) or of a local measurement (machine, run date). State the relation the code relies on; numbers for scale go in the PR description. Generated files, such as benchmarked weights, are exempt.

## Pull requests

- A PR covers one topic: a fix or a set of related fixes in one place, one feature, or one refactor, together with what it forces to change, such as callers, runtime wiring, runtime APIs and the docs it makes wrong, doc comments included.
- Everything else goes to its own PR: unrelated fixes, refactors, renames, reformatting, dependency bumps and cleanups. Report what you noticed instead of fixing it, so it can become an issue; mention it in the PR description only if reviewers need it to understand this PR. Changes to several pallets and the runtime for different reasons are several PRs. Split a large topic into a series of PRs that each build and pass tests.
- Propose runtime-wide changes and changes to the token economics in an issue before the PR.
- `Cargo.lock` changes only for the dependencies the PR changes.
- Don't add entries for your own tools or local files (IDE, agents, notes) to `.gitignore`; use `.git/info/exclude`.
- If the PR changes what users or integrators see (RPC, call arguments, EVM semantics, fees, XCM, events), describe the impact in the PR description.
- The title becomes the squash commit subject: imperative ("Fix …", "Add …", "Make …") without a `type(scope):` prefix, without spec references, and specific about what the whole PR changes. If it can't be, the PR is too wide.
