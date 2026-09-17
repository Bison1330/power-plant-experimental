//! Benchmarks for pallet-launch-treasury (spec §7.4, §10.8).
//!
//! Every call is measured at the branch that does the most: `stake` on a
//! vault that is already bonded (`bond_extra`, not `bond`) with LNRG waiting
//! to be harvested and `MaxTargets` validators to re-cooperate with;
//! `compound` selling into a broker whose depth is below the quote (so the
//! fit loop runs its shaves), paying a bounty above the existential deposit
//! and burning a slice on the pool venue; `retire` leaving enough stake
//! behind that the vault re-cooperates rather than chills;
//! `finalize_retirement` crediting `n` launches whose chunks matured in one
//! era. `n` is the only linear component: everything else is bounded by the
//! runtime's constants and measured at the bound.
//!
//! What the staking pallet needs of a validator before the vault may
//! cooperate with it, how eras advance, and what the exchange needs before
//! it quotes, is behind [`BenchmarkHelper`]: the mock answers from its
//! registries, the runtime from `energy-generation`, `pallet-reputation`
//! and `pallet-dynamic-energy`.

#![cfg(feature = "runtime-benchmarks")]

use super::*;
use crate::Pallet as LaunchTreasury;
use frame_benchmarking::v2::*;
use frame_support::traits::EnsureOrigin;
use frame_system::RawOrigin;
use pallet_launchpad::{Curves, Launches, NextLaunchId, Params, Phase};

/// The staking-side setup the benchmarks cannot do through [`TreasuryStaking`].
pub trait BenchmarkHelper<AccountId> {
    /// The `i`-th validator the vault may cooperate with: exists, is a
    /// validator, `collaborative`, and passes `is_cooperable`.
    fn cooperable_validator(i: u32) -> AccountId;
    /// Whatever `cooperate` requires of the cooperator itself (spec §2.2).
    fn clear_cooperator_gate(vault: &AccountId);
    /// Make `current_era()` return `era`.
    fn set_current_era(era: EraIndex);
    /// Whatever the exchange needs before it can quote `LNRG → VTRS` (on
    /// chain the rate is set by the first session change).
    fn prepare_exchange();
    /// Make `TreasuryExchange::depth()` report at least `native` (on chain:
    /// fund the broker's account).
    fn set_exchange_depth(native: u128);
}

fn target<T: Config>() -> u128 {
    Params::<T>::get().graduation_target.into()
}

/// Enough for a creation fee, a crossing buy and change.
fn rich<T: Config>() -> BalanceOf<T> {
    let fee: u128 = Params::<T>::get().creation_fee.into();
    (target::<T>().saturating_mul(10).saturating_add(fee)).into()
}

fn set_native<T: Config>(who: &T::AccountId, amount: BalanceOf<T>) {
    <<T as pallet_vitreus_dex::Config>::Assets as FungiblesMutate<T::AccountId>>::set_balance(
        <T as pallet_launchpad::Config>::NativeAssetKind::get(),
        who,
        amount,
    );
}

fn funded<T: Config>(name: &'static str, i: u32) -> T::AccountId {
    let who: T::AccountId = account(name, i, 0);
    set_native::<T>(&who, rich::<T>());
    who
}

fn manage_origin<T: Config>() -> Result<T::RuntimeOrigin, BenchmarkError> {
    T::TreasuryManageOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)
}

/// A launch that crosses on its initial buy: graduated, pool seeded, and
/// its treasury funded with the curve's slice of the crossing fee (above
/// `min_stake` for any terms the runtime ships).
fn graduated_launch<T: Config>(i: u32) -> LaunchId {
    let creator = funded::<T>("creator", i);
    let id = NextLaunchId::<T>::get();
    let crossing: BalanceOf<T> = target::<T>().saturating_mul(2).into();
    pallet_launchpad::Pallet::<T>::create_launch(
        RawOrigin::Signed(creator).into(),
        sp_std::vec![b'N'].try_into().expect("1 <= StringLimit"),
        sp_std::vec![b'S'].try_into().expect("1 <= StringLimit"),
        None,
        crossing,
        Zero::zero(),
        None,
        None,
    )
    .expect("create launch");
    assert_eq!(
        Curves::<T>::get(id).expect("curve").phase,
        Phase::Graduated,
        "crossing buy seeds the pool"
    );
    assert!(
        Treasuries::<T>::get(id).expect("funded by the crossing fee").pending
            >= Terms::<T>::get().min_stake
    );
    id
}

fn setup_targets<T: Config>(k: u32) {
    let targets: Vec<T::AccountId> =
        (0..k).map(<T as Config>::BenchmarkHelper::cooperable_validator).collect();
    LaunchTreasury::<T>::set_targets(manage_origin::<T>().expect("origin"), targets)
        .expect("set targets");
    <T as Config>::BenchmarkHelper::clear_cooperator_gate(&LaunchTreasury::<T>::vault());
}

