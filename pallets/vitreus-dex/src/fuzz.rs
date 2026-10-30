//! Property-based fuzzing of the DEX on the runtime-like mock
//! (`new_runtime_like_ext`): the runtime's existential deposit and asset
//! deposits, every asset but one non-sufficient, twenty-byte accounts.
//! The solver marketplace's harness (`pallet_solver_marketplace`'s `fuzz`
//! module) runs the same world with intents on top; this one is the DEX
//! alone.
//!
//! **Programs.** A `Vec<Op>` of trades, liquidity moves and fee claims,
//! plus what anyone outside the pallet can do to an account the pallet
//! owns or pays: send it one unit of any asset, fill its consumer slots,
//! reap their own account and come back later. Every
//! `Op` names an actor or pool by index modulo what exists, so any
//! subsequence is still a valid program and shrinking always lands on one
//! that runs.
//!
//! **What is judged.**
//! - After every op, `do_try_state` (what each pallet-owned account must
//!   hold) and native conservation: nothing in the DEX mints or burns
//!   native, so a drop in total issuance is native destroyed as dust.
//! - Every op's result: `Ok`, or an error that is the op's own business
//!   (`SlippageExceeded`, `InsufficientShares`…). Anything
//!   else is a refusal by `balances` or `assets` that the pallet did not
//!   plan for, or one of its own errors masking one.
//! - At the end, liveness: each LP can withdraw, and every recorded fee can
//!   be paid.
//!
//! **Modes.** `Mode::today` runs the chain as it is. `Mode::endowed` first
//! gives the fee escrow an ED, so the failures that follow only from it
//! having none do not drown out the rest.
//!
//! **Where it runs.** `cargo test` runs the properties in seconds.
//! `survey_*` (ignored) runs many programs without stopping and prints every
//! distinct finding with the program that produced it first:
//! `cargo test -p pallet-vitreus-dex --release -- --ignored survey --nocapture`.

use crate::{mock::*, *};
use frame_support::{
    storage::{with_transaction, TransactionOutcome},
    traits::tokens::Preservation,
};
use proptest::{
    prelude::*,
    strategy::ValueTree,
    test_runner::{Config as RunnerConfig, RngAlgorithm, TestRng, TestRunner},
};
use sp_runtime::{DispatchError, DispatchResult};
use std::collections::BTreeMap;

const EVE: AccountId = acc(5);
/// Four actors with every asset, and EVE with native only, as a signer
/// paying fees in VTRS can be.
const ACTORS: [AccountId; 5] = [ALICE, BOB, CHARLIE, DAVE, EVE];
/// A non-sufficient asset no actor holds at the start.
const FRESH_ID: u32 = 200;
/// A non-sufficient asset every actor holds whose minimum balance is 10^9,
/// as one testnet asset's is, pooled thinly so trades reach it.
const HIGH_MIN_ID: u32 = 201;
const HIGH_MIN: u128 = 1_000_000_000;
/// The pools, each against native: two non-sufficient assets every actor
/// holds, the one sufficient asset, a seeded reserved asset with a creator,
/// a non-sufficient asset only ALICE holds, and the high-minimum asset.
const POOL_ASSETS: [u32; 6] = [USDC_ID, VNRG_ID, SUFFICIENT_ID, LAUNCH_ID, FRESH_ID, HIGH_MIN_ID];

// ---- programs ---------------------------------------------------------------

/// An account the pallet owns or pays, as an outsider can address it.
#[derive(Clone, Debug)]
enum Target {
    FeeEscrow,
    Pool(u8),
    /// The pre-seed sweep's and default protocol fee's recipient.
    Treasury,
    Creator,
}

