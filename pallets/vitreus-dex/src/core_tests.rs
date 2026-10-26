//! The DEX's own calls on the base pallet: pool records and the base pools
//! behind them, liquidity in LP tokens, and swaps whose routed slices are
//! taken outside the pool. All on the runtime-like world.

use crate::{
    mock::*, AmmBase, CreatorFeesUnclaimed, Error, Event as DexEvent, FeeRouting, FreezeReason,
    IntentSwap, LastSwapBlock, LiquidityLocks, PoolInfo, PoolManager, Pools, ProtocolFeesUnclaimed,
    ReservedPoolSeeder, SeedOutcome, MINIMUM_LIQUIDITY,
};
use frame_support::{assert_noop, assert_ok, traits::fungibles::Inspect};
use sp_runtime::{traits::BadOrigin, DispatchError, DispatchResult, Permill};

fn native() -> NativeOrAssetId {
    NativeOrAssetId::Native
}
fn usdc() -> NativeOrAssetId {
    NativeOrAssetId::WithId(USDC_ID)
}
fn vnrg() -> NativeOrAssetId {
    NativeOrAssetId::WithId(VNRG_ID)
}
fn launch() -> NativeOrAssetId {
    NativeOrAssetId::WithId(LAUNCH_ID)
}

fn base_pool_exists(a: NativeOrAssetId, b: NativeOrAssetId) -> bool {
    <Test as crate::Config>::Base::pool_exists(&a, &b)
}
fn pool_account(a: NativeOrAssetId, b: NativeOrAssetId) -> AccountId {
    VitreusDex::pool_account_for(a, b).unwrap()
}
fn lp_of(a: NativeOrAssetId, b: NativeOrAssetId, who: AccountId) -> u128 {
    <Test as crate::Config>::Base::lp_balance(&a, &b, &who)
}
fn balance(asset: NativeOrAssetId, who: AccountId) -> u128 {
    <NativeAndAssets as Inspect<AccountId>>::balance(asset, &who)
}
fn escrow() -> AccountId {
    VitreusDex::fee_escrow_account()
}

/// A pool record as a launch's seed leaves it: every routed slice. Seeding
/// itself is refused until it is rebuilt on freezes; the record is what a
/// swap reads.
fn seeded_record(asset: NativeOrAssetId, fee_tier: u32, routing: FeeRouting) {
    Pools::<Test>::insert(
        VitreusDex::canonical_pair(native(), asset),
        PoolInfo { fee_tier, total_fees_collected: 0, routing },
    );
}

/// The launch asset, created and held by ALICE and BOB.
fn create_launch_asset() {
    assert_ok!(Assets::force_create(RuntimeOrigin::root(), LAUNCH_ID, ALICE, false, 1));
    for who in [ALICE, BOB] {
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, who, 10_000_000 * UNIT));
    }
}

fn add(who: AccountId, a: NativeOrAssetId, b: NativeOrAssetId, x: u128, y: u128) -> u128 {
    let before = lp_of(a.clone(), b.clone(), who);
    assert_ok!(VitreusDex::add_liquidity(
        RuntimeOrigin::signed(who),
        a.clone(),
        b.clone(),
        x,
        y,
        0,
        0
    ));
    lp_of(a, b, who) - before
}

fn swap(who: AccountId, a: NativeOrAssetId, b: NativeOrAssetId, x: u128) -> u128 {
    let before = balance(b.clone(), who);
    assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
        RuntimeOrigin::signed(who),
        a,
        b.clone(),
        x,
        1,
        who
    ));
    balance(b, who) - before
}

fn wide(x: u128) -> sp_core::U256 {
    sp_core::U256::from(x)
}
/// `floor(a · b / c)`.
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    (wide(a) * wide(b) / wide(c)).as_u128()
}

#[test]
fn pool_is_record_until_its_first_deposit_creates_base_pool() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 10, 5, 5));
        assert_noop!(
            VitreusDex::create_pool(RuntimeOrigin::signed(ALICE), native(), usdc(), 3),
            BadOrigin
        );
        assert_noop!(
            VitreusDex::create_pool(RuntimeOrigin::root(), native(), launch(), 3),
            Error::<Test>::ReservedAsset
        );
        assert_noop!(
            VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 2),
            Error::<Test>::InvalidFeeTier
        );
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        assert_noop!(
            VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), native(), 3),
            Error::<Test>::PoolAlreadyExists
        );
        // A governance pool routes the protocol slice alone.
        let record = Pools::<Test>::get((native(), usdc())).unwrap();
        assert_eq!(
            record.routing,
            FeeRouting { protocol_bps: 10, creator_bps: 0, treasury_bps: 0 }
        );
        assert!(!base_pool_exists(native(), usdc()));

        // The first deposit creates it, at the LP fee the record fixes, the
        // depositor paying its USDC account's deposit, and locks the base
        // pallet's minimum of LP tokens.
        let reserved = Balances::reserved_balance(BOB);
        let lp = add(BOB, usdc(), native(), 400 * UNIT, 100 * UNIT);
        assert!(base_pool_exists(native(), usdc()));
        assert_eq!(Balances::reserved_balance(BOB) - reserved, RUNTIME_ASSET_ACCOUNT_DEPOSIT);
        assert_eq!(lp, 200 * UNIT - MINIMUM_LIQUIDITY as u128);
        let fee = <Test as crate::Config>::Base::pool_fee(&native(), &usdc()).unwrap();
        // (30 − 10) / (10_000 − 10) = 2002.002… ppm, rounded up.
        assert_eq!(fee, Permill::from_parts(2_003));
        assert_eq!(fee, VitreusDex::lp_fee(3, &record.routing));
        System::assert_has_event(
            DexEvent::LiquidityAdded {
                provider: BOB,
                asset_a: native(),
                asset_b: usdc(),
                amount_a: 100 * UNIT,
                amount_b: 400 * UNIT,
                shares_minted: lp,
            }
            .into(),
        );
        assert_ok!(VitreusDex::do_try_state());
    });
}

