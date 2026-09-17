//! Tests named after LAUNCH_TREASURY_SPEC §8.4. `fm_*` are the failure
//! modes of §8.2, `t_l*` the lifecycle and invariant tests, `t_g*` the
//! governance / no-exit assertions, `i_t7` the no-drift invariant.

use super::*;
use crate::mock::*;
use frame_support::{assert_noop, assert_ok};
use pallet_launchpad::{Curves, Launches};
use pallet_vitreus_dex::{Pools, ProtocolFeesUnclaimed};
use sp_runtime::DispatchError::BadOrigin;
use std::borrow::Borrow;

// ---- helpers ---------------------------------------------------------------

fn origin(who: impl Borrow<Acc>) -> RuntimeOrigin {
    RuntimeOrigin::signed(*who.borrow())
}
fn bv(s: &[u8]) -> frame_support::BoundedVec<u8, frame_support::traits::ConstU32<50>> {
    s.to_vec().try_into().unwrap()
}
fn vtrs(who: impl Borrow<Acc>) -> u128 {
    Balances::free_balance(who.borrow())
}
fn lnrg(who: impl Borrow<Acc>) -> u128 {
    Assets::balance(LNRG_ID, who.borrow())
}
fn tok(id: LaunchId, who: impl Borrow<Acc>) -> u128 {
    Assets::balance(asset(id), who.borrow())
}
fn asset(id: LaunchId) -> u128 {
    Launches::<Test>::get(id).unwrap().asset_id
}
fn kind(id: LaunchId) -> NativeOrAssetId {
    NativeOrAssetId::WithId(asset(id))
}
fn treasury(id: LaunchId) -> TreasuryRecord<u128, u64> {
    Treasuries::<Test>::get(id).expect("treasury")
}
fn create(creator: impl Borrow<Acc>) -> LaunchId {
    let id = pallet_launchpad::NextLaunchId::<Test>::get();
    assert_ok!(Launchpad::create_launch(
        origin(creator),
        bv(b"Meme"),
        bv(b"MEME"),
        None,
        0,
        0,
        None,
        None
    ));
    id
}
fn buy(who: impl Borrow<Acc>, id: LaunchId, q: u128) {
    assert_ok!(Launchpad::buy(origin(who.borrow()), id, q, 0));
}
/// Exhaust the curve; the launch graduates into a locked pool.
fn cross(who: impl Borrow<Acc>, id: LaunchId) {
    buy(who, id, 500_000_000 * UNIT);
    assert_eq!(Curves::<Test>::get(id).unwrap().phase, Phase::Graduated);
}
fn pool_buy(who: impl Borrow<Acc>, id: LaunchId, vtrs_in: u128) {
    let who = *who.borrow();
    assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
        origin(who),
        NativeOrAssetId::Native,
        kind(id),
        vtrs_in,
        0,
        who
    ));
}
fn run_to(block: u64) {
    System::set_block_number(block);
}
fn now() -> u64 {
    System::block_number()
}
fn stake(id: LaunchId) -> DispatchResult {
    LaunchTreasury::stake(origin(KEEPER), id)
}
fn compound(id: LaunchId) -> DispatchResult {
    LaunchTreasury::compound(origin(KEEPER), id)
}
fn retire(id: LaunchId) -> DispatchResult {
    LaunchTreasury::retire(origin(KEEPER), id)
}
fn finalize(id: LaunchId) -> DispatchResult {
    LaunchTreasury::finalize_retirement(origin(KEEPER), id)
}
fn active() -> u128 {
    MockStaking::active(&vault())
}
fn cooperated() -> u128 {
    MockStaking::cooperated(&vault())
}
fn ok_state() {
    assert_ok!(LaunchTreasury::do_try_state());
}
fn drain_broker() {
    let b = vtrs(BROKER) - ED;
    assert_ok!(Balances::transfer_allow_death(origin(BROKER), ALICE, b));
}
fn fund_broker(amount: u128) {
    assert_ok!(Balances::transfer_allow_death(origin(ALICE), BROKER, amount));
}
/// A graduated launch with some pool volume: its treasury holds pool slices.
fn graduated_with_volume(creator: impl Borrow<Acc>, swaps: u32) -> LaunchId {
    let id = create(creator);
    cross(BOB, id);
    for _ in 0..swaps {
        pool_buy(CHARLIE, id, 100 * UNIT);
    }
    id
}
/// Closed: retired, and nothing left in the record. The record itself
/// stays (R3), so a closed launch is never mistaken for an unfunded one.
fn closed(id: LaunchId) -> bool {
    matches!(Treasuries::<Test>::get(id), Some(t) if t.status == TreasuryStatus::Retired && t.shares == 0 && t.pending == 0 && t.pending_burn == 0 && t.lnrg_accrued == 0)
}
fn last_event() -> Event<Test> {
    System::events()
        .into_iter()
        .rev()
        .find_map(|r| match r.event {
            RuntimeEvent::LaunchTreasury(e) => Some(e),
            _ => None,
        })
        .expect("a treasury event")
}
fn has_event(f: impl Fn(&Event<Test>) -> bool) -> bool {
    System::events()
        .iter()
        .any(|r| matches!(&r.event, RuntimeEvent::LaunchTreasury(e) if f(e)))
}

// ---- lifecycle -----------------------------------------------------------

#[test]
fn t_l1_fee_to_pending_to_shares_at_price() {
    new_test_ext().execute_with(|| {
        // Curve fees reach the vault as `pending`, exactly what the curve says it pushed.
        let a = create(ALICE);
        assert!(Treasuries::<Test>::get(a).is_none(), "no treasury until the first fee");
        buy(BOB, a, 100 * UNIT);
        let s = Curves::<Test>::get(a).unwrap();
        assert!(s.treasury_fees_paid > 0);
        let ta = treasury(a);
        assert_eq!(ta.pending, s.treasury_fees_paid);
        assert_eq!(ta.status, TreasuryStatus::Active);
        assert_eq!(ta.dormancy_blocks, DORMANCY, "snapshotted at first funding");
        assert_eq!(vtrs(vault()), ED + ta.pending);
        ok_state();

        // Too small to stake: the spam bound.
        assert!(ta.pending < UNIT);
        assert_noop!(stake(a), Error::<Test>::BelowMinStake);

        // Enough volume, then the first stake: shares == amount, a bond appears, cooperation follows.
        for _ in 0..10 {
            buy(BOB, a, 100 * UNIT);
        }
        let p = treasury(a).pending;
        assert!(p >= UNIT);
        assert!(!MockStaking::is_bonded(&vault()));
        assert_ok!(stake(a));
        let ta = treasury(a);
        assert_eq!((ta.pending, ta.shares), (0, p));
        assert_eq!(TotalShares::<Test>::get(), p);
        assert_eq!(active(), p);
        assert!(MockStaking::is_bonded(&vault()) && MockStaking::is_cooperating(&vault()));
        assert_eq!(cooperated(), active());
        assert!(!CooperationStale::<Test>::get());
        assert_eq!(LaunchTreasury::staked_value(a), Some(p));
        // The bond is a lock: pending is spendable, the stake is not.
        assert_eq!(Balances::usable_balance(vault()), ED);
        ok_state();

        // A second launch at the same price gets shares 1:1.
        let b = graduated_with_volume(CHARLIE, 5);
        let pb = treasury(b).pending;
        assert!(pb >= UNIT);
        assert_ok!(stake(b));
        assert_eq!(treasury(b).shares, pb);
        assert_eq!(active(), p + pb);
        assert_eq!(cooperated(), active());
        ok_state();

        // After a 10 % slash the share price is 0.9, so a new staker gets more shares per VTRS.
        MockStaking::slash(&vault(), 1_000);
        ok_state();
        let c = graduated_with_volume(ALICE, 5);
        let pc = treasury(c).pending;
        let (active_before, total_before) = (active(), TotalShares::<Test>::get());
        assert_ok!(stake(c));
        let expected = pc * total_before / active_before;
        assert_eq!(treasury(c).shares, expected);
        assert!(expected > pc);
        assert_eq!(cooperated(), active());
        ok_state();
    });
}