#[derive(Clone, Debug)]
enum Op {
    Swap {
        who: u8,
        pool: u8,
        native_in: bool,
        amount: u128,
    },
    AddLiquidity {
        who: u8,
        pool: u8,
        amount: u128,
    },
    /// Withdraw `permille` of the actor's position.
    RemoveLiquidity {
        who: u8,
        pool: u8,
        permille: u16,
    },
    Advance {
        blocks: u8,
    },
    WithdrawProtocolFees,
    ClaimCreatorFees,
    // Outsiders.
    /// Send `amount` of pool asset `asset` (or native, for `None`).
    Donate {
        who: u8,
        target: Target,
        asset: Option<u8>,
        amount: u128,
    },
    FillSlots {
        target: Target,
    },
    FillOwnSlots {
        who: u8,
    },
    /// Send away all native, letting the account die if nothing holds it.
    Reap {
        who: u8,
    },
    Revive {
        who: u8,
    },
}

/// The edges the account rules turn on, and log-uniform amounts between.
fn amount() -> impl Strategy<Value = u128> {
    let ed = RUNTIME_ED;
    prop_oneof![
        3 => prop::sample::select(vec![
            1,
            ed - 1,
            ed,
            ed + 1,
            2 * ed,
            5 * ed,
            10 * ed,
            UNIT,
            10 * UNIT,
            1_000 * UNIT,
        ]),
        2 => (0u32..25, 1u128..10).prop_map(|(e, m)| m * 10u128.pow(e)),
    ]
}

fn target() -> impl Strategy<Value = Target> {
    prop_oneof![
        Just(Target::FeeEscrow),
        any::<u8>().prop_map(Target::Pool),
        Just(Target::Treasury),
        Just(Target::Creator),
    ]
}

fn op(hostile: bool) -> BoxedStrategy<Op> {
    let honest = prop_oneof![
        3 => (any::<u8>(), any::<u8>(), any::<bool>(), amount())
            .prop_map(|(who, pool, native_in, amount)| Op::Swap { who, pool, native_in, amount }),
        2 => (any::<u8>(), any::<u8>(), amount())
            .prop_map(|(who, pool, amount)| Op::AddLiquidity { who, pool, amount }),
        2 => (any::<u8>(), any::<u8>(), prop::sample::select(vec![1u16, 500, 999, 1000]))
            .prop_map(|(who, pool, permille)| Op::RemoveLiquidity { who, pool, permille }),
        3 => (1u8..20).prop_map(|blocks| Op::Advance { blocks }),
        1 => Just(Op::WithdrawProtocolFees),
        1 => Just(Op::ClaimCreatorFees),
    ];
    if !hostile {
        return honest.boxed();
    }
    prop_oneof![
        12 => honest,
        2 => (any::<u8>(), target(), prop::option::of(any::<u8>()), amount())
            .prop_map(|(who, target, asset, amount)| Op::Donate { who, target, asset, amount }),
        1 => target().prop_map(|target| Op::FillSlots { target }),
        1 => any::<u8>().prop_map(|who| Op::FillOwnSlots { who }),
        1 => any::<u8>().prop_map(|who| Op::Reap { who }),
        1 => any::<u8>().prop_map(|who| Op::Revive { who }),
    ]
    .boxed()
}

fn program(hostile: bool) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op(hostile), 1..80)
}

// ---- the world --------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Mode {
    endowed: bool,
    hostile: bool,
}

impl Mode {
    fn today(hostile: bool) -> Self {
        Mode { endowed: false, hostile }
    }
    fn endowed(hostile: bool) -> Self {
        Mode { endowed: true, hostile }
    }
}

fn kind(id: u32) -> NativeOrAssetId {
    NativeOrAssetId::WithId(id)
}
fn native() -> NativeOrAssetId {
    NativeOrAssetId::Native
}
fn actor(i: u8) -> AccountId {
    ACTORS[i as usize % ACTORS.len()]
}
fn pool_asset(i: u8) -> u32 {
    POOL_ASSETS[i as usize % POOL_ASSETS.len()]
}
fn pool_account(asset: u32) -> AccountId {
    VitreusDex::pool_account_for(native(), kind(asset))
}

fn resolve(target: &Target) -> AccountId {
    match target {
        Target::FeeEscrow => VitreusDex::fee_escrow_account(),
        Target::Pool(n) => pool_account(pool_asset(*n)),
        Target::Treasury => TREASURY,
        Target::Creator => CREATOR,
    }
}

