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

/// Before the upgrade endows the fee escrow, so the routing guard is what
/// is under test: an account a sufficient asset keeps alive holds no native
/// and cannot take a sub-ED slice, whether it "exists" or not.
#[test]
fn sufficient_token_on_fee_escrow_does_not_fail_small_swaps() {
    new_runtime_like_ext_before_upgrade().execute_with(|| {
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
        // No native was dusted: what the account holds as a reserve is what
        // the pool records, and the account kept its own ED.
        assert_eq!(VitreusDex::reserve_balance(native(), &pool_account), recorded);
        assert!(native_of(pool_account) >= RUNTIME_ED);
    });
}

/// The same guard for a launch's treasury vault, which nothing endows
/// before its first fee of at least an ED.
#[test]
fn sufficient_token_on_launch_vault_does_not_fail_small_swaps() {
    new_runtime_like_ext().execute_with(|| {
        let launch = NativeOrAssetId::WithId(LAUNCH_ID);
        assert_ok!(Assets::force_create(RuntimeOrigin::root(), LAUNCH_ID, ALICE, false, 1));
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), LAUNCH_ID, ALICE, 1_000_000 * UNIT));
        assert_ok!(VitreusDex::set_default_fee_routing(RuntimeOrigin::root(), 3, 5, 5, 10));
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
        SINK_VAULT.with(|v| *v.borrow_mut() = Some(VAULT));
        // The vault now exists, holding no native.
        assert_ok!(Assets::transfer(RuntimeOrigin::signed(DAVE), SUFFICIENT_ID, VAULT, 1));

        // Its treasury slice is below one ED.
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            launch,
            UNIT / 1_000,
            1,
            BOB,
        ));
    });
}

// ---------------------------------------------------------------------------
// Assets whose minimum balance is above one unit
// ---------------------------------------------------------------------------

/// A non-sufficient asset with the minimum balance one testnet asset has
/// (10^9), minted to ALICE, and a pool of `native_each` native against
/// `token_each` of it.
fn high_minimum_pool(native_each: u128, token_each: u128) -> (u32, AccountId) {
    let id = crate::testing::next_free_asset_id::<Test>(100);
    assert_ok!(Assets::force_create(RuntimeOrigin::root(), id, ALICE, false, HIGH_MINIMUM));
    assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), id, ALICE, 1_000 * UNIT));
    let token = NativeOrAssetId::WithId(id);
    assert_ok!(VitreusDex::create_pool(RuntimeOrigin::root(), native(), token.clone(), 10));
    assert_ok!(VitreusDex::add_liquidity(
        RuntimeOrigin::signed(ALICE),
        native(),
        token.clone(),
        native_each,
        token_each,
        0,
        0
    ));
    let account = Pools::<Test>::get(VitreusDex::canonical_pair(native(), token))
        .unwrap()
        .pool_account;
    (id, account)
}

const HIGH_MINIMUM: u128 = 1_000_000_000;

/// pallet-assets pays out with a debit any remainder below the asset's
/// minimum balance; a pool's remainder belongs to its LPs.
#[test]
fn swap_never_takes_pools_token_remainder_below_its_minimum() {
    new_runtime_like_ext().execute_with(|| {
        let (id, account) = high_minimum_pool(10 * UNIT, 3 * HIGH_MINIMUM);
        let token = NativeOrAssetId::WithId(id);
        // Enough native to buy all but about 0.86 of a minimum balance.
        let _ = VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(BOB),
            native(),
            token.clone(),
            25 * UNIT,
            1,
            BOB,
        );
        let recorded = Pools::<Test>::get(VitreusDex::canonical_pair(native(), token))
            .unwrap()
            .reserve_b;
        assert_eq!(
            VitreusDex::reserve_balance(NativeOrAssetId::WithId(id), &account),
            recorded,
            "the pool holds what it records"
        );
        assert!(Assets::balance(id, account) >= HIGH_MINIMUM);
    });
}

#[test]
fn lps_exit_never_takes_pools_token_remainder_below_its_minimum() {
    new_runtime_like_ext().execute_with(|| {
        let (id, account) = high_minimum_pool(10 * UNIT, 3 * HIGH_MINIMUM);
        let token = NativeOrAssetId::WithId(id);
        let pair = VitreusDex::canonical_pair(native(), token.clone());
        let shares = crate::LiquidityPositions::<Test>::get(ALICE, &pair).unwrap().shares;
        // The locked minimum's share of 3·10^9 is far below 10^9.
        let _ = VitreusDex::remove_liquidity(
            RuntimeOrigin::signed(ALICE),
            native(),
            token.clone(),
            shares,
            0,
            0,
        );
        let recorded = Pools::<Test>::get(&pair).unwrap().reserve_b;
        assert_eq!(
            VitreusDex::reserve_balance(NativeOrAssetId::WithId(id), &account),
            recorded,
            "the pool holds what it records"
        );
        assert!(Assets::balance(id, account) >= HIGH_MINIMUM);
    });
}

/// A swap whose output a new holder could not take is refused as such, not
/// by pallet-assets with `Token(BelowMinimum)`.
#[test]
fn swap_output_below_new_holders_minimum_is_refused_by_name() {
    new_runtime_like_ext().execute_with(|| {
        let (id, _) = high_minimum_pool(10_000 * UNIT, 1_000 * HIGH_MINIMUM);
        assert_eq!(
            VitreusDex::swap_exact_tokens_for_tokens(
                RuntimeOrigin::signed(BOB),
                native(),
                NativeOrAssetId::WithId(id),
                UNIT,
                1,
                BOB,
            ),
            Err(crate::Error::<Test>::RecipientCannotReceive.into())
        );
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

#[test]
fn chain_from_genesis_endows_fee_escrow_token_already_keeps_alive() {
    use sp_runtime::BuildStorage;
    ExistentialDeposit::set(RUNTIME_ED);
    AssetDeposit::set(RUNTIME_ASSET_DEPOSIT);
    AssetAccountDeposit::set(RUNTIME_ASSET_ACCOUNT_DEPOSIT);
    let mut t = runtime_like_storage::<Test>();
    let escrow = VitreusDex::fee_escrow_account();
    pallet_assets::GenesisConfig::<Test> {
        accounts: vec![(SUFFICIENT_ID, escrow, 1)],
        ..Default::default()
    }
    .assimilate_storage(&mut t)
    .unwrap();
    crate::GenesisConfig::<Test>::default().assimilate_storage(&mut t).unwrap();
    sp_io::TestExternalities::new(t).execute_with(|| {
        assert!(native_of(escrow) >= RUNTIME_ED, "{escrow} holds {}", native_of(escrow));
    });
}