#[test]
fn t_l2_harvest_attributes_by_shares_not_by_time() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 2);
        let b = graduated_with_volume(BOB, 6);
        assert_ok!(stake(a));
        assert_ok!(stake(b));
        let (sa, sb) = (treasury(a).shares, treasury(b).shares);
        assert!(sb > sa);
        // A third launch exists all along but stakes only after the payout.
        let c = graduated_with_volume(CHARLIE, 2);

        assert_noop!(LaunchTreasury::harvest(origin(KEEPER)), Error::<Test>::NothingToDo);
        pay_rewards(400 * UNIT);
        assert_ok!(LaunchTreasury::harvest(origin(KEEPER)));
        assert_eq!(LnrgAccounted::<Test>::get(), 400 * UNIT);
        let total = sa + sb;
        let share_of = |amount: u128, s: u128, t: u128| -> u128 {
            (U256::from(amount) * U256::from(s) / U256::from(t)).try_into().unwrap()
        };
        let (ca, cb) = (
            LaunchTreasury::claimable_lnrg(a).unwrap(),
            LaunchTreasury::claimable_lnrg(b).unwrap(),
        );
        // By shares, to within the accumulator's rounding (1e18-scaled, so a few units at most).
        assert!(ca.abs_diff(share_of(400 * UNIT, sa, total)) <= 100);
        assert!(cb.abs_diff(share_of(400 * UNIT, sb, total)) <= 100);
        assert!(ca + cb <= 400 * UNIT && 400 * UNIT - (ca + cb) < 200);

        // C stakes now: it must not see a cent of the earlier payout ...
        assert_ok!(stake(c));
        assert_eq!(LaunchTreasury::claimable_lnrg(c), Some(0));
        // ... but its share of the next one.
        pay_rewards(100 * UNIT);
        assert_ok!(LaunchTreasury::harvest(origin(KEEPER)));
        let sc = treasury(c).shares;
        let cc = LaunchTreasury::claimable_lnrg(c).unwrap();
        assert!(cc.abs_diff(share_of(100 * UNIT, sc, sa + sb + sc)) <= 100);
        // A's claim grew by its share of the second payout only.
        let ca2 = LaunchTreasury::claimable_lnrg(a).unwrap();
        assert!((ca2 - ca).abs_diff(share_of(100 * UNIT, sa, sa + sb + sc)) <= 100);

        // Harvest is idempotent; nothing double-counts.
        assert_noop!(LaunchTreasury::harvest(origin(KEEPER)), Error::<Test>::NothingToDo);
        assert_eq!(LaunchTreasury::claimable_lnrg(a), Some(ca2));
        // A stakes more: its shares change, its claim must not (settle before mutate).
        for _ in 0..12 {
            pool_buy(CHARLIE, a, 100 * UNIT);
        }
        assert!(treasury(a).pending >= UNIT);
        assert_ok!(stake(a));
        assert_eq!(
            LaunchTreasury::claimable_lnrg(a),
            Some(ca2),
            "re-staking neither loses nor mints yield"
        );
        assert_eq!(treasury(a).lnrg_accrued, ca2, "realised into the record at the checkpoint");
        ok_state();
    });
}

#[test]
fn t_l3_compound_burns_everything_it_buys() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        pay_rewards(50 * UNIT);
        let supply_before = Assets::total_supply(asset(a));
        let (vault_lnrg, keeper_vtrs) = (lnrg(vault()), vtrs(KEEPER));
        let (r_native, _) =
            <VitreusDex as PoolManager<Acc, NativeOrAssetId, u128, u64>>::native_reserves(kind(a))
                .unwrap();
        let cap = r_native * IMPACT_BPS as u128 / (2 * BPS as u128);

        run_to(now() + BURN_INTERVAL);
        assert_ok!(compound(a));
        let Event::Compounded {
            lnrg_sold,
            vtrs_realised,
            bounty,
            vtrs_burned_in,
            tokens_burned,
            ..
        } = last_event()
        else {
            panic!("Compounded")
        };
        // Sold everything attributed (the broker is deep) — the accumulator
        // floors, so a few base units stay unattributed — at its rate less 1 %.
        assert!(50 * UNIT - lnrg_sold < 100 && lnrg_sold > 0);
        assert_eq!(vtrs_realised, lnrg_sold * 990 / 1000);
        assert_eq!(lnrg(vault()), vault_lnrg - lnrg_sold);
        // The keeper got exactly the bounty — the rate on the sale and on the
        // slice — and nothing else left the vault to a person.
        let sale_bounty = vtrs_realised * BOUNTY_BPS as u128 / BPS as u128;
        assert_eq!(bounty, sale_bounty + vtrs_burned_in * BOUNTY_BPS as u128 / BPS as u128);
        assert_eq!(vtrs(KEEPER), keeper_vtrs + bounty);
        // One slice, capped, bought and burned in full (I-T6).
        assert!(vtrs_burned_in <= cap && vtrs_burned_in > 0);
        assert_eq!(vtrs_burned_in, (vtrs_realised - sale_bounty).min(cap));
        assert!(tokens_burned > 0);
        assert_eq!(tok(a, vault()), 0);
        assert_eq!(Assets::total_supply(asset(a)), supply_before - tokens_burned);
        let t = treasury(a);
        assert_eq!(t.pending_burn, vtrs_realised - bounty - vtrs_burned_in);
        assert_eq!(t.lnrg_accrued, 0);
        assert_eq!(t.last_burn_block, now());
        ok_state();

        // The buy routed its own treasury slice back: the launch's pending grew.
        assert!(t.pending > 0);

        // Same block again: nothing to sell and the interval has not passed.
        assert_noop!(compound(a), Error::<Test>::NothingToDo);
        // Next interval: another slice, nothing sold.
        run_to(now() + BURN_INTERVAL);
        let before = treasury(a).pending_burn;
        assert_ok!(compound(a));
        let Event::Compounded { lnrg_sold, bounty, vtrs_burned_in, .. } = last_event() else {
            panic!("Compounded")
        };
        assert_eq!(lnrg_sold, 0);
        assert!(vtrs_burned_in > 0 && treasury(a).pending_burn == before - vtrs_burned_in - bounty);
        assert_eq!(tok(a, vault()), 0);
        ok_state();
    });
}

/// §6.4: a burn slice pays the caller the same bounty a sale does, from
/// `pending_burn`. A retired launch's principal is burned over many
/// slices with nothing to sell, so without this the keeper that closes it
/// is unpaid for every one of them.
#[test]
fn t_l9_burn_slice_pays_the_keeper() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        MockStaking::set_era(10 + BONDING_DURATION);
        assert_ok!(finalize(a));
        let principal = treasury(a).pending_burn;
        assert!(principal > 0);
        // A tight impact cap: the principal takes several slices.
        let mut terms = Terms::<Test>::get();
        terms.max_burn_impact_bps = 10;
        assert_ok!(LaunchTreasury::set_terms(RuntimeOrigin::root(), terms));

        run_to(now() + BURN_INTERVAL);
        let keeper_before = vtrs(KEEPER);
        assert_ok!(compound(a));
        let Event::Compounded { lnrg_sold, vtrs_realised, bounty, vtrs_burned_in, .. } =
            last_event()
        else {
            panic!("Compounded, got {:?}", last_event())
        };
        assert_eq!(
            (lnrg_sold, vtrs_realised),
            (0, 0),
            "nothing to sell: the bounty is the slice's"
        );
        assert!(vtrs_burned_in > 0 && vtrs_burned_in < principal, "one slice of several");
        assert_eq!(bounty, vtrs_burned_in * BOUNTY_BPS as u128 / BPS as u128);
        assert!(bounty >= ED, "the slice is large enough to pay");
        assert_eq!(vtrs(KEEPER), keeper_before + bounty);
        assert_eq!(treasury(a).pending_burn, principal - vtrs_burned_in - bounty);
        ok_state();

        // Every slice pays until the principal is gone, at the rate and no
        // more; the record still closes.
        let mut slices = 1;
        while !closed(a) && slices < 10_000 {
            run_to(now() + BURN_INTERVAL);
            assert_ok!(compound(a));
            slices += 1;
        }
        assert!(closed(a), "closed");
        assert!(slices > 2, "several slices");
        let paid = vtrs(KEEPER) - keeper_before;
        assert!(paid > bounty, "more than one slice paid");
        assert!(
            paid <= principal * BOUNTY_BPS as u128 / BPS as u128,
            "never above the rate on the principal"
        );
        ok_state();
    });
}

