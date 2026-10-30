//! Buy-and-burn when the vault is full: every asset an account holds through
//! a consumer reference takes one of its 16, and anyone can open such
//! balances on the vault's public address.

use super::*;
use crate::mock::*;
use frame_support::assert_ok;
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

#[test]
#[ignore = "fails: compound is refused when the vault, its consumer slots full, has never held the launch token"]
fn compound_burns_launch_token_vault_has_never_held_with_its_slots_full() {
    new_test_ext().execute_with(|| {
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

        System::set_block_number(System::block_number() + BURN_INTERVAL);
        assert_ok!(LaunchTreasury::compound(RuntimeOrigin::signed(KEEPER), id));
    });
}
