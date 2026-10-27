//! Tests for the Vitreus DEX pallet, from before the base pallet, ported:
//! the same scenarios, read from the base pool. A pool's reserves are its
//! account's balances (it keeps nothing of its own), positions are LP
//! tokens, and locks are `LiquidityLocks` with a freeze.

use crate::mock::*;
use crate::{AmmBase, Error, Event, LiquidityLocks, PoolManager, Pools};
use frame_support::{assert_noop, assert_ok, traits::fungibles::Inspect};
use sp_runtime::BuildStorage;

type Base = <Test as crate::Config>::Base;

/// The pool account for a pair: the base pallet's.
fn pool_acc(a: NativeOrAssetId, b: NativeOrAssetId) -> AccountId {
    VitreusDex::pool_account_for(a, b).unwrap()
}
/// A pool's reserves in its key's order: its account's balances.
fn reserves(key: &(NativeOrAssetId, NativeOrAssetId)) -> (u128, u128) {
    let account = pool_acc(key.0.clone(), key.1.clone());
    (
        NativeAndAssets::balance(key.0.clone(), &account),
        NativeAndAssets::balance(key.1.clone(), &account),
    )
}
/// `who`'s LP tokens in the pool: what its shares were.
fn shares(who: AccountId, key: &(NativeOrAssetId, NativeOrAssetId)) -> u128 {
    Base::lp_balance(&key.0, &key.1, &who)
}
/// Every LP token of the pool, the locked minimum included: what
/// `TotalLiquidity` was.
fn total(key: &(NativeOrAssetId, NativeOrAssetId)) -> u128 {
    Base::lp_issuance(&key.0, &key.1)
}
/// The block `who`'s position in the pool is locked until, if any.
fn locked_until(who: AccountId, key: &(NativeOrAssetId, NativeOrAssetId)) -> Option<u64> {
    LiquidityLocks::<Test>::get(key, who)
}

fn native() -> NativeOrAssetId {
    NativeOrAssetId::Native
}
fn usdc() -> NativeOrAssetId {
    NativeOrAssetId::WithId(USDC_ID)
}
fn vnrg() -> NativeOrAssetId {
    NativeOrAssetId::WithId(VNRG_ID)
}
fn pair() -> (NativeOrAssetId, NativeOrAssetId) {
    (usdc(), vnrg())
}

#[test]
fn test_create_pool_success() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10,));

        let pool = Pools::<Test>::get(pair()).expect("pool stored");
        assert_eq!(reserves(&pair()), (0, 0));
        assert_eq!(pool.fee_tier, 10);
        assert_eq!(pool.total_fees_collected, 0);
        assert_eq!(total(&pair()), 0);

        System::assert_has_event(
            Event::PoolCreated { asset_a: usdc(), asset_b: vnrg(), fee_tier: 10 }.into(),
        );
    });
}

#[test]
fn test_create_pool_duplicate_fails() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10,));
        assert_noop!(
            VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10),
            Error::<Test>::PoolAlreadyExists
        );
    });
}

#[test]
fn test_add_liquidity_first_deposit() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10,));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            10_000,
            40_000,
            0,
            0,
        ));

        // shares = sqrt(10_000 * 40_000) = 20_000
        // MINIMUM_LIQUIDITY = 1_000 locked permanently
        // shares_to_mint = 20_000 - 1_000 = 19_000
        assert_eq!(total(&pair()), 20_000);
        assert_eq!(shares(ALICE, &pair()), 19_000);
        assert_eq!(reserves(&pair()), (10_000, 40_000));
    });
}

#[test]
fn test_add_liquidity_subsequent_deposit() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10,));
        // First deposit: 10_000/40_000 → 19_000 user shares, 20_000 total.
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            10_000,
            40_000,
            0,
            0,
        ));
        // Second deposit: proportional half (5_000/20_000) → 10_000 shares.
        // optimal_b = 5_000 * 40_000 / 10_000 = 20_000 ✓
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            usdc(),
            vnrg(),
            5_000,
            20_000,
            0,
            0,
        ));

        assert_eq!(total(&pair()), 30_000);
        assert_eq!(shares(BOB, &pair()), 10_000);
        assert_eq!(reserves(&pair()), (15_000, 60_000));
    });
}

#[test]
fn test_remove_liquidity_full() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10,));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            10_000,
            40_000,
            0,
            0,
        ));

        // Alice has 19_000 shares out of 20_000 total.
        // amount_a = 19_000 * 10_000 / 20_000 = 9_500
        // amount_b = 19_000 * 40_000 / 20_000 = 38_000
        assert_ok!(VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            19_000,
            0,
            0,
        ));

        // MINIMUM_LIQUIDITY (1_000 LP tokens) remains — reserves can't be fully drained.
        assert_eq!(reserves(&pair()), (500, 2_000));
        assert_eq!(total(&pair()), 1_000);
        assert_eq!(shares(ALICE, &pair()), 0);
    });
}

#[test]
fn test_swap_exact_tokens() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10,));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            10_000,
            10_000,
            0,
            0,
        ));

        // amount_in = 100, fee_tier = 10 (1.0%)
        // fee = 100 * 10 / 1000 = 1
        // amount_in_after_fee = 99
        // amount_out = 10_000 * 99 / (10_000 + 99) = 990_000 / 10_099 = 98
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            usdc(),
            vnrg(),
            100,
            0,
            BOB,
        ));

        // The pool's fee stays in it, as reserve from the start (the base
        // pool reads reserves from its balances; there is nothing to sync).
        assert_eq!(reserves(&pair()), (10_100, 9_902));
        assert_eq!(Pools::<Test>::get(pair()).unwrap().total_fees_collected, 1);

        System::assert_has_event(
            Event::SwapExecuted {
                who: BOB,
                asset_in: usdc(),
                asset_out: vnrg(),
                amount_in: 100,
                amount_out: 98,
                fee: 1,
            }
            .into(),
        );
    });
}

#[test]
fn d11_swap_pays_out_to_the_signer_only() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10,));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            10_000,
            10_000,
            0,
            0,
        ));
        // Paying out to anyone but the signer fails, and changes nothing.
        assert_noop!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                usdc(),
                vnrg(),
                100,
                0,
                CHARLIE,
            ),
            Error::<Test>::RecipientNotSigner
        );
        // The same swap to the signer goes through.
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            usdc(),
            vnrg(),
            100,
            0,
            BOB,
        ));
    });
}

#[test]
fn test_swap_insufficient_liquidity() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10,));
        // Pool exists but has no liquidity.
        assert_noop!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                usdc(),
                vnrg(),
                100,
                0,
                BOB,
            ),
            Error::<Test>::InsufficientLiquidity
        );
    });
}

#[test]
fn test_swap_slippage_protection() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10,));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            10_000,
            10_000,
            0,
            0,
        ));

        // Actual output is 98; demand 500 to trigger slippage error.
        assert_noop!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                usdc(),
                vnrg(),
                100,
                500,
                BOB,
            ),
            Error::<Test>::SlippageExceeded
        );
    });
}

// ---------------------------------------------------------------------------
// Internal helpers (do_* fns and the PoolManager trait), called directly —
// not through extrinsics. These are the entry points a graduating launchpad
// pallet will use.
// ---------------------------------------------------------------------------

#[test]
fn do_create_pool_direct_creates_pool_without_origin() {
    new_test_ext().execute_with(|| {
        assert!(!VitreusDex::pool_exists(usdc(), vnrg()));

        assert_ok!(VitreusDex::do_create_pool(usdc(), vnrg(), 3));

        assert!(VitreusDex::pool_exists(usdc(), vnrg()));
        // Pair order is irrelevant to the lookup.
        assert!(VitreusDex::pool_exists(vnrg(), usdc()));

        let pool = Pools::<Test>::get(pair()).expect("pool stored");
        assert_eq!(reserves(&pair()), (0, 0));
        assert_eq!(pool.fee_tier, 3);
        assert_eq!(total(&pair()), 0);
        System::assert_has_event(
            Event::PoolCreated { asset_a: usdc(), asset_b: vnrg(), fee_tier: 3 }.into(),
        );

        // The extrinsic gate is untouched: a signed (non-Manage) origin is
        // still rejected even though the helper itself is origin-free.
        assert_noop!(
            VitreusDex::create_pool(RuntimeOrigin::signed(ALICE), usdc(), native(), 3),
            sp_runtime::DispatchError::BadOrigin
        );
        assert!(!VitreusDex::pool_exists(usdc(), native()));
    });
}

#[test]
fn do_create_pool_direct_enforces_fee_tier_and_uniqueness() {
    new_test_ext().execute_with(|| {
        assert_noop!(VitreusDex::do_create_pool(usdc(), vnrg(), 5), Error::<Test>::InvalidFeeTier);
        assert_ok!(VitreusDex::do_create_pool(usdc(), vnrg(), 1));
        // Reversed order must still collide with the canonical pair.
        assert_noop!(
            VitreusDex::do_create_pool(vnrg(), usdc(), 1),
            Error::<Test>::PoolAlreadyExists
        );
    });
}

#[test]
fn do_add_liquidity_for_direct_credits_who_and_returns_shares() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::do_create_pool(usdc(), vnrg(), 10));

        let bob_usdc_before = Assets::balance(USDC_ID, BOB);
        let bob_vnrg_before = Assets::balance(VNRG_ID, BOB);

        // First deposit: sqrt(10_000 * 40_000) = 20_000 total, 1_000 locked.
        let minted = VitreusDex::do_add_liquidity_for(&BOB, usdc(), vnrg(), 10_000, 40_000, 0, 0)
            .expect("first deposit");
        assert_eq!(minted, 19_000);

        // Tokens came out of `who`, not the caller/anyone else.
        assert_eq!(Assets::balance(USDC_ID, BOB), bob_usdc_before - 10_000);
        assert_eq!(Assets::balance(VNRG_ID, BOB), bob_vnrg_before - 40_000);
        assert_eq!(Assets::balance(USDC_ID, ALICE), 1_000_000);

        assert_eq!(shares(BOB, &pair()), 19_000);
        assert_eq!(locked_until(BOB, &pair()), None);
        assert_eq!(total(&pair()), 20_000);
        assert_eq!(reserves(&pair()), (10_000, 40_000));

        System::assert_has_event(
            Event::LiquidityAdded {
                provider: BOB,
                asset_a: usdc(),
                asset_b: vnrg(),
                amount_a: 10_000,
                amount_b: 40_000,
                shares_minted: 19_000,
            }
            .into(),
        );

        // Second deposit by a different account: proportional shares, and
        // the return value matches the storage delta.
        let minted2 =
            VitreusDex::do_add_liquidity_for(&CHARLIE, usdc(), vnrg(), 5_000, 20_000, 0, 0)
                .expect("second deposit");
        assert_eq!(minted2, 10_000);
        assert_eq!(shares(CHARLIE, &pair()), 10_000);
        assert_eq!(total(&pair()), 30_000);
    });
}

#[test]
fn do_add_liquidity_for_direct_rejects_missing_pool_and_slippage() {
    new_test_ext().execute_with(|| {
        assert_noop!(
            VitreusDex::do_add_liquidity_for(&BOB, usdc(), vnrg(), 10_000, 40_000, 0, 0),
            Error::<Test>::PoolNotFound
        );

        assert_ok!(VitreusDex::do_create_pool(usdc(), vnrg(), 10));
        assert_noop!(
            VitreusDex::do_add_liquidity_for(&BOB, usdc(), vnrg(), 0, 40_000, 0, 0),
            Error::<Test>::ZeroAmount
        );
        assert_ok!(VitreusDex::do_add_liquidity_for(&BOB, usdc(), vnrg(), 10_000, 40_000, 0, 0));

        // Imbalanced top-up: optimal_b = 5_000 * 40_000 / 10_000 = 20_000,
        // which is below the caller's 25_000 floor → slippage error, and
        // (via assert_noop) no state change.
        assert_noop!(
            VitreusDex::do_add_liquidity_for(&CHARLIE, usdc(), vnrg(), 5_000, 30_000, 0, 25_000),
            Error::<Test>::SlippageExceeded
        );
    });
}

