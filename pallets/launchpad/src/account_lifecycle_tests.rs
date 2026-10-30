//! Graduation when the accounts the seed pays into are full: every asset an
//! account holds through a consumer reference takes one of its 16, and
//! anyone can open such balances on a public address.

use crate::{mock::*, Curves, Event, LaunchId, NextLaunchId, Phase};
use frame_support::assert_ok;
use sp_runtime::DispatchError;
use std::borrow::Borrow;

fn native() -> NativeOrAssetId {
    NativeOrAssetId::Native
}
fn asset_of(id: LaunchId) -> u128 {
    ASSET_BASE + id as u128
}
fn kind(id: LaunchId) -> NativeOrAssetId {
    NativeOrAssetId::WithId(asset_of(id))
}
fn pool_account(id: LaunchId) -> Acc {
    VitreusDex::pool_account_for(native(), kind(id)).unwrap()
}
fn origin(who: impl Borrow<Acc>) -> RuntimeOrigin {
    RuntimeOrigin::signed(who.borrow().clone())
}
fn bv(s: &[u8]) -> frame_support::BoundedVec<u8, frame_support::traits::ConstU32<50>> {
    s.to_vec().try_into().unwrap()
}
fn create(creator: impl Borrow<Acc>) -> LaunchId {
    let id = NextLaunchId::<Test>::get();
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
fn phase(id: LaunchId) -> Phase {
    Curves::<Test>::get(id).expect("curve").phase
}
fn deferred_error() -> Option<DispatchError> {
    System::events().iter().rev().find_map(|r| match &r.event {
        RuntimeEvent::Launchpad(Event::GraduationDeferred { error, .. }) => Some(*error),
        _ => None,
    })
}

/// DAVE creates one-unit balances of fresh non-sufficient assets on `who`
/// until pallet-assets would refuse to open another account there.
fn fill_consumer_slots(who: &Acc) {
    let mut id = 1_000u128;
    while frame_system::Pallet::<Test>::can_accrue_consumers(who, 2) {
        while <Assets as frame_support::traits::fungibles::Inspect<Acc>>::asset_exists(id) {
            id += 1;
        }
        assert_ok!(Assets::force_create(RuntimeOrigin::root(), id.into(), DAVE, false, 1));
        assert_ok!(Assets::mint(origin(DAVE), id.into(), who.clone(), 1));
    }
}

#[test]
fn launch_graduates_although_its_pool_address_holds_fifteen_other_tokens() {
    new_test_ext().execute_with(|| {
        let id = create(ALICE);
        let pool = pool_account(id);
        assert_ok!(Balances::transfer_allow_death(origin(DAVE), pool.clone(), UNIT));
        fill_consumer_slots(&pool);

        assert_ok!(Launchpad::buy(origin(BOB), id, 500_000_000 * UNIT, 0));
        assert_eq!(phase(id), Phase::Graduated, "deferred with {:?}", deferred_error());
    });
}

#[test]
fn donation_of_launch_token_graduates_although_excess_recipient_is_full() {
    new_test_ext().execute_with(|| {
        let id = create(ALICE);
        fill_consumer_slots(&EXCESS);
        // A small holder parks some of the launch's own token at the pool address.
        assert_ok!(Launchpad::buy(origin(DAVE), id, UNIT, 0));
        let pool = pool_account(id);
        assert_ok!(Balances::transfer_allow_death(origin(DAVE), pool.clone(), UNIT));
        assert_ok!(Assets::transfer(origin(DAVE), asset_of(id).into(), pool, 1_000));

        assert_ok!(Launchpad::buy(origin(BOB), id, 500_000_000 * UNIT, 0));
        assert_eq!(phase(id), Phase::Graduated, "deferred with {:?}", deferred_error());
    });
}

/// A launch's treasury vault, before its first fee of at least an ED,
/// takes a sub-ED curve slice only if that leaves it an ED of native. One
/// a sufficient asset keeps alive "exists" and holds none: the slice must
/// fold into the protocol share, not fail the buy.
#[test]
fn sufficient_token_on_vault_does_not_fail_small_buy() {
    new_test_ext().execute_with(|| {
        let vault = Acc::new([87u8; 32]);
        SINK_VAULT.with(|v| *v.borrow_mut() = Some(vault.clone()));
        assert_ok!(Launchpad::set_params(
            RuntimeOrigin::root(),
            crate::LaunchParams {
                graduation_target: T_DEFAULT,
                curve_fee_bps: 100,
                protocol_share_bps: 2_500,
                treasury_share_bps: 2_500,
                pool_fee_tier: 3,
                creation_fee: CREATION_FEE,
            }
        ));
        let id = create(ALICE);
        let sufficient = 5_000u128;
        assert_ok!(Assets::force_create(RuntimeOrigin::root(), sufficient.into(), DAVE, true, 1));
        assert_ok!(Assets::mint(origin(DAVE), sufficient.into(), vault.clone(), 1));
        assert!(frame_system::Pallet::<Test>::account_exists(&vault));

        // 1% fee, a quarter of it to the treasury: a slice of a quarter ED.
        assert_ok!(Launchpad::buy(origin(BOB), id, ED, 0));
        assert_eq!(Balances::free_balance(&vault), 0, "the slice folded into the protocol share");
    });
}

/// A buyer whose consumer references are used up gets the launch token on a
/// deposit of their own instead of being refused.
#[test]
fn buyer_with_every_consumer_slot_used_can_still_buy_launch_token() {
    new_test_ext().execute_with(|| {
        let id = create(ALICE);
        fill_consumer_slots(&BOB);
        let reserved = Balances::reserved_balance(&BOB);
        assert_ok!(Launchpad::buy(origin(BOB), id, 10 * UNIT, 0));
        assert!(Assets::balance(asset_of(id), &BOB) > 0);
        assert_eq!(Balances::reserved_balance(&BOB), reserved + 10);
    });
}