#[test]
fn lp_fee_keeps_what_tier_leaves_lps() {
    let r = |p, c, t| FeeRouting { protocol_bps: p, creator_bps: c, treasury_bps: t };
    // Nothing routed: the whole tier.
    assert_eq!(VitreusDex::lp_fee(1, &r(0, 0, 0)), Permill::from_parts(1_000));
    assert_eq!(VitreusDex::lp_fee(10, &r(0, 0, 0)), Permill::from_parts(10_000));
    // (t − r) / (1 − r), up: 10/9980, 10/9910, 5/9995.
    assert_eq!(VitreusDex::lp_fee(3, &r(10, 5, 5)), Permill::from_parts(1_003));
    assert_eq!(VitreusDex::lp_fee(10, &r(50, 20, 20)), Permill::from_parts(1_010));
    assert_eq!(VitreusDex::lp_fee(1, &r(5, 0, 0)), Permill::from_parts(501));
    // Never below what the tier leaves: p · (1 − r) ≥ t − r.
    for tier in [1u32, 3, 10] {
        for routed in 0..=(tier * 10 - crate::pool_floor_bps(tier) as u32) as u16 {
            let p = VitreusDex::lp_fee(tier, &r(routed, 0, 0)).deconstruct() as u64;
            let lhs = p * (10_000 - routed as u64);
            let rhs = (tier as u64 * 10 - routed as u64) * 1_000_000;
            assert!(lhs >= rhs && lhs - rhs < 10_000, "tier {tier} routed {routed}: {p}");
        }
    }
}

#[test]
fn liquidity_comes_back_as_its_share_of_reserves() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        add(ALICE, native(), usdc(), 1_000 * UNIT, 2_000 * UNIT);
        let lp = add(BOB, native(), usdc(), 10 * UNIT, 20 * UNIT);
        swap(CHARLIE, native(), usdc(), 37 * UNIT);

        let pool = pool_account(native(), usdc());
        let (rn, ru) = (balance(native(), pool), balance(usdc(), pool));
        let issuance = <Test as crate::Config>::Base::lp_issuance(&native(), &usdc());
        let (n0, u0) = (balance(native(), BOB), balance(usdc(), BOB));

        assert_noop!(
            VitreusDex::remove_liquidity(
                RuntimeOrigin::signed(BOB),
                native(),
                usdc(),
                lp + 1,
                0,
                0
            ),
            Error::<Test>::InsufficientShares
        );
        assert_noop!(
            VitreusDex::remove_liquidity(RuntimeOrigin::signed(BOB), native(), usdc(), 0, 0, 0),
            Error::<Test>::ZeroAmount
        );
        // The caller's order: USDC first.
        let half = lp / 2;
        assert_ok!(VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(BOB),
            usdc(),
            native(),
            half,
            mul_div(half, ru, issuance),
            mul_div(half, rn, issuance),
        ));
        assert_eq!(balance(native(), BOB) - n0, mul_div(half, rn, issuance));
        assert_eq!(balance(usdc(), BOB) - u0, mul_div(half, ru, issuance));
        assert_eq!(lp_of(native(), usdc(), BOB), lp - half);
        System::assert_has_event(
            DexEvent::LiquidityRemoved {
                provider: BOB,
                asset_a: native(),
                asset_b: usdc(),
                amount_a: mul_div(half, rn, issuance),
                amount_b: mul_div(half, ru, issuance),
                shares_burned: half,
            }
            .into(),
        );

        // Everything else: the account stays, empty, and needs no reopening.
        let consumers = System::consumers(&BOB);
        assert_ok!(VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(BOB),
            native(),
            usdc(),
            lp - half,
            0,
            0
        ));
        assert_eq!(lp_of(native(), usdc(), BOB), 0);
        assert_eq!(System::consumers(&BOB), consumers);
        add(BOB, native(), usdc(), UNIT, 2 * UNIT);
        assert_eq!(System::consumers(&BOB), consumers);
        assert_ok!(VitreusDex::do_try_state());
    });
}