#[test]
fn do_lock_liquidity_for_direct_locks_position() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::do_create_pool(usdc(), vnrg(), 10));
        assert_ok!(VitreusDex::do_add_liquidity_for(&BOB, usdc(), vnrg(), 10_000, 40_000, 0, 0));

        // No position → InsufficientShares.
        assert_noop!(
            VitreusDex::do_lock_liquidity_for(&CHARLIE, usdc(), vnrg(), 50),
            Error::<Test>::InsufficientShares
        );

        assert_ok!(VitreusDex::do_lock_liquidity_for(&BOB, usdc(), vnrg(), 50));
        assert_eq!(locked_until(BOB, &pair()), Some(50));
        System::assert_has_event(
            Event::LiquidityLocked { who: BOB, pool: pair(), locked_until: 50 }.into(),
        );

        // The lock is honoured by remove_liquidity until the block is reached.
        assert_noop!(
            VitreusDex::remove_liquidity(RuntimeOrigin::signed(BOB), usdc(), vnrg(), 1_000, 0, 0),
            Error::<Test>::PoolLocked
        );
        System::set_block_number(50);
        assert_ok!(VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(BOB),
            usdc(),
            vnrg(),
            1_000,
            0,
            0
        ));
    });
}

#[test]
fn pool_manager_trait_supports_full_graduation_flow() {
    // Everything a graduating launchpad needs, exercised through the trait
    // surface only (as a dependent pallet would see it).
    type Dex = VitreusDex;
    fn dex_pool_exists(a: NativeOrAssetId, b: NativeOrAssetId) -> bool {
        <Dex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::pool_exists(a, b)
    }

    new_test_ext().execute_with(|| {
        assert!(!dex_pool_exists(native(), usdc()));

        assert_ok!(<Dex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::create_pool(
            native(),
            usdc(),
            3
        ));
        assert!(dex_pool_exists(native(), usdc()));

        // Native/USDC: 100_000 native (balances) + 40_000 USDC (assets) from CHARLIE.
        let native_before = Balances::free_balance(CHARLIE);
        let minted =
            <Dex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::add_liquidity_for(
                &CHARLIE,
                native(),
                usdc(),
                100_000,
                40_000,
                100_000,
                40_000,
            )
            .expect("seed liquidity");
        // sqrt(100_000 * 40_000) = 63_245; minus MINIMUM_LIQUIDITY.
        assert_eq!(minted, 63_245 - 1_000);
        // Exactly the deposit: the base pool keeps nothing of its own.
        assert_eq!(Balances::free_balance(CHARLIE), native_before - 100_000);

        assert_ok!(
            <Dex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::lock_liquidity_for(
                &CHARLIE,
                native(),
                usdc(),
                1_000
            )
        );

        let canonical = VitreusDex::canonical_pair(native(), usdc());
        assert_eq!(shares(CHARLIE, &canonical), minted);
        assert_eq!(locked_until(CHARLIE, &canonical), Some(1_000));
        assert_eq!(total(&canonical), 63_245);
    });
}

// ============================================================================
// D1: 18-decimal scale. Reserve products at launchpad seed size
// (10^22 VTRS × 2·10^26 tokens ≈ 2·10^48) exceed u128::MAX ≈ 3.4·10^38, so
// every multiply-then-divide in the pool math must go through
// `HigherPrecisionBalance` (U256). These tests fail with `Error::Overflow`
// on the pre-D1 u128 arithmetic.
// ============================================================================

/// Asset id for a launchpad-style 18-decimal token in the mock registry.
const MEME_ID: u32 = 7;
/// 10^18 sub-units per whole unit, VTRS and launch tokens alike.
const UNIT: u128 = 1_000_000_000_000_000_000;
/// Seed amounts: 10_000 VTRS against 200_000_000 tokens.
const SEED_NATIVE: u128 = 10_000 * UNIT; // 10^22
const SEED_TOKEN: u128 = 200_000_000 * UNIT; // 2·10^26

fn meme() -> NativeOrAssetId {
    NativeOrAssetId::WithId(MEME_ID)
}

/// Funds ALICE (seeder) and BOB (trader) at 18-decimal scale and creates the
/// Native/MEME pool at the 0.3% tier. Returns the canonical pair key.
fn setup_scale_pool() -> (NativeOrAssetId, NativeOrAssetId) {
    // 10^27 native for the seeder and trader — well above u64 but far below u128::MAX.
    assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), ALICE, 1_000_000_000 * UNIT));
    assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), BOB, 1_000_000_000 * UNIT));
    assert_ok!(Assets::force_create(RuntimeOrigin::root(), MEME_ID, ALICE, false, 1));
    assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), MEME_ID, ALICE, 1_000_000_000 * UNIT));
    assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), MEME_ID, BOB, 1_000_000_000 * UNIT));
    assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), meme(), 3));
    VitreusDex::canonical_pair(native(), meme())
}

/// Reference: floor(sqrt(a * b)) computed in U256, narrowed.
fn isqrt_u256(a: u128, b: u128) -> u128 {
    let p = sp_core::U256::from(a) * sp_core::U256::from(b);
    let r: sp_core::U256 = p.integer_sqrt();
    r.try_into().expect("sqrt of a u256 product of two u128 fits u128")
}

/// Reference: floor(a * b / c) in U256, narrowed.
fn mul_div_u256(a: u128, b: u128, c: u128) -> u128 {
    let x = sp_core::U256::from(a) * sp_core::U256::from(b) / sp_core::U256::from(c);
    x.try_into().expect("reference result fits u128")
}

#[test]
fn scale_add_liquidity_first_deposit_at_seed_amounts() {
    new_test_ext().execute_with(|| {
        let key = setup_scale_pool();

        // Pre-D1: `amount_a.checked_mul(amount_b)` overflows u128 → Error::Overflow.
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            meme(),
            SEED_NATIVE,
            SEED_TOKEN,
            SEED_NATIVE,
            SEED_TOKEN,
        ));

        let expected_total = isqrt_u256(SEED_NATIVE, SEED_TOKEN);
        assert_eq!(total(&key), expected_total);
        assert_eq!(shares(ALICE, &key), expected_total - u128::from(crate::MINIMUM_LIQUIDITY));
        // canonical_pair orders Native before WithId, so the first is VTRS.
        assert_eq!(reserves(&key), (SEED_NATIVE, SEED_TOKEN));
    });
}

#[test]
fn scale_add_liquidity_subsequent_deposit_uses_optimal_amounts() {
    new_test_ext().execute_with(|| {
        let key = setup_scale_pool();
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            meme(),
            SEED_NATIVE,
            SEED_TOKEN,
            0,
            0,
        ));
        let total_before = total(&key);

        // BOB offers 1_000 VTRS and far too many tokens; optimal_b = amount_a * reserve_b / reserve_a.
        // Pre-D1: `amount_a.checked_mul(&pool.reserve_b)` overflows.
        let offer_native = 1_000 * UNIT;
        let offer_token = 100_000_000 * UNIT;
        let optimal_token = mul_div_u256(offer_native, SEED_TOKEN, SEED_NATIVE);
        assert!(optimal_token < offer_token);

        let token_before = Assets::balance(MEME_ID, BOB);
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            native(),
            meme(),
            offer_native,
            offer_token,
            offer_native,
            optimal_token,
        ));
        // Only the optimal token amount was pulled (no donation of excess).
        assert_eq!(token_before - Assets::balance(MEME_ID, BOB), optimal_token);

        // shares = min(actual_a * total / reserve_a, actual_b * total / reserve_b), floored.
        let share_a = mul_div_u256(offer_native, total_before, SEED_NATIVE);
        let share_b = mul_div_u256(optimal_token, total_before, SEED_TOKEN);
        let expected = share_a.min(share_b);
        assert_eq!(shares(BOB, &key), expected);
    });
}

#[test]
fn scale_swap_native_for_token_at_seed_amounts() {
    new_test_ext().execute_with(|| {
        let key = setup_scale_pool();
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            meme(),
            SEED_NATIVE,
            SEED_TOKEN,
            0,
            0,
        ));

        // 100 VTRS in at 0.3%: fee = floor(amount_in * 3 / 1000).
        let amount_in = 100 * UNIT;
        let fee = amount_in * 3 / 1_000;
        let after_fee = amount_in - fee;
        // amount_out = floor(reserve_out * after_fee / (reserve_in + after_fee)).
        // Pre-D1: `reserve_out.checked_mul(&amount_in_after_fee)` overflows.
        let expected_out = mul_div_u256(SEED_TOKEN, after_fee, SEED_NATIVE + after_fee);
        assert!(expected_out > 0);

        let token_before = Assets::balance(MEME_ID, BOB);
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            meme(),
            amount_in,
            expected_out,
            BOB,
        ));
        assert_eq!(Assets::balance(MEME_ID, BOB) - token_before, expected_out);

        // The whole input is in the pool, its fee included.
        let (ra, rb) = reserves(&key);
        assert_eq!((ra, rb), (SEED_NATIVE + amount_in, SEED_TOKEN - expected_out));
        assert_eq!(Pools::<Test>::get(key).unwrap().total_fees_collected, fee);

        // Constant product must not decrease (floor on amount_out favours the pool).
        let k_before = sp_core::U256::from(SEED_NATIVE) * sp_core::U256::from(SEED_TOKEN);
        let k_after = sp_core::U256::from(ra) * sp_core::U256::from(rb);
        assert!(k_after >= k_before);
    });
}

#[test]
fn scale_swap_token_for_native_at_seed_amounts() {
    new_test_ext().execute_with(|| {
        let key = setup_scale_pool();
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            meme(),
            SEED_NATIVE,
            SEED_TOKEN,
            0,
            0,
        ));

        // 1_000_000 tokens in (0.5% of the token reserve).
        let amount_in = 1_000_000 * UNIT;
        let fee = amount_in * 3 / 1_000;
        let after_fee = amount_in - fee;
        // Flipped direction: reserve_in is the token side, reserve_out the native side.
        let expected_out = mul_div_u256(SEED_NATIVE, after_fee, SEED_TOKEN + after_fee);
        assert!(expected_out > 0);

        let native_before = Balances::free_balance(BOB);
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            meme(),
            native(),
            amount_in,
            expected_out,
            BOB,
        ));
        assert_eq!(Balances::free_balance(BOB) - native_before, expected_out);

        let (ra, rb) = reserves(&key);
        assert_eq!((ra, rb), (SEED_NATIVE - expected_out, SEED_TOKEN + amount_in));

        let k_before = sp_core::U256::from(SEED_NATIVE) * sp_core::U256::from(SEED_TOKEN);
        let k_after = sp_core::U256::from(ra) * sp_core::U256::from(rb);
        assert!(k_after >= k_before);
    });
}

#[test]
fn scale_remove_liquidity_at_seed_amounts() {
    new_test_ext().execute_with(|| {
        let key = setup_scale_pool();
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            meme(),
            SEED_NATIVE,
            SEED_TOKEN,
            0,
            0,
        ));
        let total_before = total(&key);
        // Withdraw half of ALICE's LP tokens.
        let half = shares(ALICE, &key) / 2;

        // amount = floor(shares * reserve / total_shares).
        // Pre-D1: `shares.checked_mul(&pool.reserve_a)` overflows.
        let expected_native = mul_div_u256(half, SEED_NATIVE, total_before);
        let expected_token = mul_div_u256(half, SEED_TOKEN, total_before);

        let native_before = Balances::free_balance(ALICE);
        let token_before = Assets::balance(MEME_ID, ALICE);
        assert_ok!(VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            meme(),
            half,
            expected_native,
            expected_token,
        ));
        assert_eq!(Balances::free_balance(ALICE) - native_before, expected_native);
        assert_eq!(Assets::balance(MEME_ID, ALICE) - token_before, expected_token);

        assert_eq!(reserves(&key), (SEED_NATIVE - expected_native, SEED_TOKEN - expected_token));
        assert_eq!(total(&key), total_before - half);
    });
}