#[test]
fn t_l4_retire_requires_dormancy_and_is_one_way() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        let staked = treasury(a).shares;
        let last = LaunchTreasury::last_trade_block(a).unwrap();
        assert_eq!(last, now());
        run_to(last + DORMANCY - 1);
        assert_noop!(retire(a), Error::<Test>::NotDormant);
        // One pool trade resets the clock.
        pool_buy(CHARLIE, a, UNIT);
        run_to(last + DORMANCY);
        assert_noop!(retire(a), Error::<Test>::NotDormant);
        run_to(now() + DORMANCY);
        let pending = treasury(a).pending;
        assert_ok!(retire(a));
        let t = treasury(a);
        let era = MockStaking::current_era() + BONDING_DURATION;
        assert_eq!(t.status, TreasuryStatus::Retiring { chunk_era: era });
        assert_eq!((t.shares, t.pending), (0, 0));
        assert_eq!(t.pending_burn, pending, "unstaked fees retire with the rest");
        assert_eq!(TotalShares::<Test>::get(), 0);
        assert_eq!(RetiringQueue::<Test>::get().to_vec(), vec![(a, era, staked)]);
        assert_eq!(MockStaking::total(&vault()), staked);
        assert_eq!(active(), 0);
        ok_state();

        // One-way: no stake, no second retire, and the pool's slice now folds into the protocol.
        assert_noop!(retire(a), Error::<Test>::NotActive);
        assert_noop!(stake(a), Error::<Test>::NotActive);
        assert_eq!(
            <LaunchTreasury as TreasurySink<NativeOrAssetId, Acc, u128>>::account_for(&kind(a)),
            None
        );
        let (proto_before, vault_before) = (ProtocolFeesUnclaimed::<Test>::get(), vtrs(vault()));
        pool_buy(CHARLIE, a, 100 * UNIT);
        assert_eq!(vtrs(vault()), vault_before);
        assert_eq!(
            ProtocolFeesUnclaimed::<Test>::get() - proto_before,
            100 * UNIT * 15 / 10_000,
            "protocol 5 + treasury 10"
        );
        assert_eq!(treasury(a).pending, 0);
        ok_state();
    });
}

#[test]
fn t_l5_finalize_credits_every_matured_launch_exactly() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        let b = graduated_with_volume(BOB, 10);
        let c = graduated_with_volume(CHARLIE, 10);
        for id in [a, b, c] {
            assert_ok!(stake(id));
        }
        // A retires in era 10, B in era 12; C stays.
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        let va = RetiringQueue::<Test>::get()[0].2;
        MockStaking::advance_eras(2);
        assert_ok!(retire(b));
        let vb = RetiringQueue::<Test>::get()[1].2;
        assert_eq!(cooperated(), active(), "C's stake is still cooperated");
        ok_state();

        assert_noop!(finalize(a), Error::<Test>::NotMatured);
        assert_noop!(finalize(c), Error::<Test>::NotRetiring);
        // A slash during unbonding: chunks and active alike lose 10 %.
        MockStaking::slash(&vault(), 1_000);
        ok_state();
        MockStaking::set_era(10 + BONDING_DURATION);
        assert_ok!(finalize(a));
        let ta = treasury(a);
        assert_eq!(ta.status, TreasuryStatus::Retired);
        assert_eq!(
            ta.pending_burn,
            va - va * 1_000 / 10_000,
            "credited what came back, not what was unbonded"
        );
        assert_eq!(
            treasury(b).status,
            TreasuryStatus::Retiring { chunk_era: 12 + BONDING_DURATION }
        );
        assert_eq!(RetiringQueue::<Test>::get().len(), 1);
        ok_state();

        // B matures later; finalizing it must not touch A again.
        assert_noop!(finalize(b), Error::<Test>::NotMatured);
        MockStaking::set_era(12 + BONDING_DURATION);
        assert_ok!(finalize(b));
        assert_eq!(treasury(b).pending_burn, vb - vb * 1_000 / 10_000);
        assert_eq!(treasury(a).pending_burn, va - va * 1_000 / 10_000);
        assert!(RetiringQueue::<Test>::get().is_empty());
        assert_noop!(finalize(b), Error::<Test>::NotRetiring);
        // Everything unbonded is out of the ledger; C's stake remains.
        assert_eq!(MockStaking::total(&vault()), active());
        assert_eq!(LaunchTreasury::staked_value(c), Some(active()));
        ok_state();

        // A retired treasury burns its principal into the pool and closes on dust.
        let supply = Assets::total_supply(asset(a));
        let mut slices = 0;
        while !closed(a) && slices < 10_000 {
            run_to(now() + BURN_INTERVAL);
            assert_ok!(compound(a));
            slices += 1;
            assert_eq!(tok(a, vault()), 0);
        }
        assert!(closed(a), "closed");
        assert!(slices >= 1, "slice size is fm_t1's concern; here the principal is under one cap");
        assert!(Assets::total_supply(asset(a)) < supply);
        ok_state();
    });
}

/// §9.6 / §10.11: a chain that ships the pallet at genesis runs no
/// `FundLaunchTreasuryVault`; the first fee creates the vault, and without
/// the ED buffer of §4 every unit it holds is accounted. The last retirement
/// slice then has to spend the vault's final unit while a consumer
/// reference keeps the account alive — here a late `payout_stakers` for an
/// era the vault was still exposed in, which lands two eras after it
/// unbonded — and Balances refuses with `Token(Frozen)`: the record can
/// never close. The first fee must leave the ED behind.
#[test]
fn t_l8_from_genesis_first_fee_withholds_ed_so_retirement_closes() {
    new_test_ext_from_genesis().execute_with(|| {
        assert_eq!(vtrs(vault()), 0, "no upgrade funded the vault");
        // I-T1 holds before any fee: nothing funded the vault, so it counts no ED.
        ok_state();
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        MockStaking::set_era(10 + BONDING_DURATION);
        assert_ok!(finalize(a));
        assert_eq!(MockStaking::total(&vault()), 0, "ledger gone: no lock keeps the account alive");
        assert_eq!(treasury(a).status, TreasuryStatus::Retired);

        // A late payout for an era the vault was still exposed in.
        pay_rewards(1);
        assert_eq!(System::consumers(&vault()), 1, "the LNRG account is the consumer");

        let mut slices = 0;
        while !closed(a) && slices < 10_000 {
            run_to(now() + BURN_INTERVAL);
            assert_ok!(compound(a));
            slices += 1;
        }
        assert!(closed(a), "closed");
        assert_eq!(vtrs(vault()), ED, "the first fee's ED outlives the launch");
        assert!(VaultFunded::<Test>::get());
        ok_state();

        // The next launch's first fee is not the first fee: nothing withheld.
        let b = create(BOB);
        let v0 = vtrs(vault());
        buy(CHARLIE, b, 50 * UNIT);
        assert_eq!(vtrs(vault()) - v0, treasury(b).pending);
        ok_state();
    });
}

#[test]
fn t_l6_snapshotted_terms_survive_set_terms() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 1);
        assert_eq!(treasury(a).dormancy_blocks, DORMANCY);
        let mut terms = Terms::<Test>::get();
        terms.dormancy_blocks = 1_000;
        assert_ok!(LaunchTreasury::set_terms(RuntimeOrigin::root(), terms.clone()));
        assert_eq!(treasury(a).dormancy_blocks, DORMANCY, "a funded treasury keeps its term");
        let b = graduated_with_volume(BOB, 1);
        assert_eq!(treasury(b).dormancy_blocks, 1_000);
        // Bounds.
        let bad = |f: fn(&mut TreasuryTerms<u128, u64>)| {
            let mut t = Terms::<Test>::get();
            f(&mut t);
            t
        };
        assert_noop!(
            LaunchTreasury::set_terms(RuntimeOrigin::root(), bad(|t| t.max_burn_impact_bps = 9)),
            Error::<Test>::TermsOutOfBounds
        );
        assert_noop!(
            LaunchTreasury::set_terms(RuntimeOrigin::root(), bad(|t| t.max_burn_impact_bps = 501)),
            Error::<Test>::TermsOutOfBounds
        );
        assert_noop!(
            LaunchTreasury::set_terms(RuntimeOrigin::root(), bad(|t| t.keeper_bounty_bps = 201)),
            Error::<Test>::TermsOutOfBounds
        );
        assert_noop!(
            LaunchTreasury::set_terms(RuntimeOrigin::root(), bad(|t| t.dormancy_blocks = 0)),
            Error::<Test>::TermsOutOfBounds
        );
        assert_noop!(
            LaunchTreasury::set_terms(RuntimeOrigin::root(), bad(|t| t.min_stake = 0)),
            Error::<Test>::TermsOutOfBounds
        );
        assert_noop!(LaunchTreasury::set_terms(origin(ALICE), terms), BadOrigin);
    });
}