#[test]
fn lp_with_no_consumer_reference_to_spare_can_add() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        add(ALICE, native(), usdc(), 1_000 * UNIT, 1_000 * UNIT);
        while System::inc_consumers(&BOB).is_ok() {}
        let full = System::consumers(&BOB);
        assert!(add(BOB, native(), usdc(), UNIT, UNIT) > 0);
        assert_eq!(System::consumers(&BOB), full);
    });
}

/// The swap as it is: the routing a seeded launch pool carries, and a
/// treasury sink for its token.
fn launch_pool(reserve_native: u128, reserve_launch: u128) {
    create_launch_asset();
    seeded_record(launch(), 3, FeeRouting { protocol_bps: 10, creator_bps: 5, treasury_bps: 5 });
    SINK_VAULT.with(|v| *v.borrow_mut() = Some(VAULT));
    assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), VAULT, RUNTIME_ED));
    add(ALICE, native(), launch(), reserve_native, reserve_launch);
}

#[test]
fn native_in_takes_routed_slices_off_input() {
    new_runtime_like_ext().execute_with(|| {
        launch_pool(1_000 * UNIT, 4_000_000 * UNIT);
        let pool = pool_account(native(), launch());
        let (rn, rl) = (balance(native(), pool), balance(launch(), pool));
        let x = 3 * UNIT + 12_345;
        let (p, c, t) = (x * 10 / 10_000, x * 5 / 10_000, x * 5 / 10_000);
        let routed = p + c + t;
        let quoted =
            <Test as crate::Config>::Base::amount_out(&native(), &launch(), x - routed).unwrap();

        System::set_block_number(7);
        let out = swap(BOB, native(), launch(), x);
        assert_eq!(out, quoted);
        assert_eq!(balance(native(), pool) - rn, x - routed);
        assert_eq!(rl - balance(launch(), pool), out);
        assert_eq!(balance(native(), escrow()) - RUNTIME_ED, p + c);
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), p);
        assert_eq!(CreatorFeesUnclaimed::<Test>::get((native(), launch())), c);
        assert_eq!(balance(native(), VAULT) - RUNTIME_ED, t);
        assert_eq!(SINK_NOTED.with(|n| n.borrow().clone()), vec![(LAUNCH_ID, t)]);
        assert_eq!(LastSwapBlock::<Test>::get((native(), launch())), Some(7));
        System::assert_has_event(
            DexEvent::FeesRouted {
                pool: (native(), launch()),
                protocol: p,
                creator: c,
                treasury: t,
            }
            .into(),
        );
        assert_ok!(VitreusDex::do_try_state());
    });
}

/// Before the base pallet, native out charged `t − r` on the input and took
/// `r` off the output, both floored.
fn old_native_out(x: u128, tier_bps: u128, routed: [u128; 3], r_in: u128, r_out: u128) -> u128 {
    let r: u128 = routed.iter().sum();
    let net = x - x * (tier_bps - r) / 10_000;
    let gross = mul_div(r_out, net, r_in + net);
    gross - routed.iter().map(|s| gross * s / 10_000).sum::<u128>()
}

#[test]
fn native_out_takes_routed_slices_off_output() {
    new_runtime_like_ext().execute_with(|| {
        launch_pool(1_000 * UNIT, 4_000_000 * UNIT);
        let pool = pool_account(native(), launch());
        let (rn, rl) = (balance(native(), pool), balance(launch(), pool));
        let x = 40_000 * UNIT;
        let gross = <Test as crate::Config>::Base::amount_out(&launch(), &native(), x).unwrap();
        let (p, c, t) = (gross * 10 / 10_000, gross * 5 / 10_000, gross * 5 / 10_000);

        let out = swap(BOB, launch(), native(), x);
        assert_eq!(out, gross - p - c - t);
        assert_eq!(balance(launch(), pool) - rl, x);
        assert_eq!(rn - balance(native(), pool), gross);
        assert_eq!(balance(native(), escrow()) - RUNTIME_ED, p + c);
        assert_eq!(balance(native(), VAULT) - RUNTIME_ED, t);
        assert_ok!(VitreusDex::do_try_state());
    });
}

#[test]
fn native_out_pays_what_it_paid_before_less_few_parts_per_million() {
    // The single-pool swap already took the routed slices off the native
    // output, and charged `t − r` on the input. The base pool charges
    // `(t − r) / (1 − r)` there instead: more by `(t − r) · r / (1 − r)` of
    // the input, about 2 ppm at 30 bps with 20 routed, plus at most 1 ppm
    // from rounding that fee up. Whatever the trade's size.
    let routed = [10u128, 5, 5];
    for (rn, rl) in [(1_000 * UNIT, 4_000_000 * UNIT), (50 * UNIT, 900_000 * UNIT)] {
        for share_ppm in [10u128, 1_000, 10_000, 100_000] {
            new_runtime_like_ext().execute_with(|| {
                launch_pool(rn, rl);
                let x = rl / 1_000_000 * share_ppm;
                let old = old_native_out(x, 30, routed, rl, rn);
                let new = swap(BOB, launch(), native(), x);
                assert!(new <= old, "x {x}: new {new} old {old}");
                let gap_ppm = (old - new) * 1_000_000 / old;
                assert!(gap_ppm <= 3, "share {share_ppm} ppm of {rl}: gap {gap_ppm} ppm");
            });
        }
    }
}