#[test]
fn scale_narrowing_errors_instead_of_truncating() {
    new_test_ext().execute_with(|| {
        // The only pool-math result that can exceed u128 after the product is
        // computed in U256 is the matched deposit amount
        // `optimal_b = amount_a * reserve_b / reserve_a`, when a depositor offers
        // far more of asset A than the pool ratio supports. With reserves at
        // 10^22 : 2·10^26 (ratio 2·10^4) and amount_a = 10^38, optimal_b ≈ 2·10^42
        // does not fit u128. The narrowing must surface as `Error::Overflow`
        // and leave state untouched — never truncate to a wrong amount.
        let key = setup_scale_pool();
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            meme(),
            SEED_NATIVE,
            SEED_TOKEN,
            0,
            0,
        ));
        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), BOB, u128::MAX));
        let huge_native: u128 = 100_000_000_000_000_000_000_000_000_000_000_000_000; // 10^38
        let total_before = total(&key);

        assert_noop!(
            VitreusDex::add_liquidity(
                RuntimeOrigin::signed(BOB),
                native(),
                meme(),
                huge_native,
                u128::MAX,
                0,
                0,
            ),
            Error::<Test>::Overflow
        );
        assert_eq!(total(&key), total_before);
        assert_eq!(shares(BOB, &key), 0);
    });
}

// ============================================================================
// D2: reserved-asset guard + ReservedPoolSeeder. D3: monotone locks.
//
// Reserved ids in the mock are `WithId(id)` with id >= RESERVED_ASSET_BASE.
// LAUNCH_ID is an 18-decimal, non-sufficient asset so the pool sub-account
// needs a native provider before it can hold it — which is what makes the
// quote-first transfer order observable.
// ============================================================================

// LAUNCH_ID lives in mock.rs (D4: the mock creator lookup keys on it).

fn launch() -> NativeOrAssetId {
    NativeOrAssetId::WithId(LAUNCH_ID)
}

/// The escrow-like account that funds a seed; deliberately not ALICE/BOB.
const ESCROW: AccountId = acc(42);

/// Mints the reserved asset and funds ESCROW at seed scale. Does NOT create a pool.
fn setup_reserved_asset() {
    assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), ESCROW, 1_000_000 * UNIT));
    assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), BOB, 1_000_000 * UNIT));
    assert_ok!(Assets::force_create(RuntimeOrigin::root(), LAUNCH_ID, ALICE, false, 1));
    assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, ESCROW, 1_000_000_000 * UNIT));
    assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, BOB, 1_000_000_000 * UNIT));
}

fn seed(who: AccountId) -> Result<u128, sp_runtime::DispatchError> {
    // D10: a seed needs a default for its tier. Tests that are not about
    // routing get the zero split, which is what they assumed before the
    // default was per tier; the ones that are about routing set their own
    // first, and the ones about the refusal call the trait directly.
    if DefaultFeeRouting::<Test>::get(3).is_none() {
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 0, 0, 0));
    }
    <VitreusDex as crate::ReservedPoolSeeder<AccountId, NativeOrAssetId, u128, u64>>::seed_reserved_pool_for(
        &who,
        launch(),
        native(),
        SEED_TOKEN,
        SEED_NATIVE,
        3,
    )
    .map(|seeded| seeded.shares)
}

#[test]
fn canonical_pair_orders_native_before_with_id() {
    // `seed_reserved_pool_for` does not depend on this, but `do_add_liquidity_for`
    // does (it transfers pair.0 first). Pin it so an encoding change is noticed.
    let (a, b) = VitreusDex::canonical_pair(launch(), native());
    assert_eq!(a, native());
    assert_eq!(b, launch());
    let (a, b) = VitreusDex::canonical_pair(native(), launch());
    assert_eq!(a, native());
    assert_eq!(b, launch());
}

#[test]
fn reserved_asset_rejected_by_create_pool_for_every_caller() {
    new_test_ext().execute_with(|| {
        setup_reserved_asset();

        // ManageOrigin (root) via the extrinsic, both pair orderings.
        assert_noop!(
            VitreusDex::create_pool(RuntimeOrigin::root(), native(), launch(), 3),
            Error::<Test>::ReservedAsset
        );
        assert_noop!(
            VitreusDex::create_pool(RuntimeOrigin::root(), launch(), native(), 3),
            Error::<Test>::ReservedAsset
        );
        // Reserved against a non-native, non-reserved asset too.
        assert_noop!(
            VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), launch(), 3),
            Error::<Test>::ReservedAsset
        );
        // Signed origin is rejected before it reaches the guard (BadOrigin), so
        // check the origin-free paths explicitly: the PoolManager trait ...
        assert_noop!(
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::create_pool(
                native(),
                launch(),
                3
            ),
            Error::<Test>::ReservedAsset
        );
        // ... and the bare helper.
        assert_noop!(
            VitreusDex::do_create_pool(launch(), native(), 3),
            Error::<Test>::ReservedAsset
        );
        // The guard runs before the fee-tier check, so a bad tier does not mask it.
        assert_noop!(
            VitreusDex::do_create_pool(launch(), native(), 7),
            Error::<Test>::ReservedAsset
        );

        assert!(!VitreusDex::pool_exists(native(), launch()));
        assert!(Pools::<Test>::get(VitreusDex::canonical_pair(native(), launch())).is_none());

        // Non-reserved pairs are unaffected.
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 3));
    });
}

#[test]
fn seed_reserved_pool_creates_pool_deposits_stored_amounts_and_locks_forever() {
    new_test_ext().execute_with(|| {
        setup_reserved_asset();
        let key = VitreusDex::canonical_pair(native(), launch());
        let pool_account = pool_acc(native(), launch());
        // Fresh pool account: no native balance and no provider, which a
        // credit of this non-sufficient asset would need; the base pool opens
        // its token account on a deposit from the seeder.
        assert_eq!(Balances::free_balance(pool_account), 0);

        let native_before = Balances::free_balance(ESCROW);
        let token_before = Assets::balance(LAUNCH_ID, ESCROW);

        let shares = seed(ESCROW).expect("seed");

        let expected_total = isqrt_u256(SEED_NATIVE, SEED_TOKEN);
        assert_eq!(shares, expected_total - u128::from(crate::MINIMUM_LIQUIDITY));
        assert_eq!(total(&key), expected_total);

        // Exactly the stored amounts left the escrow and sit in the pool
        // account; the base pool keeps nothing of its own.
        assert_eq!(native_before - Balances::free_balance(ESCROW), SEED_NATIVE);
        assert_eq!(token_before - Assets::balance(LAUNCH_ID, ESCROW), SEED_TOKEN);
        assert_eq!(Balances::free_balance(pool_account), SEED_NATIVE);
        assert_eq!(Assets::balance(LAUNCH_ID, pool_account), SEED_TOKEN);

        let pool = Pools::<Test>::get(key.clone()).unwrap();
        assert_eq!(pool.fee_tier, 3);
        assert_eq!(reserves(&key), (SEED_NATIVE, SEED_TOKEN));

        // Position: owned by the seeder, locked to the max block.
        assert_eq!(super::tests::shares(ESCROW, &key), shares);
        assert_eq!(locked_until(ESCROW, &key), Some(u64::MAX));

        // Nobody else holds a position in this pool.
        assert_eq!(super::tests::shares(ALICE, &key), 0);
        assert_eq!(super::tests::shares(BOB, &key), 0);

        // The lock holds at the last representable block.
        System::set_block_number(u64::MAX - 1);
        assert_noop!(
            VitreusDex::remove_liquidity(
                RuntimeOrigin::signed(ESCROW),
                native(),
                launch(),
                1,
                0,
                0
            ),
            Error::<Test>::PoolLocked
        );
        // ... and cannot be shortened through either lock path (D3).
        assert_noop!(
            VitreusDex::lock_liquidity(RuntimeOrigin::signed(ESCROW), native(), launch(), 10),
            Error::<Test>::LockCannotBeShortened
        );
        assert_noop!(
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::lock_liquidity_for(
                &ESCROW,
                native(),
                launch(),
                u64::MAX - 1
            ),
            Error::<Test>::LockCannotBeShortened
        );
        assert_eq!(locked_until(ESCROW, &key), Some(u64::MAX));

        System::assert_has_event(
            Event::ReservedPoolSeeded {
                who: ESCROW,
                asset: launch(),
                quote: native(),
                amount_asset: SEED_TOKEN,
                amount_quote: SEED_NATIVE,
                shares,
            }
            .into(),
        );
        System::assert_has_event(
            Event::LiquidityLocked { who: ESCROW, pool: key, locked_until: u64::MAX }.into(),
        );

        // Post-seed, ordinary LPs may join through the normal path.
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            native(),
            launch(),
            UNIT,
            u128::MAX / 4,
            0,
            0,
        ));
    });
}

#[test]
fn seed_sweeps_pre_seed_donations_so_opening_price_is_stored_ratio() {
    new_test_ext().execute_with(|| {
        setup_reserved_asset();
        let key = VitreusDex::canonical_pair(native(), launch());
        let pool_account = pool_acc(native(), launch());

        // The treasury exists (has a provider) so it can receive the token.
        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), TREASURY, UNIT));

        // FM-02: park balances at the predictable pool address before seeding.
        // Token-heavy donation would push the opening price DOWN if absorbed;
        // native would push it UP. Use both.
        let donated_token = 3 * SEED_TOKEN; // 6·10^26, would triple the token reserve if absorbed
        let donated_native = 5 * UNIT;
        assert_ok!(Balances::transfer_allow_death(
            RuntimeOrigin::signed(BOB),
            pool_account,
            donated_native
        ));
        assert_ok!(Assets::transfer(
            RuntimeOrigin::signed(BOB),
            LAUNCH_ID,
            pool_account,
            donated_token
        ));
        assert_eq!(Balances::free_balance(pool_account), donated_native);
        assert_eq!(Assets::balance(LAUNCH_ID, pool_account), donated_token);
        assert_eq!(Balances::free_balance(TREASURY), UNIT);
        assert_eq!(Assets::balance(LAUNCH_ID, TREASURY), 0);

        let supply_before = Assets::total_supply(LAUNCH_ID);
        let shares = seed(ESCROW).expect("seed");

        // Neither donation is in the pool. The native landed in Treasury, all
        // of it (the base pool keeps nothing of its own); the reserved asset
        // was burned rather than sent anywhere.
        assert_eq!(Balances::free_balance(TREASURY), UNIT + donated_native);
        assert_eq!(Assets::balance(LAUNCH_ID, TREASURY), 0);
        assert_eq!(Assets::total_supply(LAUNCH_ID), supply_before - donated_token);
        assert_eq!(Balances::free_balance(pool_account), SEED_NATIVE);
        assert_eq!(Assets::balance(LAUNCH_ID, pool_account), SEED_TOKEN);
        System::assert_has_event(
            Event::PreSeedBalanceBurned {
                pool: key.clone(),
                asset: launch(),
                amount: donated_token,
            }
            .into(),
        );
        System::assert_has_event(
            Event::PreSeedBalanceSwept {
                pool: key.clone(),
                asset: native(),
                amount: donated_native,
                to: TREASURY,
            }
            .into(),
        );

        // Reserves and LP tokens reflect only the stored amounts.
        assert_eq!(reserves(&key), (SEED_NATIVE, SEED_TOKEN));
        assert_eq!(
            shares,
            isqrt_u256(SEED_NATIVE, SEED_TOKEN) - u128::from(crate::MINIMUM_LIQUIDITY)
        );

        // The decisive check: the first swap prices from the pool account's
        // live balances. Had the donation been left in place it would be
        // priced in here, and the fill would differ from the constant-product
        // quote on the stored amounts.
        let amount_in = 100 * UNIT;
        let after_fee = amount_in - amount_in * 3 / 1_000;
        let expected_out = mul_div_u256(SEED_TOKEN, after_fee, SEED_NATIVE + after_fee);
        let bob_before = Assets::balance(LAUNCH_ID, BOB);
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            launch(),
            amount_in,
            expected_out,
            BOB,
        ));
        assert_eq!(Assets::balance(LAUNCH_ID, BOB) - bob_before, expected_out);
        assert_eq!(reserves(&key), (SEED_NATIVE + amount_in, SEED_TOKEN - expected_out));
    });
}