#[test]
fn t_l7_i_t1_conservation_under_random_ops() {
    new_test_ext().execute_with(|| {
        let ids = [graduated_with_volume(ALICE, 3), graduated_with_volume(BOB, 3), create(CHARLIE)];
        buy(BOB, ids[2], 300 * UNIT);
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for step in 0..400u64 {
            let id = ids[(next() % 3) as usize];
            match next() % 12 {
                0 | 1 => {
                    if Curves::<Test>::get(id).unwrap().phase == Phase::Graduated {
                        pool_buy(CHARLIE, id, (next() % 200 + 1) as u128 * UNIT);
                    } else {
                        buy(BOB, id, (next() % 50 + 1) as u128 * UNIT);
                    }
                },
                2 | 3 => {
                    let _ = stake(id);
                },
                4 => pay_rewards((next() % 30 + 1) as u128 * UNIT),
                5 | 6 => {
                    run_to(now() + BURN_INTERVAL);
                    let _ = compound(id);
                },
                7 => {
                    if next() % 4 == 0 {
                        run_to(now() + DORMANCY);
                    }
                    let _ = retire(id);
                },
                8 => {
                    MockStaking::advance_eras(BONDING_DURATION / 4);
                    let _ = finalize(id);
                },
                9 => MockStaking::slash(&vault(), (next() % 300) as u128),
                10 => {
                    if next() % 2 == 0 {
                        drain_broker();
                    } else {
                        fund_broker(1_000 * UNIT);
                    }
                },
                _ => {
                    let _ = LaunchTreasury::harvest(origin(KEEPER));
                },
            }
            run_to(now() + 1);
            assert!(LaunchTreasury::do_try_state().is_ok(), "step {step}");
            for id in ids {
                assert_eq!(tok(id, vault()), 0, "I-T6 at step {step}");
            }
        }
    });
}

// ---- failure modes ---------------------------------------------------------

#[test]
fn fm_t1_slice_never_exceeds_impact_cap() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        // A payout far larger than one slice, so the cap binds every time.
        pay_rewards(5_000 * UNIT);
        for _ in 0..8 {
            run_to(now() + BURN_INTERVAL);
            let (rn, rt) =
                <VitreusDex as PoolManager<Acc, NativeOrAssetId, u128, u64>>::native_reserves(
                    kind(a),
                )
                .unwrap();
            let cap = rn * IMPACT_BPS as u128 / (2 * BPS as u128);
            assert_ok!(compound(a));
            let Event::Compounded { vtrs_burned_in, .. } = last_event() else {
                panic!("Compounded")
            };
            assert!(vtrs_burned_in <= cap, "{vtrs_burned_in} > cap {cap}");
            assert!(vtrs_burned_in > 0);
            // The price moved by at most the cap's impact (plus the fee's rounding).
            let (rn2, rt2) =
                <VitreusDex as PoolManager<Acc, NativeOrAssetId, u128, u64>>::native_reserves(
                    kind(a),
                )
                .unwrap();
            let p_before = U256::from(rn) * U256::from(SCALE) / U256::from(rt);
            let p_after = U256::from(rn2) * U256::from(SCALE) / U256::from(rt2);
            let moved_bps = (p_after - p_before) * U256::from(BPS) / p_before;
            assert!(moved_bps <= U256::from(IMPACT_BPS + 1), "price moved {moved_bps} bps");
        }
        assert!(treasury(a).pending_burn > 0, "most of it is still waiting");
        ok_state();
    });
}

#[test]
fn fm_t2_stake_before_reputation_keeps_bond_and_retries() {
    new_test_ext().execute_with(|| {
        REPUTATION_OK.with(|r| *r.borrow_mut() = false);
        let a = graduated_with_volume(ALICE, 10);
        let p = treasury(a).pending;
        assert_ok!(stake(a));
        // The bond is in place and earning nothing; the state says so.
        assert_eq!(active(), p);
        assert!(!MockStaking::is_cooperating(&vault()));
        assert!(CooperationStale::<Test>::get());
        // The mock's errors are `DispatchError::Other`, whose text does not
        // survive the event encoding (a module error's index does); the
        // direct return below carries the name.
        assert!(has_event(|e| matches!(
            e,
            Event::CooperationStale { reason: DispatchError::Other(_) }
        )));
        assert_eq!(treasury(a).shares, p);
        ok_state();
        // Anyone may retry; it fails for the same reason until the record clears.
        assert_noop!(LaunchTreasury::retarget(origin(KEEPER)), err("ReputationTooLow"));
        REPUTATION_OK.with(|r| *r.borrow_mut() = true);
        assert_ok!(LaunchTreasury::retarget(origin(KEEPER)));
        assert!(!CooperationStale::<Test>::get());
        assert_eq!(cooperated(), active());
        ok_state();
    });
}

#[test]
fn fm_t3_retarget_filters_chilled_and_noncollab() {
    new_test_ext().execute_with(|| {
        assert_ok!(LaunchTreasury::set_targets(RuntimeOrigin::root(), vec![VAL_A, VAL_B, VAL_C]));
        MockStaking::set_validator(VAL_C, false);
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        let submitted = COOPERATE_CALLS.with(|c| c.borrow().last().cloned()).unwrap();
        let names: Vec<Acc> = submitted.iter().map(|(v, _)| *v).collect();
        assert_eq!(names, vec![VAL_A, VAL_B], "C is not cooperable and was left out, not failed on");
        assert_eq!(submitted.iter().map(|(_, s)| s).sum::<u128>(), active());
        assert_eq!(cooperated(), active());
        // B chills: the next retarget puts everything on A.
        MockStaking::set_validator(VAL_B, false);
        assert_ok!(LaunchTreasury::retarget(origin(KEEPER)));
        let submitted = COOPERATE_CALLS.with(|c| c.borrow().last().cloned()).unwrap();
        assert_eq!(submitted, vec![(VAL_A, active())]);
        // Nobody left: a clean error, the bond untouched.
        MockStaking::set_validator(VAL_A, false);
        assert_noop!(LaunchTreasury::retarget(origin(KEEPER)), Error::<Test>::NoTargets);
        let b = graduated_with_volume(BOB, 10);
        assert_ok!(stake(b));
        assert!(CooperationStale::<Test>::get());
        assert!(has_event(|e| matches!(e, Event::CooperationStale { reason } if *reason == Error::<Test>::NoTargets.into())));
        ok_state();
    });
}

#[test]
fn fm_t4_dry_broker_keeps_lnrg_accrued() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        pay_rewards(100 * UNIT);
        drain_broker();
        assert_ok!(LaunchTreasury::harvest(origin(KEEPER)));
        let owed = LaunchTreasury::claimable_lnrg(a).unwrap();
        assert!(100 * UNIT - owed < 100);
        run_to(now() + BURN_INTERVAL);
        // Nothing can be sold and nothing is waiting to burn: a clean no-op.
        assert_noop!(compound(a), Error::<Test>::NothingToDo);
        assert_eq!(LaunchTreasury::claimable_lnrg(a), Some(owed), "still owed, nothing lost");
        assert_eq!(lnrg(vault()), 100 * UNIT);
        // Half the depth: sells what fits, keeps the rest.
        fund_broker(30 * UNIT);
        assert_ok!(compound(a));
        let Event::Compounded { lnrg_sold, vtrs_realised, .. } = last_event() else {
            panic!("Compounded")
        };
        assert!(vtrs_realised <= 30 * UNIT && vtrs_realised > 29 * UNIT);
        assert!(lnrg_sold < owed);
        let t = treasury(a);
        assert_eq!(t.lnrg_accrued, owed - lnrg_sold);
        assert_eq!(LaunchTreasury::claimable_lnrg(a), Some(owed - lnrg_sold));
        ok_state();
        // Depth restored: the rest goes.
        fund_broker(1_000 * UNIT);
        run_to(now() + BURN_INTERVAL);
        assert_ok!(compound(a));
        assert_eq!(treasury(a).lnrg_accrued, 0);
        assert!(lnrg(vault()) < 100, "only the accumulator's dust is left");
        ok_state();
    });
}

