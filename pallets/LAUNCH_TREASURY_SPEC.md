# pallet-launch-treasury — Design Specification

**Status:** implemented on this branch (`pallets/launch-treasury`, DEX D9, launchpad L1/L2, testnet wiring); §10 lists where the code departs from the text below · **Branch:** `design/launch-treasury` off `pr/dex-launchpad` (`423740e`) · **Date:** 2026-09-16 · **Depends on:** `LAUNCHPAD_SPEC.md` (v1, §9 is the idea this document turns into a design)

A per-launch treasury for the launchpad, funded by a fixed slice of every trade in a launch token, staked with Vitreus validators as one pooled cooperator, with the staking yield returned to each launch as buy-and-burn of its token. Trading → VTRS → staking → yield → buy pressure → an incentive to trade. Every term is set by governance and identical for every launch; a creator chooses nothing.

This document answers the six questions the owner fixed before any proposal (§2), lists what needs the Foundation's agreement apart from what we can do alone (§3), and then specifies the pallet (§4–§9). Facts about the chain are read from `pallets/energy-generation`, `energy-broker`, `dynamic-energy`, `reputation`, `nac-managing`, `runtime/vitreus` at `423740e` and checked against the dev chain on 2026-09-16 (block 813,565); where §9.2 of the launchpad spec got a fact wrong, §1 says so.

Scope decisions fixed by the owner:

- **Uniform terms.** This is how the pad works, not an option. Treasury slice, validator set, distribution mode, dormancy rule: governance-set, snapshotted per launch like every curve term (LAUNCHPAD_SPEC §1.4), never chosen by a creator. The one per-launch value is the balance.
- **Design first.** Nothing here is implemented; §9 records what the implementer must decide.

---

## 0. Notation, units, constants

Inherits LAUNCHPAD_SPEC §0. Additional symbols:

| Symbol | Meaning | Value / source |
|---|---|---|
| `LNRG` | Liquid energy, `pallet_assets` id 2, 18 dp | staking rewards are minted in this asset (`EnergyAssetId = LNRG`, runtime l.804) |
| `VNRG` | Energy, id 0 — the gas token | not the reward asset; see §1 |
| era | `SessionsPerEra = 4` × 60 min epoch | **4 h** in production (`prod_or_fast!(4, 1)`; 10 min on the dev chain) |
| `BondingDuration` | eras to unbond | 42 = **7 days** |
| `HistoryDepth` | eras a payout can still be claimed | 84 = **14 days** — unclaimed after that is forfeited |
| `SlashDeferDuration` | eras before a slash applies | 36 = 6 days |
| `MinCooperatorBond` | least active bond to cooperate | 1 VTRS (genesis) |
| `MaxCooperations` / rewarded | targets per cooperator / cooperators paid per validator | 256 / 128 |
| `MaxUnlockingChunks` | concurrent unbond chunks per ledger | 64 |
| `Vanguard(1)` | reputation every validator demands of a cooperator | **7,398,066 points** = 21.4 days of account age (§1) |
| `AnnualPercentageRate` | target staking yield, VTRS terms | 100 = 10.0 % (`dynamic-energy`) |
| broker `SwapFee` | energy-broker fee, to the runtime Treasury | 10 = 1.0 % |
| `K` | treasury validator targets | `MaxTreasuryTargets = 16` |
| `bps` | basis points, `BPS = 10_000` | |

Every amount is `u128` base units; every multiply-then-divide goes through `U256` (LAUNCHPAD_SPEC D1 applies here without exception).

---

## 1. Corrections to LAUNCHPAD_SPEC §9.2

§9.2 was written from the pitch's assumptions in two places. Both change the design, so they are corrected here rather than inherited.

**C1 — Rewards are LNRG, not VNRG, and the chain sells LNRG for VTRS.** `pallet_energy_generation::Config::EnergyAssetId = LNRG` (asset 2, "Liquid Energy", runtime l.804; introduced in #76 "three energy assets"). `make_payout` deposits `LNRG` to the payee. And `pallet_energy_broker` — the runtime's own exchange, index 40 — has a `LiquidEnergyToNativeConverter` path (runtime l.1043): **LNRG → VTRS at the `dynamic-energy` exchange rate, 1 % fee to the runtime Treasury**, drawn from the broker's VTRS reserve. There is no VNRG → VTRS path; VNRG only goes the other way (VTRS → VNRG, how gas is bought). So the sentence in §9.2 "'yield → VTRS' is a swap through a VNRG/VTRS pool (the dev chain has none today)" is wrong twice: the asset is LNRG and the venue is the protocol's broker, not a DEX pool that someone must provide liquidity for. The broker implements `vitreus_runtime_common::Swap` with no origin check, so a pallet can sell from an account it owns in-runtime — the same way `pallet_energy_fee` buys gas for users. The exchange rate is set each session so that selling one session's LNRG issuance yields `AnnualPercentageRate` on total stake (`calculate_exchange_rate`, `dynamic-energy` l.470): the protocol *intends* staking yield to be realisable in VTRS at ~10 % APR. What bounds it is the broker's VTRS reserve: `InsufficientLiquidity` when it is short. That reserve is filled by every VTRS-funded gas purchase (`NativeToEnergyConverter` deposits the VTRS in the broker) and by `force_add_liquidity` (Root). On the dev chain it holds **0 VTRS** (nobody buys gas there); on mainnet it is the gas market's float.

**C2 — The reputation gate is cleared by existing, not by earning anything.** `validate()` overwrites every validator's `min_coop_reputation` with `ReputationTier::Vanguard(1)` (`update_prefs`, energy-generation l.1915; `#[cfg(not(test))]`), so the gate is one chain-wide constant, not per validator. `Vanguard(1)` is `from_rank(1)` = `ULTRAMODERN_3_POINTS × (1/9)^1.6` = **7,398,066 points** (the dev validator's prefs show exactly this). Points accrue at `REPUTATION_POINTS_PER_BLOCK = 24` from the moment an account has a reputation record, and **every account gets one at creation**: `frame_system::Config::OnNewAccount = NacManaging`, whose `on_new_account` inserts `ReputationRecord::with_blocknumber(now)` (nac-managing l.698). Records are advanced every session (`update_points_for_time` in `new_session`). So a pallet sub-account that has existed for 7,398,066 / 24 = 308,253 blocks = **21.4 days** at 6 s passes the gate for every validator, forever. Measured: the launch-0 escrow, created ~24 days before block 813,565, holds 8,424,162 points = Vanguard(1). Nothing needs bypassing and nothing needs the Foundation.

Two facts §9.2 had right and this design leans on: cooperating requires the target to have opted in (`ValidatorPrefs.collaborative = true`, checked in `cooperate`), and the pallet cannot sign — it dispatches with `RawOrigin::Signed(vault)`.

One fact §9.2 did not have: **cooperation stake is per target and explicit.** `Cooperations.targets: BTreeMap<validator, stake>`; `bond_extra` raises `ledger.active` but does not touch the targets (energy-generation l.915–941), so new principal earns nothing until `cooperate` is re-submitted with the new split. Every deposit into the stake must be followed by a re-`cooperate` (§6.2).

---

## 2. The six questions

### 2.1 The reward leg — what reaches holders

**Answer: buy pressure on the launch token, paid for by selling the treasury's LNRG to the energy broker for VTRS.** LNRG → VTRS (broker, protocol rate, 1 % fee) → launch token (the launch's own DEX pool, tier 3) → burn. Nothing is distributed as LNRG or VNRG; no one provides liquidity for the yield leg because the venue is the protocol's own exchange.

Why this is the coherent one:

- The chain already prices LNRG in VTRS at a rate it *sets* to deliver the target APR (C1). Using that path is using staking the way the runtime intends stakers to use it; a VNRG/VTRS DEX pool would be a second, thinner price for the same yield.
- Holders of a launch token are unenumerable (§2.3); the only reward that reaches them without enumeration is the price. Buy-and-burn is that reward. It also is the "incentive to trade" the loop promises — every trade funds a future buy.
- The alternative of paying LNRG out to holders fails on both counts: it needs enumeration, and LNRG in a retail wallet is a token the holder must then sell to the same broker.

What it costs, per unit of yield:

| Leg | Cost | Who receives it |
|---|---|---|
| LNRG → VTRS | 1.0 % (`SwapFee = 10`) | runtime Treasury |
| VTRS → token | pool fee 30 bps; of it 10 bps route straight back to this same treasury (§2.6), 5 / 5 to protocol / creator, 10 stay in the (locked) pool | mostly circular |
| price impact | bounded by `MaxBurnImpactBps` per slice (§6.4) | the pool, i.e. the locked LP position |
| MEV | a bracket around a predictable buy; bounded by the impact cap × slice | searchers |
| keeper bounty | `KeeperBountyBps` of each sale's VTRS and of each burn slice, default 50 bps | whoever calls `compound` |

Round trip ≈ 1.3–1.5 % of yield plus the impact leak. Against a 10 % APR target that is a rounding error; the number that matters is throughput, which is the broker's VTRS reserve.

