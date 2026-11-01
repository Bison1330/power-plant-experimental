//! How the DEX's own accounts, and the accounts it pays, live and die under
//! the runtime's rules: an existential deposit far above one unit, every
//! asset non-sufficient (so each one an account holds takes a consumer
//! reference, at most 16 of them), and deposit-backed asset accounts.
//!
//! Every test here runs in `new_runtime_like_ext`. Each states what must
//! hold, not how a fix achieves it; where a fix may either refuse an
//! operation up front or make it work, the test accepts both.

use crate::{mock::*, Pools};
use frame_support::assert_ok;

fn native() -> NativeOrAssetId {
    NativeOrAssetId::Native
}
fn usdc() -> NativeOrAssetId {
    NativeOrAssetId::WithId(USDC_ID)
}
fn vnrg() -> NativeOrAssetId {
    NativeOrAssetId::WithId(VNRG_ID)
}
fn sufficient() -> NativeOrAssetId {
    NativeOrAssetId::WithId(SUFFICIENT_ID)
}

fn native_of(who: AccountId) -> u128 {
    Balances::free_balance(who)
}

/// A plain pool (no routed fees) funded by ALICE with `each` of both sides.
fn pool(a: NativeOrAssetId, b: NativeOrAssetId, each: u128) {
    assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), a.clone(), b.clone(), 10));
    assert_ok!(VitreusDex::add_liquidity(RuntimeOrigin::signed(ALICE), a, b, each, each, 0, 0));
}

// ---------------------------------------------------------------------------
// Routed swap fees
// ---------------------------------------------------------------------------

#[test]
#[ignore = "fails: after the protocol fees are withdrawn, the creator's fee claim is refused"]
fn withdrawing_protocol_fees_leaves_creators_unclaimed_fees_claimable() {
    new_runtime_like_ext().execute_with(|| {
        let launch = NativeOrAssetId::WithId(LAUNCH_ID);
        assert_ok!(Assets::force_create(RuntimeOrigin::root(), LAUNCH_ID, ALICE, false, 1));
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, ALICE, 1_000_000 * UNIT));
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 15, 1, 0));
        assert_ok!(<VitreusDex as crate::ReservedPoolSeeder<
            AccountId,
            NativeOrAssetId,
            u128,
            u64,
        >>::seed_reserved_pool_for(
            &ALICE,
            launch.clone(),
            native(),
            100_000 * UNIT,
            1_000 * UNIT,
            3
        ));
        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), CREATOR, 100 * UNIT));
        // The creator's slice of this swap is below one ED; the protocol's is not.
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            launch.clone(),
            9 * UNIT / 10,
            1,
            BOB,
        ));
        assert!(crate::CreatorFeesUnclaimed::<Test>::iter().any(|(_, f)| f > 0 && f < RUNTIME_ED));

        assert_ok!(VitreusDex::withdraw_protocol_fees(RuntimeOrigin::signed(DAVE)));
        assert_ok!(VitreusDex::claim_pool_creator_fees(RuntimeOrigin::signed(CREATOR), launch));
    });
}

/// A plain native/USDC pool that routes a protocol slice of every trade
/// to the fee escrow.
fn routed_pool() {
    assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 10, 5, 0, 0));
    pool(native(), usdc(), 10_000 * UNIT);
}

#[test]
#[ignore = "fails: withdrawing protocol fees is refused once one unit of a token sits on the fee escrow"]
fn stray_token_unit_on_fee_escrow_does_not_freeze_protocol_fees() {
    new_runtime_like_ext().execute_with(|| {
        routed_pool();
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            usdc(),
            10 * UNIT,
            1,
            BOB,
        ));
        assert!(crate::ProtocolFeesUnclaimed::<Test>::get() >= RUNTIME_ED);
        let escrow = VitreusDex::fee_escrow_account();
        assert_ok!(Assets::transfer(RuntimeOrigin::signed(DAVE), USDC_ID, escrow, 1));

        assert_ok!(VitreusDex::withdraw_protocol_fees(RuntimeOrigin::signed(DAVE)));
    });
}