#[test]
fn seed_is_transactional_when_sweep_or_deposit_fails() {
    new_test_ext().execute_with(|| {
        setup_reserved_asset();
        let pool_account = pool_acc(native(), launch());
        assert_ok!(Balances::transfer_allow_death(
            RuntimeOrigin::signed(BOB),
            pool_account,
            5 * UNIT
        ));

        // An escrow that cannot fund the deposit: the sweep must not stick.
        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), ESCROW, 1));
        assert!(seed(ESCROW).is_err());
        assert_eq!(Balances::free_balance(pool_account), 5 * UNIT);
        assert_eq!(Balances::free_balance(TREASURY), 0);
        assert!(!VitreusDex::pool_exists(native(), launch()));
        assert!(Pools::<Test>::get(VitreusDex::canonical_pair(native(), launch())).is_none());
    });
}

#[test]
fn seed_rejects_wrong_assets_double_seed_and_mismatched_adoption() {
    new_test_ext().execute_with(|| {
        setup_reserved_asset();
        let s = |asset, quote, fee| {
            <VitreusDex as crate::ReservedPoolSeeder<AccountId, NativeOrAssetId, u128, u64>>::seed_reserved_pool_for(
                &ESCROW, asset, quote, SEED_TOKEN, SEED_NATIVE, fee,
            )
        };

        // Non-reserved asset: the seeder must not be a way around ManageOrigin.
        assert_noop!(s(usdc(), native(), 3), Error::<Test>::NotReservedAsset);
        // Reserved quote.
        assert_noop!(s(launch(), NativeOrAssetId::WithId(RESERVED_ASSET_BASE + 2), 3), Error::<Test>::NotReservedAsset);
        // Same asset twice.
        assert_noop!(s(launch(), launch(), 3), Error::<Test>::NotReservedAsset);
        // Bad fee tier.
        assert_noop!(s(launch(), native(), 7), Error::<Test>::InvalidFeeTier);
        // Zero amounts.
        assert_noop!(
            <VitreusDex as crate::ReservedPoolSeeder<AccountId, NativeOrAssetId, u128, u64>>::seed_reserved_pool_for(
                &ESCROW, launch(), native(), 0, SEED_NATIVE, 3,
            ),
            Error::<Test>::ZeroAmount
        );

        // Adoption of an empty pool record (can only arise from storage that
        // predates the guard; simulate it directly). Fee tier must match.
        let key = VitreusDex::canonical_pair(native(), launch());
        Pools::<Test>::insert(
            key.clone(),
            crate::PoolInfo {
                fee_tier: 10,
                total_fees_collected: 0,
                routing: crate::FeeRouting::default(),
            },
        );
        assert_noop!(s(launch(), native(), 3), Error::<Test>::InvalidFeeTier);
        assert_ok!(s(launch(), native(), 10));
        assert_eq!(reserves(&key).1, SEED_TOKEN);

        // Second seed: the pool has shares now.
        assert_noop!(s(launch(), native(), 10), Error::<Test>::PoolAlreadySeeded);
    });
}

#[test]
fn lock_can_be_extended_but_never_shortened() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::do_create_pool(usdc(), vnrg(), 10));
        assert_ok!(VitreusDex::do_add_liquidity_for(&BOB, usdc(), vnrg(), 10_000, 40_000, 0, 0));

        // Unlocked → 50.
        assert_ok!(VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), usdc(), vnrg(), 50));
        assert_eq!(locked_until(BOB, &pair()), Some(50));
        // Same block: idempotent.
        assert_ok!(VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), usdc(), vnrg(), 50));
        // Extend.
        assert_ok!(VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), usdc(), vnrg(), 80));
        assert_eq!(locked_until(BOB, &pair()), Some(80));
        // Shorten via extrinsic and via trait: rejected, state unchanged.
        assert_noop!(
            VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), usdc(), vnrg(), 79),
            Error::<Test>::LockCannotBeShortened
        );
        assert_noop!(
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::lock_liquidity_for(
                &BOB,
                usdc(),
                vnrg(),
                0
            ),
            Error::<Test>::LockCannotBeShortened
        );
        assert_eq!(locked_until(BOB, &pair()), Some(80));

        // Once the lock has expired it can still only move forward, not back
        // to a value below the old one.
        System::set_block_number(100);
        assert_noop!(
            VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), usdc(), vnrg(), 60),
            Error::<Test>::LockCannotBeShortened
        );
        assert_ok!(VitreusDex::lock_liquidity(RuntimeOrigin::signed(BOB), usdc(), vnrg(), 200));
    });
}

#[test]
fn seed_burns_reserved_asset_donation_whatever_recipient_can_hold() {
    // The reserved asset is burned whatever the recipient can hold: a sweep
    // of it needs a free consumer reference there, which anyone can use up.
    // The quote is swept. (A quote sweep that cannot land, which fails the
    // seed loudly, needs a non-native quote now that the pool keeps nothing
    // of its own: core_tests::seed_whose_sweep_cannot_land_fails_loudly.)
    new_test_ext().execute_with(|| {
        setup_reserved_asset();
        let key = VitreusDex::canonical_pair(native(), launch());
        let pool_account = pool_acc(native(), launch());
        ExistentialDeposit::set(1_000);
        let ed = ExistentialDeposit::get();
        // TREASURY, the excess recipient, has no native balance, so it cannot
        // open an account for the non-sufficient launch asset.
        assert_eq!(Balances::free_balance(TREASURY), 0);
        // A donor has to give the pool account a provider before it can hold
        // the non-sufficient token, so native goes in first.
        let donated_native = ed + 1;
        let donated_token = 3 * SEED_TOKEN;
        assert_ok!(Balances::transfer_allow_death(
            RuntimeOrigin::signed(BOB),
            pool_account,
            donated_native
        ));
        assert_ok!(Assets::transfer(
            RuntimeOrigin::signed(BOB),
            LAUNCH_ID,
            pool_account,
            donated_token
        ));
        let issuance_before = Assets::total_supply(LAUNCH_ID);

        assert_ok!(seed(ESCROW));

        // The token donation was burned, not delivered; the native one swept.
        assert_eq!(Assets::balance(LAUNCH_ID, TREASURY), 0);
        assert_eq!(Assets::total_supply(LAUNCH_ID), issuance_before - donated_token);
        assert_eq!(Balances::free_balance(TREASURY), donated_native);
        System::assert_has_event(
            Event::PreSeedBalanceBurned {
                pool: key.clone(),
                asset: launch(),
                amount: donated_token,
            }
            .into(),
        );
        // The pool holds the seed and nothing donated.
        assert_eq!(reserves(&key), (SEED_NATIVE, SEED_TOKEN));
    });
}

// ============================================================================
// D5: amounts and minimums follow the CANONICAL pair order, not the caller's.
//
// `canonical_pair` sorts the pair (Native before WithId, lower id first) but
// the pre-D5 code left amount_a/amount_b and amount_a_min/amount_b_min bound
// to the caller's argument positions, so `add_liquidity(USDC, VTRS, 1000, 1)`
// deposited 1000 on the VTRS side and 1 on the USDC side. Same for the
// minimums of `remove_liquidity`. These tests fail on the pre-D5 code.
// ============================================================================

#[test]
fn d5_add_liquidity_non_canonical_order_maps_amounts_to_assets() {
    new_test_ext().execute_with(|| {
        // Canonical order is (Native, USDC); the caller passes (USDC, Native).
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), native(), 3));
        let key = VitreusDex::canonical_pair(usdc(), native());
        assert_eq!(key, (native(), usdc()));

        let native_before = Balances::free_balance(ALICE);
        let usdc_before = Assets::balance(USDC_ID, ALICE);
        // 100_000 USDC and 40_000 VTRS, given in the caller's (USDC, VTRS) order,
        // with exact minimums in the same order.
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            native(),
            100_000,
            40_000,
            100_000,
            40_000,
        ));
        assert_eq!(native_before - Balances::free_balance(ALICE), 40_000, "VTRS taken");
        assert_eq!(usdc_before - Assets::balance(USDC_ID, ALICE), 100_000, "USDC taken");
        assert_eq!(reserves(&key), (40_000, 100_000), "Native side first, USDC second");
        // The event reports canonical order.
        System::assert_has_event(
            Event::LiquidityAdded {
                provider: ALICE,
                asset_a: native(),
                asset_b: usdc(),
                amount_a: 40_000,
                amount_b: 100_000,
                shares_minted: 63_245 - 1_000,
            }
            .into(),
        );

        // Subsequent deposit, still in the caller's order: the optimal-amount
        // calculation must use the USDC offer against the USDC reserve.
        let native_before = Balances::free_balance(BOB);
        let usdc_before = Assets::balance(USDC_ID, BOB);
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            usdc(),
            native(),
            50_000, // USDC offered
            30_000, // VTRS offered; only 20_000 is needed at the pool ratio
            50_000,
            20_000,
        ));
        assert_eq!(usdc_before - Assets::balance(USDC_ID, BOB), 50_000);
        assert_eq!(native_before - Balances::free_balance(BOB), 20_000);
        assert_eq!(reserves(&key), (60_000, 150_000));
    });
}

#[test]
fn d5_remove_liquidity_non_canonical_order_maps_minimums() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            usdc(),
            40_000,
            100_000,
            0,
            0,
        ));
        let key = VitreusDex::canonical_pair(native(), usdc());
        let issued = total(&key);
        let half = shares(ALICE, &key) / 2;
        let expect_native = half * 40_000 / issued;
        let expect_usdc = half * 100_000 / issued;
        assert!(expect_usdc > expect_native);

        // Caller's order is (USDC, Native): the first minimum is the USDC one.
        // Pre-D5 the (large) USDC minimum was compared against the (small)
        // Native payout and the call failed with SlippageExceeded.
        let native_before = Balances::free_balance(ALICE);
        let usdc_before = Assets::balance(USDC_ID, ALICE);
        assert_ok!(VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            native(),
            half,
            expect_usdc,
            expect_native,
        ));
        assert_eq!(Assets::balance(USDC_ID, ALICE) - usdc_before, expect_usdc);
        assert_eq!(Balances::free_balance(ALICE) - native_before, expect_native);

        // And a minimum that is genuinely too high on the USDC side still
        // rejects (recomputed: the remaining position pays a hair more per
        // share because MINIMUM_LIQUIDITY stays burned).
        let payout_usdc = half * reserves(&key).1 / total(&key);
        assert_noop!(
            VitreusDex::remove_liquidity(
                RuntimeOrigin::signed(ALICE),
                usdc(),
                native(),
                half,
                payout_usdc + 1,
                0,
            ),
            Error::<Test>::SlippageExceeded
        );
    });
}

#[test]
fn d5_pool_manager_add_liquidity_for_non_canonical_order() {
    new_test_ext().execute_with(|| {
        // The in-runtime trait path (what the launchpad's rescue uses) is the
        // same body; check it through the trait with a reversed pair.
        assert_ok!(VitreusDex::do_create_pool(native(), usdc(), 3));
        let minted =
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::add_liquidity_for(
                &CHARLIE,
                usdc(),
                native(),
                100_000,
                40_000,
                100_000,
                40_000,
            )
            .expect("deposit");
        assert_eq!(minted, 63_245 - 1_000);
        assert_eq!(reserves(&VitreusDex::canonical_pair(native(), usdc())), (40_000, 100_000));
    });
}

