//! The DEX core on the base pallet (pallet-asset-conversion): the
//! assumptions it rests on, each tested on the runtime-like world before
//! anything is built on it.
//!
//! - The base pallet's `AdminOrigin` is satisfied by this pallet signing as
//!   its own account, and by nothing else; so a pool's fee is the one this
//!   pallet set at creation.
//! - Opening a holder's LP-token account with a touch before an add takes no
//!   consumer reference, where a mint into an unopened account takes one.
//! - A native-in swap whose routed slices come off the input in native, with
//!   the rest swapped through `SwapCredit` in the same transaction, reaches
//!   the curve with what today's single formula leaves after the tier.

use crate::{mock::*, EnsureDexAccount};
use frame_support::instances::Instance2;
use frame_support::{
    assert_noop, assert_ok,
    traits::{
        fungible::Inspect as _,
        fungibles::Balanced,
        tokens::{Fortitude::Polite, Precision::Exact, Preservation::Preserve},
        AccountTouch, EnsureOrigin,
    },
};
use pallet_asset_conversion::{Pools, SwapCredit};
use sp_runtime::{traits::BadOrigin, DispatchError, PerThing, Permill, Rounding};

fn native() -> NativeOrAssetId {
    NativeOrAssetId::Native
}
fn usdc() -> NativeOrAssetId {
    NativeOrAssetId::WithId(USDC_ID)
}

fn dex_origin() -> RuntimeOrigin {
    RuntimeOrigin::signed(VitreusDex::account_id())
}

/// The base pool fee that keeps the LP share of a swap what it is under a
/// single tier: `(t − r) / (1 − r)` for tier `t` and routed share `r`, both
/// in bps, rounded up so LPs never get less than the tier leaves them.
fn base_fee(tier_bps: u32, routed_bps: u32) -> Permill {
    Permill::from_rational_with_rounding(tier_bps - routed_bps, 10_000 - routed_bps, Rounding::Up)
        .unwrap()
}

fn pool_account() -> AccountId {
    <PoolLocator as pallet_asset_conversion::PoolLocator<_, _, _>>::address(&(native(), usdc()))
        .unwrap()
}

fn lp_token() -> u32 {
    Pools::<Test>::get((native(), usdc())).expect("pool").lp_token
}

/// The native/USDC pool, created through `AdminOrigin` with `creator` paying
/// its deposits, at the base fee for tier 30 bps with 20 bps routed.
fn create_native_usdc(creator: AccountId) {
    assert_ok!(AssetConversion::create_pool_with_fee(
        dex_origin(),
        creator,
        Box::new(native()),
        Box::new(usdc()),
        base_fee(30, 20),
    ));
}

/// Opens `who`'s LP account as this pallet will before every add: a touch,
/// deposit-backed, with the holder as depositor (the deposit is zero).
///
/// Whenever the account is missing, not when `should_touch` says so: that
/// answers whether a mint could open the account on a consumer reference,
/// and says no touch is needed while the holder has room for one.
fn open_lp_account(who: AccountId) {
    let lp = lp_token();
    if !pallet_assets::Account::<Test, Instance2>::contains_key(lp, who) {
        assert_ok!(<PoolAssets as AccountTouch<u32, AccountId>>::touch(lp, &who, &who));
    }
}

fn add(who: AccountId, native_amount: u128, usdc_amount: u128) -> Result<u128, DispatchError> {
    frame_support::storage::with_storage_layer(|| {
        pallet_asset_conversion::Pallet::<Test>::add_liquidity(
            RuntimeOrigin::signed(who),
            Box::new(native()),
            Box::new(usdc()),
            native_amount,
            usdc_amount,
            0,
            0,
            who,
        )
    })?;
    Ok(PoolAssets::balance(lp_token(), who))
}

