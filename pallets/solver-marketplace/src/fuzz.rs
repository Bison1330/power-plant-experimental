//! Property-based fuzzing of the marketplace and the DEX under it, on the
//! runtime-like mock (`new_runtime_like_ext`): the runtime's existential
//! deposit and asset deposits, every asset but one non-sufficient,
//! twenty-byte accounts. The DEX's own harness, without intents, is
//! `pallet_vitreus_dex`'s `fuzz` module.
//!
//! **Programs.** A `Vec<Op>` of trades, liquidity moves and the whole intent
//! lifecycle, plus what anyone outside the pallet can do to an account the
//! pallet owns or pays: send it one unit of any asset, fill its consumer
//! slots, reap their own account after a commit and come back later. Every
//! `Op` names an actor, pool or intent by index modulo what exists, so any
//! subsequence is still a valid program and shrinking always lands on one
//! that runs.
//!
//! **What is judged.**
//! - After every op, both pallets' `do_try_state` (what each pallet-owned
//!   account must hold) and native conservation: nothing in the DEX mints or burns
//!   native, so a drop in total issuance is native destroyed as dust.
//! - Every op's result: `Ok`, or an error that is the op's own business
//!   (`SlippageExceeded`, `BidWindowClosed`, `NotIntentOwner`…). Anything
//!   else is a refusal by `balances` or `assets` that the pallet did not
//!   plan for, or one of its own errors masking one.
//! - At the end, liveness: far past every deadline, each intent can be
//!   closed (refunded or slashed), each solver without commitments can
//!   leave, each LP can withdraw, and every recorded fee can be paid.
//!
//! **Modes.** `Mode::today` runs the chain as it is. `Mode::endowed` first
//! gives the intent escrow, the fee treasury and the fee escrow an ED each,
//! so the failures that follow only from those accounts having none do not
//! drown out the rest.
//!
//! **Where it runs.** `cargo test` runs the properties in seconds.
//! `survey_*` (ignored) runs many programs without stopping and prints every
//! distinct finding with the program that produced it first:
//! `cargo test -p pallet-solver-marketplace --release -- --ignored survey --nocapture`.