fn setup(mode: Mode) {
    use frame_support::assert_ok;
    let root = RuntimeOrigin::root;
    assert_ok!(Balances::force_set_balance(root(), EVE, 10_000 * UNIT));
    assert_ok!(Balances::force_set_balance(root(), CREATOR, 10 * UNIT));
    for id in [LAUNCH_ID, FRESH_ID] {
        assert_ok!(Assets::force_create(root(), id, ALICE, false, 1));
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), id, ALICE, RUNTIME_LIKE_BALANCE));
    }
    for who in [BOB, CHARLIE, DAVE] {
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, who, 10_000 * UNIT));
    }
    assert_ok!(Assets::force_create(root(), HIGH_MIN_ID, ALICE, false, HIGH_MIN));
    for who in [ALICE, BOB, CHARLIE, DAVE] {
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), HIGH_MIN_ID, who, 1_000 * HIGH_MIN));
    }
    // Plain pools route a protocol slice of every native trade.
    assert_ok!(VitreusDex::set_default_fee_routing(root(), 10, 5, 0, 0));
    for id in [USDC_ID, VNRG_ID, SUFFICIENT_ID, FRESH_ID] {
        assert_ok!(VitreusDex::create_pool(root(), native(), kind(id), 10));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            kind(id),
            10_000 * UNIT,
            10_000 * UNIT,
            0,
            0
        ));
    }
    assert_ok!(VitreusDex::create_pool(root(), native(), kind(HIGH_MIN_ID), 10));
    assert_ok!(VitreusDex::add_liquidity(
        RuntimeOrigin::signed(ALICE),
        native(),
        kind(HIGH_MIN_ID),
        10 * UNIT,
        30 * HIGH_MIN,
        0,
        0
    ));
    // The reserved asset is seeded, so its pool routes a creator slice too.
    assert_ok!(VitreusDex::set_default_fee_routing(root(), 3, 15, 5, 0));
    assert_ok!(<VitreusDex as ReservedPoolSeeder<
        AccountId,
        NativeOrAssetId,
        u128,
        u64,
    >>::seed_reserved_pool_for(
        &ALICE, kind(LAUNCH_ID), native(), 200_000 * UNIT, 1_000 * UNIT, 3
    ));
    if mode.endowed {
        assert_ok!(Balances::force_set_balance(
            root(),
            VitreusDex::fee_escrow_account(),
            RUNTIME_ED
        ));
    }
}