#[test]
fn admin_origin_is_dex_account_and_nothing_else() {
    new_runtime_like_ext().execute_with(|| {
        // The DEX account has never existed on this chain; the origin check
        // needs no balance or account for it.
        assert!(!System::account_exists(&VitreusDex::account_id()));
        assert_ok!(EnsureDexAccount::<Test>::try_origin(dex_origin()));

        for origin in [RuntimeOrigin::root(), RuntimeOrigin::signed(ALICE), RuntimeOrigin::none()] {
            assert_noop!(
                AssetConversion::create_pool_with_fee(
                    origin.clone(),
                    ALICE,
                    Box::new(native()),
                    Box::new(usdc()),
                    Permill::from_parts(1_000),
                ),
                BadOrigin
            );
        }

        let reserved_before = Balances::reserved_balance(ALICE);
        create_native_usdc(ALICE);
        let pool_id = (native(), usdc());
        assert_eq!(
            pallet_asset_conversion::PoolFees::<Test>::get(&pool_id),
            Some(base_fee(30, 20))
        );
        // The creator, not the DEX account, paid the pool account's USDC
        // account deposit.
        assert_eq!(
            Balances::reserved_balance(ALICE) - reserved_before,
            RUNTIME_ASSET_ACCOUNT_DEPOSIT
        );
        assert!(!System::account_exists(&VitreusDex::account_id()));

        // Once set, only the same origin could change the fee; nothing a
        // runtime dispatches is that origin.
        for origin in [RuntimeOrigin::root(), RuntimeOrigin::signed(ALICE)] {
            assert_noop!(
                AssetConversion::set_pool_fee(origin, pool_id.clone(), Permill::zero()),
                BadOrigin
            );
        }
        assert_ok!(AssetConversion::set_pool_fee(dex_origin(), pool_id.clone(), base_fee(30, 20)));
    });
}

#[test]
fn touched_lp_account_takes_no_consumer_reference() {
    new_runtime_like_ext().execute_with(|| {
        create_native_usdc(ALICE);
        // The first add mints the locked minimum to the pool account, which
        // the creation touched.
        open_lp_account(ALICE);
        assert_ok!(add(ALICE, 1_000 * UNIT, 1_000 * UNIT));

        // Touched first: the holder's consumers are what they were, and the
        // account is deposit-backed.
        let before = System::consumers(&BOB);
        open_lp_account(BOB);
        let minted = add(BOB, 10 * UNIT, 10 * UNIT).unwrap();
        assert!(minted > 0);
        assert_eq!(System::consumers(&BOB), before);
        let account = pallet_assets::Account::<Test, Instance2>::get(lp_token(), BOB).unwrap();
        assert!(matches!(account.reason, pallet_assets::ExistenceReason::DepositHeld(0)));

        // Not touched: the mint opens the account on a consumer reference.
        let before = System::consumers(&CHARLIE);
        assert!(add(CHARLIE, 10 * UNIT, 10 * UNIT).unwrap() > 0);
        assert_eq!(System::consumers(&CHARLIE), before + 1);
        let account = pallet_assets::Account::<Test, Instance2>::get(lp_token(), CHARLIE).unwrap();
        assert!(matches!(account.reason, pallet_assets::ExistenceReason::Consumer));
    });
}

#[test]
fn holder_with_no_consumer_headroom_can_still_add_once_touched() {
    new_runtime_like_ext().execute_with(|| {
        create_native_usdc(ALICE);
        open_lp_account(ALICE);
        assert_ok!(add(ALICE, 1_000 * UNIT, 1_000 * UNIT));

        // Use up every consumer reference BOB has.
        while System::inc_consumers(&BOB).is_ok() {}
        let full = System::consumers(&BOB);

        // Unopened, the mint needs a reference and is refused.
        assert_eq!(
            add(BOB, 10 * UNIT, 10 * UNIT),
            Err(sp_runtime::TokenError::CannotCreate.into())
        );

        // Opened by a touch, the same add goes through and takes none.
        open_lp_account(BOB);
        assert!(add(BOB, 10 * UNIT, 10 * UNIT).unwrap() > 0);
        assert_eq!(System::consumers(&BOB), full);

        // Removing everything leaves the account, so the next add needs no
        // touch either.
        let lp = PoolAssets::balance(lp_token(), BOB);
        assert_ok!(AssetConversion::remove_liquidity(
            RuntimeOrigin::signed(BOB),
            Box::new(native()),
            Box::new(usdc()),
            lp,
            0,
            0,
            BOB,
        ));
        assert_eq!(PoolAssets::balance(lp_token(), BOB), 0);
        assert!(pallet_assets::Account::<Test, Instance2>::contains_key(lp_token(), BOB));
        assert!(add(BOB, 10 * UNIT, 10 * UNIT).unwrap() > 0);
        assert_eq!(System::consumers(&BOB), full);
    });
}

/// What the routed slices of `x` come to: each slice floors on its own, as
/// today's `bps_of` does.
fn routed_of(x: u128, slices_bps: &[u32]) -> u128 {
    slices_bps.iter().map(|s| x * *s as u128 / 10_000).sum()
}

