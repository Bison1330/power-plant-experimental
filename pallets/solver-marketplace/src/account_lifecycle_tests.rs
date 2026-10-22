//! How the marketplace's own accounts, and the accounts it pays, live and die under
//! the runtime's rules: an existential deposit far above one unit, every
//! asset non-sufficient (so each one an account holds takes a consumer
//! reference, at most 16 of them), and deposit-backed asset accounts.
//!
//! Every test here runs in `new_runtime_like_ext`. Each states what must
//! hold, not how a fix achieves it; where a fix may either refuse an
//! operation up front or make it work, the test accepts both.

use crate::{
    mock::*, settlement::IntentStatus, FillCommitments, Intents, NextIntentId, NextSolverId,
    Solvers,
};
use frame_support::{
    assert_ok,
    storage::{with_transaction, TransactionOutcome},
};
use pallet_vitreus_dex::IntentSwap;

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

/// A non-sufficient asset nobody in the test holds yet, minted to ALICE and
/// pooled against native, so an intent can buy it into an account that has
/// never held it.
fn fresh_token_with_pool() -> NativeOrAssetId {
    let id = next_free_asset_id();
    assert_ok!(Assets::force_create(RuntimeOrigin::root(), id, ALICE, false, 1));
    assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), id, ALICE, RUNTIME_LIKE_BALANCE));
    pool(native(), NativeOrAssetId::WithId(id), 10_000 * UNIT);
    NativeOrAssetId::WithId(id)
}

fn next_free_asset_id() -> u32 {
    pallet_vitreus_dex::testing::next_free_asset_id::<Test>(100)
}

/// DAVE creates one-unit balances of fresh non-sufficient assets on `who`
/// until pallet-assets would refuse to open another account there.
fn fill_consumer_slots(who: AccountId) {
    while frame_system::Pallet::<Test>::can_accrue_consumers(&who, 2) {
        let id = next_free_asset_id();
        assert_ok!(Assets::force_create(RuntimeOrigin::root(), id, DAVE, false, 1));
        assert_ok!(Assets::mint(RuntimeOrigin::signed(DAVE), id, who, 1));
    }
}

/// What a swap of `amount_in` would pay right now, without making it.
fn quote(asset_in: NativeOrAssetId, asset_out: NativeOrAssetId, amount_in: u128) -> u128 {
    with_transaction(|| {
        let out =
            <VitreusDex as IntentSwap<AccountId, NativeOrAssetId, u128>>::swap_exact_for_intent(
                &DAVE, asset_in, asset_out, amount_in, 1,
            );
        TransactionOutcome::Rollback(out)
    })
    .expect("the pool quotes this swap")
}

fn register(who: AccountId) -> u64 {
    let id = NextSolverId::<Test>::get();
    assert_ok!(Marketplace::register_solver(RuntimeOrigin::signed(who)));
    id
}

/// `who` submits an intent with a generous deadline; returns its id.
fn submit(
    who: AccountId,
    token_in: NativeOrAssetId,
    token_out: NativeOrAssetId,
    amount: u128,
) -> u64 {
    let id = NextIntentId::<Test>::get();
    assert_ok!(Marketplace::submit_intent(
        RuntimeOrigin::signed(who),
        token_in,
        token_out,
        amount,
        1,
        System::block_number() + 100,
    ));
    id
}

/// Give `who` an ED of native, so it exists and can hold any amount.
fn endow(who: AccountId) {
    assert_ok!(Balances::transfer_keep_alive(RuntimeOrigin::signed(DAVE), who, RUNTIME_ED));
}

fn past_settlement_window() {
    System::set_block_number(System::block_number() + 6);
}

fn intent_escrow() -> AccountId {
    Marketplace::intent_escrow_account()
}

fn fee_treasury() -> AccountId {
    Marketplace::protocol_treasury_account()
}

// ---------------------------------------------------------------------------
// The intent escrow
// ---------------------------------------------------------------------------

