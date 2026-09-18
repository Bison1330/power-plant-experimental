# CRX — CoreXus DEX token: design specification

**Status:** design, 2026-09-18. Nothing here is built, and nothing here should be built yet: the design assumes a business — a DEX and a launchpad with volume that grows after the token exists — that does not exist today (LAUNCH_TREASURY_SPEC §11–12 record the current numbers). Every section that sizes anything says so where it does. **Depends on:** the solver marketplace, D4's `set_protocol_fee_recipient`, and the treasury's keeper path, all under review; counsel on §4, which is the same question as the redemption one; the Foundation on §6.

---

## 0. The headline: the buyback is the weakest of the four variables, and the one carrying the securities question

Two venue tokens, both funding buybacks from protocol revenue, opposite outcomes. Read from public figures on 2026-09-18:

| | HYPE (Hyperliquid) | PUMP (pump.fun) |
|---|---|---|
| Sale | None: no VC, no presale. A year of points, then 31 % of supply to ~94k users who had traded. | 33 % sold at $0.004 — 18 % private, 12.5 % public, $600M at $4B FDV, **unlocked on day one**; 13 % existing investors; 20 % team. |
| Timing | After a year of product, into a business taking share. | At the memecoin cycle's revenue peak; platform fees roughly halved over the following year (DefiLlama: $150M in Jan 2025 → $30–48M/month through 2026). |
| First holders | Users with no cost basis. | Buyers with a $0.004 basis and no lock. |
| Demand sinks | Staking tiers (5–40 % fee discount); **500k HYPE bond to deploy a HIP-3 market**; 1M HYPE to bid a HyperEVM slot; validator staking. | None. |
| Buyback | Protocol-level Assistance Fund, holds rather than burns: ~$1.3B over ~21 months on a ~$20B mcap ≈ **3.7 %/yr**. | Company-executed (a locked contract for 50 % since Apr 2026, per press): $456M, 36 % of supply, ≈ **20 %/yr** on ~$2B mcap. |
| Price | At all-time high (Sept 2026). | −52 % from high. |

**The check:** PUMP's buyback yield is about five times HYPE's, and PUMP lost. So the buyback cannot be the variable that decides the outcome. What differed is the other three: no sale (so no seller with a basis and no unlock), first holders who use the venue, and revenue that grew *after* issuance — plus sinks that make using the venue require holding the token. The buyback is the least important of the four. It is also the one that reads, in the March 2026 SEC/CFTC interpretation's own words, as "supporting secondary markets" and "enhancing token value" (§4). That is the reason this design does not lead with it, and the reason the mechanism we can make trust-free (§2) is not the mechanism this design rests on (§3).