#[test]
fn fm_t6_slash_devalues_every_launch_equally() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        let b = graduated_with_volume(BOB, 4);
        assert_ok!(stake(a));
        assert_ok!(stake(b));
        let (va, vb) =
            (LaunchTreasury::staked_value(a).unwrap(), LaunchTreasury::staked_value(b).unwrap());
        MockStaking::slash(&vault(), 2_000);
        let (va2, vb2) =
            (LaunchTreasury::staked_value(a).unwrap(), LaunchTreasury::staked_value(b).unwrap());
        assert!(va2.abs_diff(va * 8 / 10) <= 1);
        assert!(vb2.abs_diff(vb * 8 / 10) <= 1);
        assert!(
            active() - (va2 + vb2) <= 1,
            "nothing is created or lost by the accounting beyond a floor"
        );
        assert!(active() - cooperated() <= 2, "targets scaled with the slash, floor per target");
        ok_state();
        // A retires at the post-slash price.
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        assert_eq!(RetiringQueue::<Test>::get()[0].2, va2);
        assert_eq!(LaunchTreasury::staked_value(b), Some(active()));
        ok_state();
    });
}

#[test]
fn fm_t7_no_more_chunks_is_retryable() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 20);
        let b = graduated_with_volume(BOB, 10);
        assert_ok!(stake(a));
        assert_ok!(stake(b));
        // Fill the ledger's chunk slots with unbonds in 64 distinct eras
        // (as if 64 other launches had retired; the staking pallet does not
        // care who they were).
        for _ in 0..MAX_CHUNKS {
            MockStaking::advance_eras(1);
            assert_ok!(MockStaking::unbond(&vault(), ED));
        }
        run_to(now() + DORMANCY);
        assert_noop!(retire(b), err("NoMoreChunks"));
        assert_eq!(treasury(b).status, TreasuryStatus::Active, "nothing changed");
        // One chunk matures and is withdrawn (by anyone's withdraw): retry succeeds.
        MockStaking::set_era(MockStaking::current_era() + BONDING_DURATION - MAX_CHUNKS + 1);
        assert!(MockStaking::withdraw_unbonded(&vault()).unwrap() > 0);
        assert_ok!(retire(b));
        assert_eq!(
            treasury(b).status,
            TreasuryStatus::Retiring { chunk_era: MockStaking::current_era() + BONDING_DURATION }
        );
    });
}

#[test]
fn fm_t8_last_retire_chills_first_and_next_stake_recooperates() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        assert!(MockStaking::is_cooperating(&vault()));
        run_to(now() + DORMANCY);
        // The only launch retires: `unbond` alone would leave a cooperator
        // under MinCooperatorBond and fail; the pallet chills first.
        assert_ok!(retire(a));
        assert!(!MockStaking::is_cooperating(&vault()));
        assert_eq!(active(), 0);
        assert!(MockStaking::is_bonded(&vault()), "the ledger holds the unlocking chunk");
        assert_eq!(cooperated(), 0);
        ok_state();
        // The next launch bonds extra into the existing ledger and cooperates again.
        let b = graduated_with_volume(BOB, 10);
        assert_ok!(stake(b));
        assert!(MockStaking::is_cooperating(&vault()));
        assert_eq!(cooperated(), active());
        assert_eq!(active(), treasury(b).shares);
        assert!(!CooperationStale::<Test>::get());
        ok_state();
        // And once everything is withdrawn the ledger is gone, so the next
        // stake after that starts with `bond` again.
        run_to(now() + DORMANCY);
        assert_ok!(retire(b));
        MockStaking::set_era(MockStaking::current_era() + BONDING_DURATION);
        assert_ok!(finalize(a));
        assert!(!MockStaking::is_bonded(&vault()));
        let c = graduated_with_volume(CHARLIE, 10);
        assert_ok!(stake(c));
        assert!(MockStaking::is_bonded(&vault()) && MockStaking::is_cooperating(&vault()));
        ok_state();
    });
}

#[test]
fn fm_t11_retirement_can_graduate_a_curve() {
    new_test_ext().execute_with(|| {
        // The widest slice, so the loop is short: 200 is the term's ceiling
        // (R7), and on a 1 % curve the venue bound is 199 bps.
        let mut terms = Terms::<Test>::get();
        terms.max_burn_impact_bps = 200;
        assert_ok!(LaunchTreasury::set_terms(RuntimeOrigin::root(), terms));

        // A curve with enough volume to stake, then a payout worth far more
        // than the graduation target: the retirement's slices will cross it.
        let a = create(ALICE);
        for _ in 0..5 {
            buy(BOB, a, 100 * UNIT);
        }
        assert_eq!(Curves::<Test>::get(a).unwrap().phase, Phase::Trading);
        let supply = Assets::total_supply(asset(a));
        assert_ok!(stake(a));
        pay_rewards(2 * T_DEFAULT);
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        assert_eq!(
            treasury(a).status,
            TreasuryStatus::Retiring { chunk_era: MockStaking::current_era() + BONDING_DURATION }
        );
        assert!(
            LaunchTreasury::claimable_lnrg(a).unwrap() > T_DEFAULT,
            "the yield is the launch's even while it retires"
        );
        // Slices buy on the curve — ordinary buys — until it crosses.
        let mut n = 0;
        while Curves::<Test>::get(a).unwrap().phase == Phase::Trading && n < 2_000 {
            run_to(now() + BURN_INTERVAL);
            assert_ok!(compound(a));
            assert_eq!(tok(a, vault()), 0);
            n += 1;
        }
        assert_eq!(Curves::<Test>::get(a).unwrap().phase, Phase::Graduated, "after {n} slices");
        assert!(Pools::<Test>::contains_key(VitreusDex::canonical_pair(
            NativeOrAssetId::Native,
            kind(a)
        )));
        assert!(Assets::total_supply(asset(a)) < supply, "what was bought was burned");
        ok_state();
        // The rest keeps burning into the pool it just seeded.
        run_to(now() + BURN_INTERVAL);
        assert_ok!(compound(a));
        assert_eq!(tok(a, vault()), 0);
    });
}

#[test]
fn i_t7_cooperation_matches_active_after_every_bond_change() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        let b = graduated_with_volume(BOB, 10);
        let check = || {
            if !CooperationStale::<Test>::get() {
                assert_eq!(cooperated(), active());
            }
            ok_state();
        };
        assert_ok!(stake(a));
        check();
        assert_ok!(stake(b));
        check();
        assert_ok!(LaunchTreasury::set_targets(RuntimeOrigin::root(), vec![VAL_C]));
        check();
        for _ in 0..5 {
            pool_buy(CHARLIE, a, 300 * UNIT);
        }
        assert_ok!(stake(a));
        check();
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        check();
        assert_eq!(cooperated(), active());
        assert_eq!(
            COOPERATE_CALLS.with(|c| c.borrow().len()),
            5,
            "one cooperate per bond change, none otherwise"
        );
    });
}

// ---- governance / no exit ----------------------------------------------------