#[test]
fn pair_with_no_native_side_routes_nothing() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 10, 0, 0));
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 3));
        add(ALICE, usdc(), vnrg(), 1_000 * UNIT, 1_000 * UNIT);
        assert_eq!(
            <Test as crate::Config>::Base::pool_fee(&usdc(), &vnrg()),
            Some(Permill::from_parts(3_000))
        );
        let quoted = <Test as crate::Config>::Base::amount_out(&usdc(), &vnrg(), UNIT).unwrap();
        assert_eq!(swap(BOB, usdc(), vnrg(), UNIT), quoted);
        assert_eq!(balance(native(), escrow()), RUNTIME_ED);
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), 0);
    });
}

#[test]
fn swap_that_cannot_complete_moves_nothing() {
    new_runtime_like_ext().execute_with(|| {
        launch_pool(1_000 * UNIT, 4_000_000 * UNIT);
        let pool = pool_account(native(), launch());
        let snapshot = || {
            (
                balance(native(), BOB),
                balance(launch(), BOB),
                balance(native(), pool),
                balance(launch(), pool),
                balance(native(), escrow()),
                Balances::total_issuance(),
                ProtocolFeesUnclaimed::<Test>::get(),
            )
        };
        let before = snapshot();
        assert_noop!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                native(),
                launch(),
                UNIT,
                u128::MAX,
                BOB
            ),
            Error::<Test>::SlippageExceeded
        );
        assert_noop!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                native(),
                launch(),
                UNIT,
                1,
                ALICE
            ),
            Error::<Test>::RecipientNotSigner
        );
        // Too small to pay anything.
        assert_noop!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                launch(),
                native(),
                1,
                0,
                BOB
            ),
            Error::<Test>::ZeroAmount
        );
        assert_noop!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                native(),
                vnrg(),
                UNIT,
                1,
                BOB
            ),
            Error::<Test>::PoolNotFound
        );
        assert_eq!(snapshot(), before);

        // A record whose first deposit has not come.
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), vnrg(), 3));
        assert_noop!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                native(),
                vnrg(),
                UNIT,
                1,
                BOB
            ),
            Error::<Test>::InsufficientLiquidity
        );
    });
}

#[test]
fn slice_escrow_cannot_take_goes_to_lps() {
    // Before the upgrade the escrow holds no native, so a slice below the
    // ED cannot open it.
    new_runtime_like_ext_before_upgrade().execute_with(|| {
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 10, 0, 0));
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        add(ALICE, native(), usdc(), 1_000 * UNIT, 1_000 * UNIT);
        let pool = pool_account(native(), usdc());
        let rn = balance(native(), pool);
        let x = UNIT / 100; // routes 10^13, below the 10^14 ED
        swap(BOB, native(), usdc(), x);
        assert_eq!(balance(native(), escrow()), 0);
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), 0);
        assert_eq!(balance(native(), pool) - rn, x);
    });
}

#[test]
fn only_persons_swap_moves_dormancy_clock() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        add(ALICE, native(), usdc(), 1_000 * UNIT, 1_000 * UNIT);
        assert_eq!(<VitreusDex as PoolManager<_, _, _, _>>::last_swap_block(usdc()), Some(0));
        System::set_block_number(5);
        assert_ok!(<VitreusDex as PoolManager<_, _, _, _>>::swap_for(
            &BOB,
            native(),
            usdc(),
            UNIT,
            1
        ));
        assert_eq!(<VitreusDex as PoolManager<_, _, _, _>>::last_swap_block(usdc()), Some(0));
        System::set_block_number(6);
        assert_ok!(VitreusDex::swap_exact_for_intent(&BOB, usdc(), native(), UNIT, 1));
        assert_eq!(<VitreusDex as PoolManager<_, _, _, _>>::last_swap_block(usdc()), Some(6));
        assert_eq!(<VitreusDex as PoolManager<_, _, _, _>>::last_swap_block(vnrg()), None);
        let (n, u) = <VitreusDex as PoolManager<_, _, _, _>>::native_reserves(usdc()).unwrap();
        let pool = pool_account(native(), usdc());
        assert_eq!((n, u), (balance(native(), pool), balance(usdc(), pool)));
        assert_eq!(<VitreusDex as PoolManager<_, _, _, _>>::fee_bps(usdc()), Some(30));
    });
}