// ---- findings ---------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Finding {
    /// An op failed with an error that is not its own business.
    Refused { op: &'static str, error: String },
    /// `do_try_state` failed after an op.
    Invariant { op: &'static str, broke: String },
    /// Native left total issuance (dust) during an op.
    Burned { op: &'static str },
    /// At the end, something that must close could not.
    Stuck { what: &'static str, error: String },
}

/// Errors an op may return as its own business. Everything else is a
/// finding.
fn ordinary(op: &'static str, e: &DispatchError) -> bool {
    use Error::*;
    let own = |errs: &[Error<Test>]| {
        errs.iter().any(|x| {
            DispatchError::from(Error::<Test>::decode(&mut &x.encode()[..]).unwrap()) == *e
        })
    };
    if own(&[ZeroAmount, PoolNotFound, Overflow]) {
        return true;
    }
    match op {
        "swap" => own(&[
            SlippageExceeded,
            InsufficientLiquidity,
            ReserveBelowMinimum,
            RecipientCannotReceive,
        ]),
        "add_liquidity" => own(&[SlippageExceeded, InsufficientInitialLiquidity]),
        "remove_liquidity" => own(&[
            InsufficientShares,
            PoolLocked,
            SlippageExceeded,
            ReserveBelowMinimum,
            RecipientCannotReceive,
        ]),
        _ => false,
    }
}

struct World {
    /// Each finding and the index of the op that first produced it (the
    /// program's length for one found while draining).
    findings: BTreeMap<Finding, usize>,
    /// The op being run.
    at: usize,
    /// The invariant `do_try_state` reported after the previous op, so a
    /// break is noted once, on the op that caused it.
    broken: Option<String>,
}

impl World {
    fn note(&mut self, f: Finding) {
        let at = self.at;
        self.findings.entry(f).or_insert(at);
    }

    fn check_invariants(&mut self, op: &'static str) {
        let now = VitreusDex::do_try_state().err().map(|e| format!("{e:?}"));
        if let Some(broke) = &now {
            if self.broken.as_ref() != Some(broke) {
                self.note(Finding::Invariant { op, broke: broke.clone() });
            }
        }
        self.broken = now;
    }

    /// Run one op; `None` when the op had nothing to act on.
    fn step(&mut self, op: &Op) -> Option<(&'static str, DispatchResult)> {
        let signed = |who: AccountId| RuntimeOrigin::signed(who);
        Some(match op.clone() {
            Op::Swap { who, pool, native_in, amount } => {
                let who = actor(who);
                let asset = kind(pool_asset(pool));
                let (a_in, a_out) = if native_in { (native(), asset) } else { (asset, native()) };
                let amount = affordable(&who, &a_in, amount)?;
                (
                    "swap",
                    VitreusDex::swap_exact_tokens_for_tokens(
                        signed(who),
                        a_in,
                        a_out,
                        amount,
                        1,
                        who,
                    ),
                )
            },
            Op::AddLiquidity { who, pool, amount } => {
                let who = actor(who);
                let asset = kind(pool_asset(pool));
                let amount = affordable(&who, &native(), amount)?;
                let amount = affordable(&who, &asset, amount)?;
                (
                    "add_liquidity",
                    VitreusDex::add_liquidity(signed(who), native(), asset, amount, amount, 0, 0),
                )
            },
            Op::RemoveLiquidity { who, pool, permille } => {
                let who = actor(who);
                let asset = kind(pool_asset(pool));
                let pair = VitreusDex::canonical_pair(native(), asset.clone());
                let shares = LiquidityPositions::<Test>::get(who, &pair)?.shares;
                let shares = (shares * permille as u128 / 1000).max(1);
                (
                    "remove_liquidity",
                    VitreusDex::remove_liquidity(signed(who), native(), asset, shares, 0, 0),
                )
            },
            Op::Advance { blocks } => {
                System::set_block_number(System::block_number() + blocks as u64);
                return None;
            },
            Op::WithdrawProtocolFees => {
                if ProtocolFeesUnclaimed::<Test>::get() == 0 {
                    return None;
                }
                ("withdraw_protocol_fees", VitreusDex::withdraw_protocol_fees(signed(DAVE)))
            },
            Op::ClaimCreatorFees => {
                let pair = VitreusDex::canonical_pair(kind(LAUNCH_ID), native());
                if CreatorFeesUnclaimed::<Test>::get(pair) == 0 {
                    return None;
                }
                (
                    "claim_pool_creator_fees",
                    VitreusDex::claim_pool_creator_fees(signed(CREATOR), kind(LAUNCH_ID)),
                )
            },
            Op::Donate { who, target, asset, amount } => {
                let who = actor(who);
                let to = resolve(&target);
                let asset = asset.map(|a| kind(pool_asset(a))).unwrap_or_else(native);
                let amount = affordable(&who, &asset, amount)?;
                // An outsider's own transfer; its failure is not the pallet's.
                let _ = with_transaction(|| {
                    let r = match asset {
                        NativeOrAssetId::Native => {
                            Balances::transfer_allow_death(signed(who), to, amount)
                        },
                        NativeOrAssetId::WithId(id) => {
                            Assets::transfer(signed(who), id, to, amount)
                        },
                    };
                    match r {
                        Ok(()) => TransactionOutcome::Commit(Ok(())),
                        Err(e) => TransactionOutcome::Rollback(Err::<(), DispatchError>(e)),
                    }
                });
                return None;
            },
            Op::FillSlots { target } => {
                fill_consumer_slots(resolve(&target));
                return None;
            },
            Op::FillOwnSlots { who } => {
                fill_consumer_slots(actor(who));
                return None;
            },
            Op::Reap { who } => {
                let who = actor(who);
                let _ = Balances::transfer_all(signed(who), TREASURY, false);
                return None;
            },
            Op::Revive { who } => {
                let _ = Balances::force_set_balance(
                    RuntimeOrigin::root(),
                    actor(who),
                    Balances::free_balance(actor(who)) + 100 * UNIT,
                );
                return None;
            },
        })
    }

    /// Run a program, judging every op, then try to close everything.
    fn run(&mut self, ops: &[Op]) {
        for (at, op) in ops.iter().enumerate() {
            self.at = at;
            let issuance = Balances::total_issuance();
            let revive = matches!(op, Op::Revive { .. });
            if let Some((name, result)) = self.step(op) {
                if let Err(e) = result {
                    if std::env::var("DEX_FUZZ_TRACE").is_ok() {
                        eprintln!("TRACE {name} {e:?} ordinary={}", ordinary(name, &e));
                    }
                    if !ordinary(name, &e) {
                        self.note(Finding::Refused { op: name, error: format!("{e:?}") });
                    }
                }
                self.check_invariants(name);
                if Balances::total_issuance() < issuance {
                    self.note(Finding::Burned { op: name });
                }
            } else {
                if !revive && Balances::total_issuance() < issuance {
                    self.note(Finding::Burned { op: "outsider" });
                }
                self.check_invariants("outsider");
            }
        }
        self.at = ops.len();
        self.drain();
    }

    /// Everything that must close.
    fn drain(&mut self) {
        System::set_block_number(System::block_number() + 1_000);
        let stuck = |what: &'static str, r: DispatchResult, me: &mut Self| {
            if let Err(e) = r {
                me.note(Finding::Stuck { what, error: format!("{e:?}") });
            }
        };
        if ProtocolFeesUnclaimed::<Test>::get() > 0 {
            stuck(
                "the protocol fee withdrawal",
                VitreusDex::withdraw_protocol_fees(RuntimeOrigin::signed(DAVE)),
                self,
            );
        }
        let pair = VitreusDex::canonical_pair(kind(LAUNCH_ID), native());
        if CreatorFeesUnclaimed::<Test>::get(pair) > 0 {
            stuck(
                "the creator fee claim",
                VitreusDex::claim_pool_creator_fees(
                    RuntimeOrigin::signed(CREATOR),
                    kind(LAUNCH_ID),
                ),
                self,
            );
        }
        // Every LP but the pool's last may leave in full; the last leaves
        // all but what the pool must keep, which is not judged here.
        let positions: Vec<_> = LiquidityPositions::<Test>::iter()
            .filter(|(_, _, p)| p.locked_until.is_none())
            .collect();
        for (who, pair, p) in positions {
            let others = LiquidityPositions::<Test>::iter()
                .filter(|(w, q, l)| *q == pair && *w != who && l.shares > 0)
                .count();
            if others == 0 {
                continue;
            }
            // An LP whose own account cannot take its payout (it sent its
            // native away) is refused by name; that is its own business.
            let exit = VitreusDex::remove_liquidity(
                RuntimeOrigin::signed(who),
                pair.0,
                pair.1,
                p.shares,
                0,
                0,
            );
            if exit != Err(Error::<Test>::RecipientCannotReceive.into()) {
                stuck("an LP's exit", exit, self);
            }
        }
        self.check_invariants("drain");
    }
}

/// `amount` capped to half of what `who` holds of `asset`; `None` if that
/// is nothing.
fn affordable(who: &AccountId, asset: &NativeOrAssetId, amount: u128) -> Option<u128> {
    use frame_support::traits::fungibles::Inspect;
    // What `who` can spend and keep its account, so a refusal is never the
    // actor's own balance.
    let held = <Test as Config>::Assets::reducible_balance(
        asset.clone(),
        who,
        Preservation::Preserve,
        frame_support::traits::tokens::Fortitude::Polite,
    );
    let a = amount.min(held / 2);
    (a > 0).then_some(a)
}

/// One-unit balances of fresh non-sufficient assets on `who` until
/// pallet-assets would refuse to open another account there.
fn fill_consumer_slots(who: AccountId) {
    while frame_system::Pallet::<Test>::can_accrue_consumers(&who, 2) {
        let id = crate::testing::next_free_asset_id::<Test>(300);
        if Assets::force_create(RuntimeOrigin::root(), id, DAVE, false, 1).is_err()
            || Assets::mint(RuntimeOrigin::signed(DAVE), id, who, 1).is_err()
        {
            return;
        }
    }
}

fn run_program(mode: Mode, ops: &[Op]) -> BTreeMap<Finding, usize> {
    let mut world = World { findings: BTreeMap::new(), at: 0, broken: None };
    new_runtime_like_ext().execute_with(|| {
        setup(mode);
        world.run(ops);
    });
    world.findings
}

// ---- surveys ----------------------------------------------------------------

/// The smallest program found by dropping ops one at a time that still
/// produces `finding`.
fn minimise(mode: Mode, mut ops: Vec<Op>, finding: &Finding) -> Vec<Op> {
    loop {
        let before = ops.len();
        let mut i = 0;
        while i < ops.len() {
            let mut fewer = ops.clone();
            fewer.remove(i);
            if run_program(mode, &fewer).contains_key(finding) {
                ops = fewer;
            } else {
                i += 1;
            }
        }
        if ops.len() == before {
            return ops;
        }
    }
}

/// Run `cases` programs from a fixed seed and print each distinct finding:
/// how many programs hit it and a minimal program that does.
fn survey(mode: Mode, cases: u32) {
    let mut runner = TestRunner::new_with_rng(
        RunnerConfig::default(),
        TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );
    let strategy = program(mode.hostile);
    let mut seen: BTreeMap<Finding, (usize, Vec<Op>)> = BTreeMap::new();
    for _ in 0..cases {
        let ops = strategy.new_tree(&mut runner).expect("program").current();
        for (f, at) in run_program(mode, &ops) {
            let prefix = ops[..(at + 1).min(ops.len())].to_vec();
            let e = seen.entry(f).or_insert((0, prefix.clone()));
            e.0 += 1;
            if prefix.len() < e.1.len() {
                e.1 = prefix;
            }
        }
    }
    println!("\n==== {mode:?}: {cases} programs, {} distinct findings", seen.len());
    for (f, (n, ops)) in &seen {
        let ops = minimise(mode, ops.clone(), f);
        println!("\n[{n:>4}] {f:?}");
        for (k, op) in ops.iter().enumerate() {
            println!("       {k:>2}. {op:?}");
        }
    }
}

#[test]
#[ignore = "a survey, not a property: run by hand with --nocapture"]
fn survey_today_honest() {
    survey(Mode::today(false), 400);
}

#[test]
#[ignore = "a survey, not a property: run by hand with --nocapture"]
fn survey_today_hostile() {
    survey(Mode::today(true), 400);
}

#[test]
#[ignore = "a survey, not a property: run by hand with --nocapture"]
fn survey_endowed_honest() {
    survey(Mode::endowed(false), 400);
}

#[test]
#[ignore = "a survey, not a property: run by hand with --nocapture"]
fn survey_endowed_hostile() {
    survey(Mode::endowed(true), 400);
}

// ---- properties -------------------------------------------------------------

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(32)
}

fn judge(mode: Mode, ops: &[Op]) -> Result<(), TestCaseError> {
    let findings = run_program(mode, ops);
    prop_assert!(
        findings.is_empty(),
        "\n{}\nprogram:\n{}",
        findings.keys().map(|f| format!("  {f:?}")).collect::<Vec<_>>().join("\n"),
        ops.iter()
            .enumerate()
            .map(|(i, o)| format!("  {i:>2}. {o:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(())
}

proptest! {
    #![proptest_config(RunnerConfig { cases: cases(), max_shrink_iters: 4096, ..RunnerConfig::default() })]

    /// The DEX as it is, with outsiders acting on every account it owns
    /// or pays.
    #[test]
    fn fuzz_dex_runtime_like_with_outsiders(ops in program(true)) {
        judge(Mode::today(true), &ops)?;
    }

    /// The same, from a chain whose fee escrow holds an ED.
    #[test]
    fn fuzz_dex_endowed_fee_escrow_with_outsiders(ops in program(true)) {
        judge(Mode::endowed(true), &ops)?;
    }
}
