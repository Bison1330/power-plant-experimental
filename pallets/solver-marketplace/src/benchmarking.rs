//! Benchmarks for pallet-solver-marketplace.
//!
//! Assets are created through `fungibles::Create`, the force-create path that
//! does not consult `pallet_assets::CreateOrigin`, so these run unchanged
//! under `mainnet-runtime` (where `CreateOrigin = EnsureNever`). Non-native
//! assets come from [`BenchmarkHelper::asset_kind`].
//!
//! `settle_intent` is measured through the DEX the runtime binds as
//! `Config::Dex`, on a pool [`BenchmarkHelper::create_pool`] sets up there:
//! its weight is the swap's as the real DEX runs it, plus the marketplace's
//! own work. A change to the DEX's swap therefore needs this pallet
//! re-benchmarked too.
//!
//! Where an extrinsic has a cheaper and a dearer branch, the dearer one is
//! set up here.

use super::*;
use crate::{settlement::IntentStatus, Pallet as Marketplace};
use frame_benchmarking::v2::*;
use frame_support::traits::{fungibles::Create, EnsureOrigin, Get};
use frame_system::RawOrigin;
use sp_runtime::traits::{One, UniqueSaturatedFrom};

/// What the benchmarks need set up outside this pallet. Bound in `Config`
/// under `runtime-benchmarks` only.
pub trait BenchmarkHelper<AssetKind, AccountId, Balance> {
    /// A non-native asset kind for `seed`. Distinct seeds must yield
    /// distinct assets.
    fn asset_kind(seed: u32) -> AssetKind;

    /// Create a pool of `asset` against native on the DEX bound as
    /// `Config::Dex`, with `liquidity` of each side from `provider`, who
    /// holds both.
    fn create_pool(provider: &AccountId, asset: AssetKind, liquidity: Balance);
}

const ASSET_A: u32 = 100;

/// 10^24 — comfortably above any runtime's solver bond (≈10^21) and ED.
fn big<T: Config>() -> T::Balance {
    T::Balance::unique_saturated_from(1_000_000_000_000_000_000_000_000u128)
}
/// 10^21 per side of liquidity.
fn liq<T: Config>() -> T::Balance {
    T::Balance::unique_saturated_from(1_000_000_000_000_000_000_000u128)
}
/// 10^18 per trade.
fn trade<T: Config>() -> T::Balance {
    T::Balance::unique_saturated_from(1_000_000_000_000_000_000u128)
}

fn native<T: Config>() -> T::AssetKind {
    T::NativeAsset::get()
}

fn fund_native<T: Config>(who: &T::AccountId) {
    T::Assets::mint_into(native::<T>(), who, big::<T>()).expect("mint native");
}

/// Create asset `seed` (sufficient, min balance 1) and give every holder `big()`.
fn setup_asset<T: Config>(seed: u32, holders: &[&T::AccountId]) -> T::AssetKind
where
    T::Assets: Create<T::AccountId>,
{
    let asset = T::BenchmarkHelper::asset_kind(seed);
    T::Assets::create(asset.clone(), holders[0].clone(), true, One::one()).expect("create asset");
    for h in holders {
        T::Assets::mint_into(asset.clone(), h, big::<T>()).expect("mint asset");
    }
    asset
}

/// Pool (native, ASSET_A) with `liq()` on each side from `provider`.
fn setup_pool<T: Config>(provider: &T::AccountId) -> T::AssetKind
where
    T::Assets: Create<T::AccountId>,
{
    fund_native::<T>(provider);
    let asset = setup_asset::<T>(ASSET_A, &[provider]);
    T::BenchmarkHelper::create_pool(provider, asset.clone(), liq::<T>());
    asset
}

fn manage_origin<T: Config>() -> Result<T::RuntimeOrigin, BenchmarkError> {
    T::ManageOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)
}

fn register<T: Config>(who: &T::AccountId) -> u64 {
    fund_native::<T>(who);
    let id = NextSolverId::<T>::get();
    Marketplace::<T>::register_solver(RawOrigin::Signed(who.clone()).into()).expect("register");
    id
}