What it depends on: broker depth. If the broker is dry the sell fails with `InsufficientLiquidity`; the design sells `min(accrued, quotable)` and keeps the rest as LNRG (§6.4, FM-T4). LNRG accrued is never lost, only delayed. Whether a mainnet broker holds enough VTRS to absorb the pad's yield is a Foundation question (§3.2).

Rejected: **direct LNRG to holders** (enumeration; §2.3). **VNRG to holders as gas** (no VNRG payout path exists — rewards are LNRG; LNRG → VNRG is 1:1 and free, but it re-introduces enumeration for no gain). **A VNRG/VTRS or LNRG/VTRS DEX pool** (duplicates the broker at worse depth; §9.2's premise, now moot).

### 2.2 The reputation gate

**Answer: earn it, and it costs nothing — the vault has it from its first block.** `NacManaging::on_new_account` grants every new account `Vanguard(1)` at creation (§10.10, found on the dev chain), and `validate` pins every validator's `min_coop_reputation` at exactly `Vanguard(1)`, so the gate is cleared by existing, with no waiting: a vault created by its first fee cooperates in that same `stake`. No bypass, no Foundation involvement. (C2's 21.4-day accrual figure below is the record of how this was first understood — points do accrue at that rate, but the grant at creation already meets the bar.) What remains of the gate is the *stale* case: a vault whose reputation has fallen **below a target's `min_coop_reputation`** — a slash does it — fails `cooperate` with `ReputationTooLow`, and the machinery below handles that.

Concretely: the runtime upgrade's `on_runtime_upgrade` funds the vault with its ED (from the launchpad's `Treasury`-bound recipient or a fixed genesis-style transfer; §7.4), which fires `OnNewAccount` and starts the clock. The pallet can accept fees and bond from block one; `cooperate` fails with `ReputationTooLow` until the record clears, and `stake()` (§6.2) is written so a failed re-cooperate leaves the bond in place and is simply retried. On testnet the three weeks pass during testing. On mainnet the vault exists from the upgrade block, so the gate clears before any launch has graduated and filled it.

Why not the alternatives:

- **Root seeds the record** (`reputation.increase_points(vault, 7_398_066)`): works in one block, but it is a governance motion asking the Foundation to hand a pallet three weeks of standing — a favour the design does not need. Keep it as the emergency lever if the vault ever has to be re-derived (§9), not as the plan.
- **A pallet-origin bypass in `energy-generation`**: a change to a Foundation pallet that removes a check for one caller. Slower to land than 21 days, and it makes the pad an exception in the staking system it is meant to be an ordinary participant of.
- **Per-launch stashes each earning their own** (§9.3.2): every launch waits 21 days after its first fee before earning anything, and every one is a separate record the session hook has to advance. This is one of three independent reasons the stash is pooled (§2.5 has the others).

What still needs an operator's decision, not ours: validators must set `collaborative: true` to accept cooperators at all, and `is_legit_for_collab` requires the *validator's* own reputation ≥ Vanguard(1). The treasury can only target validators who have opted in (§3.1).

### 2.3 Distribution without enumerating holders

**Answer: buy-and-burn, exclusively, in v2.** Holder distribution is not offered, not because pull accrual is wrong — it is the D4 shape and it is what this pallet uses *between launches* (§6.3) — but because it cannot be made correct for holders of a `pallet_assets` token without one of two things this design refuses to require:

- a **transfer hook** in `pallet_assets`, so the accumulator can checkpoint every balance change — a change to an upstream pallet, with consequences for every asset on the chain, that the Foundation would have to carry forever; or
- a **lock/stake of launch tokens inside the pad**, so the reward set is the stakers the pad can see — a v2 feature in its own right that changes what holding the token means (an unstaked holder earns nothing), and one that empties the DEX pool of the very tokens that make it tradeable.

Anything short of those — reading `pallet_assets` balances at claim time against a "last claimed" mark — is exploitable by moving balance between accounts before each claim; the launchpad spec's D4 reasoning already ruled it out.

Buy-and-burn has none of this: the treasury is the only party enumerated (it is one account), and the benefit reaches every holder through the price, proportionally, including holders who never interact. It is also what the market reads as "yield" for a launch token.

Where pull accrual *is* used: the vault is one pooled stash, and each launch's claim on the pooled LNRG is a Synthetix-style accumulator (`lnrg_per_share`, per-launch debt). The catch that kills it for holders — a checkpoint is needed on every balance change — does not exist here, because the only thing that changes a launch's share balance is this pallet's own `stake()` and `retire()`, which checkpoint as they go. Launches are bounded (one per `LaunchId`), never iterated, and every claim is O(1).

Honest cost of buy-and-burn: the pool's token reserve appreciates along with everyone else's, and the LP position that owns most of it is locked forever (LAUNCHPAD_SPEC §4.3). That fraction of every burn — the pool's share of supply, 20 % at graduation and shrinking as tokens are bought out — is value that supports the price but is never held by a person. It is the same dead-weight D4 identified for un-routed LP fees, and it is why §2.6 routes the treasury slice out of the LP share rather than adding to it.

### 2.4 Death and exit

**Answer: a dormant launch is retired permissionlessly; its principal unbonds (7 days), and the proceeds are returned to its holders the only way that enumerates nobody — one final buy-and-burn in capped slices. Nothing requires a signer, nothing is strandable except two EDs and forfeited payouts, and the design closes the second.**

Under a pooled stash a launch never has *its own* ledger, so "what happens to a treasury when the token dies" is a question about shares, not about unbonding:

1. **Dormancy is objective.** `retire(launch_id)` (anyone) succeeds when the launch's venue has had no trade for `DormancyBlocks` (default 90 days): for a graduated launch, the DEX pool's `last_swap_block` (a new `PoolInfo` field, §7.1); for one still on the curve, `CurveState`'s last buy or sell block (§7.2). One wash trade resets the clock — that keeps a treasury alive, which harms no one; nothing lets anyone retire a treasury early.
2. **Retire = burn shares, unbond principal.** The launch's shares are redeemed at the current share price (`ledger.active / TotalShares`, so any slash has already been taken), `unbond(amount)` is dispatched, and the launch enters `Retiring { chunk_era }`. Its accrued LNRG is compounded one last time (§6.4) and its future fee slice, which the pool's immutable routing keeps sending, is redirected: `account_for(asset)` returns `None` for a retired launch, so the DEX folds the slice into the protocol share from then on (§7.1). Retirement is one-way; a token that revives keeps trading with the treasury slice going to the protocol.
3. **Finalize after `BondingDuration`.** `finalize_retirement(launch_id)` (anyone) dispatches `withdraw_unbonded`, moves the returned VTRS to `PendingBurn[launch]`, and from there `compound(launch_id)` burns it into the venue in `MaxBurnImpactBps` slices, one per `MinBurnInterval` blocks, until it is gone. For a dead *pool* this deepens the reserves: the VTRS becomes the exit liquidity of whoever still holds the token — extractable by selling into it, which is the honest meaning of "returned to holders" for an unenumerable set. For a dead *curve*, buying on the curve is the same act; if the slices cross the graduation target the launch graduates and the pool gets seeded (LAUNCHPAD_SPEC §4.3), which is the correct outcome for a token that someone still holds — the treasury money ends up as locked depth they can sell into.
4. **What can be stranded, and what closes it.**
   - The vault's ED and its `pallet_assets` account deposits: constant, two accounts, not per launch. Accepted.
   - **Staking payouts not claimed within `HistoryDepth` (14 days) are forfeited** — the one real leak. `payout_stakers(validator, era)` is permissionless and **fee-waived on success** (energy-generation l.1534), so the cost of closing it is a keeper that calls it; the pad's indexer already watches every era boundary. The pallet does not wrap the call (it is O(128) and someone with a financial interest — every validator, for their commission — usually triggers it); it exposes `unclaimed_eras()` so the frontend shows the exposure. FM-T5.
   - Unbonding chunk slots: `MaxUnlockingChunks = 64` on the one ledger; the 65th concurrent `retire` fails with `NoMoreChunks` until a `finalize_retirement` frees one. `retire` is retryable; nothing is lost. FM-T7.
   - LNRG that the broker cannot absorb stays as LNRG in the vault, attributed to its launch, until it can. Never stranded, only slow. FM-T4.
   - Dust: a `PendingBurn` below what a slice can quote (`Unquotable` on the venue) is swept to the protocol recipient by `finalize`'s last call. Bounded by one minimum quote.