fn mint_lnrg<T: Config>(amount: BalanceOf<T>) {
    <<T as pallet_vitreus_dex::Config>::Assets as FungiblesMutate<T::AccountId>>::mint_into(
        T::LnrgAsset::get(),
        &LaunchTreasury::<T>::vault(),
        amount,
    )
    .expect("mint LNRG");
}

fn stake_for<T: Config>(id: LaunchId) {
    LaunchTreasury::<T>::stake(RawOrigin::Signed(account("staker", 0, 0)).into(), id)
        .expect("stake");
    assert!(!CooperationStale::<T>::get(), "setup cooperated");
}

/// Past the launch's dormancy window.
fn make_dormant<T: Config>(id: LaunchId) {
    let last = LaunchTreasury::<T>::last_trade_block(id).expect("venue");
    let dormancy = Treasuries::<T>::get(id).expect("treasury").dormancy_blocks;
    frame_system::Pallet::<T>::set_block_number(last + dormancy + 1u32.into());
}

fn lnrg_units<T: Config>(n: u128) -> BalanceOf<T> {
    (n * 1_000_000_000_000_000_000).into()
}

#[benchmarks]
mod benchmarks {
    use super::*;

    /// `bond_extra` + harvest with a delta + `cooperate(MaxTargets)`.
    #[benchmark]
    fn stake() {
        setup_targets::<T>(T::MaxTargets::get());
        let first = graduated_launch::<T>(0);
        stake_for::<T>(first);
        let id = graduated_launch::<T>(1);
        mint_lnrg::<T>(lnrg_units::<T>(10));
        let caller: T::AccountId = whitelisted_caller();
        let shares_before = TotalShares::<T>::get();

        #[extrinsic_call]
        _(RawOrigin::Signed(caller), id);

        assert!(TotalShares::<T>::get() > shares_before);
        assert_eq!(Treasuries::<T>::get(id).expect("treasury").pending, Zero::zero());
        assert!(!CooperationStale::<T>::get(), "re-cooperated in the same call");
        assert_eq!(
            T::Staking::cooperated(&LaunchTreasury::<T>::vault()),
            T::Staking::active(&LaunchTreasury::<T>::vault())
        );
    }

    /// `cooperate(MaxTargets)` over a bonded vault.
    #[benchmark]
    fn retarget() {
        setup_targets::<T>(T::MaxTargets::get());
        let id = graduated_launch::<T>(0);
        stake_for::<T>(id);
        CooperationStale::<T>::put(true);
        let caller: T::AccountId = whitelisted_caller();

        #[extrinsic_call]
        _(RawOrigin::Signed(caller));

        assert!(!CooperationStale::<T>::get());
    }

    #[benchmark]
    fn harvest() {
        setup_targets::<T>(1);
        let id = graduated_launch::<T>(0);
        stake_for::<T>(id);
        mint_lnrg::<T>(lnrg_units::<T>(10));
        let caller: T::AccountId = whitelisted_caller();

        #[extrinsic_call]
        _(RawOrigin::Signed(caller));

        assert!(LnrgPerShare::<T>::get() > 0);
        assert_eq!(LnrgAccounted::<T>::get(), lnrg_units::<T>(10));
    }

    /// Sell into a broker shallower than the quote (the fit loop shaves),
    /// pay a bounty above ED, burn one slice on the pool.
    #[benchmark]
    fn compound() {
        setup_targets::<T>(1);
        let id = graduated_launch::<T>(0);
        stake_for::<T>(id);
        mint_lnrg::<T>(lnrg_units::<T>(1_000));
        <T as Config>::BenchmarkHelper::prepare_exchange();
        let ed = <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<T::AccountId>>::minimum_balance(
            <T as pallet_launchpad::Config>::NativeAssetKind::get(),
        );
        // Depth: a tenth of the graduation target.
        let depth: BalanceOf<T> = (target::<T>() / 10).into();
        <T as Config>::BenchmarkHelper::set_exchange_depth(target::<T>() / 10);
        let quote = T::Exchange::quote(lnrg_units::<T>(1_000)).expect("quote");
        assert!(quote > depth, "the broker must be the binding constraint");
        frame_system::Pallet::<T>::set_block_number(
            frame_system::Pallet::<T>::block_number() + Terms::<T>::get().min_burn_interval,
        );
        let caller: T::AccountId = whitelisted_caller();
        set_native::<T>(&caller, ed);
        let supply_before = <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<
            T::AccountId,
        >>::total_issuance(
            <T as pallet_launchpad::Config>::IntoAssetKind::convert(
                Launches::<T>::get(id).expect("launch").asset_id,
            ),
        );

        #[extrinsic_call]
        _(RawOrigin::Signed(caller.clone()), id);

        let t = Treasuries::<T>::get(id).expect("treasury");
        assert!(
            t.lnrg_accrued > Zero::zero() && t.lnrg_accrued < lnrg_units::<T>(1_000),
            "partial fill"
        );
        assert!(t.pending_burn > Zero::zero(), "one slice of many");
        let supply_after = <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<
            T::AccountId,
        >>::total_issuance(
            <T as pallet_launchpad::Config>::IntoAssetKind::convert(
                Launches::<T>::get(id).expect("launch").asset_id,
            ),
        );
        assert!(supply_after < supply_before, "tokens burned");
        assert!(
            <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<T::AccountId>>::balance(
                <T as pallet_launchpad::Config>::NativeAssetKind::get(),
                &caller
            ) > ed,
            "bounty paid"
        );
    }