#[test]
#[ignore = "fails: a swap whose protocol slice is below the ED is refused while the fee escrow holds a sufficient token"]
fn sufficient_token_on_fee_escrow_does_not_fail_small_swaps() {
    new_runtime_like_ext().execute_with(|| {
        routed_pool();
        // The fee escrow now exists, holding no native.
        let escrow = VitreusDex::fee_escrow_account();
        assert_ok!(Assets::transfer(RuntimeOrigin::signed(DAVE), SUFFICIENT_ID, escrow, 1));

        // Its protocol slice is below one ED.
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            usdc(),
            UNIT / 1_000,
            1,
            BOB,
        ));
    });
}

// ---------------------------------------------------------------------------
// Pool accounts
// ---------------------------------------------------------------------------

#[test]
#[ignore = "fails: the first deposit into a pool with no native side is refused"]
fn pool_without_native_side_can_be_funded() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10));

        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            10 * UNIT,
            10 * UNIT,
            0,
            0,
        ));
    });
}

#[test]
#[ignore = "fails: adding liquidity after the last LP has left is refused"]
fn pool_against_sufficient_token_still_takes_liquidity_after_its_last_lp_leaves() {
    new_runtime_like_ext().execute_with(|| {
        pool(native(), sufficient(), 10 * UNIT);
        let pair = VitreusDex::canonical_pair(native(), sufficient());
        let shares = crate::LiquidityPositions::<Test>::get(ALICE, &pair).expect("position").shares;
        if VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            sufficient(),
            shares,
            0,
            0,
        )
        .is_err()
        {
            return;
        }

        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            native(),
            sufficient(),
            10 * UNIT,
            10 * UNIT,
            0,
            0,
        ));
    });
}

#[test]
#[ignore = "fails: after one LP exits, the pool account holds no native although another LP still has a share"]
fn lps_exit_never_destroys_another_lps_share_of_pool() {
    new_runtime_like_ext().execute_with(|| {
        pool(native(), sufficient(), 10 * UNIT);
        let small = 9 * RUNTIME_ED / 100;
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            native(),
            sufficient(),
            small,
            small,
            0,
            0,
        ));
        let pair = VitreusDex::canonical_pair(native(), sufficient());
        let shares = crate::LiquidityPositions::<Test>::get(ALICE, &pair).expect("position").shares;
        if VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            sufficient(),
            shares,
            0,
            0,
        )
        .is_err()
        {
            return;
        }

        let pool_account = Pools::<Test>::get(&pair).expect("pool").pool_account;
        assert!(native_of(pool_account) > 0, "BOB's native left with ALICE's exit");
    });
}

#[test]
#[ignore = "fails: after a native-out swap the pool account holds no native while its recorded reserve is above zero"]
fn native_out_swap_never_leaves_pool_holding_less_native_than_it_records() {
    new_runtime_like_ext().execute_with(|| {
        pool(native(), sufficient(), 3 * RUNTIME_ED);
        let pair = VitreusDex::canonical_pair(native(), sufficient());
        let pool_account = Pools::<Test>::get(&pair).expect("pool").pool_account;
        // Enough of the token to buy all but about half an ED of the native.
        let swapped = VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            sufficient(),
            native(),
            12 * RUNTIME_ED,
            1,
            BOB,
        );
        if swapped.is_err() {
            return;
        }

        let recorded = Pools::<Test>::get(&pair).expect("pool").reserve_a;
        assert!(recorded > 0, "the swap left native in the pool");
        assert_eq!(native_of(pool_account), recorded);
    });
}

// ---------------------------------------------------------------------------
// Genesis
// ---------------------------------------------------------------------------

#[test]
fn chain_from_genesis_starts_with_fee_escrow_holding_ed() {
    new_runtime_like_ext_from_genesis().execute_with(|| {
        let escrow = VitreusDex::fee_escrow_account();
        assert!(native_of(escrow) >= RUNTIME_ED, "{escrow} holds {}", native_of(escrow));
    });
}