#[test]
fn t_g1_no_origin_can_withdraw() {
    new_test_ext().execute_with(|| {
        // The complete call surface; nothing here takes a recipient.
        assert_eq!(
            LaunchTreasury::call_names(),
            [
                "stake",
                "retarget",
                "harvest",
                "compound",
                "retire",
                "finalize_retirement",
                "set_terms",
                "set_targets"
            ]
        );
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        pay_rewards(10 * UNIT);
        let held = vtrs(vault()) + MockStaking::total(&vault());
        // Governance calls move no funds and are root-only.
        assert_noop!(LaunchTreasury::set_targets(origin(ALICE), vec![VAL_C]), BadOrigin);
        assert_ok!(LaunchTreasury::set_targets(RuntimeOrigin::root(), vec![VAL_C]));
        assert_ok!(LaunchTreasury::set_terms(RuntimeOrigin::root(), Terms::<Test>::get()));
        assert_eq!(vtrs(vault()) + MockStaking::total(&vault()), held);
        assert_eq!(lnrg(vault()), 10 * UNIT);
        // The one transfer to a person is the bounty, bounded by the term.
        run_to(now() + BURN_INTERVAL);
        let k = vtrs(KEEPER);
        assert_ok!(compound(a));
        let Event::Compounded { vtrs_realised, bounty, .. } = last_event() else { panic!() };
        assert_eq!(vtrs(KEEPER) - k, bounty);
        assert!(bounty <= vtrs_realised * 200 / BPS as u128);
        ok_state();
    });
}

#[test]
fn t_g2_set_targets_recooperates_without_touching_shares() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        let b = graduated_with_volume(BOB, 10);
        assert_ok!(stake(a));
        assert_ok!(stake(b));
        let (sa, sb, total) = (treasury(a).shares, treasury(b).shares, TotalShares::<Test>::get());
        assert_ok!(LaunchTreasury::set_targets(RuntimeOrigin::root(), vec![VAL_C]));
        assert_eq!(
            COOPERATE_CALLS.with(|c| c.borrow().last().cloned()).unwrap(),
            vec![(VAL_C, active())]
        );
        assert_eq!(
            (treasury(a).shares, treasury(b).shares, TotalShares::<Test>::get()),
            (sa, sb, total)
        );
        assert!(!CooperationStale::<Test>::get());
        // With no bond yet, setting targets is just storage.
        LEDGER.with(|l| l.borrow_mut().clear());
        let n = COOPERATE_CALLS.with(|c| c.borrow().len());
        assert_ok!(LaunchTreasury::set_targets(RuntimeOrigin::root(), vec![VAL_A]));
        assert_eq!(COOPERATE_CALLS.with(|c| c.borrow().len()), n);
        assert_eq!(Targets::<Test>::get().to_vec(), vec![VAL_A]);
    });
}

#[test]
fn t_g3_retired_launch_slice_folds_into_protocol() {
    new_test_ext().execute_with(|| {
        // A launch on the curve whose treasury is retired: the curve's
        // treasury share joins the protocol's, nothing reaches the vault.
        let a = create(ALICE);
        buy(BOB, a, 50 * UNIT);
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        assert_eq!(
            treasury(a).status,
            TreasuryStatus::Retired,
            "nothing was staked: retired at once"
        );
        let (t0, v0) = (vtrs(TREASURY), vtrs(vault()));
        buy(BOB, a, 50 * UNIT);
        let s = Curves::<Test>::get(a).unwrap();
        assert_eq!(vtrs(vault()), v0);
        assert!(vtrs(TREASURY) - t0 > 0);
        assert_eq!(s.treasury_fees_paid, treasury(a).pending_burn, "paid before retirement only");
        ok_state();
    });
}

#[test]
fn t_l5b_finalize_prorates_a_slash_across_matured_launches() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 12);
        let b = graduated_with_volume(BOB, 6);
        let c = graduated_with_volume(CHARLIE, 6);
        for id in [a, b, c] {
            assert_ok!(stake(id));
        }
        // A and B retire in the same era: the staking pallet merges their
        // chunks into one, so only this pallet's queue knows the split.
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        assert_ok!(retire(b));
        let (va, vb) = (RetiringQueue::<Test>::get()[0].2, RetiringQueue::<Test>::get()[1].2);
        assert_eq!(MockStaking::ledger(&vault()).unwrap().unlocking.len(), 1, "one merged chunk");
        MockStaking::slash(&vault(), 2_500);
        MockStaking::set_era(MockStaking::current_era() + BONDING_DURATION);
        // Finalizing either credits both, each pro rata to what came back.
        assert_ok!(finalize(b));
        let came_back = (va + vb) - (va + vb) * 2_500 / 10_000;
        let (ca, cb) = (treasury(a).pending_burn, treasury(b).pending_burn);
        assert_eq!(ca + cb, came_back, "everything withdrawn is credited, nothing else");
        assert!(ca.abs_diff(va - va * 2_500 / 10_000) <= 1);
        assert!(cb.abs_diff(vb - vb * 2_500 / 10_000) <= 1);
        assert_eq!(treasury(a).status, TreasuryStatus::Retired);
        assert_eq!(treasury(b).status, TreasuryStatus::Retired);
        assert!(RetiringQueue::<Test>::get().is_empty());
        assert_noop!(finalize(a), Error::<Test>::NotRetiring);
        // C's stake took its 25 % too and is still cooperated.
        assert_eq!(LaunchTreasury::staked_value(c), Some(active()));
        ok_state();
    });
}

#[test]
fn stake_refuses_when_the_vault_is_fully_slashed() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        MockStaking::slash(&vault(), 10_000);
        assert_eq!(active(), 0);
        assert!(TotalShares::<Test>::get() > 0);
        let b = graduated_with_volume(BOB, 10);
        // New principal would be shared with shares that are worth nothing.
        assert_noop!(stake(b), Error::<Test>::VaultInsolvent);
        assert_eq!(LaunchTreasury::staked_value(a), Some(0));
        // The dead shares retire out (nothing to unbond), and staking resumes.
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        assert_eq!(treasury(a).status, TreasuryStatus::Retired);
        assert_eq!(TotalShares::<Test>::get(), 0);
        assert_ok!(stake(b));
        assert_eq!(treasury(b).shares, active());
        ok_state();
    });
}

// ---- adversarial review, 2026-09-17: red tests --------------------------
//
// Each of these encodes a finding from the review pass, not a spec test.
// They are expected to FAIL on this commit; a fix turns them green.

/// R1 — a sale never lowers `LnrgAccounted`, so the next `x` LNRG of
/// rewards after selling `x` are attributed to nobody: `harvest` sees
/// `balance ≤ accounted` and returns nothing until cumulative new rewards
/// exceed what was sold. The LNRG stays in the vault, owned by no launch,
/// forever. In steady state (sell whenever anything accrued) half the
/// yield is never attributed.
#[test]
fn r1_rewards_after_a_sale_are_attributed_to_nobody() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        fund_broker(10_000 * UNIT);
        pay_rewards(100 * UNIT);
        assert_ok!(LaunchTreasury::harvest(origin(KEEPER)));
        run_to(now() + BURN_INTERVAL);
        assert_ok!(compound(a));
        assert_eq!(treasury(a).lnrg_accrued, 0, "everything sold");
        assert!(lnrg(vault()) < 100, "the vault holds only accumulator dust");

        // The next era pays the same again. It is real LNRG in the vault…
        pay_rewards(100 * UNIT);
        assert_eq!(lnrg(vault()), 100 * UNIT + lnrg(vault()) % UNIT);
        // …and nobody is owed it.
        let r = LaunchTreasury::harvest(origin(KEEPER));
        assert_ok!(r);
        assert!(
            LaunchTreasury::claimable_lnrg(a).unwrap() >= 100 * UNIT - 100,
            "a second era's rewards must be claimable by the only launch; claimable = {}",
            LaunchTreasury::claimable_lnrg(a).unwrap()
        );
    });
}