    /// `unbond` with enough stake left that the vault re-cooperates
    /// (`cooperate(MaxTargets)`) instead of chilling.
    #[benchmark]
    fn retire() {
        setup_targets::<T>(T::MaxTargets::get());
        let keep = graduated_launch::<T>(0);
        stake_for::<T>(keep);
        let id = graduated_launch::<T>(1);
        stake_for::<T>(id);
        mint_lnrg::<T>(lnrg_units::<T>(10));
        make_dormant::<T>(id);
        let caller: T::AccountId = whitelisted_caller();
        let active_before = T::Staking::active(&LaunchTreasury::<T>::vault());

        #[extrinsic_call]
        _(RawOrigin::Signed(caller), id);

        let t = Treasuries::<T>::get(id).expect("treasury");
        assert!(matches!(t.status, TreasuryStatus::Retiring { .. }));
        assert_eq!(t.shares, Zero::zero());
        assert!(T::Staking::active(&LaunchTreasury::<T>::vault()) < active_before);
        assert!(
            T::Staking::is_cooperating(&LaunchTreasury::<T>::vault()),
            "still cooperating on the rest"
        );
        assert!(!CooperationStale::<T>::get());
    }

    /// `withdraw_unbonded` then credit `n` matured launches from the queue.
    #[benchmark]
    fn finalize_retirement(n: Linear<1, { T::MaxUnlockingChunks::get() }>) {
        setup_targets::<T>(1);
        let ids: Vec<LaunchId> = (0..n).map(graduated_launch::<T>).collect();
        for id in &ids {
            stake_for::<T>(*id);
        }
        make_dormant::<T>(ids[0]);
        for id in &ids {
            LaunchTreasury::<T>::retire(RawOrigin::Signed(account("keeper", 0, 0)).into(), *id)
                .expect("retire");
        }
        assert_eq!(RetiringQueue::<T>::get().len() as u32, n);
        let TreasuryStatus::Retiring { chunk_era } =
            Treasuries::<T>::get(ids[0]).expect("treasury").status
        else {
            panic!("retiring")
        };
        <T as Config>::BenchmarkHelper::set_current_era(chunk_era);
        let caller: T::AccountId = whitelisted_caller();

        #[extrinsic_call]
        _(RawOrigin::Signed(caller), ids[0]);

        assert!(RetiringQueue::<T>::get().is_empty());
        for id in &ids {
            let t = Treasuries::<T>::get(*id).expect("treasury");
            assert_eq!(t.status, TreasuryStatus::Retired);
            assert!(t.pending_burn > Zero::zero());
        }
        assert_eq!(T::Staking::total(&LaunchTreasury::<T>::vault()), Zero::zero());
    }

    #[benchmark]
    fn set_terms() -> Result<(), BenchmarkError> {
        let mut terms = Terms::<T>::get();
        terms.keeper_bounty_bps = 10;

        #[extrinsic_call]
        _(manage_origin::<T>()? as T::RuntimeOrigin, terms.clone());

        assert_eq!(Terms::<T>::get(), terms);
        Ok(())
    }

    /// The write plus the re-cooperate a bonded vault performs at once.
    #[benchmark]
    fn set_targets() -> Result<(), BenchmarkError> {
        setup_targets::<T>(1);
        let id = graduated_launch::<T>(0);
        stake_for::<T>(id);
        let targets: Vec<T::AccountId> = (0..T::MaxTargets::get())
            .map(<T as Config>::BenchmarkHelper::cooperable_validator)
            .collect();

        #[extrinsic_call]
        _(manage_origin::<T>()? as T::RuntimeOrigin, targets.clone());

        assert_eq!(Targets::<T>::get().into_inner(), targets);
        assert!(!CooperationStale::<T>::get());
        assert_eq!(
            T::Staking::cooperated(&LaunchTreasury::<T>::vault()),
            T::Staking::active(&LaunchTreasury::<T>::vault())
        );
        Ok(())
    }

    impl_benchmark_test_suite!(LaunchTreasury, crate::mock::new_test_ext(), crate::mock::Test);
}