// ===========================================================================
// D4 — per-pool fee routing. Routed slices are always native (VTRS), pulled
// by the beneficiaries from a fee escrow sub-account, never pushed inside a
// swap. Splits are snapshotted per pool at creation / seeding.
// ===========================================================================

use crate::{
    CreatorFeesUnclaimed, DefaultFeeRouting, FeeRouting, ProtocolFeeRecipient,
    ProtocolFeesUnclaimed,
};

/// D10: the default is per tier. These tests predate that and mean "the
/// routing a new pool gets", so set every tier the whitelist allows, with
/// the creator slice only where a launch pool can exist (tier 3 and 10).
fn set_routing(protocol_bps: u16, creator_bps: u16) {
    // Tier 1 is 10 bps and keeps 5, so it takes the protocol slice only when
    // the slice fits; these tests are about tiers 3 and 10 either way.
    if protocol_bps <= pool_floor_bps(1) {
        assert_ok!(VitreusDex::set_default_fee_routing(
            RuntimeOrigin::root(),
            1,
            protocol_bps,
            0,
            0
        ));
    }
    for tier in [3, 10] {
        assert_ok!(VitreusDex::set_default_fee_routing(
            RuntimeOrigin::root(),
            tier,
            protocol_bps,
            creator_bps,
            0
        ));
    }
}

fn escrow() -> AccountId {
    VitreusDex::fee_escrow_account()
}

/// The fee the pool charges, which its LPs keep: the record's tier less
/// its routed share, `(t − r) / (1 − r)`, rounded up.
fn lp_fee_of(key: &(NativeOrAssetId, NativeOrAssetId)) -> sp_runtime::Permill {
    let pool = Pools::<Test>::get(key).unwrap();
    VitreusDex::lp_fee(pool.fee_tier, &pool.routing)
}
/// What the base pool pays for `x` in at `fee`: its own formula.
fn base_out(fee: sp_runtime::Permill, x: u128, reserve_in: u128, reserve_out: u128) -> u128 {
    pallet_asset_conversion::Pallet::<Test>::get_amount_out(fee, &x, &reserve_in, &reserve_out)
        .unwrap()
}

/// A seeded launch pool with the given default split in force at seed time.
fn seeded_launch_pool(protocol_bps: u16, creator_bps: u16) -> (NativeOrAssetId, NativeOrAssetId) {
    setup_reserved_asset();
    set_routing(protocol_bps, creator_bps);
    seed(ESCROW).expect("seed");
    VitreusDex::canonical_pair(native(), launch())
}

#[test]
fn finding14_genesis_funds_the_fee_escrow() {
    // The from-genesis path (SECURITY_AUDIT Finding 14): the DEX genesis endows
    // the fee escrow with the native ED, so it exists before the first swap.
    let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
    crate::GenesisConfig::<Test>::default().assimilate_storage(&mut t).unwrap();
    let mut ext = sp_io::TestExternalities::new(t);
    ext.execute_with(|| {
        assert!(
            frame_system::Pallet::<Test>::account_exists(&VitreusDex::fee_escrow_account()),
            "genesis funds the fee escrow"
        );
    });
}

#[test]
fn d4_default_routing_is_zero_and_swaps_route_nothing() {
    new_test_ext().execute_with(|| {
        // D10: unset is not zero — a seed at an unconfigured tier is refused.
        assert_eq!(DefaultFeeRouting::<Test>::get(3), None);
        let key = seeded_launch_pool(0, 0);
        assert_eq!(Pools::<Test>::get(key.clone()).unwrap().routing, FeeRouting::default());

        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            launch(),
            100 * UNIT,
            0,
            BOB,
        ));
        assert_eq!(Balances::free_balance(escrow()), 0);
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), 0);
        assert_eq!(CreatorFeesUnclaimed::<Test>::get(key), 0);
        assert!(!System::events()
            .iter()
            .any(|r| matches!(r.event, RuntimeEvent::VitreusDex(Event::FeesRouted { .. }))));
    });
}

#[test]
fn d4_set_default_fee_routing_validates_and_requires_manage_origin() {
    new_test_ext().execute_with(|| {
        assert_noop!(
            VitreusDex::set_default_fee_routing(RuntimeOrigin::signed(ALICE), 3, 5, 5, 0),
            sp_runtime::DispatchError::BadOrigin
        );
        assert_noop!(
            VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 2, 5, 5, 0),
            Error::<Test>::InvalidFeeTier
        );
        // D10: tier 1 is 10 bps and keeps 5, so 6 does not fit ...
        assert_noop!(
            VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 1, 6, 0, 0),
            Error::<Test>::InvalidFeeRouting
        );
        // ... and it can carry no creator or treasury slice at all.
        assert_noop!(
            VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 1, 0, 5, 0),
            Error::<Test>::InvalidFeeRouting
        );
        // Tier 3 is 30 bps and keeps 10, so 21 does not fit.
        assert_noop!(
            VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 5, 5, 11),
            Error::<Test>::InvalidFeeRouting
        );
        set_routing(5, 5);
        assert_eq!(
            DefaultFeeRouting::<Test>::get(3),
            Some(FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 0 })
        );
        assert_eq!(
            DefaultFeeRouting::<Test>::get(1),
            Some(FeeRouting { protocol_bps: 5, creator_bps: 0, treasury_bps: 0 }),
            "tier 1 carries the protocol slice alone"
        );
        System::assert_has_event(
            Event::DefaultFeeRoutingSet {
                fee_tier: 3,
                routing: FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 0 },
            }
            .into(),
        );
        // D10: a tier-10 pool may route what a tier-3 pool cannot.
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 10, 5, 5, 60));
        assert_eq!(DefaultFeeRouting::<Test>::get(10).unwrap().routed_bps(), 70);
        assert_noop!(
            VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 5, 5, 60),
            Error::<Test>::InvalidFeeRouting
        );
    });
}

#[test]
fn d4_create_pool_snapshots_protocol_share_and_folds_creator_share() {
    new_test_ext().execute_with(|| {
        set_routing(5, 5);
        // Governance-created pool with a native side: nobody could claim a
        // creator share, so it folds into the pool.
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        let key = VitreusDex::canonical_pair(native(), usdc());
        assert_eq!(
            Pools::<Test>::get(key).unwrap().routing,
            FeeRouting { protocol_bps: 5, creator_bps: 0, treasury_bps: 0 }
        );
        // No native side: nothing can be routed in VTRS.
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 3));
        assert_eq!(Pools::<Test>::get(pair()).unwrap().routing, FeeRouting::default());
    });
}

#[test]
fn d4_seed_snapshots_full_default_and_later_changes_never_touch_existing_pools() {
    new_test_ext().execute_with(|| {
        let key = seeded_launch_pool(5, 5);
        assert_eq!(
            Pools::<Test>::get(key.clone()).unwrap().routing,
            FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 0 }
        );

        // D10: tier 1 keeps 5 bps, so the new default's protocol slice is 4.
        set_routing(4, 0);
        assert_eq!(
            Pools::<Test>::get(key).unwrap().routing,
            FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 0 },
            "a live pool's split is a snapshot, not a live read"
        );
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 1));
        let usdc_key = VitreusDex::canonical_pair(native(), usdc());
        assert_eq!(
            Pools::<Test>::get(usdc_key).unwrap().routing,
            FeeRouting { protocol_bps: 4, creator_bps: 0, treasury_bps: 0 }
        );
    });
}

#[test]
fn d4_swap_native_in_routes_slices_out_of_the_fee() {
    new_test_ext().execute_with(|| {
        let key = seeded_launch_pool(5, 5);

        // The routed slices come off the input in native; the pool charges
        // its LP fee on the rest. Together that is the tier: the output is
        // within a part per million of one formula charging the whole tier.
        let amount_in = 100 * UNIT;
        let fee = amount_in * 3 / 1_000;
        let after_fee = amount_in - fee;
        let single_tier_out = mul_div_u256(SEED_TOKEN, after_fee, SEED_NATIVE + after_fee);
        let protocol = amount_in * 5 / 10_000;
        let creator = amount_in * 5 / 10_000;
        assert!(protocol + creator < fee);
        let to_pool = amount_in - protocol - creator;
        let lp_fee = lp_fee_of(&key);
        let expected_out = base_out(lp_fee, to_pool, SEED_NATIVE, SEED_TOKEN);
        assert!(expected_out.abs_diff(single_tier_out) <= single_tier_out / 1_000_000);

        let token_before = Assets::balance(LAUNCH_ID, BOB);
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            launch(),
            amount_in,
            expected_out,
            BOB,
        ));
        assert_eq!(Assets::balance(LAUNCH_ID, BOB) - token_before, expected_out);

        // Slices went to the escrow and were counted.
        assert_eq!(Balances::free_balance(escrow()), protocol + creator);
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), protocol);
        assert_eq!(CreatorFeesUnclaimed::<Test>::get(key.clone()), creator);
        System::assert_has_event(
            Event::FeesRouted { pool: key.clone(), protocol, creator, treasury: 0 }.into(),
        );

        // The pool holds the input less the routed slices — its LP fee
        // included — so nothing routed is ever counted as depth.
        let (ra, rb) = reserves(&key);
        assert_eq!((ra, rb), (SEED_NATIVE + to_pool, SEED_TOKEN - expected_out));
        assert_eq!(
            Pools::<Test>::get(key).unwrap().total_fees_collected,
            protocol + creator + lp_fee.mul_floor(to_pool)
        );

        let k_before = sp_core::U256::from(SEED_NATIVE) * sp_core::U256::from(SEED_TOKEN);
        let k_after = sp_core::U256::from(ra) * sp_core::U256::from(rb);
        assert!(k_after >= k_before);
    });
}

#[test]
fn d4_swap_native_out_routes_slices_from_the_gross_output() {
    new_test_ext().execute_with(|| {
        let key = seeded_launch_pool(5, 5);

        // Token in: the pool charges its LP fee on the input; the routed
        // 10 bps come off the gross native output.
        let amount_in = 1_000_000 * UNIT;
        let lp_fee = lp_fee_of(&key);
        let gross = base_out(lp_fee, amount_in, SEED_TOKEN, SEED_NATIVE);
        let protocol = gross * 5 / 10_000;
        let creator = gross * 5 / 10_000;
        let net = gross - protocol - creator;
        assert!(net > 0);
        // Against the pool keeping (tier − routed) = 20 bps of the input, as
        // the single-pool swap did: at most 3 parts per million less.
        let old_after_fee = amount_in - amount_in * 20 / 10_000;
        let old_gross = mul_div_u256(SEED_NATIVE, old_after_fee, SEED_TOKEN + old_after_fee);
        let old_net = old_gross - 2 * (old_gross * 5 / 10_000);
        assert!(net <= old_net && old_net - net <= 3 * old_net / 1_000_000);

        // The slippage bound applies to what the trader actually receives.
        assert_noop!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                launch(),
                native(),
                amount_in,
                net + 1,
                BOB,
            ),
            Error::<Test>::SlippageExceeded
        );
        let native_before = Balances::free_balance(BOB);
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            launch(),
            native(),
            amount_in,
            net,
            BOB,
        ));
        assert_eq!(Balances::free_balance(BOB) - native_before, net);
        assert_eq!(Balances::free_balance(escrow()), protocol + creator);
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), protocol);
        assert_eq!(CreatorFeesUnclaimed::<Test>::get(key.clone()), creator);

        let (ra, rb) = reserves(&key);
        assert_eq!((ra, rb), (SEED_NATIVE - gross, SEED_TOKEN + amount_in));
        assert_eq!(
            Pools::<Test>::get(key).unwrap().total_fees_collected,
            lp_fee.mul_floor(amount_in)
        );

        let k_before = sp_core::U256::from(SEED_NATIVE) * sp_core::U256::from(SEED_TOKEN);
        let k_after = sp_core::U256::from(ra) * sp_core::U256::from(rb);
        assert!(k_after >= k_before);
    });
}