#[test]
fn native_intent_settles_into_non_sufficient_token_when_it_is_escrows_only_native() {
    new_runtime_like_ext().execute_with(|| {
        pool(native(), usdc(), 10_000 * UNIT);
        endow(fee_treasury());
        register(BOB);
        let id = submit(ALICE, native(), usdc(), 10 * UNIT);
        let out = quote(native(), usdc(), 10 * UNIT);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out));

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

#[test]
fn non_sufficient_token_intent_can_be_submitted_before_any_native_intent() {
    new_runtime_like_ext().execute_with(|| {
        assert_ok!(Marketplace::submit_intent(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            native(),
            UNIT,
            1,
            System::block_number() + 100,
        ));
    });
}

#[test]
fn last_native_intent_can_be_cancelled_while_token_intent_is_open() {
    new_runtime_like_ext().execute_with(|| {
        let native_intent = submit(ALICE, native(), usdc(), 10 * UNIT);
        submit(DAVE, usdc(), native(), 1);

        assert_ok!(Marketplace::cancel_intent(RuntimeOrigin::signed(ALICE), native_intent));
    });
}

#[test]
fn stray_token_unit_on_intent_escrow_does_not_lock_last_native_intent() {
    new_runtime_like_ext().execute_with(|| {
        let id = submit(ALICE, native(), usdc(), 10 * UNIT);
        assert_ok!(Assets::transfer(RuntimeOrigin::signed(DAVE), USDC_ID, intent_escrow(), 1));

        assert_ok!(Marketplace::cancel_intent(RuntimeOrigin::signed(ALICE), id));
    });
}

#[test]
fn token_intent_opened_during_settlement_window_does_not_stop_settlement() {
    new_runtime_like_ext().execute_with(|| {
        pool(native(), usdc(), 10_000 * UNIT);
        endow(fee_treasury());
        register(BOB);
        // A native intent that is the escrow's only native...
        let id = submit(ALICE, native(), usdc(), 10 * UNIT);
        let out = quote(native(), usdc(), 10 * UNIT);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out));
        // ...and a one-unit token intent opened after the commit.
        submit(DAVE, usdc(), native(), 1);

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

#[test]
fn native_intent_below_ed_is_refused_or_refunded_in_full() {
    new_runtime_like_ext().execute_with(|| {
        let large = submit(BOB, native(), usdc(), 10 * UNIT);
        let small = NextIntentId::<Test>::get();
        let small_amount = RUNTIME_ED / 2;
        if Marketplace::submit_intent(
            RuntimeOrigin::signed(ALICE),
            native(),
            usdc(),
            small_amount,
            1,
            System::block_number() + 100,
        )
        .is_err()
        {
            return;
        }
        assert_ok!(Marketplace::cancel_intent(RuntimeOrigin::signed(BOB), large));

        let before = native_of(ALICE);
        assert_ok!(Marketplace::cancel_intent(RuntimeOrigin::signed(ALICE), small));
        assert_eq!(native_of(ALICE), before + small_amount);
    });
}

#[test]
fn intent_escrow_accepts_new_token_with_its_consumer_slots_full() {
    new_runtime_like_ext().execute_with(|| {
        submit(CHARLIE, native(), usdc(), 10 * UNIT);
        fill_consumer_slots(intent_escrow());

        assert_ok!(Marketplace::submit_intent(
            RuntimeOrigin::signed(ALICE),
            usdc(),
            native(),
            UNIT,
            1,
            System::block_number() + 100,
        ));
    });
}

#[test]
fn native_intent_settles_into_new_token_with_escrows_consumer_slots_full() {
    new_runtime_like_ext().execute_with(|| {
        let token = fresh_token_with_pool();
        endow(fee_treasury());
        register(BOB);
        // Other native on the escrow, so its account outlives the swap input.
        submit(CHARLIE, native(), usdc(), 10 * UNIT);
        fill_consumer_slots(intent_escrow());
        let id = submit(DAVE, native(), token.clone(), 10 * UNIT);
        let out = quote(native(), token, 10 * UNIT);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out));

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

// ---------------------------------------------------------------------------
// The protocol's share of solver profit
// ---------------------------------------------------------------------------

#[test]
fn first_profitable_settlement_into_non_sufficient_token_pays_its_protocol_fee() {
    new_runtime_like_ext().execute_with(|| {
        pool(native(), usdc(), 10_000 * UNIT);
        register(BOB);
        submit(CHARLIE, native(), vnrg(), 10 * UNIT);
        let id = submit(ALICE, native(), usdc(), 10 * UNIT);
        let out = quote(native(), usdc(), 10 * UNIT);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out / 2));

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

#[test]
fn first_settlement_into_native_with_profit_below_five_eds_pays_its_fee() {
    new_runtime_like_ext().execute_with(|| {
        pool(native(), usdc(), 10_000 * UNIT);
        register(BOB);
        submit(CHARLIE, native(), vnrg(), 10 * UNIT);
        let id = submit(ALICE, usdc(), native(), 10 * UNIT);
        let out = quote(usdc(), native(), 10 * UNIT);
        // A profit of 2 EDs: the protocol's 20 % of it is below one ED.
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out - 2 * RUNTIME_ED));

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

#[test]
fn fee_treasury_holding_fifteen_tokens_still_takes_fee_in_sixteenth() {
    new_runtime_like_ext().execute_with(|| {
        let token = fresh_token_with_pool();
        endow(fee_treasury());
        fill_consumer_slots(fee_treasury());
        register(BOB);
        submit(CHARLIE, native(), usdc(), 10 * UNIT);
        let id = submit(DAVE, native(), token.clone(), 10 * UNIT);
        let out = quote(native(), token, 10 * UNIT);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out / 2));

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

// ---------------------------------------------------------------------------
// The user's own account after a commit
// ---------------------------------------------------------------------------

#[test]
fn user_filling_their_consumer_slots_after_commit_does_not_fail_settlement() {
    new_runtime_like_ext().execute_with(|| {
        let token = fresh_token_with_pool();
        endow(fee_treasury());
        register(BOB);
        submit(CHARLIE, native(), usdc(), 10 * UNIT);
        let id = submit(DAVE, native(), token.clone(), 10 * UNIT);
        let out = quote(native(), token, 10 * UNIT);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out));
        fill_consumer_slots(DAVE);

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

#[test]
fn user_reaping_their_account_after_commit_does_not_fail_settlement() {
    new_runtime_like_ext().execute_with(|| {
        const EVE: AccountId = acc(5);
        let token = fresh_token_with_pool();
        endow(fee_treasury());
        register(BOB);
        submit(CHARLIE, native(), usdc(), 10 * UNIT);
        // EVE holds only native, as a signer paying fees in VTRS can.
        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), EVE, 100 * UNIT));
        let id = submit(EVE, native(), token.clone(), 10 * UNIT);
        let out = quote(native(), token, 10 * UNIT);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out));
        assert_ok!(Balances::transfer_all(RuntimeOrigin::signed(EVE), DAVE, false));
        assert!(!frame_system::Pallet::<Test>::account_exists(&EVE));

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

#[test]
fn token_intent_settles_into_native_below_ed_when_escrow_holds_no_native() {
    new_runtime_like_ext().execute_with(|| {
        pool(native(), sufficient(), 10_000 * UNIT);
        endow(fee_treasury());
        register(BOB);
        // The escrow exists through the sufficient token alone.
        let amount = RUNTIME_ED / 2;
        let id = submit(ALICE, sufficient(), native(), amount);
        let out = quote(sufficient(), native(), amount);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out));

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

#[test]
fn solver_whose_slots_third_party_filled_still_settles_into_new_token() {
    new_runtime_like_ext().execute_with(|| {
        let token = fresh_token_with_pool();
        endow(fee_treasury());
        register(BOB);
        submit(CHARLIE, native(), usdc(), 10 * UNIT);
        let id = submit(ALICE, native(), token.clone(), 10 * UNIT);
        let out = quote(native(), token, 10 * UNIT);
        // A profit, which the solver takes in a token it has never held.
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, out / 2));
        fill_consumer_slots(BOB);

        assert_ok!(Marketplace::settle_intent(RuntimeOrigin::signed(BOB), id));
    });
}

#[test]
fn user_reaping_their_account_does_not_lock_solvers_bond() {
    new_runtime_like_ext().execute_with(|| {
        const EVE: AccountId = acc(5);
        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), EVE, 100 * UNIT));
        submit(CHARLIE, native(), usdc(), 10 * UNIT);
        let solver = register(BOB);
        let id = NextIntentId::<Test>::get();
        if Marketplace::submit_intent(
            RuntimeOrigin::signed(EVE),
            native(),
            usdc(),
            RUNTIME_ED / 2,
            1,
            System::block_number() + 100,
        )
        .is_err()
        {
            return;
        }
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, 1));
        assert_ok!(Balances::transfer_all(RuntimeOrigin::signed(EVE), DAVE, false));
        past_settlement_window();

        assert_ok!(Marketplace::slash_solver(RuntimeOrigin::signed(CHARLIE), id));
        assert_eq!(Solvers::<Test>::get(solver).map(|s| s.active_commitments), Some(0));
    });
}