use crate::{mock::*, settlement::IntentStatus, *};
use frame_support::{
    storage::{with_transaction, TransactionOutcome},
    traits::tokens::Preservation,
};
use pallet_vitreus_dex::{
    CreatorFeesUnclaimed, IntentSwap, LiquidityPositions, ProtocolFeesUnclaimed, ReservedPoolSeeder,
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
/// The pools, each against native: two non-sufficient assets every actor
/// holds, the one sufficient asset, a seeded reserved asset with a creator,
/// and a non-sufficient asset only ALICE holds.
const POOL_ASSETS: [u32; 5] = [USDC_ID, VNRG_ID, SUFFICIENT_ID, LAUNCH_ID, FRESH_ID];

// ---- programs ---------------------------------------------------------------

/// An account the pallet owns or pays, as an outsider can address it.
#[derive(Clone, Debug)]
enum Target {
    IntentEscrow,
    FeeTreasury,
    FeeEscrow,
    /// The escrow of solver id `n` modulo the ids issued.
    SolverEscrow(u8),
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
    Register {
        who: u8,
    },
    Deregister {
        who: u8,
    },
    Submit {
        who: u8,
        pool: u8,
        native_in: bool,
        amount: u128,
        deadline: u8,
    },
    Cancel {
        intent: u8,
    },
    /// Bid `permille` of what settling would pay right now.
    Commit {
        who: u8,
        intent: u8,
        permille: u16,
    },
    Settle {
        intent: u8,
    },
    Slash {
        who: u8,
        intent: u8,
    },
    Refund {
        intent: u8,
    },
    Advance {
        blocks: u8,
    },
    WithdrawProtocolFees,
    ClaimCreatorFees,
    SetBond {
        amount: u128,
    },
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
        Just(Target::IntentEscrow),
        Just(Target::FeeTreasury),
        Just(Target::FeeEscrow),
        any::<u8>().prop_map(Target::SolverEscrow),
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
        2 => any::<u8>().prop_map(|who| Op::Register { who }),
        1 => any::<u8>().prop_map(|who| Op::Deregister { who }),
        6 => (any::<u8>(), any::<u8>(), any::<bool>(), amount(), 1u8..40).prop_map(
            |(who, pool, native_in, amount, deadline)| Op::Submit {
                who,
                pool,
                native_in,
                amount,
                deadline
            }
        ),
        2 => any::<u8>().prop_map(|intent| Op::Cancel { intent }),
        5 => (any::<u8>(), any::<u8>(), prop::sample::select(vec![500u16, 900, 990, 1000]))
            .prop_map(|(who, intent, permille)| Op::Commit { who, intent, permille }),
        5 => any::<u8>().prop_map(|intent| Op::Settle { intent }),
        2 => (any::<u8>(), any::<u8>()).prop_map(|(who, intent)| Op::Slash { who, intent }),
        2 => any::<u8>().prop_map(|intent| Op::Refund { intent }),
        3 => (1u8..20).prop_map(|blocks| Op::Advance { blocks }),
        1 => Just(Op::WithdrawProtocolFees),
        1 => Just(Op::ClaimCreatorFees),
        1 => amount().prop_map(|amount| Op::SetBond { amount }),
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
        Target::IntentEscrow => Marketplace::intent_escrow_account(),
        Target::FeeTreasury => Marketplace::protocol_treasury_account(),
        Target::FeeEscrow => VitreusDex::fee_escrow_account(),
        Target::SolverEscrow(n) => {
            let issued = NextSolverId::<Test>::get().max(1);
            Marketplace::solver_escrow_account(*n as u64 % issued)
        },
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
        for who in [
            Marketplace::intent_escrow_account(),
            Marketplace::protocol_treasury_account(),
            VitreusDex::fee_escrow_account(),
        ] {
            assert_ok!(Balances::force_set_balance(root(), who, RUNTIME_ED));
        }
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
    use pallet_vitreus_dex::Error as Dex;
    use Error::*;
    let own = |errs: Vec<DispatchError>| errs.contains(e);
    let dex = |errs: Vec<Dex<Test>>| own(errs.into_iter().map(Into::into).collect());
    let mkt = |errs: Vec<Error<Test>>| own(errs.into_iter().map(Into::into).collect());
    if dex(vec![Dex::ZeroAmount, Dex::PoolNotFound, Dex::Overflow]) {
        return true;
    }
    match op {
        "swap" => dex(vec![Dex::SlippageExceeded, Dex::InsufficientLiquidity]),
        "add_liquidity" => dex(vec![Dex::SlippageExceeded, Dex::InsufficientInitialLiquidity]),
        "remove_liquidity" => {
            dex(vec![Dex::InsufficientShares, Dex::PoolLocked, Dex::SlippageExceeded])
        },
        "register_solver" => mkt(vec![SolverAlreadyRegistered]),
        "deregister_solver" => {
            mkt(vec![SolverNotRegistered, SolverNotActive, ActiveCommitmentsExist])
        },
        "submit_intent" => mkt(vec![InvalidDeadline]),
        "cancel_intent" => mkt(vec![IntentNotFound, IntentNotOpen]),
        "commit_fill" => mkt(vec![
            SolverNotRegistered,
            SolverNotActive,
            IntentNotFound,
            IntentNotOpen,
            BelowMinAmountOut,
            BidWindowClosed,
            IntentExpired,
            BidNotBetter,
        ]),
        // A trade that no longer fills the bid is the solver's market risk.
        "settle_intent" => {
            mkt(vec![IntentNotCommitted, NotCommittedSolver, SettlementWindowPassed])
                || dex(vec![Dex::SlippageExceeded, Dex::InsufficientLiquidity])
        },
        "slash_solver" => mkt(vec![IntentNotCommitted, SettlementWindowNotPassed]),
        "refund_expired_intent" => mkt(vec![IntentNotFound, IntentNotOpen, DeadlineNotPassed]),
        "set_solver_bond_amount" => mkt(vec![InvalidAmount]),
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
        let now = Marketplace::do_try_state()
            .and_then(|()| VitreusDex::do_try_state())
            .err()
            .map(|e| format!("{e:?}"));
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
        let intents = NextIntentId::<Test>::get();
        let intent_id = |i: u8| (intents > 0).then(|| i as u64 % intents);
        let solvers: Vec<AccountId> =
            Solvers::<Test>::iter_values().filter(|s| s.active).map(|s| s.account).collect();
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
            Op::Register { who } => {
                ("register_solver", Marketplace::register_solver(signed(actor(who))))
            },
            Op::Deregister { who } => {
                ("deregister_solver", Marketplace::deregister_solver(signed(actor(who))))
            },
            Op::Submit { who, pool, native_in, amount, deadline } => {
                let who = actor(who);
                let asset = kind(pool_asset(pool));
                let (a_in, a_out) = if native_in { (native(), asset) } else { (asset, native()) };
                let amount = affordable(&who, &a_in, amount)?;
                (
                    "submit_intent",
                    Marketplace::submit_intent(
                        signed(who),
                        a_in,
                        a_out,
                        amount,
                        1,
                        System::block_number() + deadline as u64,
                    ),
                )
            },
            Op::Cancel { intent } => {
                let id = intent_id(intent)?;
                let owner = Intents::<Test>::get(id)?.user;
                ("cancel_intent", Marketplace::cancel_intent(signed(owner), id))
            },
            Op::Commit { who, intent, permille } => {
                let id = intent_id(intent)?;
                let i = Intents::<Test>::get(id)?;
                let out = settlement_quote(&i).unwrap_or(1);
                let prior = FillCommitments::<Test>::get(id).map(|c| c.committed_amount_out);
                let bid = (out / 1000 * permille as u128).max(prior.map_or(1, |p| p + 1));
                let solver = *solvers.get(who as usize % solvers.len().max(1))?;
                ("commit_fill", Marketplace::commit_fill(signed(solver), id, bid))
            },
            Op::Settle { intent } => {
                let id = intent_id(intent)?;
                let c = FillCommitments::<Test>::get(id)?;
                ("settle_intent", Marketplace::settle_intent(signed(c.solver_account), id))
            },
            Op::Slash { who, intent } => {
                let id = intent_id(intent)?;
                FillCommitments::<Test>::get(id)?;
                ("slash_solver", Marketplace::slash_solver(signed(actor(who)), id))
            },
            Op::Refund { intent } => {
                let id = intent_id(intent)?;
                let owner = Intents::<Test>::get(id)?.user;
                ("refund_expired_intent", Marketplace::refund_expired_intent(signed(owner), id))
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
            Op::SetBond { amount } => (
                "set_solver_bond_amount",
                Marketplace::set_solver_bond_amount(RuntimeOrigin::root(), amount),
            ),
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

    /// Far past every deadline and window: everything that must close.
    fn drain(&mut self) {
        System::set_block_number(System::block_number() + 1_000);
        let stuck = |what: &'static str, r: DispatchResult, me: &mut Self| {
            if let Err(e) = r {
                me.note(Finding::Stuck { what, error: format!("{e:?}") });
            }
        };
        let ids: Vec<u64> = Intents::<Test>::iter_keys().collect();
        for id in ids {
            let Some(i) = Intents::<Test>::get(id) else { continue };
            match i.status {
                IntentStatus::Open => stuck(
                    "an open intent's refund",
                    Marketplace::refund_expired_intent(RuntimeOrigin::signed(i.user), id),
                    self,
                ),
                IntentStatus::Committed => stuck(
                    "a committed intent's slash",
                    Marketplace::slash_solver(RuntimeOrigin::signed(CHARLIE), id),
                    self,
                ),
                _ => {},
            }
        }
        let solvers: Vec<_> = Solvers::<Test>::iter_values().filter(|s| s.active).collect();
        for s in solvers {
            stuck(
                "an idle solver's exit",
                Marketplace::deregister_solver(RuntimeOrigin::signed(s.account)),
                self,
            );
        }
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
            stuck(
                "an LP's exit",
                VitreusDex::remove_liquidity(
                    RuntimeOrigin::signed(who),
                    pair.0,
                    pair.1,
                    p.shares,
                    0,
                    0,
                ),
                self,
            );
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

/// What settling `intent` would pay right now, swapping from the escrow.
fn settlement_quote(
    i: &crate::settlement::Intent<AccountId, NativeOrAssetId, u128, u64>,
) -> Option<u128> {
    let escrow = Marketplace::intent_escrow_account();
    with_transaction(|| {
        let out =
            <VitreusDex as IntentSwap<AccountId, NativeOrAssetId, u128>>::swap_exact_for_intent(
                &escrow,
                i.token_in.clone(),
                i.token_out.clone(),
                i.amount_in,
                1,
            );
        TransactionOutcome::Rollback(out)
    })
    .ok()
}

/// One-unit balances of fresh non-sufficient assets on `who` until
/// pallet-assets would refuse to open another account there.
fn fill_consumer_slots(who: AccountId) {
    while frame_system::Pallet::<Test>::can_accrue_consumers(&who, 2) {
        let id = pallet_vitreus_dex::testing::next_free_asset_id::<Test>(300);
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

    /// The chain as it is, with outsiders acting on every pallet account.
    #[test]
    fn fuzz_runtime_like_with_outsiders(ops in program(true)) {
        judge(Mode::today(true), &ops)?;
    }

    /// The same, from a chain whose escrows each hold an ED.
    #[test]
    fn fuzz_endowed_escrows_with_outsiders(ops in program(true)) {
        judge(Mode::endowed(true), &ops)?;
    }
}