#[test]
fn d4_pool_without_native_side_routes_nothing() {
    new_test_ext().execute_with(|| {
        set_routing(5, 5);
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 3));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            100_000,
            100_000,
            0,
            0
        ));
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            usdc(),
            vnrg(),
            10_000,
            0,
            BOB,
        ));
        assert_eq!(Balances::free_balance(escrow()), 0);
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), 0);
        assert_eq!(CreatorFeesUnclaimed::<Test>::get(pair()), 0);
        // The whole 0.3% stayed with the pool, as before D4.
        assert_eq!(Pools::<Test>::get(pair()).unwrap().total_fees_collected, 10_000 * 3 / 1_000);
    });
}

#[test]
fn d4_claim_pool_creator_fees_pays_only_the_lookup_recipient() {
    new_test_ext().execute_with(|| {
        let key = seeded_launch_pool(5, 5);
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            launch(),
            100 * UNIT,
            0,
            BOB,
        ));
        let accrued = CreatorFeesUnclaimed::<Test>::get(key.clone());
        assert_eq!(accrued, 100 * UNIT * 5 / 10_000);

        // Not the creator on record.
        assert_noop!(
            VitreusDex::claim_pool_creator_fees(RuntimeOrigin::signed(BOB), launch()),
            Error::<Test>::NotCreatorFeeRecipient
        );
        // No creator is known for this asset at all.
        assert_noop!(
            VitreusDex::claim_pool_creator_fees(RuntimeOrigin::signed(ALICE), usdc()),
            Error::<Test>::NoCreatorForAsset
        );

        let before = Balances::free_balance(CREATOR);
        assert_ok!(VitreusDex::claim_pool_creator_fees(RuntimeOrigin::signed(CREATOR), launch()));
        assert_eq!(Balances::free_balance(CREATOR) - before, accrued);
        assert_eq!(CreatorFeesUnclaimed::<Test>::get(key.clone()), 0);
        System::assert_has_event(
            Event::CreatorFeesClaimed { pool: key, recipient: CREATOR, amount: accrued }.into(),
        );
        // Protocol share is untouched by a creator claim.
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), 100 * UNIT * 5 / 10_000);
        assert_eq!(Balances::free_balance(escrow()), ProtocolFeesUnclaimed::<Test>::get());

        assert_noop!(
            VitreusDex::claim_pool_creator_fees(RuntimeOrigin::signed(CREATOR), launch()),
            Error::<Test>::ZeroAmount
        );
    });
}

#[test]
fn d4_withdraw_protocol_fees_is_permissionless_and_follows_the_recipient() {
    new_test_ext().execute_with(|| {
        assert_noop!(
            VitreusDex::withdraw_protocol_fees(RuntimeOrigin::signed(BOB)),
            Error::<Test>::ZeroAmount
        );
        seeded_launch_pool(5, 5);
        let swap = || {
            assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                native(),
                launch(),
                100 * UNIT,
                0,
                BOB,
            ));
        };
        let slice = 100 * UNIT * 5 / 10_000;

        // Default recipient is the runtime-bound treasury; anyone may trigger.
        swap();
        assert_eq!(VitreusDex::protocol_fee_recipient(), TREASURY);
        let treasury_before = Balances::free_balance(TREASURY);
        assert_ok!(VitreusDex::withdraw_protocol_fees(RuntimeOrigin::signed(BOB)));
        assert_eq!(Balances::free_balance(TREASURY) - treasury_before, slice);
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), 0);
        System::assert_has_event(
            Event::ProtocolFeesWithdrawn { recipient: TREASURY, amount: slice }.into(),
        );

        // Governance redirects; fees accrued before the change follow it.
        swap();
        assert_noop!(
            VitreusDex::set_protocol_fee_recipient(RuntimeOrigin::signed(ALICE), Some(CHARLIE)),
            sp_runtime::DispatchError::BadOrigin
        );
        assert_ok!(VitreusDex::set_protocol_fee_recipient(RuntimeOrigin::root(), Some(CHARLIE)));
        assert_eq!(ProtocolFeeRecipient::<Test>::get(), Some(CHARLIE));
        let charlie_before = Balances::free_balance(CHARLIE);
        assert_ok!(VitreusDex::withdraw_protocol_fees(RuntimeOrigin::signed(ALICE)));
        assert_eq!(Balances::free_balance(CHARLIE) - charlie_before, slice);

        // `None` restores the default.
        assert_ok!(VitreusDex::set_protocol_fee_recipient(RuntimeOrigin::root(), None));
        assert_eq!(ProtocolFeeRecipient::<Test>::get(), None);
        assert_eq!(VitreusDex::protocol_fee_recipient(), TREASURY);
        // Creator accrual is untouched by protocol withdrawals.
        let key = VitreusDex::canonical_pair(native(), launch());
        assert_eq!(CreatorFeesUnclaimed::<Test>::get(key), 2 * slice);
        assert_eq!(Balances::free_balance(escrow()), 2 * slice);
    });
}

// ---- add_liquidity must price against the reserves it pays out of -----------
//
// The single-pool core left the pool's own share of each fee in the pool
// account uncounted until a sync, and `add_liquidity` priced against the
// stale reserves while `remove_liquidity` paid out of the synced ones, so
// swap → add → remove skimmed the existing LPs' fees. The base pool reads
// its reserves from its balances for every operation; these keep the
// scenario, and its exact figures, so a change there shows as a number. With
// no routing every fee stays in the pool, so the numbers below are exact.

/// swap → add → remove, with zero trading service provided, must not return
/// more than was deposited. Fails before the D6 sync (Bob withdraws 10_995
/// USDC against 10_990 deposited: half of the uncounted 10 USDC fee).
#[test]
fn d6_add_liquidity_cannot_capture_unsynced_fees() {
    new_test_ext().execute_with(|| {
        // 1.0 % tier so the uncounted fee is visible in whole units.
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            10_000,
            40_000,
            0,
            0,
        ));
        // 20_000 LP tokens in all.
        assert_eq!(total(&pair()), 20_000);

        // A swap: the 10 USDC fee stays in the pool, which then holds
        // 11_000 USDC and 40_000 − 3_603 = 36_397 VNRG — all of it reserve.
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(CHARLIE),
            usdc(),
            vnrg(),
            1_000,
            0,
            CHARLIE,
        ));
        let pool_account = pool_acc(usdc(), vnrg());
        assert_eq!(reserves(&pair()), (11_000, 36_397));

        // Bob deposits at the ratio before the fee and immediately withdraws.
        let bob_usdc_before = Assets::balance(USDC_ID, BOB);
        let bob_vnrg_before = Assets::balance(VNRG_ID, BOB);
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            usdc(),
            vnrg(),
            10_990,
            36_397,
            0,
            0,
        ));
        let bob_shares = shares(BOB, &pair());
        assert_ok!(VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(BOB),
            usdc(),
            vnrg(),
            bob_shares,
            0,
            0,
        ));
        // Floor rounding is in the pool's favour on both legs: Bob gets back
        // at most what he put in, never a slice of Alice's fees.
        assert!(Assets::balance(USDC_ID, BOB) <= bob_usdc_before);
        assert!(Assets::balance(VNRG_ID, BOB) <= bob_vnrg_before);
        // And the fee is still Alice's: the pool holds more USDC than it did
        // before Bob touched it.
        assert!(Assets::balance(USDC_ID, pool_account) >= 11_000);
    });
}

/// The exact figures of the D6 scenario after the fix, so a change to the
/// rounding or the sync order shows up as a number and not just a boolean.
#[test]
fn d6_add_liquidity_matches_optimal_amount_against_synced_reserves() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), usdc(), vnrg(), 10));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            vnrg(),
            10_000,
            40_000,
            0,
            0,
        ));
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(CHARLIE),
            usdc(),
            vnrg(),
            1_000,
            0,
            CHARLIE,
        ));
        // Synced reserves are (11_000, 36_397). For amount_a = 10_990:
        //   optimal_b = 10_990 × 36_397 / 11_000 = 36_363 (floor)
        //   shares    = min(10_990 × 20_000 / 11_000, 36_363 × 20_000 / 36_397)
        //             = min(19_981, 19_981) = 19_981
        let bob_usdc_before = Assets::balance(USDC_ID, BOB);
        let bob_vnrg_before = Assets::balance(VNRG_ID, BOB);
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            usdc(),
            vnrg(),
            10_990,
            36_397,
            0,
            0,
        ));
        assert_eq!(bob_usdc_before - Assets::balance(USDC_ID, BOB), 10_990);
        assert_eq!(bob_vnrg_before - Assets::balance(VNRG_ID, BOB), 36_363);
        assert_eq!(shares(BOB, &pair()), 19_981);
        assert_eq!(total(&pair()), 39_981);
        assert_eq!(reserves(&pair()), (21_990, 72_760));
    });
}

// ---------------------------------------------------------------------------
// D8: pool accounts must be unique per pair.
//
// `into_sub_account_truncating` keeps the first `size_of::<AccountId>()`
// bytes of "modl" ++ PalletId ++ SCALE(seed): twelve bytes of prefix, then
// whatever is left. With the pair key itself as the seed, an AccountId20
// runtime keeps eight bytes of it — `04 00 44 01` plus the low four bytes of
// the second asset id for a native pair, so `WithId(1)` and `WithId(2^64+1)`
// (chain asset 1 and launch 1) share one account, and for a `(WithId,
// WithId)` pair the second asset never appears at all. In this mock the
// account is sixteen bytes, four of the key survive, and every native pair
// collides. The hash-derived account (the fix) has neither problem.
// ---------------------------------------------------------------------------

/// Two pools, one account: the second pool's `sync_reserves` reads the
/// first pool's deposits as its own reserves, and a swap on the empty pool
/// pays out of the full one.
#[test]
fn d8_distinct_pairs_derive_distinct_pool_accounts() {
    new_test_ext().execute_with(|| {
        let a = pool_acc(native(), usdc());
        let b = pool_acc(native(), vnrg());
        assert_ne!(a, b, "VTRS/USDC and VTRS/VNRG must not share a pool account");
        let c = pool_acc(usdc(), vnrg());
        assert_ne!(a, c);
        assert_ne!(b, c);
        // Order-independent, as before.
        assert_eq!(pool_acc(vnrg(), native()), b);
    });
}

#[test]
fn d8_second_pool_on_a_shared_account_would_read_the_first_pools_reserves() {
    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            usdc(),
            1_000_000,
            10_000,
            0,
            0,
        ));
        let usdc_key = VitreusDex::canonical_pair(native(), usdc());
        let usdc_account = pool_acc(native(), usdc());
        assert_eq!(reserves(&usdc_key), (1_000_000, 10_000));

        // A second native-quoted pool. Before D8 it was written with the same
        // pool_account, and syncing it counted Alice's VTRS as its reserve.
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), vnrg(), 3));
        let vnrg_pair = VitreusDex::canonical_pair(native(), vnrg());
        let vnrg_account = pool_acc(native(), vnrg());

        // The consequence first, so the red run shows it. Bob makes the first
        // deposit into the VNRG pool and withdraws it. Before D8 the shared
        // account made the VNRG pool's synced reserves (1_000_000 VTRS from
        // Alice's USDC deposit, plus Bob's), so Bob's shares — all of the
        // pool's — cashed out Alice's VTRS along with his own.
        let bob_before = Balances::free_balance(BOB);
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(BOB),
            native(),
            vnrg(),
            10_000,
            10_000,
            0,
            0,
        ));
        let bob_shares = shares(BOB, &vnrg_pair);
        assert_ok!(VitreusDex::remove_liquidity(RuntimeOrigin::signed(BOB), native(), vnrg(), bob_shares, 0, 0));
        let bob_after = Balances::free_balance(BOB);
        assert!(
            bob_after <= bob_before,
            "Bob deposited 10_000 VTRS into an empty pool and withdrew {} more than he had; the USDC pool's account went from 1_000_000 to {}",
            bob_after - bob_before,
            Balances::free_balance(usdc_account),
        );
        // The USDC pool still holds every unit Alice deposited.
        assert_eq!(Balances::free_balance(usdc_account), 1_000_000);
        assert_eq!(reserves(&usdc_key), (1_000_000, 10_000));

        // And the reason: the two pools have their own accounts. The VNRG
        // pool's holds only what Bob's round trip left behind (the locked
        // MINIMUM_LIQUIDITY's share), never Alice's million.
        assert_ne!(vnrg_account, usdc_account);
        assert!(Balances::free_balance(vnrg_account) <= 10_000);
    });
}