/// R2 — the treasury's own burn slice is a trade on the venue: `buy_for`
/// runs `do_buy`, which writes `last_trade_block`; the DEX's `swap_for`
/// writes `LastSwapBlock`. A funded launch that nobody trades never
/// becomes dormant while its yield keeps compounding, so `retire` is
/// unreachable for exactly the launches it was designed for.
#[test]
fn r2_the_treasurys_own_buybacks_reset_the_dormancy_clock() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        fund_broker(10_000 * UNIT);
        let last_user_trade = LaunchTreasury::last_trade_block(a).unwrap();
        // Yield arrives and is compounded well inside the dormancy window,
        // then the venue is quiet for the whole window. No user trades.
        pay_rewards(10 * UNIT);
        run_to(last_user_trade + BURN_INTERVAL);
        assert_ok!(compound(a));
        run_to(last_user_trade + DORMANCY + 1);
        assert!(retire(a).is_ok(), "no user has traded for a full dormancy window; the pallet's own buyback is not a trade: {:?}", retire(a));

        // The curve venue, the same way.
        let b = create(BOB);
        buy(CHARLIE, b, 1_000 * UNIT);
        assert_ok!(stake(b));
        let last_user_trade = LaunchTreasury::last_trade_block(b).unwrap();
        pay_rewards(10 * UNIT);
        run_to(now() + BURN_INTERVAL);
        assert_ok!(compound(b));
        let Event::Compounded { tokens_burned, .. } = last_event() else { panic!("Compounded") };
        assert!(tokens_burned > 0, "the buyback bought on the curve");
        assert_eq!(LaunchTreasury::last_trade_block(b), Some(last_user_trade), "and did not move the clock");
        run_to(last_user_trade + DORMANCY + 1);
        assert!(retire(b).is_ok(), "{:?}", retire(b));
    });
}

/// R3 — after a retired treasury closes (`Treasuries::remove`), the pallet
/// cannot tell it from a launch never funded: `account_for` answers the
/// vault again, the next fee creates a fresh `Active` record, and the
/// launch has a treasury again. I-T5 says Retired never returns to Active
/// and the site says "its slice goes to the protocol forever".
#[test]
fn r3_a_closed_treasury_is_reopened_by_the_next_fee() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        run_to(now() + DORMANCY);
        assert_ok!(retire(a));
        MockStaking::set_era(10 + BONDING_DURATION);
        assert_ok!(finalize(a));
        let mut slices = 0;
        while !closed(a) && slices < 10_000 {
            run_to(now() + BURN_INTERVAL);
            assert_ok!(compound(a));
            slices += 1;
        }
        assert!(closed(a), "closed");

        // The token revives.
        let proto_before = ProtocolFeesUnclaimed::<Test>::get();
        pool_buy(CHARLIE, a, 100 * UNIT);
        assert_eq!(
            ProtocolFeesUnclaimed::<Test>::get() - proto_before,
            100 * UNIT * 15 / 10_000,
            "a retired launch's slice folds into the protocol share forever (FM-T9)"
        );
        assert!(closed(a), "retirement is one-way: the closed record stands, nothing reopened");
        assert_eq!(
            <LaunchTreasury as TreasurySink<NativeOrAssetId, Acc, u128>>::account_for(&kind(a)),
            None
        );
    });
}

/// R4 — `pending_burn` dust on an *Active* curve launch. The Retired path
/// sweeps a remainder below ED; the Active path tries to buy with it, the
/// curve says `Unquotable` (a 1 % fee rounds a sub-100-wei buy to
/// nothing), the error propagates, and the whole `compound` reverts — the
/// sale in the same call included. Reachable when a sale realises under
/// 100 wei (a 1-wei LNRG remainder from the accumulator's rounding, at a
/// broker rate under 100 VTRS per LNRG); it heals itself once a later
/// sale adds enough to the same `pending_burn`, and never heals if no
/// more yield comes. Low, and the fix is a line: treat an unquotable
/// slice as "nothing to burn" instead of an error.
#[test]
fn r4_dust_in_pending_burn_bricks_compound_for_an_active_launch() {
    new_test_ext().execute_with(|| {
        let a = create(ALICE);
        buy(BOB, a, 1_000 * UNIT);
        assert_ok!(stake(a));
        fund_broker(10_000 * UNIT);
        // 1 wei waiting to burn: what a 1-wei LNRG sale realises at a rate
        // under 100 VTRS/LNRG (the accumulator leaves 1-wei remainders).
        // Under 100 wei the curve's fee rounds the buy to nothing.
        Treasuries::<Test>::mutate(a, |t| t.as_mut().unwrap().pending_burn = 1);
        // Keep I-T1 honest about it.
        assert_ok!(Balances::transfer_allow_death(origin(ALICE), vault(), 1));
        run_to(now() + BURN_INTERVAL);
        // Nothing to sell, one unquotable wei to burn: the call should be a
        // no-op (`NothingToDo`, which a keeper's predicate understands), not
        // an error it retries at every interval.
        let r = compound(a);
        assert!(
            r == Err(Error::<Test>::NothingToDo.into()) || r.is_ok(),
            "an unquotable slice is not an error of compound; got {:?}",
            r
        );
        // And once there is something to sell, the sale must go through with
        // the dust still there.
        pay_rewards(10 * UNIT);
        run_to(now() + BURN_INTERVAL);
        assert_ok!(compound(a));
        assert!(
            treasury(a).lnrg_accrued <= 1,
            "sold, bar the asset's min balance the vault keeps (R8)"
        );
    });
}

/// R5 — I-T1 is checked as strict equality on an account anyone can send
/// VTRS to. One wei sent to the vault makes `try_state` fail on every
/// block from then on, for as long as the chain lives: nothing accounts
/// for the wei, nothing sweeps it, and no call can. An invariant a
/// stranger can break for the price of a transfer is a griefing handle on
/// whatever gates on `try-runtime` (upgrade rehearsals, CI).
#[test]
fn r5_one_wei_sent_to_the_vault_fails_try_state_forever() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 3);
        assert_ok!(stake(a));
        ok_state();
        assert_ok!(Balances::transfer_allow_death(origin(CHARLIE), vault(), 1));
        assert!(
            LaunchTreasury::do_try_state().is_ok(),
            "a donation is not an accounting error: {:?}",
            LaunchTreasury::do_try_state()
        );
    });
}

/// R7 — the term is a ceiling, the venue's fee is the bound. Governance
/// sets the widest impact allowed; on a 0.3 % pool a slice is still sized
/// at 59 bps of impact (strictly under the 60 bps round trip a bracket
/// pays), and the modelled bracket P&L at that point is negative on every
/// venue. Above the round trip it turns positive within one step.
#[test]
fn r7_burn_impact_is_bounded_under_the_venues_round_trip_fee() {
    new_test_ext().execute_with(|| {
        let mut terms = Terms::<Test>::get();
        terms.max_burn_impact_bps = 200;
        assert_ok!(LaunchTreasury::set_terms(RuntimeOrigin::root(), terms.clone()));
        terms.max_burn_impact_bps = 500;
        assert_noop!(
            LaunchTreasury::set_terms(RuntimeOrigin::root(), terms),
            Error::<Test>::TermsOutOfBounds
        );

        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        // Enough VTRS waiting to burn that only the cap limits the slice.
        Treasuries::<Test>::mutate(a, |t| t.as_mut().unwrap().pending_burn = 10_000 * UNIT);
        assert_ok!(Balances::transfer_allow_death(origin(ALICE), vault(), 10_000 * UNIT));
        let (reserve, _) = <VitreusDex as pallet_vitreus_dex::PoolManager<
            Acc,
            NativeOrAssetId,
            u128,
            u64,
        >>::native_reserves(kind(a))
        .unwrap();
        run_to(now() + BURN_INTERVAL);
        assert_ok!(compound(a));
        let Event::Compounded { vtrs_burned_in, .. } = last_event() else { panic!("Compounded") };
        let fee_bps = <VitreusDex as pallet_vitreus_dex::PoolManager<
            Acc,
            NativeOrAssetId,
            u128,
            u64,
        >>::fee_bps(kind(a))
        .unwrap() as u128;
        let bound = reserve * (2 * fee_bps - 1) / 20_000;
        let term = reserve * 200 / 20_000;
        assert!(vtrs_burned_in <= bound, "slice {vtrs_burned_in} within the venue bound {bound}");
        assert!(vtrs_burned_in > bound * 99 / 100, "and sized to it, not to something smaller");
        assert!(bound < term, "the term (200 bps → {term}) did not apply");
    });
}

// ---- fork-only migration ----------------------------------------------------