#[test]
fn routed_fees_are_claimed_from_escrow() {
    new_runtime_like_ext().execute_with(|| {
        launch_pool(1_000 * UNIT, 4_000_000 * UNIT);
        swap(BOB, native(), launch(), 10 * UNIT);
        swap(BOB, launch(), native(), 10_000 * UNIT);
        let creator = CreatorFeesUnclaimed::<Test>::get((native(), launch()));
        let protocol = ProtocolFeesUnclaimed::<Test>::get();
        assert!(creator > 0 && protocol > 0);

        let c0 = balance(native(), CREATOR);
        assert_ok!(VitreusDex::claim_pool_creator_fees(RuntimeOrigin::signed(CREATOR), launch()));
        assert_eq!(balance(native(), CREATOR) - c0, creator);
        let t0 = balance(native(), TREASURY);
        assert_ok!(VitreusDex::withdraw_protocol_fees(RuntimeOrigin::signed(DAVE)));
        assert_eq!(balance(native(), TREASURY) - t0, protocol);
        assert_eq!(balance(native(), escrow()), RUNTIME_ED);
        assert_ok!(VitreusDex::do_try_state());
    });
}

fn lock_frozen(who: AccountId, a: NativeOrAssetId, b: NativeOrAssetId) -> u128 {
    <Test as crate::Config>::Base::lp_frozen(&a, &b, &who, &FreezeReason::LiquidityLock.into())
}
fn seed_frozen(who: AccountId, a: NativeOrAssetId, b: NativeOrAssetId) -> u128 {
    <Test as crate::Config>::Base::lp_frozen(&a, &b, &who, &FreezeReason::LaunchSeed.into())
}
fn remove(who: AccountId, a: NativeOrAssetId, b: NativeOrAssetId, lp: u128) -> DispatchResult {
    VitreusDex::remove_liquidity(RuntimeOrigin::signed(who), a, b, lp, 0, 0)
}

#[test]
fn lock_holds_whole_position_until_its_block() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        add(ALICE, native(), usdc(), 1_000 * UNIT, 1_000 * UNIT);
        assert_noop!(
            VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), native(), usdc(), 100),
            Error::<Test>::InsufficientShares
        );
        let first = add(BOB, native(), usdc(), 10 * UNIT, 10 * UNIT);
        assert_ok!(VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), usdc(), native(), 100));
        assert_eq!(LiquidityLocks::<Test>::get((native(), usdc()), BOB), Some(100));
        assert_eq!(lock_frozen(BOB, native(), usdc()), first);
        assert_noop!(remove(BOB, native(), usdc(), 1), Error::<Test>::PoolLocked);

        // Added while the lock is in force: locked with the rest.
        let more = add(BOB, native(), usdc(), 5 * UNIT, 5 * UNIT);
        assert_eq!(lock_frozen(BOB, native(), usdc()), first + more);
        assert_noop!(remove(BOB, native(), usdc(), more), Error::<Test>::PoolLocked);

        // A lock only lengthens.
        assert_noop!(
            VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), native(), usdc(), 99),
            Error::<Test>::LockCannotBeShortened
        );
        assert_ok!(VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), native(), usdc(), 120));

        System::set_block_number(119);
        assert_noop!(remove(BOB, native(), usdc(), 1), Error::<Test>::PoolLocked);
        // From its block on, the next remove lifts it.
        System::set_block_number(120);
        assert_ok!(remove(BOB, native(), usdc(), first + more));
        assert_eq!(LiquidityLocks::<Test>::get((native(), usdc()), BOB), None);
        assert_eq!(lock_frozen(BOB, native(), usdc()), 0);
        // An add after it is unlocked liquidity.
        let later = add(BOB, native(), usdc(), UNIT, UNIT);
        assert_ok!(remove(BOB, native(), usdc(), later));
        assert_ok!(VitreusDex::do_try_state());
    });
}

#[test]
fn lock_to_last_block_is_for_good() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        add(ALICE, native(), usdc(), 1_000 * UNIT, 1_000 * UNIT);
        let lp = add(BOB, native(), usdc(), 10 * UNIT, 10 * UNIT);
        assert_ok!(VitreusDex::lock_liquidity(
            RuntimeOrigin::signed(BOB),
            native(),
            usdc(),
            u64::MAX
        ));
        System::set_block_number(u64::MAX - 1);
        assert_noop!(remove(BOB, native(), usdc(), lp), Error::<Test>::PoolLocked);
        assert_noop!(
            <VitreusDex as PoolManager<_, _, _, _>>::lock_liquidity_for(&BOB, native(), usdc(), 5),
            Error::<Test>::LockCannotBeShortened
        );
    });
}

/// CHARLIE as a launch's escrow at graduation: exactly `tokens` of the
/// launch token, and plenty of native.
fn seeder_with(tokens: u128) -> AccountId {
    assert_ok!(Assets::force_create(RuntimeOrigin::root(), LAUNCH_ID, ALICE, false, 1));
    assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, CHARLIE, tokens));
    CHARLIE
}
fn seed(who: AccountId, tokens: u128, quote: u128) -> Result<SeedOutcome<u128>, DispatchError> {
    <VitreusDex as ReservedPoolSeeder<_, _, _, _>>::seed_reserved_pool_for(
        &who,
        launch(),
        native(),
        tokens,
        quote,
        3,
    )
}