/// Intent from `user`: `trade()` of `asset` for native, `min_out` = 1,
/// deadline far in the future. Returns the intent id.
fn submit<T: Config>(user: &T::AccountId, asset: T::AssetKind) -> u64 {
    let id = NextIntentId::<T>::get();
    let deadline = frame_system::Pallet::<T>::block_number() + 1_000u32.into();
    Marketplace::<T>::submit_intent(
        RawOrigin::Signed(user.clone()).into(),
        asset,
        native::<T>(),
        trade::<T>(),
        One::one(),
        deadline,
    )
    .expect("submit intent");
    id
}

fn advance_blocks<T: Config>(n: BlockNumberFor<T>) {
    let now = frame_system::Pallet::<T>::block_number();
    frame_system::Pallet::<T>::set_block_number(now + n);
}

#[benchmarks(where T::Assets: Create<T::AccountId>)]
mod benchmarks {
    use super::*;

    /// Dearer branch: the caller has a prior, inactive registration.
    #[benchmark]
    fn register_solver() {
        let caller: T::AccountId = whitelisted_caller();
        register::<T>(&caller);
        Marketplace::<T>::deregister_solver(RawOrigin::Signed(caller.clone()).into())
            .expect("deregister");
        let next = NextSolverId::<T>::get();

        #[extrinsic_call]
        _(RawOrigin::Signed(caller.clone()));

        assert_eq!(SolverAccountToId::<T>::get(&caller), Some(next));
        assert!(Solvers::<T>::get(next).expect("solver").active);
    }

    #[benchmark]
    fn deregister_solver() {
        let caller: T::AccountId = whitelisted_caller();
        let id = register::<T>(&caller);

        #[extrinsic_call]
        _(RawOrigin::Signed(caller.clone()));

        assert!(!Solvers::<T>::get(id).expect("solver").active);
    }

    #[benchmark]
    fn submit_intent() {
        let caller: T::AccountId = whitelisted_caller();
        fund_native::<T>(&caller);
        let asset = setup_asset::<T>(ASSET_A, &[&caller]);
        let id = NextIntentId::<T>::get();
        let deadline = frame_system::Pallet::<T>::block_number() + 1_000u32.into();

        #[extrinsic_call]
        _(
            RawOrigin::Signed(caller.clone()),
            asset,
            native::<T>(),
            trade::<T>(),
            One::one(),
            deadline,
        );

        assert_eq!(Intents::<T>::get(id).expect("intent").status, IntentStatus::Open);
    }

    #[benchmark]
    fn cancel_intent() {
        let caller: T::AccountId = whitelisted_caller();
        fund_native::<T>(&caller);
        let asset = setup_asset::<T>(ASSET_A, &[&caller]);
        let id = submit::<T>(&caller, asset);

        #[extrinsic_call]
        _(RawOrigin::Signed(caller.clone()), id);

        assert_eq!(Intents::<T>::get(id).expect("intent").status, IntentStatus::Cancelled);
    }

    /// Dearer branch: a prior commitment from another solver is displaced.
    #[benchmark]
    fn commit_fill() {
        let user: T::AccountId = account("user", 0, 0);
        fund_native::<T>(&user);
        let asset = setup_asset::<T>(ASSET_A, &[&user]);
        let id = submit::<T>(&user, asset);

        let rival: T::AccountId = account("rival", 0, 0);
        register::<T>(&rival);
        Marketplace::<T>::commit_fill(RawOrigin::Signed(rival).into(), id, One::one())
            .expect("rival commit");

        let caller: T::AccountId = whitelisted_caller();
        let solver_id = register::<T>(&caller);
        let bid: T::Balance = 2u32.into();

        #[extrinsic_call]
        _(RawOrigin::Signed(caller.clone()), id, bid);

        let c = FillCommitments::<T>::get(id).expect("commitment");
        assert_eq!(c.solver_id, solver_id);
        assert_eq!(c.committed_amount_out, bid);
    }