/// v1: the R1 recount. A chain that sold under the old rule has
/// `LnrgAccounted` above what any launch can claim; after the recount it is
/// their sum, and the next harvest attributes the stranded rewards.
#[test]
fn m_v1_recount_makes_stranded_rewards_attributable() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        fund_broker(10_000 * UNIT);
        pay_rewards(100 * UNIT);
        assert_ok!(LaunchTreasury::harvest(origin(KEEPER)));
        run_to(now() + BURN_INTERVAL);
        assert_ok!(compound(a));
        // What the old code left behind: accounted still at the harvested balance.
        let sold = 100 * UNIT - treasury(a).lnrg_accrued;
        LnrgAccounted::<Test>::mutate(|x| *x += sold);
        pay_rewards(30 * UNIT);
        // Stranded under v0: nothing to attribute although 30 LNRG arrived.
        assert_noop!(LaunchTreasury::harvest(origin(KEEPER)), Error::<Test>::NothingToDo);

        frame_support::traits::StorageVersion::new(0).put::<LaunchTreasury>();
        use frame_support::traits::OnRuntimeUpgrade;
        let _ = migrations::v1::MigrateToV1::<Test>::on_runtime_upgrade();
        assert_eq!(frame_support::traits::StorageVersion::get::<LaunchTreasury>(), 1);
        assert_eq!(LnrgAccounted::<Test>::get(), LaunchTreasury::claimable_lnrg(a).unwrap());
        assert_ok!(LaunchTreasury::harvest(origin(KEEPER)));
        assert!(
            LaunchTreasury::claimable_lnrg(a).unwrap() >= 30 * UNIT - 100,
            "the 30 LNRG are the launch's now"
        );
        ok_state();
    });
}

/// R8 — seen live on 222, the first compound after the R1 recount: the
/// harvest attributed exactly the vault's whole LNRG balance to the one
/// launch, the sale asked the broker for all of it with `keep_alive`, and
/// the broker refused — reducible under `Preserve` is balance minus the
/// asset's min balance. Every earlier sale had worked only because the
/// accumulator's floor rounding left a wei behind. The sale must never ask
/// for more than the vault can part with.
#[test]
fn r8_a_sale_never_asks_for_the_vaults_whole_lnrg() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 10);
        assert_ok!(stake(a));
        fund_broker(10_000 * UNIT);
        // A reward the accumulator attributes without a remainder: a multiple of the shares.
        let shares = treasury(a).shares;
        pay_rewards(shares * 3);
        run_to(now() + BURN_INTERVAL);
        let r = compound(a);
        assert!(r.is_ok(), "the sale must fit what the vault can part with: {:?}", r);
        assert_eq!(lnrg(vault()), 1, "the asset's min balance stays");
        assert_eq!(treasury(a).lnrg_accrued, 1, "and is still the launch's");
        ok_state();
    });
}

/// R9 — found by the fuzzer: a swap of an account's whole native balance.
/// `do_swap` took the input with `Expendable`, the buyer's account died,
/// and the tokens could not be delivered to it (`CannotCreate` for a
/// non-sufficient asset): the whole swap reverted with an error the user
/// cannot read. Fees are paid in energy on this chain, so spending the
/// last VTRS is reachable. A person's swap keeps their ED, as the curve's
/// buy and the treasury's own transfers do; only a settled intent, which
/// swaps from the intent escrow, spends to zero.
#[test]
fn r9_a_swap_of_ones_whole_balance_keeps_the_ed_instead_of_failing() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 1);
        // Dave has exactly 5 VTRS and no tokens.
        let dave: Acc = acc(4);
        assert_ok!(Balances::transfer_allow_death(origin(ALICE), dave, 5 * UNIT));
        // Everything above the ED: goes through, the account lives, the tokens arrive.
        let r = VitreusDex::swap_exact_tokens_for_tokens(
            origin(dave),
            NativeOrAssetId::Native,
            kind(a),
            5 * UNIT - ED,
            0,
            dave,
        );
        assert!(r.is_ok(), "a swap of everything above the ED: {:?}", r);
        assert_eq!(vtrs(dave), ED, "the ED stays");
        assert!(tok(a, dave) > 0, "and the tokens arrived");
        // The ED itself: refused up front with the same answer the curve gives,
        // not `CannotCreate` after the account has died.
        let r = VitreusDex::swap_exact_tokens_for_tokens(
            origin(dave),
            NativeOrAssetId::Native,
            kind(a),
            ED,
            0,
            dave,
        );
        assert!(
            matches!(
                r,
                Err(DispatchError::Token(
                    sp_runtime::TokenError::Frozen | sp_runtime::TokenError::NotExpendable
                ))
            ),
            "the ED cannot be spent, said up front: {:?}",
            r
        );
        assert_eq!(vtrs(dave), ED, "and nothing moved");
    });
}

/// vitreus-dex SECURITY_AUDIT Finding 14 — a routed fee slice below ED could
/// not create its recipient (the DEX fee escrow, never funded), so a swap whose
/// protocol+creator slice was under ED failed in full with `Token(BelowMinimum)`
/// — a token error the caller cannot read as "smaller than the fee floor". The
/// fix leaves a sub-ED slice in the pool (it accrues to LPs) rather than failing.
#[test]
fn finding14_a_sub_ed_routed_slice_does_not_fail_the_swap() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 0);
        // The DEX fee escrow has never received a fee, so it does not exist.
        assert!(!System::account_exists(&VitreusDex::fee_escrow_account()));
        let dave: Acc = acc(30);
        assert_ok!(Balances::transfer_allow_death(origin(ALICE), dave, 10 * UNIT));
        // 9×10^14 wei native in: protocol+creator = 10 bps = 9×10^11 < ED (10^12).
        let small = 900_000_000_000_000u128;
        let r = VitreusDex::swap_exact_tokens_for_tokens(
            origin(dave),
            NativeOrAssetId::Native,
            kind(a),
            small,
            0,
            dave,
        );
        assert!(r.is_ok(), "a sub-ED routed slice must not fail the swap: {:?}", r);
        assert!(tok(a, dave) > 0, "and the buyer got tokens");
    });
}

/// R11 — R9 over-reached: `Preserve` was applied to the input whatever the
/// asset, and pallet-assets reads `Preserve` as "keep the minimum balance",
/// so a holder selling their whole token position was refused with
/// `NotExpendable`. The ED is a property of the native account; a token
/// balance may go to zero. (The fuzz allowlist accepted any funds error on
/// a sell and so missed it; the launchpad's fm03/fm13 caught it.)
#[test]
fn r11_selling_ones_whole_token_position_goes_through() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 0);
        let dave: Acc = acc(6);
        assert_ok!(Balances::transfer_allow_death(origin(ALICE), dave, 20 * UNIT));
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            origin(dave),
            NativeOrAssetId::Native,
            kind(a),
            10 * UNIT,
            0,
            dave
        ));
        let held = tok(a, dave);
        assert!(held > 0);
        let before = vtrs(dave);
        let r = VitreusDex::swap_exact_tokens_for_tokens(
            origin(dave),
            kind(a),
            NativeOrAssetId::Native,
            held,
            0,
            dave,
        );
        assert!(r.is_ok(), "the whole position sells: {:?}", r);
        assert_eq!(tok(a, dave), 0, "nothing left");
        assert!(vtrs(dave) > before, "and the VTRS arrived");
    });
}

/// R10 — found by the fuzzer: a swap whose output rounds to zero. `do_swap`
/// computed `amount_out = 0` for 1 wei into a nearly drained pool and went
/// on to transfer it, and pallet-assets refused to open the buyer's token
/// account with nothing in it — `Token(BelowMinimum)`, from the wrong
/// pallet, for "you would get nothing". Say `ZeroAmount` before anything moves.
#[test]
fn r10_a_swap_that_would_deliver_nothing_says_so() {
    new_test_ext().execute_with(|| {
        let a = graduated_with_volume(ALICE, 0);
        // Drain the pool's token side: a buy of 774,000 VTRS leaves under a
        // millionth of the tokens, so 1 wei buys 0.99 of a unit.
        pool_buy(ALICE, a, 774_000 * UNIT);
        let dave: Acc = acc(5);
        assert_ok!(Balances::transfer_allow_death(origin(ALICE), dave, UNIT));
        let r = VitreusDex::swap_exact_tokens_for_tokens(
            origin(dave),
            NativeOrAssetId::Native,
            kind(a),
            1,
            0,
            dave,
        );
        assert_eq!(
            r,
            Err(pallet_vitreus_dex::Error::<Test>::ZeroAmount.into()),
            "nothing would be delivered: {:?}",
            r
        );
        assert_eq!(vtrs(dave), UNIT, "and nothing moved");
    });
}