// ===========================================================================
// D9 — treasury slice (LAUNCH_TREASURY_SPEC §7.1). A third routed slice,
// pushed to the launch's treasury sink inside the swap (safe because the
// sink is a pallet-owned account, never a user's), folded into the protocol
// share when the asset has no sink. The routing bound is tier-relative.
// ===========================================================================

use crate::{
    mock::{SINK_NOTED, SINK_VAULT, VAULT},
    pool_floor_bps, LastSwapBlock, MIN_FEE_TIER, MIN_LAUNCH_FEE_TIER, MIN_POOL_BPS,
};

/// Set the tiers a test needs. Tier 1 takes the protocol slice when it fits
/// its 5 bps of room; tier 10 always fits what tier 3 does.
fn set_routing3(protocol_bps: u16, creator_bps: u16, treasury_bps: u16) {
    if protocol_bps <= pool_floor_bps(1) {
        assert_ok!(VitreusDex::set_default_fee_routing(
            RuntimeOrigin::root(),
            1,
            protocol_bps,
            0,
            0
        ));
    }
    let routed = protocol_bps + creator_bps + treasury_bps;
    for tier in [3, 10] {
        if (FeeRouting { protocol_bps, creator_bps, treasury_bps }).is_valid_for(tier) {
            assert_ok!(VitreusDex::set_default_fee_routing(
                RuntimeOrigin::root(),
                tier,
                protocol_bps,
                creator_bps,
                treasury_bps
            ));
        } else {
            assert!(routed > 0, "a zero split fits every tier");
        }
    }
}

fn seeded_launch_pool3(
    protocol_bps: u16,
    creator_bps: u16,
    treasury_bps: u16,
) -> (NativeOrAssetId, NativeOrAssetId) {
    setup_reserved_asset();
    set_routing3(protocol_bps, creator_bps, treasury_bps);
    seed(ESCROW).expect("seed");
    VitreusDex::canonical_pair(native(), launch())
}

#[test]
fn d9_routing_is_tier_relative() {
    // The struct rule, independent of storage.
    let r = FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 10 };
    assert!(r.is_valid_for(3) && r.is_valid_for(10));
    assert!(!r.is_valid_for(1), "20 bps does not fit a 10 bps tier");
    // D10: the pool keeps a floor, so a pool may no longer route its whole tier.
    assert_eq!((MIN_FEE_TIER, MIN_LAUNCH_FEE_TIER, MIN_POOL_BPS), (1, 3, 10));
    assert_eq!((pool_floor_bps(1), pool_floor_bps(3), pool_floor_bps(10)), (5, 10, 10));
    assert!(
        !FeeRouting { protocol_bps: 10, creator_bps: 10, treasury_bps: 10 }.is_valid_for(3),
        "30 bps leaves a 30 bps tier nothing"
    );
    assert!(FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 60 }.is_valid_for(10));
    assert!(
        !FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 81 }.is_valid_for(10),
        "91 bps leaves a 100 bps tier less than the floor"
    );
    assert!(FeeRouting { protocol_bps: 5, creator_bps: 0, treasury_bps: 0 }.is_valid_for(1));
    assert!(!FeeRouting { protocol_bps: 6, creator_bps: 0, treasury_bps: 0 }.is_valid_for(1));
    // The arithmetic invariant is weaker, and is what `try_state` keeps: a
    // pool created under the old bound still routes no more than it charges.
    assert!(FeeRouting { protocol_bps: 10, creator_bps: 10, treasury_bps: 10 }.fits_tier(3));
    assert!(!FeeRouting { protocol_bps: 10, creator_bps: 10, treasury_bps: 11 }.fits_tier(3));

    new_test_ext().execute_with(|| {
        // A tier-1 `create_pool` pool carries the protocol slice only, so a
        // 5/5/10 default is fine for it ...
        set_routing3(5, 5, 10);
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 1));
        let key = VitreusDex::canonical_pair(native(), usdc());
        assert_eq!(
            Pools::<Test>::get(key).unwrap().routing,
            FeeRouting { protocol_bps: 5, creator_bps: 0, treasury_bps: 0 }
        );
        // ... and a seeded tier-3 pool carries all three.
        let key = seeded_launch_pool3(5, 5, 10);
        assert_eq!(
            Pools::<Test>::get(key).unwrap().routing,
            FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 10 }
        );
    });
}

#[test]
fn d9_treasury_push_goes_to_vault_and_notes() {
    new_test_ext().execute_with(|| {
        SINK_VAULT.with(|v| *v.borrow_mut() = Some(VAULT));
        let key = seeded_launch_pool3(5, 5, 10);
        let lp_fee = lp_fee_of(&key);

        // Native in: every slice comes off the input, the pool's fee off the rest.
        let amount_in = 100 * UNIT;
        let fee = amount_in * 3 / 1_000;
        let (protocol, creator, treasury) =
            (amount_in * 5 / 10_000, amount_in * 5 / 10_000, amount_in * 10 / 10_000);
        assert_eq!(protocol + creator + treasury, fee * 2 / 3);
        let to_pool = amount_in - protocol - creator - treasury;
        let expected_out = base_out(lp_fee, to_pool, SEED_NATIVE, SEED_TOKEN);
        let single_tier_out =
            mul_div_u256(SEED_TOKEN, amount_in - fee, SEED_NATIVE + amount_in - fee);
        assert!(expected_out.abs_diff(single_tier_out) <= single_tier_out / 1_000_000);

        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            launch(),
            amount_in,
            expected_out,
            BOB
        ));

        // The vault got its slice and was told about it; the other two went to the escrow as before.
        assert_eq!(Balances::free_balance(VAULT), treasury);
        assert_eq!(SINK_NOTED.with(|n| n.borrow().clone()), vec![(LAUNCH_ID, treasury)]);
        assert_eq!(Balances::free_balance(escrow()), protocol + creator);
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), protocol);
        assert_eq!(CreatorFeesUnclaimed::<Test>::get(key.clone()), creator);
        System::assert_has_event(
            Event::FeesRouted { pool: key.clone(), protocol, creator, treasury }.into(),
        );

        // The pool holds the input less every routed slice.
        assert_eq!(reserves(&key).0, SEED_NATIVE + to_pool);

        // Native out: the slices come off the gross output and the trader
        // gets the net, priced from the pool's balances.
        let tokens_in = 1_000_000 * UNIT;
        let (r_native, r_token) = reserves(&key);
        let gross = base_out(lp_fee, tokens_in, r_token, r_native);
        let (p2, c2, t2) = (gross * 5 / 10_000, gross * 5 / 10_000, gross * 10 / 10_000);
        let vtrs_before = Balances::free_balance(BOB);
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            launch(),
            native(),
            tokens_in,
            gross - p2 - c2 - t2,
            BOB
        ));
        assert_eq!(Balances::free_balance(BOB) - vtrs_before, gross - p2 - c2 - t2);
        assert_eq!(Balances::free_balance(VAULT), treasury + t2);
        assert_eq!(SINK_NOTED.with(|n| n.borrow().len()), 2);
        assert_eq!(LastSwapBlock::<Test>::get(key), Some(1));
    });
}

#[test]
fn d9_no_sink_folds_into_protocol() {
    new_test_ext().execute_with(|| {
        // Mainnet shape: the asset has no treasury. The slice is still taken
        // from the trader (the split is the pool's snapshot) but lands with
        // the protocol, so nothing is stranded and nothing is pushed.
        SINK_VAULT.with(|v| *v.borrow_mut() = None);
        let key = seeded_launch_pool3(5, 5, 10);
        let amount_in = 100 * UNIT;
        let (protocol, creator, treasury) =
            (amount_in * 5 / 10_000, amount_in * 5 / 10_000, amount_in * 10 / 10_000);
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            launch(),
            amount_in,
            0,
            BOB
        ));
        assert_eq!(Balances::free_balance(VAULT), 0);
        assert!(SINK_NOTED.with(|n| n.borrow().is_empty()));
        assert_eq!(ProtocolFeesUnclaimed::<Test>::get(), protocol + treasury);
        assert_eq!(CreatorFeesUnclaimed::<Test>::get(key.clone()), creator);
        assert_eq!(Balances::free_balance(escrow()), protocol + creator + treasury);
        System::assert_has_event(
            Event::FeesRouted { pool: key, protocol: protocol + treasury, creator, treasury: 0 }
                .into(),
        );
    });
}

#[test]
fn d9_seed_rejects_a_split_the_tier_cannot_carry() {
    new_test_ext().execute_with(|| {
        // D10: the default is validated for its own tier when it is set, so
        // a stored default the tier cannot carry no longer exists. Seeding
        // at a tier with no default at all is what must be refused — a
        // launch that graduated on a silent zero would feed its treasury
        // nothing for the life of the pool.
        setup_reserved_asset();
        assert_noop!(
            <VitreusDex as crate::ReservedPoolSeeder<AccountId, NativeOrAssetId, u128, u64>>::seed_reserved_pool_for(
                &ESCROW, launch(), native(), SEED_TOKEN, SEED_NATIVE, 3
            ),
            Error::<Test>::NoDefaultFeeRouting
        );
        // Tier 10 set, tier 3 forgotten: a tier-3 launch still stops.
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 10, 5, 5, 60));
        assert_noop!(
            <VitreusDex as crate::ReservedPoolSeeder<AccountId, NativeOrAssetId, u128, u64>>::seed_reserved_pool_for(
                &ESCROW, launch(), native(), SEED_TOKEN, SEED_NATIVE, 3
            ),
            Error::<Test>::NoDefaultFeeRouting
        );
        // At the tier governance did set, it seeds and snapshots that split.
        assert_ok!(<VitreusDex as crate::ReservedPoolSeeder<AccountId, NativeOrAssetId, u128, u64>>::seed_reserved_pool_for(
            &ESCROW, launch(), native(), SEED_TOKEN, SEED_NATIVE, 10
        ));
        let key = VitreusDex::canonical_pair(native(), launch());
        assert_eq!(
            Pools::<Test>::get(key).unwrap().routing,
            FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 60 }
        );
    });
}

#[test]
fn d10_unset_tier_blocks_a_seed_but_not_a_create_pool() {
    new_test_ext().execute_with(|| {
        // A governance pool has no creator and no treasury, so an
        // unconfigured tier means it routes nothing — as before any default
        // was ever set. It must not be blocked.
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        assert_eq!(
            Pools::<Test>::get(VitreusDex::canonical_pair(native(), usdc())).unwrap().routing,
            FeeRouting::default()
        );
        // A seed at the same unconfigured tier is refused, and succeeds once
        // governance sets it.
        setup_reserved_asset();
        assert_noop!(
            <VitreusDex as crate::ReservedPoolSeeder<AccountId, NativeOrAssetId, u128, u64>>::seed_reserved_pool_for(
                &ESCROW, launch(), native(), SEED_TOKEN, SEED_NATIVE, 3
            ),
            Error::<Test>::NoDefaultFeeRouting
        );
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 5, 5, 10));
        assert_ok!(<VitreusDex as crate::ReservedPoolSeeder<AccountId, NativeOrAssetId, u128, u64>>::seed_reserved_pool_for(
            &ESCROW, launch(), native(), SEED_TOKEN, SEED_NATIVE, 3
        ));
    });
}