Rejected: **send retirement proceeds to the protocol** (rent extraction from the one group that already lost); **burn the VTRS** (a gift to every VTRS holder from a launch's traders — coherent but it is not what "the yield belongs to the launch" promised); **leave dormant treasuries staked forever** (earns yield to buy a token nobody trades; the design allows it in practice — retire is permissionless, not automatic — but it must not be the only option).

### 2.5 Where it lives

**Answer: a new pallet, `pallet-launch-treasury` (runtime index 59, testnet-runtime only), with one trait in each direction to the launchpad and the DEX.** Not an extension of `pallet-launchpad`.

Why a new pallet:

- **The launchpad's central invariant stays auditable in isolation.** LAUNCHPAD_SPEC §5 is built on "no party has a path to withdraw curve or pool funds" and the pallet's fund movements are exactly three (escrow → pool, escrow → treasury sweep, fee claims). A treasury adds a fourth kind — VTRS leaving a pallet account into a *staking lock* and coming back seven days later — and a dispatcher that signs as a pallet account. Putting that in the launchpad widens what a reviewer of #100 has to hold in their head; putting it beside the launchpad keeps §5's proof the size it is.
- **Different dependency surface.** The launchpad depends on `pallet_assets` and the DEX. The treasury depends on `energy-generation`, `energy-broker` (`Swap`), `reputation` (only transitively, via cooperate) and the DEX's swap. Those are the Foundation's pallets; a pad that couples to them should do it in one place, and the launchpad should still compile and test without them.
- **Mainnet wiring stays a no-op.** The DEX binds `Config::TreasurySink = ()` on mainnet exactly as it binds `CreatorFeeRecipient` to a `None` adapter; the launchpad binds `Config::Treasury = ()`. The new pallet is not in the mainnet runtime at all, like the other two.
- **It has its own storage version and migrations**, and its own benchmarks; the launchpad's do not change when this pallet's do.

What the split costs: one more `PalletId`/vault account, two small traits (§7.3), and the launchpad and DEX each gain one field in a snapshotted struct (§7.1, §7.2), which are fork-only migrations of the kind D4 and D8 already did.

The stash is **pooled**, one ledger for every launch, for three reasons that are each sufficient: (i) the reputation clock runs once (§2.2); (ii) `MaxCooperatorRewardedPerValidator = 128` and `MinCooperatorBond` are cleared once by a large cooperator rather than N times by small ones, several of which would earn nothing for months; (iii) a re-target on a validator change is one `cooperate(K)` instead of N. The cost is that a slash touches every launch — but it does under per-launch stashes too, since every stash would target the same uniform set; pooled only makes it visible in one number. Per-launch accounting is shares (§5.2), the same arithmetic as LP shares.

### 2.6 Scope — which fees

**Answer: both legs, from the first block the pallet is live, with the treasury slice taken from a different party on each leg: out of the locked-LP share on the pool, out of the protocol share on the curve. The creator's terms do not change on either leg.**

*Pool (graduated).* Tier 3 = 30 bps. D4's target split is 5 protocol / 5 creator / 20 pool (LAUNCHPAD_SPEC D4; the dev chain's default is still 0 / 0). Proposed: **5 protocol / 5 creator / 10 treasury / 10 pool**. The 10 bps come from the pool's share, and the pool's share of a launch pool belongs to an LP position that is locked forever — it is depth nobody can withdraw, the dead-weight D4 was written about. Moving half of it into a treasury that stakes it and buys the token back with the yield takes nothing from any person. The one party that does lose is a third-party LP who added liquidity to a launch pool after graduation: their fee falls from 20 to 10 bps. Uniform, disclosed on the pool page, and rare (the pad's pools are seeded 100 % locked). `FeeRouting` gains `treasury_bps`; the validity bound `MAX_ROUTED_BPS = 10` becomes tier-relative — `protocol + creator + treasury ≤ fee_tier × 10 − MIN_POOL_BPS (10)` — checked where the tier is known (§7.1). The launchpad's `pool_fee_tier` bound tightens from `1 | 3 | 10` to `3 | 10` so every launch pool can carry 20 routed bps.

*Curve (pre-graduation).* `curve_fee_bps = 100`, split `protocol_share_bps = 5_000` / creator. Proposed: **creator 50 / protocol 25 / treasury 25** — `CurveParams` gains `treasury_share_bps`, taken from the protocol's half. The protocol funds the loop on the leg where the protocol is the only other party, and the creator's pitch — half the curve fee, unchanged since v1 — is untouched, so nothing in #100's creator story moves. In absolute terms this leg is small: cumulative curve volume is on the order of `T` plus round trips, so at `T = 3,000 VTRS` the treasury enters graduation holding ~7–10 VTRS; the pool leg is where the balance comes from. It is included anyway because a launch that graduates should already *have* a treasury, and because one uniform rule for the whole life of a token is the point.

Does D4 change? The routing struct and its bound change (§7.1, D9); the *destinations* do not — protocol and creator still pull from the fee escrow exactly as D4 specified. The treasury slice is the one D4 stream that is pushed rather than pulled, because the recipient is a pallet-owned account that cannot be reaped or mis-set (§7.1 explains why that is safe where a push to a creator was not).

Rejected splits: **from the creator** (turns the pad's creator terms into a v1 → v2 change mid-review, and creators are the supply side); **from the protocol on both legs** (on the pool the protocol slice is 5 bps — there is nothing to take); **a fourth party in the swap fee (raise the tier)** (a tier change is what traders notice; the treasury should be invisible on the trade ticket).

---

## 3. What needs the Foundation, and what does not

This list is the real cost of the feature. Everything in §3.1 is a decision someone at the Foundation makes; §3.2 is a dependency we cannot supply; §3.3 is ours.

### 3.1 Needs a yes

1. **Validators must opt in.** `cooperate` only accepts targets with `ValidatorPrefs.collaborative = true` whose own reputation is ≥ Vanguard(1). The treasury's target list can only name validators who have set that flag. If the Foundation runs the mainnet validator set, the pad's staking loop starts only when some of them opt in. No code change; an operator setting.
2. **Governance holds the target list.** `set_targets` is `TreasuryManageOrigin` (Council or Root, wired like `LaunchManageOrigin`). Whoever holds it decides which validators receive staked VTRS from the pad — a governance power that should be written down as one, as §9.3.2 said.
3. **Testnet-only wiring stays testnet-only.** Like the DEX and launchpad, this pallet ships under `testnet-runtime`. Reaching mainnet is the same Foundation decision the pad already waits on (`CreateOrigin`, LAUNCHPAD_SPEC §5.3), now with staking in the package.

### 3.2 Dependencies we cannot supply

4. **Broker VTRS depth.** The LNRG → VTRS leg draws on the energy broker's VTRS reserve, filled by gas purchases and by `force_add_liquidity` (Root). If the mainnet broker is thin, the loop's throughput is bounded by gas demand and burns lag accrual. The design degrades gracefully (§2.1) but does not fix this; only the Foundation can (seed the broker, or accept the bound). The dev chain's broker holds 0 VTRS; the dev-chain demo needs `force_add_liquidity` or a VNRG purchase before the first `compound` succeeds.
5. **Yield size is the Foundation's dial.** `AnnualPercentageRate` (10 %), the exchange-rate smoothing and the warehouse multiplier are `dynamic-energy` governance parameters. The pad's yield is whatever staking yields; the design promises the mechanism, not a rate.

### 3.3 Ours alone

- Everything in `pallets/launch-treasury`, the D9 change to `pallets/vitreus-dex`, the L-changes to `pallets/launchpad`, and the testnet runtime wiring.
- The reputation gate (earned by age — §2.2), the staking calls (dispatched as the vault — §6.2), the yield sale (`Swap` trait on the broker — §6.4), the buy (DEX in-runtime swap — §7.1), the burn (`fungibles::Mutate::burn_from` — permissionless in-runtime).
- **Not asked for, deliberately:** an in-runtime staking trait on `energy-generation` (cleaner than dispatch; a Foundation pallet change — §9 lists it as a later ask if dispatch weights become a problem); a `pallet_assets` transfer hook (§2.3 — not needed under buy-and-burn); a reputation bypass (§2.2).

---

## 4. Accounts

| Account | Derivation | Holds |
|---|---|---|
| **vault** (stash = controller) | `PalletId(*b"vtrs/lpt").into_account_truncating()` | bonded VTRS (locked by `energy-generation`), pending VTRS (free), LNRG rewards (payee), launch tokens for the instant between buy and burn |

One account. It is the stash *and* the controller (standard; `bond` allows it), and the `RewardDestination::Account(vault)` payee — LNRG is a `pallet_assets` balance and is not covered by the staking lock on VTRS, so it can be sold from the same account the principal is bonded in. The 20-byte truncation caveat (LAUNCHPAD_SPEC §0) does not bite: there is no per-launch sub-account to derive.

The vault is created in the upgrade that adds the pallet (§7.4): funded with `ExistentialDeposit` so `OnNewAccount` fires and its reputation record starts. `bond` requires `value ≥ ED`, so the first `stake()` bonds pending plus nothing else; the ED stays free. On a chain that ships the pallet at genesis no upgrade runs and the first fee creates the vault instead; `note_fee` then withholds the ED from that fee (`VaultFunded`, §9.6), so either way the vault carries an ED that no record accounts for and that outlives every launch — the last retirement slice would otherwise have to spend the account's final unit while a consumer reference (late-era LNRG) keeps it alive, which Balances refuses.

---

## 5. Terms and storage

### 5.1 Governance parameters (live; snapshotted where marked)

```rust
pub struct TreasuryTerms {
    // The two fee slices are governance terms too, but they live where they
    // are snapshotted: `pool_treasury_bps` (10) in the DEX's DefaultFeeRouting
    // (§7.1) and `curve_treasury_share_bps` (2_500 of the fee) in the
    // launchpad's Params (§7.2). This struct holds the rest.
    /// Blocks without a trade after which `retire` is allowed. Snapshotted per launch when first funded.
    pub dormancy_blocks: BlockNumber,      // 90 * DAYS
    /// Operational (live, not snapshotted) — they bound a keeper's call, not a launch's economics.
    pub min_stake: Balance,                // 1 VTRS: `stake` refuses smaller pending (§6.2 spam bound)
    pub max_burn_impact_bps: u16,          // 50: a slice may move the venue price ≤ 0.5 %
    pub min_burn_interval: BlockNumber,    // 10: one slice per launch per interval
    pub keeper_bounty_bps: u16,            // 50 of the VTRS realised by the sale and of each burn slice, to the caller
}
```

`set_terms(TreasuryTerms)` — `TreasuryManageOrigin`. Bounds: `max_burn_impact_bps ∈ [10, 200]` (a ceiling; the slice is also bounded where it is sized, strictly under the venue's round-trip fee — R7, 2026-09-17), `keeper_bounty_bps ≤ 200`, `dormancy_blocks > 0`, `min_stake > 0`; the slices are bounded where they live (`is_valid_default`, `MinProtocolShareBps`). The snapshotted fields follow LAUNCHPAD_SPEC §1.4: a change never touches an existing launch.

`TreasuryTargets: BoundedVec<AccountId, MaxTreasuryTargets = 16>` — `set_targets` (`TreasuryManageOrigin`). The vault cooperates with every listed validator that passes the pre-flight filter (§6.2), splitting `ledger.active` equally. Live: a change re-cooperates on the next `stake()` or immediately via `retarget()` (anyone; §6.2).

### 5.2 Per-launch state (hot)

```rust
pub struct TreasuryRecord<Balance, BlockNumber> {   // `LaunchTreasury` is the pallet's runtime name
    /// VTRS received from fees, sitting free in the vault, not yet bonded.
    pub pending: Balance,
    /// Claim on the pooled stake. Value = shares × ledger.active / TotalShares.
    pub shares: Balance,
    /// Synthetix checkpoint: LNRG already attributed at the last share change.
    pub lnrg_debt: U256,                    // shares × lnrg_per_share at checkpoint, 1e18-scaled
    /// LNRG attributed and not yet sold (survives a dry broker).
    pub lnrg_accrued: Balance,
    /// VTRS realised (yield sold, or principal withdrawn on retirement) and not yet burned into the venue.
    pub pending_burn: Balance,
    pub last_burn_block: BlockNumber,
    pub dormancy_blocks: BlockNumber,       // snapshot
    pub status: TreasuryStatus,             // Active | Retiring { chunk_era: EraIndex } | Retired
}
StorageMap<LaunchId, LaunchTreasury<T>>
```

### 5.3 Pool-level state

```rust
TotalShares: Balance
/// Cumulative LNRG per share, 1e18-scaled, U256 arithmetic.
LnrgPerShare: U256
/// LNRG the accumulator has already distributed; harvest() attributes balance − this.
LnrgAccounted: Balance
Terms: TreasuryTerms
TreasuryTargets: BoundedVec<AccountId, MaxTreasuryTargets>
/// True between a bond change whose re-cooperate failed and the next successful retarget (§6.2).
CooperationStale: bool
```

Share price is `ledger.active / TotalShares` read from `energy-generation`'s `Ledger(vault)` at every mint and redeem — never cached — so a deferred slash that lands between two calls is simply reflected in the next one. A launch's *retiring* principal is not in `active` and not in shares; it is the chunk it holds.

### 5.4 What snapshots vs what reads live

| Term | Where | When |
|---|---|---|
| `pool_treasury_bps` | `PoolInfo.routing.treasury_bps` | at seed (D4 immutability) |
| `curve_treasury_share_bps` | `CurveParams.treasury_share_bps` | at `create_launch` |
| `dormancy_blocks` | `TreasuryRecord.dormancy_blocks` | at the first fee that reaches the vault (the launchpad does not call this pallet at create) |
| validator targets | `TreasuryTargets` | live — where staked VTRS sits is a live governance choice, and one cooperate serves every launch |
| min stake, impact cap, interval, bounty | `Terms` | live — operational bounds on keepers |

---

## 6. Extrinsics and flows

Every call except the two governance setters is permissionless. Nothing in this pallet has a path that moves VTRS or LNRG to a caller-chosen account: the only outbound transfers are the broker sale (vault → broker), the venue buy (vault → pool or curve escrow), the keeper bounty (bounded, to the caller), and the retirement dust sweep (to the protocol recipient).

### 6.1 Fee intake (called by the DEX and the launchpad, not extrinsics)

`TreasurySink::note_fee(asset, amount)` — the DEX has already transferred `amount` VTRS to `account_for(asset)` = vault (§7.1); the launchpad likewise from escrow (§7.2). The pallet resolves `asset → launch_id` (`AssetToLaunch`), and: `Active` → `pending += amount`; `Retiring | Retired` → unreachable, because `account_for` returned `None` and the caller folded the slice into the protocol share. O(1), two reads, one write. This is the only code on the trade path.

### 6.2 `stake(launch_id)` — pending → bonded → cooperating

1. `p = pending; pending = 0`. Require `p > 0`.
2. If the vault has no ledger (`Bonded(vault)` is empty in `energy-generation` — read, not cached): dispatch `bond(controller = vault, value = p, payee = Account(vault))` as `Signed(vault)`. Else `bond_extra(p)`. Either way the ledger's `active` rises by `p`.
3. Mint shares: `s = TotalShares == 0 ? p : p × TotalShares / (active_before)`; checkpoint the launch's `lnrg_debt` first (§6.3), then `shares += s; TotalShares += s`.
4. `retarget()` inline (below), **in the same extrinsic**. Its failure is **not** an error of `stake`: the bond is in place, `CooperationStale` is set to `true`, and event `CooperationStale { reason }` is emitted; the next `retarget()` — which anyone may call, and which every later `stake` and `retire` runs again — clears it.

`retarget()` (anyone): filter `TreasuryTargets` to validators that currently satisfy what `cooperate` will check — `Validators::contains_key`, `prefs.collaborative`, `is_legit_for_collab` — split `ledger.active` equally among survivors, dispatch `cooperate(targets)` as `Signed(vault)`. `cooperate` is all-or-nothing, which is why the filter runs first (FM-T3). Fails cleanly with `ReputationTooLow` while the vault's reputation is below a target's `min_coop_reputation` (FM-T2) or `NoTargets` if nothing survives the filter. On success `CooperationStale = false`.

**Decision: cooperation is re-submitted in the same extrinsic as every change to the bond, and this pallet has no `on_initialize`.** The problem (§1, last paragraph) is that `bond_extra` grows `ledger.active` but not the per-target stakes, so bonded and cooperated drift apart until `cooperate` is re-sent. Three ways to close it were priced:

| Option | Cost | Failure mode |
|---|---|---|
| **(a) in-extrinsic, atomic with the bond change** — chosen | `cooperate(K)` = 66 µs + 3.3 µs·K + 12+K reads + 6 writes ≈ **1.4 ms ref-time at K = 16** (energy-generation `weights.rs`), paid by the caller of `stake`/`retire`/`retarget` | a `DispatchError` the caller sees and the chain records; the bond is never lost, only un-cooperated, and the state says so (`CooperationStale`) |
| (b) once per era from `on_initialize` | one `CurrentEra` read on **every block** (25 µs, the era boundary test) + `cooperate(K)` on the first block of each era (1.4 ms, 0.07 % of a 2 s block) — cheap in weight | a hook cannot fail loudly: an `Err` from `cooperate` inside `on_initialize` can only be logged or turned into the same `CooperationStale` flag that (a) needs anyway, and a hook that panics halts block production. The bound is `K`, not the number of launches (the pooled ledger is one `cooperate`), so this is not the unbounded-loop brick — but it buys nothing (a) does not, at the price of code that runs when nothing changed |
| (c) leave the drift, re-cooperate lazily on the next `stake` | zero extra | new principal earns nothing until the next deposit — for a launch that has just gone quiet, indefinitely |

Under (a) the invariant is exact: **after any successful `stake`, `retire` or `retarget`, `Σ Cooperations.targets == ledger.active`**, checked in `try_state` (I-T7). The only state in which they differ is `CooperationStale == true`, which is observable, emitted, and cleared by a permissionless call. There is no per-block code in this pallet; `Hooks` implements `try_state` and `integrity_test` only.

Spam bound: `stake` requires `pending ≥ MinStake` (term; default 1 VTRS = `MinCooperatorBond`), so a dust deposit cannot make a keeper trigger K writes for nothing; above it, the caller pays the weight and the pallet does not care how often it is called. `cooperate` re-sent mid-era does not change the current era's exposure (exposure is taken at election from `Cooperators`), so there is no reward-side reason to throttle it.

Why stake is a separate, permissionless step and not part of the swap: `bond_extra + cooperate(K)` is K-proportional weight and a reputation check; putting it in `do_swap` would make a third party's trade pay for it and fail on it. Same reasoning as D4's pull.

### 6.3 `harvest()` — attribute arrived LNRG to shares

`delta = LNRG balance(vault) − LnrgAccounted`. If `delta > 0` and `TotalShares > 0`: `LnrgPerShare += delta × 1e18 / TotalShares`; `LnrgAccounted += delta`. O(1). A launch's claim at any moment is `shares × LnrgPerShare / 1e18 − lnrg_debt`, realised into `lnrg_accrued` whenever its shares change (`stake`, `retire`) or it compounds. This is D4's pull with launches as the beneficiaries, and it is correct because every share change goes through this pallet (§2.3).

Where the LNRG comes from: `payout_stakers(validator, era)`, called by anyone, deposits the vault's share of that validator's era reward to the payee. This pallet does not call it (§2.4.4).

### 6.4 `compound(launch_id)` — yield → VTRS → token → burn

1. `harvest()`; realise the launch's claim into `lnrg_accrued`.
2. **Sell:** the broker's quote does not know its own depth (`get_amount_out` is pure arithmetic; the reserve check is `withdraw`'s `InsufficientLiquidity` at execution), so the pallet reads `Exchange::depth()` (the broker's reducible VTRS) and sells the largest `x ≤ lnrg_accrued` whose quote fits in it: `q = quote(LNRG → VTRS, x)`, then `Swap::swap_exact_tokens_for_tokens(vault, [LNRG, Native], x, min_out = q × (1 − 10 bps), vault, keep_alive = true)`. `lnrg_accrued −= x`. A zero fill (dry broker) is allowed and not an error (FM-T4).
3. **Bounty:** `b = realised × keeper_bounty_bps / BPS` to the caller; `pending_burn += realised − b`.
4. **Burn one slice:** require `now − last_burn_block ≥ min_burn_interval`. Venue = the launch's pool if `Graduated`, else its curve; a `Complete` curve waiting for its seed has no venue and the slice waits. `cap` = the VTRS amount whose quote moves the venue price by `max_burn_impact_bps` (constant-product: `cap = reserve_vtrs × impact / (2 × BPS)`, in `U256`; on the curve, from the virtual reserves). `y = min(pending_burn, cap)`. Pool: `Dex::swap_for(vault, Native → asset, y, 0)`; curve: `Launchpad::buy_for(vault, launch_id, y, 0)` (both in-runtime methods, §7). Then `burn_from(asset, vault, tokens_received, Exact, Force)` — `fungibles::Mutate::burn_from` is an in-runtime call with no origin or admin check (the asset's admin is the launch's escrow; it is not consulted). **What was spent is measured, not assumed**: the vault's VTRS balance before and after, corrected for the launch's own slice that the buy routes back into `pending` (via `note_fee`, in storage, underneath the record `compound` holds — so `pending` is re-read after the venue call). A curve buy that crosses the target is a partial fill (`do_buy` takes `quote_used`, not the offer), which is why. `pending_burn −= spent; last_burn_block = now`. No `min_out`: a same-block front-run has already moved the state the quote would come from, so a quote-derived minimum protects nothing the cap does not (FM-T1). **The slice pays its caller as the sale does:** `b′ = spent × keeper_bounty_bps / BPS` from `pending_burn` to the caller, when `b′ ≥ ED` and `pending_burn` covers it; otherwise nothing. A retired launch's principal goes out over many slices with nothing to sell (the dev-chain run took 19), and the keeper that runs them is paid for each; the very last slice, which empties `pending_burn`, pays nothing.
5. Event `Compounded { launch_id, lnrg_sold, vtrs_realised, bounty, vtrs_burned_in, tokens_burned }` — `bounty` is `b + b′`.

Steps 2–3 run only when there is something to sell; step 4 only when `pending_burn > 0`. Either alone is a valid call, so a launch with a dry broker still burns what it has, and one with nothing accrued still finishes a retirement.

Why capped slices: a buy that anyone can see coming is a sandwich target. The cap bounds what a bracket can extract to a fraction of `max_burn_impact_bps` of the slice; the interval bounds how much of a large `pending_burn` (a retirement) any one block exposes. That is the whole MEV story for this pallet and it is accepted as the cost of on-chain predictability (FM-T1).

### 6.5 `retire(launch_id)` — dormancy → unbond

Require `Active`; require the venue's last trade block `+ dormancy_blocks ≤ now`. The venue's last trade is the later of the curve's `last_trade_block` (§7.2) and the pool's `LastSwapBlock` (§7.1) — a pool that never traded reads as block 0 and must not count, and the graduating buy is the curve's last trade. Then: harvest and settle the launch's LNRG claim into `lnrg_accrued` (the sale itself is `compound`'s, which works on a retiring or retired treasury exactly as on an active one); `pending` joins `pending_burn` (unstaked fees retire with the rest); redeem `v = shares × active / TotalShares`; `shares = 0; TotalShares −= shares`; if `v > 0`, dispatch `unbond(v)` as `Signed(vault)` and `status = Retiring { chunk_era: current_era + BondingDuration }`, else `status = Retired` at once. From here `account_for(asset)` returns `None`. Fails with `NoMoreChunks` if 64 chunks are outstanding (FM-T7), or `QueueFull` if this pallet's own queue is; retry after any `finalize_retirement`. Fails with `VaultInsolvent`-adjacent nothing: a launch whose shares are worth zero (every target slashed to nothing) retires as `Retired` and frees its shares, which is how `TotalShares` returns to zero and `stake` (which refuses `TotalShares > 0 ∧ active == 0` as `VaultInsolvent`) resumes.

`unbond` refuses to leave a cooperator with `active < MinCooperatorBond` (1 VTRS): `InsufficientBond`, "chill first" (energy-generation l.1010–1020). It does **not** chill for you. So `retire` checks `active − v < MinCooperatorBond` and, if so, dispatches `chill` before `unbond`; the vault then holds its remaining dust bonded but un-cooperating, and the next `stake()`'s `retarget` cooperates again (`cooperate` requires `active ≥ MinCooperatorBond`, which the new principal supplies). This only arises when the last meaningful launch retires. `unbond` also merges chunks that mature in the same era into one, so two retirements in one era share a chunk — the `RetiringQueue` (§6.6) credits by launch from its own record, not from chunk boundaries. Test `fm_t8`.

### 6.6 `finalize_retirement(launch_id)`

Require `Retiring` and `current_era ≥ chunk_era`. Dispatch `withdraw_unbonded(num_slashing_spans)` as `Signed(vault)` — this withdraws *every* matured chunk on the ledger, so `finalize` credits each `Retiring` launch whose era has passed, not only the one named; the pallet keeps `RetiringQueue: BoundedVec<(LaunchId, EraIndex, Balance), MaxUnlockingChunks>` in unbond order so the credit is exact even when the staking pallet has merged two launches' chunks into one era. What is credited is what actually came back, pro rata: a slash during unbonding reduces the chunks too, and the queue's amounts are what was unbonded, not what returns. Credited launches: `pending_burn += credit; status = Retired`. Their VTRS then leaves through `compound` slices (§6.4 step 4) like any yield. A retired treasury closes on the `compound` that leaves it with nothing staked, nothing accrued and `pending_burn` below the existential deposit: that remainder goes to the protocol recipient (`DustSwept`) and the record is removed.

### 6.7 Governance

- `set_terms(TreasuryTerms)`, `set_targets(BoundedVec<AccountId>)` — `TreasuryManageOrigin`.
- **No** `force_unbond`, **no** `withdraw`, **no** recipient setter. Governance can steer where the stake sits and what the future slices are; it cannot take the principal or the yield. That is the property that lets the pad say "uniform terms" with a straight face.

### 6.8 Events and errors (reference)

`FeeNoted`, `Staked { launch_id, amount, shares }`, `Retargeted { targets }`, `StakeRetargetFailed { reason }`, `Harvested { lnrg }`, `Compounded { … }`, `Retiring { launch_id, amount, chunk_era }`, `Retired { launch_id, amount }`, `DustSwept`, `TermsSet`, `TargetsSet`. Errors: `NoPending`, `NotDormant`, `NotActive`, `NotRetiring`, `NotMatured`, `TooSoon` (burn interval), `NoTargets`, `NothingToDo`, `TermsOutOfBounds`, plus pass-through of `energy-generation` and broker errors as `DispatchError`.

---

## 7. Changes to the other pallets and the runtime

### 7.1 `pallets/vitreus-dex` — **D9: treasury slice in fee routing**

```rust
pub struct FeeRouting { pub protocol_bps: u16, pub creator_bps: u16, pub treasury_bps: u16 }
pub const MIN_FEE_TIER: u32 = 1;          // a create_pool pool carries the protocol slice only
pub const MIN_LAUNCH_FEE_TIER: u32 = 3;   // a seeded pool carries all three
impl FeeRouting {
    pub fn routed_bps(&self) -> u16 { protocol + creator + treasury }
    /// Tier-relative: routed ≤ fee_tier × 10 (a pool may route its whole tier). Replaces MAX_ROUTED_BPS.
    pub fn is_valid_for(&self, fee_tier: u32) -> bool
    /// What set_default_fee_routing checks: protocol ≤ MIN_FEE_TIER × 10 and routed ≤ MIN_LAUNCH_FEE_TIER × 10.
    pub fn is_valid_default(&self) -> bool
}
/// For dormancy (§6.5). A map beside PoolInfo, not a field in it: the pool
/// record is what every quoter decodes, and one reader wanting one block
/// number is not a reason to change its shape.
pub type LastSwapBlock<T> = StorageMap<(AssetKind, AssetKind), BlockNumber>;

pub trait TreasurySink<AssetKind, AccountId, Balance> {
    /// The account that receives `asset`'s treasury slice, or None to fold it into the protocol share.
    fn account_for(asset: &AssetKind) -> Option<AccountId>;
    /// Called after the transfer, inside do_swap, so the sink can attribute it. Must be infallible and O(1).
    fn note_fee(asset: &AssetKind, amount: Balance);
}
type TreasurySink: TreasurySink<…>;   // runtime: LaunchTreasury on testnet, () on mainnet
```

- `set_default_fee_routing(protocol, creator, treasury)` validates with `is_valid_default`; `insert_new_pool` validates the snapshot against the actual tier (`InvalidFeeRouting`, which at seed time is a deferred graduation, FM-11). `routing_for_new_pool` folds `creator_bps` and `treasury_bps` into the pool for `create_pool` pools (no launch, no treasury).
- `do_swap`: after computing the routed slices, if `treasury > 0`: `match T::TreasurySink::account_for(&launch_asset) { Some(vault) => transfer(pool_account → vault, treasury); note_fee(...), None => protocol += treasury }`. **This is a push inside a swap, which D4 forbade for creators and the protocol recipient.** It is safe here for the reason D4 gave for the fee escrow: the recipient is a pallet-owned account that always exists (it is bonded, so it has a consumer and cannot be reaped) and is never user-settable, so the two failure modes D4 protected against — a reaped recipient, a mis-set one — cannot occur. The transfer is the same one hop the fee escrow costs.
- `LastSwapBlock[pair] = now` in `do_swap`.
- New in-runtime methods on `PoolManager`: `swap_for(who, asset_in, asset_out, amount_in, min_out) -> Result<Balance>` — `do_swap` without the extrinsic layer, delivering to `who`; `native_reserves(asset) -> Option<(native, other)>` from live balances (the quoting rule); `last_swap_block(asset)`. Same trust argument as `ReservedPoolSeeder` (D2): no extrinsic reaches them.
- Storage version 1 → 2 on this branch, **no migration**: like the D8 submission, no chain the branch targets has a v1 pool; the fork (`feature/solver-marketplace`, at its own v2) carries a migration that gives existing `PoolInfo`s `treasury_bps = 0`. **Pools that graduated before D9 keep zero treasury routing**, the same per-launch immutability D4 applied to DLNCH.

### 7.2 `pallets/launchpad` — L-changes

- `LaunchParams` / `CurveParams` gain `treasury_share_bps: u16` (snapshot at create). `split_fee` becomes three-way: treasury floors, protocol floors, creator gets the remainder. The treasury part is transferred escrow → `T::Treasury::account_for(asset)` (or, `None`, to the protocol recipient as today) and `note_fee`d. `CurveState` gains `last_trade_block` (written by `do_buy` and `do_sell`, which already write `CurveState`).
- `pool_fee_tier` bound: `matches!(p.pool_fee_tier, 3 | 10)`.
- New in-runtime method `buy_for(who, launch_id, quote_in) -> Result<Balance>`: `do_buy` for a pallet caller. It is the same path a user's `buy` takes — anti-snipe hook included — so a retirement's curve slices are ordinary buys that can graduate the launch (§2.4.3).
- `Config::Treasury` is rebound from `Get<AccountId>` to the same `TreasurySink` trait; the launchpad's *protocol* destination (`DexProtocolFeeRecipient`) is unchanged.
- Storage version 0 → 1 on this branch, no migration (as above); the fork's migration gives existing `CurveParams` `treasury_share_bps = 0` — they were created under the terms in force — and `Curves` `treasury_fees_paid = 0`, `last_trade_block = created_at`.
- `do_buy` returns `(crossed, tokens_out)` so `buy_for` can report what the buyer received.

### 7.3 Traits, direction, and no cycles

DEX → treasury and launchpad → treasury both go through `TreasurySink`, bound in the runtime (like `LaunchpadCreators`), so neither pallet depends on the treasury crate. Treasury → DEX uses `PoolManager::swap_for` and reads `PoolInfo`; treasury → launchpad uses `buy_for`, `AssetToLaunch`, `Curves`; treasury → `energy-generation` requires `T: pallet_energy_generation::Config` and calls its `pub fn` extrinsics with `RawOrigin::Signed(vault).into()`; treasury → broker via `Config::Exchange: TreasuryExchange<AccountId, Balance>` — a pallet-local trait (`quote`, `depth`, `sell`), like `TreasuryStaking` — bound to a runtime adapter over `EnergyBroker`. The treasury crate depends on the DEX and the launchpad and on no runtime trait crate (§10.12); nothing depends on it.

### 7.4 Runtime wiring (testnet-runtime only)

```rust
LaunchTreasury: pallet_launch_treasury = 59,   // 58 is TechnicalCommitteeTreasury
```

`TreasuryManageOrigin = EnsureRoot` (as `LaunchManageOrigin`); `Exchange = EnergyBrokerExchange`, an adapter in the runtime that implements `pallet_launch_treasury::TreasuryExchange`: `quote` is the broker's `QuotePrice::quote_price_exact_tokens_for_tokens(LNRG, Native, x, true)`, `depth` is `Balances::reducible_balance(EnergyBroker::account_id(), Preserve)`, `sell` is the broker's `Swap::swap_exact_tokens_for_tokens(vault, [LNRG, Native], x, Some(min), vault, keep_alive = true)`; `Staking = EnergyGenerationStaking`, an adapter in the runtime that implements `pallet_launch_treasury::TreasuryStaking` by reading `Bonded`/`Ledger`/`Cooperators`/`Validators`/`MinCooperatorBond`/`CurrentEra` and dispatching `bond` (controller = payee = vault), `bond_extra`, `cooperate`, `chill`, `unbond`, `withdraw_unbonded` with `RawOrigin::Signed(vault)`; `is_cooperable` is what `cooperate` checks of a target (`Validators` membership, `collaborative`, `is_legit_for_collab`); `LnrgAsset = WithId(LNRG)`; `DefaultTerms` as §5.1; `Targets` starts empty (governance sets after the validators opt in). `migrations::Unreleased` carries `FundLaunchTreasuryVault`: `ExistentialDeposit` from `xcm_config::TreasuryAccount` to the vault if it has no provider — one transfer, once — so the reputation clock starts at the upgrade block (§2.2). DEX: `TreasurySink = LaunchTreasury`; launchpad: `CurveTreasurySink = LaunchTreasury` (named apart from the DEX's, which is a supertrait of the launchpad's `Config`). Mainnet: none of the three pallets is wired.

Weights: `stake` = `bond_extra` + `cooperate(K)` + this pallet's writes, `K = MaxTreasuryTargets`; `compound` = broker `do_swap` + DEX `do_swap` + `burn_from` + writes; `retire` = `unbond`; `finalize_retirement` = `withdraw_unbonded(SPECULATIVE_NUM_SPANS)`. Each is a sum of already-benchmarked calls plus O(1); bench the O(1) part and add.

---

## 8. Invariants and failure modes

### 8.1 Invariants

- **I-T1 (conservation).** `free(vault) + ledger.total + Σ retiring chunks ≥ ED + Σ pending + Σ pending_burn + (share value of every Active launch) + Σ unbonded-but-unfinalized` — a floor, not an equality, since anyone can send the vault VTRS that nothing accounts for (R5, 2026-09-17) — the `ED` term only once `VaultFunded` is set (§9.6: by the upgrade, or withheld from the first fee); before that the vault holds nothing — up to slashes (which reduce `ledger.active` and therefore every share's value uniformly) and floor rounding in the pallet's favour. `try_state` checks it.
- **I-T2.** `Σ shares == TotalShares`; `Σ claimable LNRG ≤ LnrgAccounted ≤ LNRG balance(vault)` — a sale lowers `LnrgAccounted` by what left (R1, 2026-09-17; the earlier form, `≤ balance + Σ sold`, described the bug that made the next equal amount of rewards unattributable).
- **I-T3 (no exit).** No extrinsic moves VTRS out of the vault except: broker sale input (LNRG, not VTRS), venue buy (vault → pool account / curve escrow), keeper bounty (≤ `keeper_bounty_bps` of one sale plus one slice, to the caller), retirement dust (≤ one minimum quote, to the protocol recipient). In particular no governance origin can withdraw, redirect or unbond. Tests T-G1..G3.
- **I-T4 (uniformity).** Every `LaunchTreasury` created in the same block has identical snapshotted terms; no extrinsic takes a per-launch term as an argument.
- **I-T5 (one-way).** `Retiring → Retired` only; `Retired` never returns to `Active`; `account_for` is `None` for both. A closed treasury keeps its record — `Retired` with nothing in it — rather than being removed, so the next fee cannot mistake it for a launch never funded (R3, 2026-09-17).
- **I-T6 (burn is total).** After every `compound`, the vault's balance of the launch asset is zero.
- **I-T7 (no drift).** `CooperationStale == false` ⇒ `Σ Cooperations(vault).targets == ledger.active`. §6.2.

### 8.2 Failure modes

| FM | What | Handled by |
|---|---|---|
| FM-T1 | Sandwich around a burn slice | impact cap + interval (§6.4). The cap only bounds a bracket while the slice's impact is under the venue's round-trip fee (the bracket pays the fee twice, the slice moves the price once, `min_out` is 0): modelled, the bracket breaks even at exactly `2 × fee_bps` and is profitable one step above, taking 14 % of a slice at 100 bps on a 0.3 % pool and 73 % at 500. So the slice is sized at `min(term, 2 × venue_fee_bps − 1)` in `burn_slice` (R7); the term is a ceiling, not the guarantee |
| FM-T2 | Vault below a target's `min_coop_reputation` (a slash): `cooperate` → `ReputationTooLow` | `stake` keeps the bond, emits `StakeRetargetFailed`; `retarget` retried by anyone |
| FM-T3 | A target chilled, un-collaborative, or its reputation fell | pre-flight filter before `cooperate`; equal split among survivors |
| FM-T4 | Broker short of VTRS | sell the quotable maximum, keep the rest as `lnrg_accrued`; zero fill is not an error |
| FM-T5 | Payout not claimed within `HistoryDepth` | forfeited; permissionless, fee-waived `payout_stakers`; indexer keeper; `unclaimed_eras()` shown |
| FM-T6 | Validator slash | `ledger.active` falls; share price falls uniformly; nothing to do — honest consequence of staking, and the filter in FM-T3 drops a slashed-and-chilled validator on the next retarget |
| FM-T7 | 64 unbonding chunks outstanding | `retire` fails `NoMoreChunks`; retry after a `finalize` |
| FM-T8 | Last active launch retires; `unbond` would leave `active < MinCooperatorBond` | `retire` dispatches `chill` first; next `stake` re-cooperates; the ledger is read, never cached |
| FM-T9 | Retired token revives | slice goes to protocol forever; disclosed on the token page |
| FM-T10 | Wash trade to keep a treasury from dormancy | harmless; the attacker pays 30 bps to keep yield flowing to a token they hold. The pallet's own buybacks (`buy_for`, `swap_for`) do not move either venue's last-trade block — before R2 (2026-09-17) they did, and a funded launch with a keeper could never become dormant |
| FM-T11 | Retirement burns graduate a dead curve | intended (§2.4.3): treasury VTRS becomes locked depth holders can sell into |

### 8.3 Decided: the slash-deferral asymmetry is accepted

Slashes apply `SlashDeferDuration = 36` eras (6 days) after the offence, and until then `Ledger.active` still counts the stake that will be removed. A launch that retires inside that window redeems its shares at the pre-slash price and escapes its share of the slash; the launches still active absorb it. Symmetrically, a launch whose principal is staked *into* the window pays for an offence it was never exposed to. This is accepted, and here is why, so that nobody re-derives it:

1. **The alternative is to hold every retirement for six days.** The only way to price shares net of a pending slash is to wait for it to apply (or to discount by `UnappliedSlashes`, which the next point rules out). That delays every exit — every one of which already waits `BondingDuration` = 7 days — by a further `SlashDeferDuration` for an event that has usually not happened.
2. **Discounting by `UnappliedSlashes` can over-charge.** The deferral exists so that governance can `cancel_deferred_slash` a slash it judges wrong. A redemption priced against a slash that is later cancelled has taken money from a launch that owed none, and there is no way to give it back — the shares are gone. Under-charging by the pending amount, which is what accepting does, is at least reversible in aggregate: the vault keeps earning.
3. **The amount is small by construction.** The vault splits equally across `K ≤ 16` targets, so one validator's slash at fraction `f` removes `f / K` of the vault: a 1 % slash on one of 16 targets is 0.06 % of every launch's principal; the 100 % equivocation case is 6.25 %. What a retiring launch escapes is its share of that, and what a joining launch overpays is the same figure. Dormancy is 90 days, so a retire is not a strategic act by a holder watching for offences; `retire` is permissionless, so a bot could time one, and what it would gain for a third party is bounded by the numbers above.
4. **It is the honest consequence of pooling.** A per-launch stash would expose each launch to exactly its own slash, and §2.5 rejected that shape for three independent reasons. Pooled means shared, in both directions.

`try_state` does not assert anything about `UnappliedSlashes`; the share price is `active / TotalShares` and nothing else.

### 8.4 Test plan (names for the implementer)

*Failure-mode:* `fm_t1_slice_never_exceeds_impact_cap`, `fm_t2_stake_before_reputation_keeps_bond_and_retries`, `fm_t3_retarget_filters_chilled_and_noncollab`, `fm_t4_dry_broker_keeps_lnrg_accrued`, `fm_t6_slash_devalues_every_launch_equally`, `fm_t7_no_more_chunks_is_retryable`, `fm_t8_last_retire_chills_first_and_next_stake_recooperates`, `fm_t11_retirement_can_graduate_a_curve`, `i_t7_cooperation_matches_active_after_every_bond_change`.

*Invariant/lifecycle:* `t_l1_fee_to_pending_to_shares_at_price`, `t_l2_harvest_attributes_by_shares_not_by_time` (including a re-stake that must neither lose nor mint yield), `t_l3_compound_burns_everything_it_buys`, `t_l4_retire_requires_dormancy_and_is_one_way`, `t_l5_finalize_credits_every_matured_launch_exactly`, `t_l5b_finalize_prorates_a_slash_across_matured_launches` (two launches in one merged chunk), `t_l6_snapshotted_terms_survive_set_terms`, `t_l7_i_t1_conservation_under_random_ops` (400 random operations, `try_state` after each), `t_g1_no_origin_can_withdraw`, `t_g2_set_targets_recooperates_without_touching_shares`, `t_g3_retired_launch_slice_folds_into_protocol`, `stake_refuses_when_the_vault_is_fully_slashed`.

*Red-then-green, in the pallet's own suite:* each of these was made to fail by removing the rule it guards and confirmed to pass with it — settle before a share change (`t_l2`), burn after the buy (`t_l3`), pro-rata credit (`t_l5b`), the impact cap (`fm_t1`), chill before the last unbond (`fm_t8`), the dormancy check (`t_l4`), a retired launch receiving nothing (four tests), the insolvency guard, and measured-not-assumed spend (`fm_t11`). Two of the green runs found real bugs first: the venue buy's own routed slice was being overwritten by the record `compound` held, and a crossing curve buy's partial fill was being accounted at the offer. Both are §6.4 now.

*DEX D9:* `d9_routing_is_tier_relative`, `d9_treasury_push_goes_to_vault_and_notes`, `d9_no_sink_folds_into_protocol`, `d9_migration_gives_existing_pools_zero_treasury`, `d9_last_swap_block_written`. *Launchpad:* `l1_three_way_split_floors_in_creator_favour_last`, `l2_buy_for_runs_the_hook_and_can_graduate`, `l3_tier_bound_is_3_or_10`.

---

## 9. Open items for the implementer

1. **Curve-venue impact cap.** The curve's constant-product is over virtual reserves (LAUNCHPAD_SPEC §3.1); `cap` must use them, not `real_quote`.
2. **Bounty vs dust.** A 50 bps bounty on a compound realising 0.01 VTRS is dust the caller cannot receive above ED considerations; below `ED / 10` pay no bounty.
3. **An in-runtime staking trait.** Dispatching `energy-generation` calls as `Signed(vault)` couples this pallet to their argument shapes and re-runs `ensure_signed`. If the Foundation is open to it, a `trait PalletStaker { bond, bond_extra, cooperate, unbond, withdraw }` on `energy-generation` for in-runtime callers would be cleaner and would let the reputation check be applied deliberately rather than incidentally. Ask *after* v2 works with dispatch, with the working version as the argument.
4. **Frontend.** Token and launch pages: treasury balance (pending, staked value, LNRG accrued, pending burn), last compound, cumulative burned, status, and the unclaimed-era warning. The indexer: `payout_stakers` keeper for the treasury's targets, and `stake`/`compound` pokes. *The keeper exists* (vitreus-dex-frontend `indexer/keeper.ts`, 2026-09-16): payouts per closed era from a ledger of exposures, `stake`/`compound`/`finalize_retirement`/`retarget` pokes decided from state, dormancy detected and alarmed but never acted on — `retire` stays a human's call — with unclaimed / lost / unknown eras surfaced in status and on the launch page. The treasury panel itself is still to build.
5. **The §9 record in LAUNCHPAD_SPEC.** After this design is accepted, §9.2's two wrong facts (C1, C2) should be corrected in place with a pointer here; not done on this branch, since that file is under review in #100.
7. **The exchange trait must be pallet-local before this pallet moves to experimental (§10.12).** It imports `QuotePrice` and `Swap` from `vitreus-runtime-common`, a power-plant-internal crate that experimental cannot depend on without two copies of the traits in every consumer. As with staking (`TreasuryStaking`), the pallet declares what it needs — a quote and an exact-in swap of LNRG for VTRS — and the runtime adapts the broker to it. Applied on the fork on 2026-09-17; it moves with the pallet.

6. **The vault's ED on a chain that ships the pallet at genesis** (§10.11) — *applied*. `FundLaunchTreasuryVault` is a migration and does not run at genesis, so on such a chain the first fee creates the vault and every unit it holds is accounted (`pending`); the ED buffer §4 assumes does not exist, I-T1 is false, and the last retirement slice — which would spend the vault's final unit while its LNRG dust keeps a consumer reference on the account — fails with `Token(Frozen)`, so a retired record can never close. Two fixes, both O(1): (a) `note_fee` withholds `ED` from the first fee it ever notes (a `VaultFunded` flag, set by the migration too), so `pending` is always what is above ED; (b) a pallet `GenesisConfig` that funds the vault. (a) covers both paths with one rule and is what the branch does: `VaultFunded` is set by the migration (whether it funded the vault or found it existing) and by the first `note_fee` otherwise, which notes `amount − ED`. `t_l8_from_genesis_first_fee_withholds_ed_so_retirement_closes` is the record: red with `Token(Frozen)` on the last slice, green with the withholding, I-T1 holding on both paths (`mock.rs` builds either).

---

## 10. Where the implementation departs from the text above

Recorded on the branch that implements it; each is small and none changes a decision in §2.

1. **The fee slices are not in `TreasuryTerms`** (§5.1). They are governance parameters of the pallets that snapshot them — `treasury_bps` in the DEX's `DefaultFeeRouting`, `treasury_share_bps` in the launchpad's `Params` — so there is one place each is set and one place each is bounded. `TreasuryTerms` holds dormancy, min stake, impact cap, burn interval and bounty.
2. **`dormancy_blocks` snapshots at first funding, not at `create_launch`** (§5.4). The launchpad does not call this pallet at create; the first fee that reaches the vault creates the record. Same effect for any launch that ever trades.
3. **`retire` does not sell** (§6.5). It settles the LNRG claim into the record; `compound` sells, and works on a retiring or retired treasury. One code path for the sale.
4. **The routing bound is `routed ≤ tier × 10`** (§7.1), i.e. a pool may route its whole tier, rather than `tier × 10 − 10`. D4's rationale was "the pool keeps ≥ 0", and a `create_pool` pool at tier 1 with today's 5-bps protocol slice needs the smaller reserve; the default is separately required to fit the protocol slice into tier 1 and all three into tier 3.
5. **`LastSwapBlock` is a map beside `PoolInfo`**, not a field (§7.1); `last_trade_block` and `treasury_fees_paid` *are* fields of `CurveState` (§7.2), which only the launchpad and its page decode.
6. **No migrations on this branch** (§7.1, §7.2): the branch is the upstream-shaped submission, where no chain has a pre-existing pool or launch. The fork carries them.
7. **`min_out = 0` on the venue buy** (§6.4): a quote-derived minimum would be computed from state a same-block front-run has already moved; the impact cap is the protection.
8. **Weights are measured** (`weights.rs`, 2026-09-16, commit `f768be5`, c-16 per `pallets/BENCHMARKING.md`, `--steps 50 --repeat 20`): one benchmark per call, `finalize_retirement` linear in the queue length `n ≤ MaxUnlockingChunks`, everything else at `MaxTargets`, registered in the testnet runtime's `define_benchmarks!` behind `Config::BenchmarkHelper`. The machine scored 4/5 (memory bandwidth 39.1 % of reference), so the figures are conservative on storage-heavy calls; the composed placeholders they replaced were 2.1–3.6× lighter.
9. **The staking and broker are mocked in the pallet's tests** (`mock.rs`), to the rules read from `energy-generation` and `energy-broker` at `423740e`. The runtime adapter has since been driven end to end on a dev chain from genesis (2026-09-16, runtime spec 213, this branch): curve and pool fees → `stake` (real `bond` + `cooperate`) → era exposure → `payout_stakers` paid the vault LNRG → `harvest` → `compound` against a dry, a shallow and a full broker (FM-T4 as in `fm_t4_dry_broker_keeps_lnrg_accrued`) → burn on the pool → dormancy → `retire` (`chill` + `unbond`, FM-T8) → `finalize_retirement` (`withdraw_unbonded`) → principal burned in impact-capped slices. Two things it found are items 10 and 11.
10. **There is no 21.4-day gate** (§2.2, FM-T2). `NacManaging::on_new_account` mints the NAC NFT for every new account and grants `Vanguard(1)` reputation at once; `validate` pins each validator's `min_coop_reputation` at `Vanguard(1)`; `cooperate` refuses only `record.reputation < min_coop_reputation`. A vault created by its first fee cooperates in that same `stake`. The stale path (`CooperationStale`, `retarget`) is still reachable — a slashed vault reproduces it — and stays; §2.2's timing argument was wrong and has been reworded to "below the target's `min_coop_reputation`" (§2.2, §6.2, FM-T2); C2's accrual arithmetic stays as the record.
11. **A from-genesis chain does not run `FundLaunchTreasuryVault`** (`frame_system` writes `LastRuntimeUpgrade` at genesis). See §9.6: without the ED buffer the retirement cannot close — the dev-chain run stranded 2.187 VTRS of `pending_burn` on slice 19 with `Token(Frozen)`. On a chain that receives the pallet by upgrade the migration runs and none of this arises; the §9.6 fix (applied) makes the pallet not depend on which path it took.
12. **Where pallet code lives, and the one rule a later session must not get wrong.** *Superseded on 2026-09-17, when the Foundation split #100 into three: the benchmark repairs against `develop`, the pallets into `power-plant-experimental`, and #100 itself reduced to testnet wiring that depends on experimental by a pinned commit.* The earlier form of this item — `design/launch-treasury` as the submission, `feature/solver-marketplace` as what runs, migrations only in the fork and never ported back — rested on the premise that no upstream chain would ever hold old-shape state. Under the split that premise is gone: experimental is the pallets' home and the chains that run it (the dev chain, testnet) do hold state. The rules now:

    - **One home for pallet code: `Vitreus-Foundation/power-plant-experimental`.** `pallets/vitreus-dex`, `pallets/launchpad` and, once its exchange trait is pallet-local (§9.7 below), `pallets/launch-treasury` — sources, tests, specs, the review, measured weights, **and their migrations**. A migration is a `VersionedMigration` inside its pallet: it runs where the on-chain version says so and is a no-op on a fresh chain, so a crate that ships it is right for every consumer. Storage versions have one truth, the crate's.
    - **Consumers pin a commit SHA and are never where a fix is made.** The Foundation's `power-plant` (#100 and whatever follows it) and our fork `feature/solver-marketplace` depend on experimental as `git = "…/power-plant-experimental", rev = "<sha>"` — a SHA, never a branch, and a tag only once the Foundation cuts one. A bug in a pallet is fixed by a commit on experimental and reaches a chain by bumping that consumer's pin. Nothing is patched in a consumer: a consumer cannot hold a path crate and a git crate of the same name, and a fix that lives only in one consumer is the failure the old rule was written to prevent, now with four places instead of two. If it is tempting to fix it "just in the fork for now", that is the moment to commit on experimental and bump the pin.
    - **A consumer's own tree holds only what is not pallet code:** `construct_runtime!` wiring, `Config` impls, the runtime adapters (`EnergyGenerationStaking`, the broker exchange adapter), `FundLaunchTreasuryVault` and the runtime's `Unreleased` list, `spec_version`, and the fork's `dev-ops/preflight/` records. The fork's local `pallets/{vitreus-dex,launchpad,launch-treasury}` directories are deleted when its pin lands, and `design/launch-treasury` and the pallet copies on `pr/dex-launchpad` retire with them.
    - **The fork may run ahead, by SHA.** A dev-chain trial pins an experimental commit the Foundation has not tagged; the pre-flight (`dev-ops/preflight/`) runs against that SHA exactly as it did against a branch. When the Foundation is ready, #100's pin (or the tag) moves to the same commit. Two consumers on different SHAs is a normal state; a consumer on a SHA that does not exist on experimental's history is not.
    - **Experimental's history is the record.** `git log` there answers "is this fixed"; the `-x` cherry-pick lines of the old rule are no longer how anything crosses. A release is a tag on a green commit — experimental carries CI so that every pinnable commit has been built and tested as its consumers will build it.

    What crosses between the two consumers is therefore nothing but the pin and the runtime-side code around it. Dependency alignment is by exact source string: experimental's `[workspace.dependencies]` name polkadot-sdk as power-plant does (`git = "https://github.com/paritytech/polkadot-sdk", branch = "stable2407"`), and its `Cargo.lock` resolves to the same commit, so that a consumer's build sees one `frame-support`, not two.