    /// Dearer branch: non-zero profit, so user, treasury and solver are all paid.
    #[benchmark]
    fn settle_intent() {
        let provider: T::AccountId = account("provider", 0, 0);
        let asset = setup_pool::<T>(&provider);

        let user: T::AccountId = account("user", 0, 0);
        fund_native::<T>(&user);
        T::Assets::mint_into(asset.clone(), &user, big::<T>()).expect("mint");
        let id = submit::<T>(&user, asset);

        let caller: T::AccountId = whitelisted_caller();
        let solver_id = register::<T>(&caller);
        Marketplace::<T>::commit_fill(RawOrigin::Signed(caller.clone()).into(), id, One::one())
            .expect("commit");
        let before = T::Assets::balance(native::<T>(), &caller);

        #[extrinsic_call]
        _(RawOrigin::Signed(caller.clone()), id);

        assert_eq!(Intents::<T>::get(id).expect("intent").status, IntentStatus::Settled);
        assert!(T::Assets::balance(native::<T>(), &caller) > before);
        assert_eq!(Solvers::<T>::get(solver_id).expect("solver").fills_completed, 1);
    }

    /// Dearer branch: bond split pays both treasury and slasher, and the user is refunded.
    #[benchmark]
    fn slash_solver() {
        let user: T::AccountId = account("user", 0, 0);
        fund_native::<T>(&user);
        let asset = setup_asset::<T>(ASSET_A, &[&user]);
        let id = submit::<T>(&user, asset);

        let solver: T::AccountId = account("solver", 0, 0);
        let solver_id = register::<T>(&solver);
        Marketplace::<T>::commit_fill(RawOrigin::Signed(solver).into(), id, One::one())
            .expect("commit");
        advance_blocks::<T>(Marketplace::<T>::current_settlement_window() + One::one());

        let caller: T::AccountId = whitelisted_caller();
        fund_native::<T>(&caller);
        let before = T::Assets::balance(native::<T>(), &caller);

        #[extrinsic_call]
        _(RawOrigin::Signed(caller.clone()), id);

        assert_eq!(Intents::<T>::get(id).expect("intent").status, IntentStatus::Expired);
        assert!(!Solvers::<T>::get(solver_id).expect("solver").active);
        assert!(T::Assets::balance(native::<T>(), &caller) > before);
    }

    #[benchmark]
    fn refund_expired_intent() {
        let caller: T::AccountId = whitelisted_caller();
        fund_native::<T>(&caller);
        let asset = setup_asset::<T>(ASSET_A, &[&caller]);
        let id = submit::<T>(&caller, asset);
        advance_blocks::<T>(1_000u32.into());

        #[extrinsic_call]
        _(RawOrigin::Signed(caller.clone()), id);

        assert_eq!(Intents::<T>::get(id).expect("intent").status, IntentStatus::Expired);
        assert!(IntentEscrowBalances::<T>::get(id).is_none());
    }

    #[benchmark]
    fn set_bid_window() -> Result<(), BenchmarkError> {
        let origin = manage_origin::<T>()?;
        let v: BlockNumberFor<T> = 42u32.into();

        #[extrinsic_call]
        _(origin as T::RuntimeOrigin, v);

        assert_eq!(BidWindowBlocks::<T>::get(), Some(v));
        Ok(())
    }

    #[benchmark]
    fn set_settlement_window() -> Result<(), BenchmarkError> {
        let origin = manage_origin::<T>()?;
        let v: BlockNumberFor<T> = 42u32.into();

        #[extrinsic_call]
        _(origin as T::RuntimeOrigin, v);

        assert_eq!(SettlementWindowBlocks::<T>::get(), Some(v));
        Ok(())
    }

    #[benchmark]
    fn set_solver_bond_amount() -> Result<(), BenchmarkError> {
        let origin = manage_origin::<T>()?;
        let v = trade::<T>();

        #[extrinsic_call]
        _(origin as T::RuntimeOrigin, v);

        assert_eq!(SolverBondAmount::<T>::get(), Some(v));
        Ok(())
    }

    impl_benchmark_test_suite!(Marketplace, crate::mock::new_test_ext(), crate::mock::Test);
}