Sources: [Decrypt on the HYPE genesis](https://decrypt.co/294067/hyperliquid-airdrop-1-6-billion); [CoinDesk on the PUMP sale](https://www.coindesk.com/markets/2025/07/12/pumpfun-swiftly-raises-500m-in-public-sale-at-4b-fully-diluted-valuation); [HIP-3 docs](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals); buyback totals from KuCoin/AMINA press summaries (the AF's hold-vs-burn status and PUMP's "locked contract" are press claims not verified on chain).

---

## 1. Distribution

**No presale.** Three grounds: it is the clearest securities offering (money in, promise of the issuer's efforts); low-float/high-FDV is read post-2024 as the setup for the dump it usually is; and it manufactures the one holder class — cost-basis sellers — that §0 shows decides the outcome. The reference case funded its first year without one. We are self-funded and the audit is $10–15k. Nothing a sale answers is a need we have.

**Supply:** 1,000,000,000 CRX, fixed, a `pallet_assets` asset minted once at genesis into pallet accounts; no mint authority afterwards.

| Tranche | Share | Recipient | Enforced by |
|---|---|---|---|
| Genesis distribution | **30 %** | Users, by points from one 6–12 month season (§1.1). Claim once, unlocked. | A claim pallet holding the tranche; a published Merkle root; a six-month window; unclaimed → future rewards. |
| Future user rewards | **40 %** | Later seasons on the same criteria, over ≥ 4 years; continuous rewards to locked LPs (HIP-2-shaped). | A release schedule in the same pallet; unreleased supply has no path out. |
| Team | **20 %** | Us. | **The L3 lock** (LAUNCHPAD_SPEC §10): pallet-held, 12-month cliff, 36-month linear vest, `claim_locked` only. Shown on the board as "200M CRX held by the pallet until block B", the way a creator's lock is. |
| Ecosystem | **10 %** | Audit, grants, listings, a broker top-up if the Foundation asks. | A multisig. The only discretionary tranche, and the smallest. |

### 1.1 Season criteria that survive Sybil on this chain

Accounts cost an existential deposit of 10⁻⁶ VTRS, so account count is free. Reputation accrues per block from creation and cannot be transferred (`pallet-reputation`), so *age* cannot be bought after the fact — but an early Sybil has old wallets. Age is therefore a multiplier, never a source. The sources are the things that cost money at the fee the pad sets:

| Signal | Weight | Why it can't be farmed below cost |
|---|---|---|
| **Fees paid**, VTRS, on the DEX and the curve | ~70 % | Points per VTRS of fee, flat. Wash-trading buys points at the fee; the worst case is that the airdrop refunds fees pro rata to whoever paid them, which is the intended outcome and the one Hyperliquid accepted. |
| **Time-weighted locked liquidity** | ~20 % | Only `lock_liquidity` positions count, weighted by lock length × size. Cannot be added, counted and removed. |
| **Committed graduations** | ~10 % | A launch that graduated with an L3 commitment. Costs `T` of real buying to fake, and a creator who fakes it has locked or burned what they faked with. |
| **Reputation age** | ×1.0–1.5 multiplier | Age at the season's *announcement* block, so it rewards being early and cannot be gamed by accounts made after. |
| **NAC** | open — §6 | If minted against identity, one-account-per-person; if not, nothing. |

Published at season start; snapshot at a block announced in advance; the list published before the claim opens.

---

## 2. The fee commitment

**Rule:** a fixed share of *protocol* revenue — everything that today reaches `ProtocolFeeRecipient`: the DEX's `protocol_bps`, the curve's protocol share, retirement dust — flows to a pallet-derived **fund account** whose only code path buys CRX on the CRX/VTRS pool in capped slices (the treasury's slice machinery, LAUNCH_TREASURY_SPEC §6.4) and holds it. **75 %** of protocol revenue to the fund, 25 % retained as operating income; both numbers stated in public.

**Code, not policy — with two caveats.** Today the recipient is a setter (`set_protocol_fee_recipient`, D4, `ManageOrigin`). The rule becomes consensus code when the runtime binds `ProtocolFeeRecipient` to the fund account as a constant and the setter is removed; the fund pallet exposes `buy(slice)` (anyone) and nothing that moves VTRS or CRX to a chosen address — verifiable from metadata exactly as `/treasury` proves "no exit" today. The only path to change it is a runtime upgrade, in public, by governance. The caveats: the setter is D4 code under review, so this is designed now and built after; and governance *can* upgrade — the claim is "no key and no call", never "immutable", the same claim `/treasury` makes.

**What here is a promise:** the 75/25 split (a constant we pick once); what the 25 % is spent on. **What is code:** every VTRS that enters the fund leaves it only as a market buy of CRX.

---

## 3. Buyback, burn, or staking — and the sinks in detail

§0 says the evidence does not separate burn from hold: the holder won, the burner lost, and neither was the cause. Choose on other grounds:

- **Burn:** irreversible; reads cleanest as "no distribution"; it is what PUMP does and converts revenue into nothing the protocol can use later.
- **Hold in the fund:** keeps bought CRX on the protocol's balance sheet for validator incentives, LP rewards, or a burn if governance ever votes one; it is what the surviving case does.
- **Staking:** the only one of the three that creates *demand* rather than reducing supply, and the thing every surviving venue token has and every dead launched token lacked (the launch-outcome research: Heaven, Boop, Bonk.fun, Flaunch all had buybacks and no reason to hold).

Decision: **hold in the fund; no burn; the design rests on the sinks.** The buyback supports; the sinks carry. The line every sink must hold: **a creator's terms stay uniform** (LAUNCHPAD_SPEC §1.4, §10.4). A sink may make *using the venue as a trader, solver, keeper or deployer* require CRX; it may never make a *launch* cheaper, better-routed, or differently fee'd for holding CRX.

### 3.1 Solver bond in CRX

*What it is.* The solver marketplace already bonds solvers: `register_solver` transfers `current_solver_bond()` of the native asset into a per-solver escrow, slashable via `slash_solver`, sized by `set_solver_bond_amount` (`vitreus-dex` l.1138–1144, l.1683). The sink is denominating that bond in CRX instead of VTRS: a solver must buy and lock CRX to fill intents.

*What it takes to build.* The bond currency is `T::NativeAsset` in one transfer and one refund; parameterising it as `T::SolverBondAsset: Get<AssetKind>` is ~30 lines plus the slash path paying out in the bond asset. Existing solvers migrate by re-registering; a transition where either asset is accepted for one epoch avoids a cliff.

*Touches code under review.* **Yes** — `register_solver`, `deregister_solver`, `slash_solver` are the solver marketplace, out of the treasury audit's scope but inside the DEX PR. Build after the PR lands.

*Uniform creator terms.* Holds. Solvers are traders' counterparties; nothing about a launch changes.

*How much demand.* Bond × active solvers. HIP-3's 500k HYPE bond is the reference; at our scale the bond should be sized so that a solver's expected fill income over a month exceeds the bond's cost of capital, and no larger — an over-sized bond has no solvers, which is worse than a small sink.

### 3.2 Keeper bounties to stakers

*What it is.* The treasury pays `keeper_bounty_bps` of each sale and slice to whoever calls `compound` (LAUNCH_TREASURY_SPEC §6.4); L3's `disburse` and the retirement slices are permissionless and unpaid. The sink: bounties are paid **only to callers with ≥ `KeeperStake` CRX staked** — a stake gates the right to be paid, not the right to call (anyone may still call; an unstaked caller earns nothing, so the calls stay permissionless and the invariant "nothing leaves to a caller-chosen address except the bounty" is unchanged).

*What it takes to build.* A `Stakes: Map<AccountId, Balance>` in a small `crx-stake` pallet with `stake`/`unstake` (7-day unbond, so a keeper can't stake, claim, unstake in one block), and one read in the bounty path: `if stake(caller) >= KeeperStake { pay } else { skip }`. ~150 lines plus the pallet scaffold.

*Touches code under review.* **Yes**, one line in `compound`'s bounty payment (launch-treasury, experimental #2). Build after.

*Uniform creator terms.* Holds. Keepers serve every launch alike; a launch cannot buy a better keeper.

*How much demand.* Small — tens of keepers — but it is the sink that ties CRX to the pad's *operation*: the people who keep the treasury and the commitments running are CRX holders by construction.

### 3.3 Deployment rights (our HIP-3)

*What it is.* Today every launch is quoted in VTRS and every pool is native-paired. The launchpad's next surface is custom-quote venues — a launch quoted in a tokenized asset, a stablecoin, or another launch token (the StonkFun shape; pump.fun's "custom pairs" since 2026). The sink: **deploying a quote venue requires a CRX bond**, slashable by governance for a venue that lists an asset with no liquidity or misrepresents its quote. Launching *on* a venue costs no CRX; only *creating the venue* does.

*What it takes to build.* Substantial: a `Venues` registry in the launchpad (quote asset, bond, deployer, status), `create_launch` gaining a venue id, the curve math and seed path generalised from `Native` to `quote`, the treasury's slice and the L3 burn path following the quote asset. Weeks, not days; it is a feature in its own right and this document only fixes that CRX is its bond.

*Touches code under review.* **Yes, heavily** — `create_launch`, `do_seed`, the DEX's `seed_reserved_pool_for`. Not before the PRs land.

*Uniform creator terms.* Holds, with care: terms must be uniform *within a venue*, and every venue's terms must be the same protocol `Params` snapshot. A venue deployer sets the quote asset and nothing else — no fee, no share, no target. If a deployer could set terms, the deployer would be a pad, and the line would be gone.

*How much demand.* Bond × venues. This is the sink with the reference case's shape (500k HYPE per HIP-3 market, 1M per HyperEVM slot) and the only one whose demand scales with the product's success rather than its operation.

### 3.4 Trader fee tiers

*What it is.* A trader's DEX swap fee reduced by a staked-CRX tier (Hyperliquid: 5–40 % off across 10 to 500k HYPE). The discount comes out of the **pool's share** of the fee, never the routed slices — the protocol, creator and treasury bps are untouched, so no constituency's income moves and the treasury's §11 arithmetic is unchanged.

*What it takes to build.* A tier read in `do_swap` (`fee = tier × (1 − discount(stake(who)))` applied to the pool's share only) and the `crx-stake` pallet of §3.2. ~60 lines in the DEX.

*Touches code under review.* **Yes** — `do_swap` is D4/D6/D9 territory and the exact path R6, R7 and Finding 14 live in. Last to build, after everything else has landed, and re-audited.

*Uniform creator terms.* Holds: the discount is on the trader's side of the trade and applies identically on every pool. It must not be extended to the curve — a discount on `buy` would make holding CRX a better *launch* outcome for a creator's bundle and would breach the line.

*How much demand.* The broadest: every active trader has a reason to hold some. It is also the one with the weakest legal reading (a fee discount is the classic "digital tool" utility) and the strongest, which is why it's last to build and first to describe.

### 3.5 Summary

| Sink | Demand | Build | Touches review | Uniform terms |
|---|---|---|---|---|
| Solver bond | medium, operational | small | DEX solver calls | holds |
| Keeper bounties to stakers | small, operational | small + stake pallet | one line in `compound` | holds |
| Deployment rights | scales with product | large, a feature | launchpad + DEX seed | holds, if deployers set only the quote asset |
| Trader fee tiers | broad | small, in the hot path | `do_swap` | holds; never on the curve |

Order: stake pallet and keeper bounties first (smallest, ties CRX to operation); solver bond second; fee tiers when the DEX PR has landed and been re-audited; deployment rights when the product needs venues. None before the PRs.

---

## 4. Regulatory shape

**What the reference cases do.** Hyperliquid: no sale; an offshore Foundation airdropped; the product geo-blocks US users; the Assistance Fund is protocol code; the token's stated uses are staking, fee tiers and deployment bonds; "revenue share" is not a phrase they use. Pump.fun: a sale that excluded US and UK persons; a buyback the company announces and executes; no utility.

**The March 17, 2026 interpretation** turns on issuer representations: an investment contract exists where the issuer induces investment through representations or promises to undertake *essential managerial efforts* — building functionality, **supporting secondary markets, enhancing token value**, driving adoption — such that purchasers reasonably expect profit; a *digital tool* is a token whose purpose is functional access or usage; and revenue-sharing tokenomics, price-appreciation marketing or a team-driven roadmap can collapse a collectible or tool back into an investment contract. Against this design:

- **A fee-funded buyback is "supporting secondary markets" in the interpretation's words.** Executing it in consensus code changes *who* undertakes the effort — a real distinction and the one Hyperliquid relies on — but not whether the effort exists, and we wrote and deployed the code.
- **A no-sale airdrop** removes "investment of money" for recipients; it does not for anyone who later buys CRX on the DEX.
- **The sinks are the digital-tool argument.** The more the token is needed to *use* the venue — bond, stake, deploy, discount — the stronger that reading, and §3 is ordered by it.

**The conservative version:** no sale, ever; claims closed to US and UK persons, geo-gated at the claim UI, as PUMP's sale was; the fund described as what the code does, never as a return; no "revenue share", "yield", "APR" or price language anywhere we control; utilities first in every description; team tokens pallet-locked so the roadmap is visibly not funded by selling; and the Reg Crypto Assets proposal's investment-contract safe harbor (issuer's essential managerial efforts cease) as the destination once the DEX's parameters are governance's and not ours. Counsel reads §2 on the same day it reads the redemption question (LAUNCH_TREASURY_SPEC, and the redemption doc); they are one question.

---

## 5. What we hold back

30 %: the team's 20 % and the ecosystem 10 %.

**For:** a self-funded team with no sale has no other income; the ecosystem tranche pays for the audit, listings and a broker top-up if asked; a team position that is *locked and visible* is the opposite of the low-float pattern. **Against:** any insider tranche is the overhang the reference case is praised for avoiding — except that it didn't avoid it: Hyperliquid's team holds 23.8 %, vested; the interpretation reads a team allocation as evidence of managerial centrality; 30 % held back is 30 % not with users.

Decision: hold back 30 %; the team's 20 % under the L3 lock so the board shows it as held supply the way it shows a creator's; the 10 % as the only discretionary money in the design.

---

## 6. The NAC question — one question for the Foundation

What the chain shows: `pallet-nac-managing` mints NAC NFTs with a `nac_level` (`mint(origin, nac_level, owner)`, `AdminOrigin = EnsureRoot` on the runtime); on mainnet at block 12,665,929 there are **1,568 NAC holders** (1,391 at level 1, 177 at level 2), against 683 cooperators and 76 validators; `energy-generation` binds `ValidatorNacLevel = NacManaging`, so the NAC already gates who may validate. What the chain does not show is whether a NAC was minted to a *person* or to an *address*.

The question, as one question:

> **Is a NAC minted against a verified identity — one per natural person or entity, after a KYC/KYB check the Foundation performs or relies on — such that two NACs held by the same person would be a policy violation the Foundation would act on; and if so, does the Foundation permit third-party pallets on the chain to read `UsersNft` as a proof-of-personhood signal?**

If yes: a NAC is the one Sybil-proof signal on the chain, and the season gives NAC holders a personhood weight (one share of a per-person pool, alongside the fee-paid weight) that no wash volume can buy. If no — NACs are minted to addresses on request, or to validators' operational keys — the NAC is a validator-eligibility badge and §1.1 stands as written, fees-paid and age only.

---

## 7. Code, or a promise we keep

| | Enforced by code | A promise |
|---|---|---|
| Team tokens locked and vesting | L3 lock, `claim_locked` only | — |
| Fund's execution: every VTRS in leaves only as a CRX market buy | fund pallet, no exit path (after D4 lands) | the 75/25 split, chosen once |
| Future rewards released on schedule | claim pallet, no discretionary path | the season criteria, set before each season |
| Airdrop can't be farmed below cost | fees paid are the weight | the snapshot block, announced in advance |
| Team and reputation age as facts | the chain | — |
| No sale | — | **a decision, repeated every time money is short** |
| The 10 % | — | what it is spent on |
| **Revenue grows after the token exists** | — | **the whole design assumes it, and no pallet enforces it** |

The last row is §0's real lesson: the reference token was issued into a rising business. Every mechanism here amplifies volume; none creates it. The pad and the DEX have to earn the volume first. That is not tokenomics, and this document does not pretend otherwise.