/// Native in, the way the rebuilt swap will do it: the routed slices come off
/// the input in native (to `escrow`), and the rest is swapped through
/// `SwapCredit`, all in one storage layer. Returns the USDC delivered.
fn swap_native_in(
    who: AccountId,
    escrow: AccountId,
    x: u128,
    slices_bps: &[u32],
    min_out: u128,
) -> Result<u128, DispatchError> {
    frame_support::storage::with_storage_layer(|| {
        let credit = NativeAndAssets::withdraw(native(), &who, x, Exact, Preserve, Polite)?;
        let (routed, rest) = credit.split(routed_of(x, slices_bps));
        NativeAndAssets::resolve(&escrow, routed).map_err(|_| DispatchError::Other("escrow"))?;
        let out = <AssetConversion as SwapCredit<AccountId>>::swap_exact_tokens_for_tokens(
            vec![native(), usdc()],
            rest,
            (min_out > 0).then_some(min_out),
        )
        .map_err(|(_, e)| e)?;
        let amount = out.peek();
        NativeAndAssets::resolve(&who, out).map_err(|_| DispatchError::Other("deliver"))?;
        Ok(amount)
    })
}

/// Today's output for the same swap: the whole tier off the input, floored
/// once, then the constant-product formula.
fn today_out(x: u128, tier_bps: u32, reserve_in: u128, reserve_out: u128) -> u128 {
    let net = x - x * tier_bps as u128 / 10_000;
    let n = sp_core::U256::from(reserve_out) * sp_core::U256::from(net);
    (n / sp_core::U256::from(reserve_in + net)).as_u128()
}

#[test]
fn native_in_reaches_curve_with_what_tier_leaves() {
    // Tier 30 bps; protocol 10, creator 5, treasury 5 routed.
    let slices = [10u32, 5, 5];
    for (reserve_native, reserve_usdc) in
        [(1_000 * UNIT, 1_000 * UNIT), (50_000 * UNIT, 3_000 * UNIT), (7 * UNIT, 900_000 * UNIT)]
    {
        for x in [UNIT, 3 * UNIT + 12_345, UNIT / 1_000 + 7, 25 * UNIT, 999_999_999_999] {
            new_runtime_like_ext().execute_with(|| {
                create_native_usdc(ALICE);
                open_lp_account(ALICE);
                assert_ok!(add(ALICE, reserve_native, reserve_usdc));
                let pool = pool_account();
                let r_in = Balances::balance(&pool);
                let r_out = Assets::balance(USDC_ID, pool);
                let escrow = VitreusDex::fee_escrow_account();
                assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), escrow, RUNTIME_ED));

                let usdc_before = Assets::balance(USDC_ID, BOB);
                let out = swap_native_in(BOB, escrow, x, &slices, 0).unwrap();

                // Exact: the pool got the input less the routed slices, the
                // escrow got the slices, and the trader got what the base
                // pallet's formula gives for that input at the pool fee.
                let routed = routed_of(x, &slices);
                assert_eq!(Balances::balance(&pool) - r_in, x - routed);
                assert_eq!(Balances::balance(&escrow) - RUNTIME_ED, routed);
                assert_eq!(Assets::balance(USDC_ID, BOB) - usdc_before, out);
                assert_eq!(
                    out,
                    AssetConversion::get_amount_out(base_fee(30, 20), &(x - routed), &r_in, &r_out)
                        .unwrap()
                );

                // Against today: the curve input differs only by the routed
                // slices' floors (under one unit each) and the base fee's
                // round-up (under one part per million of the input).
                let today = today_out(x, 30, r_in, r_out);
                let slack_in = slices.len() as u128 + x / 1_000_000 + 1;
                let slack_out = r_out * slack_in / r_in + 1;
                assert!(
                    out.abs_diff(today) <= slack_out,
                    "x {x} reserves {r_in}/{r_out}: new {out} today {today} slack {slack_out}"
                );
            });
        }
    }
}

#[test]
fn failed_swap_leaves_nothing_behind() {
    new_runtime_like_ext().execute_with(|| {
        create_native_usdc(ALICE);
        open_lp_account(ALICE);
        assert_ok!(add(ALICE, 1_000 * UNIT, 1_000 * UNIT));
        let pool = pool_account();
        let escrow = VitreusDex::fee_escrow_account();
        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), escrow, RUNTIME_ED));
        let snapshot = (
            Balances::balance(&BOB),
            Balances::balance(&pool),
            Balances::balance(&escrow),
            Balances::total_issuance(),
            Assets::balance(USDC_ID, BOB),
        );

        let err = swap_native_in(BOB, escrow, UNIT, &[10, 5, 5], u128::MAX).unwrap_err();
        assert_eq!(
            err,
            pallet_asset_conversion::Error::<Test>::ProvidedMinimumNotSufficientForSwap.into()
        );
        assert_eq!(
            snapshot,
            (
                Balances::balance(&BOB),
                Balances::balance(&pool),
                Balances::balance(&escrow),
                Balances::total_issuance(),
                Assets::balance(USDC_ID, BOB),
            )
        );
    });
}