// ---------------------------------------------------------------------------
// Solver bonds
// ---------------------------------------------------------------------------

#[test]
fn bond_below_ed_is_refused_or_lets_solvers_register() {
    new_runtime_like_ext().execute_with(|| {
        if Marketplace::set_solver_bond_amount(RuntimeOrigin::root(), RUNTIME_ED / 2).is_err() {
            return;
        }
        assert_ok!(Marketplace::register_solver(RuntimeOrigin::signed(BOB)));
    });
}

#[test]
fn bond_between_one_and_ten_eds_is_refused_or_slashable() {
    new_runtime_like_ext().execute_with(|| {
        if Marketplace::set_solver_bond_amount(RuntimeOrigin::root(), 5 * RUNTIME_ED).is_err() {
            return;
        }
        register(BOB);
        let id = submit(ALICE, native(), usdc(), 10 * UNIT);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, 1));
        past_settlement_window();

        assert_ok!(Marketplace::slash_solver(RuntimeOrigin::signed(CHARLIE), id));
    });
}

#[test]
fn stray_token_unit_on_solver_escrow_does_not_lock_its_bond() {
    new_runtime_like_ext().execute_with(|| {
        let solver = register(BOB);
        let escrow = Marketplace::solver_escrow_account(solver);
        assert_ok!(Assets::transfer(RuntimeOrigin::signed(DAVE), USDC_ID, escrow, 1));

        let before = native_of(BOB);
        assert_ok!(Marketplace::deregister_solver(RuntimeOrigin::signed(BOB)));
        assert_eq!(native_of(BOB), before + RUNTIME_SOLVER_BOND);
    });
}