#[test]
fn seed_clears_pool_address_deposits_and_freezes_for_good() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 10, 5, 5));
        let tokens = 200_000_000 * UNIT;
        let quote = 3_000 * UNIT;
        let escrow = seeder_with(tokens);
        // Donations parked at the pool address before the seed.
        let pool = pool_account(native(), launch());
        assert_ok!(Balances::transfer_allow_death(RuntimeOrigin::signed(DAVE), pool, 7 * UNIT));
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, pool, 5 * UNIT));
        let treasury = balance(native(), TREASURY);

        let out = seed(escrow, tokens, quote).unwrap();
        // The quote whole; the token less the unit the escrow keeps.
        assert_eq!(out.quote_seeded, quote);
        assert_eq!(out.asset_seeded, tokens - 1);
        assert_eq!(balance(launch(), escrow), 1);
        // The donations did not reach the opening price: swept and burned.
        assert_eq!(balance(native(), pool), quote);
        assert_eq!(balance(launch(), pool), tokens - 1);
        assert_eq!(balance(native(), TREASURY) - treasury, 7 * UNIT);
        // The full default split, and the escrow's LP tokens frozen for good.
        let record = Pools::<Test>::get((native(), launch())).unwrap();
        assert_eq!(
            record.routing,
            FeeRouting { protocol_bps: 10, creator_bps: 5, treasury_bps: 5 }
        );
        assert_eq!(lp_of(native(), launch(), escrow), out.shares);
        assert_eq!(seed_frozen(escrow, native(), launch()), out.shares);
        assert_noop!(remove(escrow, native(), launch(), 1), Error::<Test>::PoolLocked);
        // Locked to the last block, as seed positions always were: no lock
        // can shorten it, and no block reaches it.
        assert_eq!(LiquidityLocks::<Test>::get((native(), launch()), escrow), Some(u64::MAX));
        assert_noop!(
            VitreusDex::lock_liquidity(RuntimeOrigin::signed(escrow), native(), launch(), 5),
            Error::<Test>::LockCannotBeShortened
        );
        System::set_block_number(u64::MAX - 1);
        assert_noop!(remove(escrow, native(), launch(), 1), Error::<Test>::PoolLocked);
        assert_eq!(seed_frozen(escrow, native(), launch()), out.shares);

        // Once only.
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, escrow, tokens));
        assert_noop!(seed(escrow, tokens, quote), Error::<Test>::PoolAlreadySeeded);
        assert_ok!(VitreusDex::do_try_state());
    });
}

#[test]
fn seed_is_refused_where_it_cannot_be_what_it_promises() {
    new_runtime_like_ext().execute_with(|| {
        let tokens = 1_000 * UNIT;
        let escrow = seeder_with(tokens);
        // No default split for the tier: deferred, not graduated at zero.
        assert_noop!(seed(escrow, tokens, UNIT), Error::<Test>::NoDefaultFeeRouting);
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 10, 5, 5));
        // Not a reserved asset, or a reserved quote.
        assert_noop!(
            <VitreusDex as ReservedPoolSeeder<_, _, _, _>>::seed_reserved_pool_for(
                &escrow,
                usdc(),
                native(),
                UNIT,
                UNIT,
                3
            ),
            Error::<Test>::NotReservedAsset
        );
        // More than the seeder holds, by more than its kept minimum.
        assert_noop!(seed(escrow, tokens + 2, UNIT), Error::<Test>::InsufficientInitialLiquidity);
        // A record at another tier.
        Pools::<Test>::insert(
            (native(), launch()),
            PoolInfo { fee_tier: 10, total_fees_collected: 0, routing: FeeRouting::default() },
        );
        assert_noop!(seed(escrow, tokens, UNIT), Error::<Test>::InvalidFeeTier);
    });
}

#[test]
fn seed_whose_sweep_cannot_land_fails_loudly() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 10, 0, 0));
        assert_ok!(Assets::force_create(RuntimeOrigin::root(), LAUNCH_ID, ALICE, false, 1));
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, CHARLIE, 1_000 * UNIT));
        // A non-native quote parked at the pool address, and an excess
        // recipient that cannot open an account for it.
        let pool = pool_account(usdc(), launch());
        assert_ok!(Balances::transfer_allow_death(RuntimeOrigin::signed(DAVE), pool, UNIT));
        assert_ok!(Assets::transfer(RuntimeOrigin::signed(DAVE), USDC_ID, pool, UNIT));
        while System::inc_consumers(&TREASURY).is_ok() {}
        assert_noop!(
            <VitreusDex as ReservedPoolSeeder<_, _, _, _>>::seed_reserved_pool_for(
                &CHARLIE,
                launch(),
                usdc(),
                1_000 * UNIT,
                1_000 * UNIT,
                3
            ),
            Error::<Test>::ExcessRecipientCannotReceive
        );
    });
}

