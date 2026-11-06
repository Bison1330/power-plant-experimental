//! Buy-and-burn when the vault is full: every asset an account holds through
//! a consumer reference takes one of its 16, and anyone can open such
//! balances on the vault's public address.

use super::*;
use crate::mock::*;
use frame_support::{assert_ok, storage::with_storage_layer};
use pallet_launchpad::{Curves, Launches};

fn bv(s: &[u8]) -> frame_support::BoundedVec<u8, frame_support::traits::ConstU32<50>> {
    s.to_vec().try_into().unwrap()
}
fn kind(id: LaunchId) -> NativeOrAssetId {
    NativeOrAssetId::WithId(Launches::<Test>::get(id).unwrap().asset_id)
}

/// ALICE creates one-unit balances of fresh non-sufficient assets on `who`
/// until pallet-assets would refuse to open another account there.
fn fill_consumer_slots(who: &Acc) {
    let mut id = 1_000u128;
    while frame_system::Pallet::<Test>::can_accrue_consumers(who, 2) {
        while <Assets as frame_support::traits::fungibles::Inspect<Acc>>::asset_exists(id) {
            id += 1;
        }
        assert_ok!(Assets::force_create(RuntimeOrigin::root(), id.into(), ALICE, false, 1));
        assert_ok!(Assets::mint(RuntimeOrigin::signed(ALICE), id.into(), *who, 1));
    }
}

/// A graduated launch with rewards to sell and the vault's slots full, so
/// its first burn opens the token's account in the vault on the caller's
/// deposit.
fn rewards_on_a_full_vault() -> LaunchId {
    let id = pallet_launchpad::NextLaunchId::<Test>::get();
    assert_ok!(Launchpad::create_launch(
        RuntimeOrigin::signed(ALICE),
        bv(b"Meme"),
        bv(b"MEME"),
        None,
        0,
        0,
        None,
        None
    ));
    assert_ok!(Launchpad::buy(RuntimeOrigin::signed(BOB), id, 500_000_000 * UNIT, 0));
    assert_eq!(Curves::<Test>::get(id).unwrap().phase, Phase::Graduated);
    for _ in 0..10 {
        assert_ok!(VitreusDex::swap_exact_tokens_for_tokens(
            RuntimeOrigin::signed(CHARLIE),
            NativeOrAssetId::Native,
            kind(id),
            100 * UNIT,
            0,
            CHARLIE
        ));
    }
    assert_ok!(LaunchTreasury::stake(RuntimeOrigin::signed(KEEPER), id));
    pay_rewards(50 * UNIT);
    fill_consumer_slots(&LaunchTreasury::vault());
    id
}

#[test]
fn compound_burns_launch_token_vault_has_never_held_with_its_slots_full() {
    new_test_ext().execute_with(|| {
        let id = rewards_on_a_full_vault();
        System::set_block_number(System::block_number() + BURN_INTERVAL);
        assert_ok!(LaunchTreasury::compound(RuntimeOrigin::signed(KEEPER), id));
    });
}

/// The deposit is the caller's to pay. A sale pays its bounty first, so this
/// is a burn with nothing to sell: a caller who cannot pay the deposit and
/// keep an ED is refused by name with nothing moved, and anyone who can
/// still burns.
#[test]
fn caller_short_of_the_vault_deposit_is_refused_by_name() {
    new_test_ext().execute_with(|| {
        let id = rewards_on_a_full_vault();
        assert_ok!(LaunchTreasury::compound(RuntimeOrigin::signed(KEEPER), id));
        let held = Treasuries::<Test>::get(id).unwrap();
        assert!(held.pending_burn > 0);
        assert_eq!(Balances::reserved_balance(KEEPER), 0);

        System::set_block_number(System::block_number() + BURN_INTERVAL);
        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), KEEPER, ED + 9));
        assert_eq!(
            with_storage_layer(|| LaunchTreasury::compound(RuntimeOrigin::signed(KEEPER), id)),
            Err(pallet_vitreus_dex::Error::<Test>::RecipientCannotReceive.into())
        );
        assert_eq!(Treasuries::<Test>::get(id).unwrap().pending_burn, held.pending_burn);
        assert_eq!(Balances::free_balance(KEEPER), ED + 9);

        assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), KEEPER, 100 * UNIT));
        assert_ok!(LaunchTreasury::compound(RuntimeOrigin::signed(KEEPER), id));
        assert!(Treasuries::<Test>::get(id).unwrap().pending_burn < held.pending_burn);
        assert_eq!(Balances::reserved_balance(KEEPER), 10);
    });
}
