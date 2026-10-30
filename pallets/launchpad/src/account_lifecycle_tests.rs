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
    VitreusDex::pool_account_for(native(), kind(id))
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
#[ignore = "fails: graduation is deferred when the pool address already holds fifteen other tokens"]
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
#[ignore = "fails: graduation is deferred when the excess recipient has no free consumer slot"]
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