fn quote_in(a: NativeOrAssetId, b: NativeOrAssetId, x: u128) -> Option<u128> {
    VitreusDex::quote_exact_tokens_for_tokens(a, b, x)
}
fn quote_out(a: NativeOrAssetId, b: NativeOrAssetId, y: u128) -> Option<u128> {
    VitreusDex::quote_tokens_for_exact_tokens(a, b, y)
}

/// A launch pool with every slice routed, and a pair with no native side.
fn quote_world() {
    launch_pool(1_000 * UNIT, 4_000_000 * UNIT);
    assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10));
    add(ALICE, usdc(), vnrg(), 1_000 * UNIT, 3_000 * UNIT);
}

#[test]
fn exact_in_quote_is_what_swap_pays() {
    let cases = [
        (native(), launch(), [UNIT / 1_000 + 7, 3 * UNIT + 12_345, 90 * UNIT]),
        (launch(), native(), [1_000 * UNIT, 40_000 * UNIT + 99, 700_000 * UNIT]),
        (usdc(), vnrg(), [UNIT, 17 * UNIT + 3, 400 * UNIT]),
    ];
    for (a, b, sizes) in cases {
        for x in sizes {
            new_runtime_like_ext().execute_with(|| {
                quote_world();
                let quoted = quote_in(a.clone(), b.clone(), x).expect("a quote");
                assert_eq!(swap(BOB, a.clone(), b.clone(), x), quoted, "{a:?} → {b:?}, {x}");
            });
        }
    }
}

#[test]
fn exact_out_quote_is_least_input_that_pays_it() {
    let cases = [
        (native(), launch(), [1u128, 777 * UNIT, 12_345 * UNIT + 1]),
        (launch(), native(), [1u128, UNIT / 3, 25 * UNIT + 5]),
        (usdc(), vnrg(), [1u128, UNIT, 300 * UNIT + 11]),
    ];
    for (a, b, wants) in cases {
        for want in wants {
            new_runtime_like_ext().execute_with(|| {
                quote_world();
                let x = quote_out(a.clone(), b.clone(), want).expect("an input");
                assert!(quote_in(a.clone(), b.clone(), x).unwrap() >= want);
                assert!(quote_in(a.clone(), b.clone(), x - 1).unwrap_or(0) < want);
                assert!(swap(BOB, a.clone(), b.clone(), x) >= want, "{a:?} → {b:?}, {want}");
            });
        }
    }
}

#[test]
fn quote_swap_could_not_honour_is_none() {
    new_runtime_like_ext().execute_with(|| {
        quote_world();
        let pool = pool_account(native(), launch());
        let reserve = balance(native(), pool);
        // More than the pool holds, or than it can pay and keep its ED.
        assert_eq!(quote_out(launch(), native(), reserve), None);
        assert_eq!(quote_out(launch(), native(), reserve - RUNTIME_ED + 1), None);
        // Nothing to quote: zero, too little to pay anything, no pool, a
        // record whose first deposit has not come.
        assert_eq!(quote_in(native(), launch(), 0), None);
        assert_eq!(quote_out(native(), launch(), 0), None);
        assert_eq!(quote_in(launch(), native(), 1), None);
        assert_eq!(quote_in(native(), vnrg(), UNIT), None);
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), vnrg(), 3));
        assert_eq!(quote_in(native(), vnrg(), UNIT), None);
        assert_eq!(quote_out(native(), vnrg(), UNIT), None);
    });
}

#[test]
fn quotes_answer_through_runtimes_view_functions() {
    use crate::{QuoteExactTokensForTokensViewFunction, QuoteTokensForExactTokensViewFunction};
    use frame_support::view_functions::ViewFunction;
    use parity_scale_codec::{Decode, Encode};
    new_runtime_like_ext().execute_with(|| {
        quote_world();
        let call = QuoteExactTokensForTokensViewFunction::<Test>::new(native(), launch(), UNIT);
        let out = Test::execute_view_function(
            QuoteExactTokensForTokensViewFunction::<Test>::id(),
            call.encode(),
        )
        .unwrap();
        assert_eq!(
            Option::<u128>::decode(&mut &out[..]).unwrap(),
            quote_in(native(), launch(), UNIT)
        );

        let call = QuoteTokensForExactTokensViewFunction::<Test>::new(launch(), native(), UNIT);
        let out = Test::execute_view_function(
            QuoteTokensForExactTokensViewFunction::<Test>::id(),
            call.encode(),
        )
        .unwrap();
        assert_eq!(
            Option::<u128>::decode(&mut &out[..]).unwrap(),
            quote_out(launch(), native(), UNIT)
        );
    });
}