#[test]
fn d9_swap_for_native_reserves_and_last_swap_block() {
    new_test_ext().execute_with(|| {
        SINK_VAULT.with(|v| *v.borrow_mut() = Some(VAULT));
        let key = seeded_launch_pool3(5, 5, 10);
        assert_eq!(
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::native_reserves(
                launch()
            ),
            Some((SEED_NATIVE, SEED_TOKEN))
        );
        assert_eq!(
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::native_reserves(
                usdc()
            ),
            None
        );
        assert_eq!(
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::last_swap_block(
                launch()
            ),
            Some(0),
            "a pool that never traded reads as block 0"
        );
        assert_eq!(
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::last_swap_block(
                usdc()
            ),
            None
        );

        // The in-runtime swap is the extrinsic's body — same output, same
        // routing, delivered to `who` — except for the dormancy clock: it is
        // the treasury buying the token back, not a person trading it (R2).
        System::set_block_number(7);
        let amount_in = 10 * UNIT;
        let before = Assets::balance(LAUNCH_ID, BOB);
        let out = <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::swap_for(
            &BOB,
            native(),
            launch(),
            amount_in,
            0,
        )
        .unwrap();
        assert_eq!(Assets::balance(LAUNCH_ID, BOB) - before, out);
        assert_eq!(Balances::free_balance(VAULT), amount_in * 10 / 10_000);
        assert_eq!(LastSwapBlock::<Test>::get(&key), None, "swap_for does not move the clock");
        assert_eq!(
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::last_swap_block(
                launch()
            ),
            Some(0)
        );
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            launch(),
            amount_in,
            0,
            BOB
        ));
        assert_eq!(LastSwapBlock::<Test>::get(&key), Some(7), "a person's swap does");
        // Slippage still binds.
        assert_noop!(
            <VitreusDex as PoolManager<AccountId, NativeOrAssetId, u128, u64>>::swap_for(
                &BOB,
                native(),
                launch(),
                amount_in,
                u128::MAX / 4
            ),
            Error::<Test>::SlippageExceeded
        );
    });
}

#[test]
fn migration_v5_removes_every_marketplace_item_and_nothing_else() {
    use crate::migrations::v5::{any_left, MigrateToV5, ITEMS};
    use frame_support::{
        storage::{storage_prefix, unhashed},
        traits::{GetStorageVersion, OnRuntimeUpgrade, StorageVersion},
    };

    new_test_ext().execute_with(|| {
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        // A value under each item, and two under each map-like one.
        for item in ITEMS {
            let p = storage_prefix(b"VitreusDex", item.as_bytes());
            unhashed::put_raw(&p, &[1]);
            unhashed::put_raw(&[&p[..], &[7u8; 16]].concat(), &[2]);
        }
        StorageVersion::new(4).put::<VitreusDex>();
        assert!(any_left::<Test>());

        MigrateToV5::<Test>::on_runtime_upgrade();

        assert!(!any_left::<Test>());
        assert!(Pools::<Test>::contains_key(VitreusDex::canonical_pair(native(), usdc())));
        assert_eq!(VitreusDex::on_chain_storage_version(), StorageVersion::new(5));
    });
}

/// A seeder holding only its own ED besides the quote — a launch created
/// before launches set an ED aside for the pool — still seeds, with all of
/// its quote: the base pool keeps no ED of its own, so a graduation never
/// fails on one.
#[test]
fn seeder_holding_only_its_ed_and_quote_seeds_all_of_it() {
    new_test_ext().execute_with(|| {
        setup_reserved_asset();
        let ed = 1_000;
        ExistentialDeposit::set(ed);
        // Exactly its own ED and the quote.
        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), ESCROW, ed + SEED_NATIVE));
        let key = VitreusDex::canonical_pair(native(), launch());
        let pool_account = pool_acc(native(), launch());

        assert_ok!(seed(ESCROW));

        assert_eq!(reserves(&key).0, SEED_NATIVE);
        assert_eq!(Balances::free_balance(pool_account), SEED_NATIVE);
        assert_eq!(Balances::free_balance(ESCROW), ed);
    });
}

/// v4 storage as testnet and the dev chain hold it, written raw: a pool, a
/// position, its total, its last swap and its creator fees, the
/// marketplace's items, and the settings that carry over.
fn write_v4_storage() -> (NativeOrAssetId, NativeOrAssetId) {
    use crate::migrations::{v5, v7::OldPoolInfo};
    use frame_support::{
        storage::{storage_prefix, unhashed},
        traits::StorageVersion,
        Blake2_128Concat, StorageHasher,
    };
    use parity_scale_codec::Encode;
    let pair = VitreusDex::canonical_pair(native(), usdc());
    let key =
        |item: &str, k: &[u8]| [&storage_prefix(b"VitreusDex", item.as_bytes())[..], k].concat();
    let pair_key = Blake2_128Concat::hash(&pair.encode());
    let old_account = acc(55);
    unhashed::put(
        &key("Pools", &pair_key),
        &OldPoolInfo::<u128, AccountId> {
            reserve_a: 1_000,
            reserve_b: 2_000,
            fee_tier: 3,
            total_fees_collected: 7,
            pool_account: old_account,
            routing: crate::FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 0 },
        },
    );
    unhashed::put(
        &key(
            "LiquidityPositions",
            &[&Blake2_128Concat::hash(&ALICE.encode())[..], &pair_key[..]].concat(),
        ),
        &(1_414u128, 1u64, Option::<u64>::None),
    );
    unhashed::put(&key("TotalLiquidity", &pair_key), &2_414u128);
    unhashed::put(&key("LastSwapBlock", &pair_key), &9u64);
    unhashed::put(&key("CreatorFeesUnclaimed", &pair_key), &3u128);
    for item in v5::ITEMS {
        unhashed::put_raw(&key(item, &[7u8; 16]), &[1]);
    }
    assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 5, 5, 0));
    assert_ok!(VitreusDex::set_protocol_fee_recipient(RuntimeOrigin::root(), Some(DAVE)));
    crate::ProtocolFeesUnclaimed::<Test>::put(500);
    assert_ok!(Balances::force_set_balance(
        RuntimeOrigin::root(),
        VitreusDex::fee_escrow_account(),
        500
    ));
    StorageVersion::new(4).put::<VitreusDex>();
    pair
}

frame_support::parameter_types! {
    pub const V7Source: AccountId = CHARLIE;
}

#[test]
fn migration_v7_clears_single_pool_core_and_keeps_settings() {
    use crate::migrations::{v5, v7};
    use frame_support::traits::{GetStorageVersion, OnRuntimeUpgrade, StorageVersion};

    new_test_ext().execute_with(|| {
        let pair = write_v4_storage();
        // An ED above what the escrow holds besides the fees it owes.
        ExistentialDeposit::set(1_000);
        assert_eq!(v7::counts::<Test>(), vec![1, 1, 1, 1]);
        assert!(v5::any_left::<Test>());
        let issuance = Balances::total_issuance();
        let source = Balances::free_balance(CHARLIE);

        v7::MigrateToV7::<Test, V7Source>::on_runtime_upgrade();

        // Cleared, the marketplace's items included.
        assert_eq!(v7::counts::<Test>(), vec![0, 0, 0, 0]);
        assert!(!v5::any_left::<Test>());
        assert!(Pools::<Test>::get(&pair).is_none());
        // Kept with the settings: the fees owed. A creator claims under the same key.
        assert_eq!(crate::CreatorFeesUnclaimed::<Test>::get(&pair), 3);
        // Kept.
        assert_eq!(
            crate::DefaultFeeRouting::<Test>::get(3),
            Some(crate::FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 0 })
        );
        assert_eq!(crate::ProtocolFeeRecipient::<Test>::get(), Some(DAVE));
        assert_eq!(crate::ProtocolFeesUnclaimed::<Test>::get(), 500);
        // The fee escrow's ED, from the source, nothing minted.
        // ED above the fees owed (500 + 3).
        assert_eq!(Balances::free_balance(VitreusDex::fee_escrow_account()), 1_503);
        assert_eq!(Balances::free_balance(CHARLIE), source - 1_003);
        assert_eq!(Balances::total_issuance(), issuance);
        assert_eq!(VitreusDex::on_chain_storage_version(), StorageVersion::new(7));
        assert_ok!(VitreusDex::do_try_state());

        // Gated: a second run does nothing.
        crate::ProtocolFeesUnclaimed::<Test>::put(501);
        v7::MigrateToV7::<Test, V7Source>::on_runtime_upgrade();
        assert_eq!(crate::ProtocolFeesUnclaimed::<Test>::get(), 501);

        // And the core works on the cleared chain: the pair can be created again.
        assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), usdc(), 3));
        assert_ok!(VitreusDex::add_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            usdc(),
            100_000,
            100_000,
            0,
            0
        ));
    });
}

#[cfg(feature = "try-runtime")]
#[test]
fn migration_v7_passes_its_own_try_runtime_checks() {
    use frame_support::traits::OnRuntimeUpgrade;
    new_test_ext().execute_with(|| {
        write_v4_storage();
        ExistentialDeposit::set(1_000);
        assert_ok!(crate::migrations::v7::MigrateToV7::<Test, V7Source>::try_on_runtime_upgrade(
            true
        ));
    });
}

#[test]
fn migration_v7_runs_only_on_v4_chain() {
    use frame_support::traits::{GetStorageVersion, OnRuntimeUpgrade, StorageVersion};
    new_test_ext().execute_with(|| {
        let pair = write_v4_storage();
        StorageVersion::new(6).put::<VitreusDex>();
        crate::migrations::v7::MigrateToV7::<Test, V7Source>::on_runtime_upgrade();
        assert_eq!(crate::migrations::v7::counts::<Test>(), vec![1, 1, 1, 1], "untouched");
        assert_eq!(VitreusDex::on_chain_storage_version(), StorageVersion::new(6));
        let _ = pair;
    });
}

/// A live chain's whole `VitreusDex` storage, exported raw: tier 3's
/// default split, and storage version 4. No pool, position or fee; its fee
/// escrow held nothing.
const TESTNET_DEX_STORAGE: [(&str, &str); 2] = [
    (
        "ce14eba2a7fea92eb5298d5a9e3cf30a0155e5add1f2cc8b1e913cc30b281729bfb27f1eaef06bb903000000",
        "050005000a00",
    ),
    ("ce14eba2a7fea92eb5298d5a9e3cf30a4e7b9012096b41c4eb3aaf947f6ea429", "0400"),
];

#[test]
fn migration_v7_on_testnets_own_storage() {
    use frame_support::traits::{GetStorageVersion, OnRuntimeUpgrade, StorageVersion};
    frame_support::parameter_types! {
        pub const Treasury: AccountId = DAVE;
    }
    new_runtime_like_ext_before_upgrade().execute_with(|| {
        for (k, v) in TESTNET_DEX_STORAGE {
            frame_support::storage::unhashed::put_raw(
                &sp_core::bytes::from_hex(k).unwrap(),
                &sp_core::bytes::from_hex(v).unwrap(),
            );
        }
        assert_eq!(VitreusDex::on_chain_storage_version(), StorageVersion::new(4));
        let escrow = VitreusDex::fee_escrow_account();
        assert_eq!(Balances::free_balance(escrow), 0);
        let issuance = Balances::total_issuance();

        #[cfg(feature = "try-runtime")]
        assert_ok!(crate::migrations::v7::MigrateToV7::<Test, Treasury>::try_on_runtime_upgrade(
            true
        ));
        #[cfg(not(feature = "try-runtime"))]
        crate::migrations::v7::MigrateToV7::<Test, Treasury>::on_runtime_upgrade();

        assert_eq!(VitreusDex::on_chain_storage_version(), StorageVersion::new(7));
        assert_eq!(
            crate::DefaultFeeRouting::<Test>::get(3),
            Some(crate::FeeRouting { protocol_bps: 5, creator_bps: 5, treasury_bps: 10 })
        );
        assert_eq!(Balances::free_balance(escrow), RUNTIME_ED);
        assert_eq!(Balances::total_issuance(), issuance);
        assert_ok!(VitreusDex::do_try_state());
    });
}