#[test]
fn solver_that_poisons_its_own_escrow_is_still_slashed_and_user_refunded() {
    new_runtime_like_ext().execute_with(|| {
        let solver = register(BOB);
        let escrow = Marketplace::solver_escrow_account(solver);
        assert_ok!(Assets::transfer(RuntimeOrigin::signed(BOB), USDC_ID, escrow, 1));
        let id = submit(ALICE, native(), usdc(), 10 * UNIT);
        assert_ok!(Marketplace::commit_fill(RuntimeOrigin::signed(BOB), id, 1));
        past_settlement_window();

        let before = native_of(ALICE);
        assert_ok!(Marketplace::slash_solver(RuntimeOrigin::signed(CHARLIE), id));
        assert_eq!(native_of(ALICE), before + 10 * UNIT);
        assert!(FillCommitments::<Test>::get(id).is_none());
        assert_eq!(Intents::<Test>::get(id).map(|i| i.status), Some(IntentStatus::Expired));
        assert_eq!(Solvers::<Test>::get(solver).map(|s| s.active), Some(false));
    });
}

// ---------------------------------------------------------------------------
// Genesis
// ---------------------------------------------------------------------------

#[test]
fn chain_from_genesis_starts_with_every_escrow_holding_ed() {
    new_runtime_like_ext_from_genesis().execute_with(|| {
        for escrow in [intent_escrow(), fee_treasury()] {
            assert!(native_of(escrow) >= RUNTIME_ED, "{escrow} holds {}", native_of(escrow));
        }
    });
}