#[test]
fn pool_manager_creates_and_funds_pools_for_other_pallets() {
    type Dex = VitreusDex;
    new_runtime_like_ext().execute_with(|| {
        assert!(!<Dex as PoolManager<_, _, _, _>>::pool_exists(usdc(), native()));
        assert_ok!(<Dex as PoolManager<_, _, _, _>>::create_pool(native(), usdc(), 10));
        // A record is a pool for every caller, before and after its first deposit.
        assert!(<Dex as PoolManager<_, _, _, _>>::pool_exists(usdc(), native()));
        assert_eq!(<Dex as PoolManager<_, _, _, _>>::fee_bps(usdc()), Some(100));
        assert_eq!(<Dex as PoolManager<_, _, _, _>>::native_reserves(usdc()), None);
        assert_noop!(
            <Dex as PoolManager<_, _, _, _>>::create_pool(native(), launch(), 3),
            Error::<Test>::ReservedAsset
        );
        // Amounts in the caller's order: USDC first.
        let lp = <Dex as PoolManager<_, _, _, _>>::add_liquidity_for(
            &BOB,
            usdc(),
            native(),
            900 * UNIT,
            100 * UNIT,
            900 * UNIT,
            100 * UNIT,
        )
        .unwrap();
        assert_eq!(lp, lp_of(native(), usdc(), BOB));
        assert_eq!(
            <Dex as PoolManager<_, _, _, _>>::native_reserves(usdc()),
            Some((100 * UNIT, 900 * UNIT))
        );
    });
}

#[test]
fn intent_swap_can_spend_whole_token_balance() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        add(ALICE, native(), usdc(), 500_000 * UNIT, 500_000 * UNIT);
        let all = balance(usdc(), CHARLIE);
        let quoted = quote_in(usdc(), native(), all).unwrap();
        assert_eq!(
            VitreusDex::swap_exact_for_intent(&CHARLIE, usdc(), native(), all, 1),
            Ok(quoted)
        );
        assert_eq!(balance(usdc(), CHARLIE), 0);
    });
}

#[test]
fn add_takes_pools_ratio_and_answers_in_our_errors() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        add(ALICE, native(), usdc(), 777 * UNIT + 13, 1_301 * UNIT + 7);
        let pool = pool_account(native(), usdc());
        for (x, y) in [(UNIT, 1_000 * UNIT), (1_000 * UNIT, 3 * UNIT + 1), (5_000, 9_000)] {
            let (rn, ru) = (balance(native(), pool), balance(usdc(), pool));
            // Whichever side binds, the other is the floor of the ratio.
            let (tn, tu) = if mul_div(x, ru, rn) <= y {
                (x, mul_div(x, ru, rn))
            } else {
                (mul_div(y, rn, ru), y)
            };
            // Minting no more than the locked minimum is too little to record.
            let issuance = <Test as crate::Config>::Base::lp_issuance(&native(), &usdc());
            if mul_div(tn, issuance, rn).min(mul_div(tu, issuance, ru)) <= MINIMUM_LIQUIDITY as u128
            {
                assert_noop!(
                    VitreusDex::add_liquidity(
                        RuntimeOrigin::signed(BOB),
                        native(),
                        usdc(),
                        x,
                        y,
                        0,
                        0
                    ),
                    Error::<Test>::ZeroAmount
                );
                continue;
            }
            assert_noop!(
                VitreusDex::add_liquidity(
                    RuntimeOrigin::signed(BOB),
                    native(),
                    usdc(),
                    x,
                    y,
                    tn + 1,
                    0
                ),
                Error::<Test>::SlippageExceeded
            );
            assert_noop!(
                VitreusDex::add_liquidity(
                    RuntimeOrigin::signed(BOB),
                    native(),
                    usdc(),
                    x,
                    y,
                    0,
                    tu + 1
                ),
                Error::<Test>::SlippageExceeded
            );
            assert_ok!(VitreusDex::add_liquidity(
                RuntimeOrigin::signed(BOB),
                native(),
                usdc(),
                x,
                y,
                tn,
                tu
            ));
            assert_eq!((balance(native(), pool) - rn, balance(usdc(), pool) - ru), (tn, tu));
        }
        // A ratio past what a balance can hold is ours to refuse.
        assert_noop!(
            VitreusDex::add_liquidity(
                RuntimeOrigin::signed(BOB),
                native(),
                usdc(),
                u128::MAX,
                UNIT,
                0,
                0
            ),
            Error::<Test>::Overflow
        );
    });
}

#[test]
fn whole_balance_offered_gives_all_but_minimum() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        add(ALICE, native(), usdc(), 500 * UNIT, 1_000 * UNIT);
        let all = balance(usdc(), BOB);
        // Short by exactly the minimum: it keeps that and adds the rest.
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            usdc(),
            native(),
            all,
            u128::MAX / 4,
            0,
            0
        ));
        assert_eq!(balance(usdc(), BOB), 1);
        // Short by more: it fails on its funds, as a transfer would.
        assert!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            usdc(),
            native(),
            2,
            UNIT,
            0,
            0
        )
        .is_err());
    });
}
