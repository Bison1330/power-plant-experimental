//! # pallet-launch-treasury
//!
//! Validator-backed treasuries for launchpad tokens (`pallets/LAUNCH_TREASURY_SPEC.md`).
//!
//! A fixed slice of every trade in a launch token — the pool's `treasury_bps`
//! (DEX D9) and the curve's `treasury_share_bps` (launchpad L1) — is pushed
//! to one pallet-owned account, the **vault**. The vault bonds everything it
//! holds as a single cooperator of `energy-generation` and re-cooperates in
//! the same extrinsic as every change to its bond, so bonded and cooperated
//! stake never drift apart (spec §6.2). Each launch's claim on the pooled
//! stake is a share balance; the LNRG the stake earns is attributed to
//! shares with a Synthetix-style accumulator whose only participants are
//! launches, which this pallet alone can change (§2.3). `compound` sells a
//! launch's LNRG to the energy broker for VTRS, buys the launch token on its
//! own venue in impact-capped slices and burns it (§6.4). A launch whose
//! venue has been quiet for `dormancy_blocks` is retired permissionlessly:
//! its shares unbond and, seven days later, the principal is burned into the
//! venue the same way (§2.4).
//!
//! No extrinsic here moves VTRS or LNRG to a caller-chosen account. The only
//! outbound transfers are the broker sale, the venue buy, a bounded keeper
//! bounty to the caller of `compound`, and a sub-ED dust sweep to the
//! protocol recipient when a retired treasury closes (I-T3). Governance can
//! steer where the stake sits and what the operational bounds are; it has no
//! withdraw, redirect or unbond path.
//!
//! This pallet has no `on_initialize`. `Hooks` implements `try_state` and
//! `integrity_test` only (§6.2, option (a)).

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;
pub mod weights;
pub use weights::WeightInfo;

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;
#[cfg(test)]
mod fuzz;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
#[cfg(feature = "runtime-benchmarks")]
pub use benchmarking::BenchmarkHelper;

use frame_support::{
    dispatch::DispatchResult,
    traits::{
        fungibles::{Inspect as FungiblesInspect, Mutate as FungiblesMutate},
        tokens::{
            Fortitude::{Force, Polite},
            Precision::Exact,
            Preservation::{Expendable, Preserve},
        },
        EnsureOrigin, Get,
    },
    BoundedVec, PalletId,
};
use frame_system::pallet_prelude::BlockNumberFor;
use pallet_launchpad::{AssetIdOf, AssetKindOf, BalanceOf, CurveVenue, Phase};
use pallet_vitreus_dex::{PoolManager, TreasurySink, BPS};
use parity_scale_codec::{Decode, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_core::U256;
use sp_runtime::{
    traits::{AccountIdConversion, Convert, Saturating, Zero},
    DispatchError, RuntimeDebug,
};
use sp_std::{vec, vec::Vec};

pub type LaunchId = pallet_launchpad::LaunchId;
pub type EraIndex = u32;

/// Fixed-point scale of [`LnrgPerShare`]: LNRG base units per share, × 1e18.
pub const SCALE: u128 = 1_000_000_000_000_000_000;

/// The staking pallet as this pallet needs it. `energy-generation` has no
/// in-runtime staking trait (spec §3.3, §9.3); the runtime implements this
/// by dispatching its `pub fn` extrinsics with `RawOrigin::Signed(vault)`,
/// and the test mock by a small ledger that enforces the same rules. Every
/// method that changes the ledger returns the pallet's own `DispatchError`
/// unchanged so a caller sees `ReputationTooLow`, `NoMoreChunks`,
/// `InsufficientBond` for what they are.
pub trait TreasuryStaking<AccountId, Balance> {
    /// Whether `stash` has a ledger (`Bonded` contains it).
    fn is_bonded(stash: &AccountId) -> bool;
    /// `ledger.active`, zero without a ledger.
    fn active(stash: &AccountId) -> Balance;
    /// `ledger.total` (active + unlocking), zero without a ledger.
    fn total(stash: &AccountId) -> Balance;
    /// Whether `stash` is currently a cooperator.
    fn is_cooperating(stash: &AccountId) -> bool;
    /// Sum of `stash`'s per-target cooperation stakes, zero if not cooperating.
    fn cooperated(stash: &AccountId) -> Balance;
    /// `MinCooperatorBond`.
    fn min_cooperator_bond() -> Balance;
    fn current_era() -> EraIndex;
    fn bonding_duration() -> EraIndex;
    /// What `cooperate` will check of a target: a validator, `collaborative`,
    /// and of collaborative reputation. Lets the caller filter before an
    /// all-or-nothing call.
    fn is_cooperable(validator: &AccountId) -> bool;
    /// `bond(controller = stash, value, payee = Account(stash))`.
    fn bond(stash: &AccountId, value: Balance) -> DispatchResult;
    fn bond_extra(stash: &AccountId, value: Balance) -> DispatchResult;
    fn cooperate(stash: &AccountId, targets: Vec<(AccountId, Balance)>) -> DispatchResult;
    fn chill(stash: &AccountId) -> DispatchResult;
    fn unbond(stash: &AccountId, value: Balance) -> DispatchResult;
    /// `withdraw_unbonded`; returns the amount that left the ledger.
    fn withdraw_unbonded(stash: &AccountId) -> Result<Balance, DispatchError>;
}

/// The venue the vault sells LNRG on (spec §6.4): the energy broker on
/// chain, a fixed-rate mock in tests. Shaped like [`TreasuryStaking`] —
/// pallet-local, so this crate depends on no runtime trait crate, and a
/// consumer's adapter is a few lines over whatever broker it has. Every
/// method that moves funds returns the venue's own `DispatchError`
/// unchanged.
pub trait TreasuryExchange<AccountId, Balance> {
    /// VTRS the venue would pay right now for exactly `lnrg`, fee
    /// included; `None` if it cannot quote.
    fn quote(lnrg: Balance) -> Option<Balance>;
    /// VTRS the venue can pay out right now (the broker's own reducible
    /// balance). A sale is sized under it (§6.4).
    fn depth() -> Balance;
    /// Sell exactly `lnrg` from `who` for at least `min_native`, delivered
    /// to `who`; returns what arrived. The venue takes the input keeping
    /// `who` alive (R8): a caller must not ask for the account's whole
    /// balance of the asset.
    fn sell(who: &AccountId, lnrg: Balance, min_native: Balance) -> Result<Balance, DispatchError>;
}

/// Operational terms (spec §5.1). Live, except `dormancy_blocks`, which is
/// snapshotted into each treasury when it is first funded. The fee slices
/// themselves live where they are snapshotted: the DEX's `DefaultFeeRouting`
/// (`treasury_bps`) and the launchpad's `Params` (`treasury_share_bps`).
#[derive(Clone, Encode, Decode, Eq, PartialEq, RuntimeDebug, TypeInfo, MaxEncodedLen)]
pub struct TreasuryTerms<Balance, BlockNumber> {
    /// Blocks without a trade on the venue after which `retire` is allowed.
    pub dormancy_blocks: BlockNumber,
    /// `stake` refuses a smaller `pending` (§6.2 spam bound).
    pub min_stake: Balance,
    /// A burn slice may move the venue price by at most this, in bps.
    pub max_burn_impact_bps: u16,
    /// Blocks between two burn slices of one launch.
    pub min_burn_interval: BlockNumber,
    /// Share of the VTRS a `compound` realises that goes to its caller, in bps.
    pub keeper_bounty_bps: u16,
}

#[derive(Clone, Copy, Encode, Decode, Eq, PartialEq, RuntimeDebug, TypeInfo, MaxEncodedLen)]
pub enum TreasuryStatus {
    Active,
    /// Shares redeemed and unbonded; the chunk matures at `chunk_era`.
    Retiring {
        chunk_era: EraIndex,
    },
    /// Principal withdrawn into `pending_burn`; closes when it is burned.
    Retired,
}

/// Per-launch state (spec §5.2).
#[derive(Clone, Encode, Decode, Eq, PartialEq, RuntimeDebug, TypeInfo, MaxEncodedLen)]
pub struct TreasuryRecord<Balance, BlockNumber> {
    /// VTRS received from fees, free in the vault, not yet bonded.
    pub pending: Balance,
    /// Claim on the pooled stake: `shares × active / TotalShares`.
    pub shares: Balance,
    /// `shares × LnrgPerShare / SCALE` at the last checkpoint.
    pub lnrg_debt: u128,
    /// LNRG attributed and not yet sold (survives a dry broker).
    pub lnrg_accrued: Balance,
    /// VTRS realised (yield sold, or principal withdrawn) and not yet burned.
    pub pending_burn: Balance,
    pub last_burn_block: BlockNumber,
    /// Snapshot of `Terms.dormancy_blocks` when this treasury was first funded.
    pub dormancy_blocks: BlockNumber,
    pub status: TreasuryStatus,
}

#[frame_support::pallet]
pub mod pallet {
    use super::*;
    use frame_support::pallet_prelude::*;
    use frame_system::pallet_prelude::*;

    /// 1 on the fork: `migrations::v1` recounted `LnrgAccounted` after R1.
    const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

    #[pallet::pallet]
    #[pallet::storage_version(STORAGE_VERSION)]
    pub struct Pallet<T>(_);

    /// `pallet_launchpad::Config` (and through it the DEX Config) is a
    /// supertrait so the three pallets share `Balance`, `AssetKind`,
    /// `AssetId` and the DEX handle; the launchpad itself is reached as
    /// `pallet_launchpad::Pallet<T>` through [`CurveVenue`].
    #[pallet::config]
    pub trait Config: frame_system::Config + pallet_launchpad::Config {
        type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

        /// `set_terms`, `set_targets`.
        type TreasuryManageOrigin: EnsureOrigin<Self::RuntimeOrigin>;

        /// The staking pallet (spec §6.2).
        type Staking: TreasuryStaking<Self::AccountId, BalanceOf<Self>>;

        /// The energy broker: sells the vault's LNRG for VTRS at the
        /// protocol rate (spec §6.4).
        type Exchange: TreasuryExchange<Self::AccountId, BalanceOf<Self>>;

        /// LNRG as the DEX's asset kind.
        #[pallet::constant]
        type LnrgAsset: Get<AssetKindOf<Self>>;

        /// `AssetKind → AssetId` for `WithId`, `None` for native.
        type AssetIdOf: Convert<AssetKindOf<Self>, Option<AssetIdOf<Self>>>;

        #[pallet::constant]
        type PalletId: Get<PalletId>;

        /// `K`, most validators the vault cooperates with.
        #[pallet::constant]
        type MaxTargets: Get<u32>;

        /// `MaxUnlockingChunks` of the staking pallet; bounds the retiring queue.
        #[pallet::constant]
        type MaxUnlockingChunks: Get<u32>;

        #[pallet::constant]
        type DefaultTerms: Get<TreasuryTerms<BalanceOf<Self>, BlockNumberFor<Self>>>;

        type WeightInfo: WeightInfo;

        /// Staking-side setup the benchmarks cannot reach through
        /// [`TreasuryStaking`]: cooperable validators, the cooperator gate,
        /// the era clock, the exchange rate.
        #[cfg(feature = "runtime-benchmarks")]
        type BenchmarkHelper: BenchmarkHelper<Self::AccountId>;
    }

    // ---- storage (§5) ---------------------------------------------------

    #[pallet::storage]
    pub type Treasuries<T: Config> = StorageMap<
        _,
        Twox64Concat,
        LaunchId,
        TreasuryRecord<BalanceOf<T>, BlockNumberFor<T>>,
        OptionQuery,
    >;

    #[pallet::storage]
    pub type TotalShares<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

    /// Cumulative LNRG per share, × [`SCALE`].
    #[pallet::storage]
    pub type LnrgPerShare<T: Config> = StorageValue<_, u128, ValueQuery>;

    /// LNRG in the vault that the accumulator has already attributed:
    /// `harvest` attributes `balance − this`, and a sale lowers it by what
    /// left (R1: a sale that did not lower it made the next equal amount
    /// of rewards unattributable). `Σ claims ≤ this ≤ balance` always.
    #[pallet::storage]
    pub type LnrgAccounted<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

    #[pallet::type_value]
    pub fn DefaultTermsValue<T: Config>() -> TreasuryTerms<BalanceOf<T>, BlockNumberFor<T>> {
        T::DefaultTerms::get()
    }

    #[pallet::storage]
    pub type Terms<T: Config> = StorageValue<
        _,
        TreasuryTerms<BalanceOf<T>, BlockNumberFor<T>>,
        ValueQuery,
        DefaultTermsValue<T>,
    >;

    #[pallet::storage]
    pub type Targets<T: Config> =
        StorageValue<_, BoundedVec<T::AccountId, T::MaxTargets>, ValueQuery>;

    /// True between a bond change whose re-cooperate failed and the next
    /// successful `retarget` (§6.2). `false` ⇒ cooperated == active (I-T7).
    #[pallet::storage]
    pub type CooperationStale<T: Config> = StorageValue<_, bool, ValueQuery>;

    /// Whether the vault holds its existential deposit above what the
    /// records account for (§4, I-T1). Set by the upgrade that funds the
    /// vault (§7.4) or, on a chain that ships the pallet at genesis and
    /// runs no such upgrade, by the first fee, which withholds the ED
    /// itself (§9.6).
    #[pallet::storage]
    pub type VaultFunded<T: Config> = StorageValue<_, bool, ValueQuery>;

    /// Retiring launches in unbond order: `(launch, chunk era, amount)`.
    /// `withdraw_unbonded` releases every matured chunk at once, so
    /// `finalize_retirement` credits from this record, not from chunk
    /// boundaries (chunks that mature in one era are merged by the staking
    /// pallet).
    #[pallet::storage]
    pub type RetiringQueue<T: Config> = StorageValue<
        _,
        BoundedVec<(LaunchId, EraIndex, BalanceOf<T>), T::MaxUnlockingChunks>,
        ValueQuery,
    >;

    // ---- events / errors (§6.8) -----------------------------------------

    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        /// A fee slice reached the vault for this launch.
        FeeNoted {
            launch_id: LaunchId,
            amount: BalanceOf<T>,
        },
        Staked {
            launch_id: LaunchId,
            amount: BalanceOf<T>,
            shares: BalanceOf<T>,
        },
        Retargeted {
            targets: Vec<(T::AccountId, BalanceOf<T>)>,
        },
        /// A bond change could not be followed by `cooperate`; the bond
        /// stands, the cooperation is stale until a `retarget` succeeds.
        CooperationStale {
            reason: DispatchError,
        },
        Harvested {
            lnrg: BalanceOf<T>,
        },
        Compounded {
            launch_id: LaunchId,
            lnrg_sold: BalanceOf<T>,
            vtrs_realised: BalanceOf<T>,
            bounty: BalanceOf<T>,
            vtrs_burned_in: BalanceOf<T>,
            tokens_burned: BalanceOf<T>,
        },
        Retiring {
            launch_id: LaunchId,
            amount: BalanceOf<T>,
            chunk_era: EraIndex,
        },
        Retired {
            launch_id: LaunchId,
            amount: BalanceOf<T>,
        },
        /// A retired treasury's sub-minimum remainder went to the protocol recipient.
        DustSwept {
            launch_id: LaunchId,
            amount: BalanceOf<T>,
        },
        TermsSet,
        TargetsSet {
            targets: Vec<T::AccountId>,
        },
    }

    #[pallet::error]
    pub enum Error<T> {
        /// No treasury for this launch (no fee has reached it yet).
        NoTreasury,
        /// `pending` is below `Terms.min_stake`.
        BelowMinStake,
        /// The venue traded within `dormancy_blocks`.
        NotDormant,
        NotActive,
        NotRetiring,
        /// The unbond chunk has not matured.
        NotMatured,
        /// Fewer than `min_burn_interval` blocks since this launch's last slice.
        TooSoon,
        /// No target passes the pre-flight filter.
        NoTargets,
        /// `TotalShares > 0` with `active == 0`: every target was slashed to
        /// nothing. Nothing can be staked until the shares retire.
        VaultInsolvent,
        /// The retiring queue is full; retry after a `finalize_retirement`.
        QueueFull,
        /// The call would do nothing.
        NothingToDo,
        TermsOutOfBounds,
        ArithmeticOverflow,
    }

    #[pallet::hooks]
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        #[cfg(feature = "try-runtime")]
        fn try_state(_n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
            Self::do_try_state()
        }

        fn integrity_test() {
            let t = T::DefaultTerms::get();
            assert!(Self::terms_in_bounds(&t), "default treasury terms out of bounds");
        }
    }

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        /// Bond a launch's `pending` into the pooled stake, mint its shares,
        /// and re-cooperate in the same call (§6.2). Permissionless.
        #[pallet::call_index(0)]
        #[pallet::weight(<T as Config>::WeightInfo::stake())]
        pub fn stake(origin: OriginFor<T>, launch_id: LaunchId) -> DispatchResult {
            ensure_signed(origin)?;
            Self::do_stake(launch_id)
        }

        /// Re-submit the vault's cooperation over the current targets. Clears
        /// `CooperationStale` on success. Permissionless.
        #[pallet::call_index(1)]
        #[pallet::weight(<T as Config>::WeightInfo::retarget())]
        pub fn retarget(origin: OriginFor<T>) -> DispatchResult {
            ensure_signed(origin)?;
            Self::do_retarget()?;
            Ok(())
        }

        /// Attribute LNRG that has arrived since the last harvest to shares. Permissionless.
        #[pallet::call_index(2)]
        #[pallet::weight(<T as Config>::WeightInfo::harvest())]
        pub fn harvest(origin: OriginFor<T>) -> DispatchResult {
            ensure_signed(origin)?;
            ensure!(Self::do_harvest()? > Zero::zero(), Error::<T>::NothingToDo);
            Ok(())
        }

        /// Sell what LNRG the launch has accrued for VTRS, pay the caller the
        /// bounty, buy one impact-capped slice of the token and burn it
        /// (§6.4). Either half alone is a valid call. Permissionless.
        #[pallet::call_index(3)]
        #[pallet::weight(<T as Config>::WeightInfo::compound())]
        pub fn compound(origin: OriginFor<T>, launch_id: LaunchId) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::do_compound(&who, launch_id)
        }

        /// Retire a dormant launch: redeem its shares, unbond the principal (§6.5). Permissionless.
        #[pallet::call_index(4)]
        #[pallet::weight(<T as Config>::WeightInfo::retire())]
        pub fn retire(origin: OriginFor<T>, launch_id: LaunchId) -> DispatchResult {
            ensure_signed(origin)?;
            Self::do_retire(launch_id)
        }

        /// Withdraw matured unbond chunks and credit every retiring launch
        /// whose era has passed (§6.6). Permissionless.
        #[pallet::call_index(5)]
        #[pallet::weight(<T as Config>::WeightInfo::finalize_retirement(T::MaxUnlockingChunks::get()))]
        pub fn finalize_retirement(origin: OriginFor<T>, launch_id: LaunchId) -> DispatchResult {
            ensure_signed(origin)?;
            Self::do_finalize(launch_id)
        }

        #[pallet::call_index(6)]
        #[pallet::weight(<T as Config>::WeightInfo::set_terms())]
        pub fn set_terms(
            origin: OriginFor<T>,
            terms: TreasuryTerms<BalanceOf<T>, BlockNumberFor<T>>,
        ) -> DispatchResult {
            T::TreasuryManageOrigin::ensure_origin(origin)?;
            ensure!(Self::terms_in_bounds(&terms), Error::<T>::TermsOutOfBounds);
            Terms::<T>::put(terms);
            Self::deposit_event(Event::TermsSet);
            Ok(())
        }

        /// Set the validators the vault cooperates with and re-cooperate at
        /// once if it has a bond. A failed re-cooperate leaves the targets
        /// set and the cooperation stale, like any other bond change.
        #[pallet::call_index(7)]
        #[pallet::weight(<T as Config>::WeightInfo::set_targets())]
        pub fn set_targets(origin: OriginFor<T>, targets: Vec<T::AccountId>) -> DispatchResult {
            T::TreasuryManageOrigin::ensure_origin(origin)?;
            let bounded: BoundedVec<_, T::MaxTargets> =
                targets.clone().try_into().map_err(|_| Error::<T>::TermsOutOfBounds)?;
            Targets::<T>::put(bounded);
            Self::deposit_event(Event::TargetsSet { targets });
            if T::Staking::is_bonded(&Self::vault()) {
                Self::retarget_after_bond_change();
            }
            Ok(())
        }
    }

    impl<T: Config> Pallet<T> {
        /// The vault: stash, controller and payee in one pallet-owned account.
        pub fn vault() -> T::AccountId {
            <T as Config>::PalletId::get().into_account_truncating()
        }

        fn native() -> AssetKindOf<T> {
            <T as pallet_launchpad::Config>::NativeAssetKind::get()
        }

        pub(crate) fn assets_balance(asset: AssetKindOf<T>, who: &T::AccountId) -> BalanceOf<T> {
            <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<T::AccountId>>::balance(
                asset, who,
            )
        }

        fn protocol_recipient() -> T::AccountId {
            <T as pallet_launchpad::Config>::Treasury::get()
        }

        /// `max_burn_impact_bps` is a ceiling: the slice is also bounded,
        /// where it is sized, strictly under the venue's round-trip fee
        /// (R7). 200 is twice the highest launch pool tier; nothing above
        /// it could ever apply.
        pub fn terms_in_bounds(t: &TreasuryTerms<BalanceOf<T>, BlockNumberFor<T>>) -> bool {
            (10..=200).contains(&t.max_burn_impact_bps)
                && t.keeper_bounty_bps <= 200
                && !t.dormancy_blocks.is_zero()
                && !t.min_stake.is_zero()
        }

        fn launch_of(asset: &AssetKindOf<T>) -> Option<LaunchId> {
            let id = T::AssetIdOf::convert(asset.clone())?;
            <pallet_launchpad::Pallet<T> as CurveVenue<_, _, _, _>>::launch_of_asset(id)
        }

        fn asset_kind_of(launch_id: LaunchId) -> Option<AssetKindOf<T>> {
            let id = <pallet_launchpad::Pallet<T> as CurveVenue<_, _, _, _>>::asset_of(launch_id)?;
            Some(<T as pallet_launchpad::Config>::IntoAssetKind::convert(id))
        }

        fn mul_div(
            a: BalanceOf<T>,
            b: BalanceOf<T>,
            c: BalanceOf<T>,
        ) -> Result<BalanceOf<T>, Error<T>> {
            let (a, b, c): (u128, u128, u128) = (a.into(), b.into(), c.into());
            if c == 0 {
                return Err(Error::<T>::ArithmeticOverflow);
            }
            let r = U256::from(a) * U256::from(b) / U256::from(c);
            u128::try_from(r).map(Into::into).map_err(|_| Error::<T>::ArithmeticOverflow)
        }

        // ---- §6.3 accumulator ------------------------------------------

        /// Attribute LNRG that arrived since the last harvest. Returns it.
        pub fn do_harvest() -> Result<BalanceOf<T>, DispatchError> {
            let balance = Self::assets_balance(T::LnrgAsset::get(), &Self::vault());
            let accounted = LnrgAccounted::<T>::get();
            let delta = balance.saturating_sub(accounted);
            let total = TotalShares::<T>::get();
            if delta.is_zero() || total.is_zero() {
                // With no shares the LNRG waits for the next holder of any.
                return Ok(Zero::zero());
            }
            let d: u128 = delta.into();
            let t: u128 = total.into();
            let inc = U256::from(d) * U256::from(SCALE) / U256::from(t);
            let inc = u128::try_from(inc).map_err(|_| Error::<T>::ArithmeticOverflow)?;
            LnrgPerShare::<T>::mutate(|p| *p = p.saturating_add(inc));
            LnrgAccounted::<T>::put(balance);
            Self::deposit_event(Event::Harvested { lnrg: delta });
            Ok(delta)
        }

        /// `shares × LnrgPerShare / SCALE`.
        fn owed_gross(shares: BalanceOf<T>) -> Result<u128, Error<T>> {
            let s: u128 = shares.into();
            let g = U256::from(s) * U256::from(LnrgPerShare::<T>::get()) / U256::from(SCALE);
            u128::try_from(g).map_err(|_| Error::<T>::ArithmeticOverflow)
        }

        /// Realise the launch's claim into `lnrg_accrued` and re-checkpoint.
        /// Must run before every change to `t.shares`.
        fn settle(t: &mut TreasuryRecord<BalanceOf<T>, BlockNumberFor<T>>) -> Result<(), Error<T>> {
            let gross = Self::owed_gross(t.shares)?;
            let owed = gross.saturating_sub(t.lnrg_debt);
            t.lnrg_accrued = t.lnrg_accrued.saturating_add(owed.into());
            t.lnrg_debt = gross;
            Ok(())
        }

        fn checkpoint(
            t: &mut TreasuryRecord<BalanceOf<T>, BlockNumberFor<T>>,
        ) -> Result<(), Error<T>> {
            t.lnrg_debt = Self::owed_gross(t.shares)?;
            Ok(())
        }

        // ---- §6.2 stake / retarget -------------------------------------

        pub fn do_stake(launch_id: LaunchId) -> DispatchResult {
            let mut t = Treasuries::<T>::get(launch_id).ok_or(Error::<T>::NoTreasury)?;
            ensure!(t.status == TreasuryStatus::Active, Error::<T>::NotActive);
            let p = t.pending;
            ensure!(p >= Terms::<T>::get().min_stake, Error::<T>::BelowMinStake);

            let vault = Self::vault();
            let total_shares = TotalShares::<T>::get();
            let active_before = T::Staking::active(&vault);
            ensure!(total_shares.is_zero() || !active_before.is_zero(), Error::<T>::VaultInsolvent);

            Self::do_harvest()?;
            Self::settle(&mut t)?;

            if T::Staking::is_bonded(&vault) {
                T::Staking::bond_extra(&vault, p)?;
            } else {
                T::Staking::bond(&vault, p)?;
            }
            let added = T::Staking::active(&vault).saturating_sub(active_before);
            ensure!(!added.is_zero(), Error::<T>::NothingToDo);
            t.pending = p.saturating_sub(added);

            let shares = if total_shares.is_zero() {
                added
            } else {
                Self::mul_div(added, total_shares, active_before)?
            };
            t.shares = t.shares.saturating_add(shares);
            TotalShares::<T>::put(total_shares.saturating_add(shares));
            Self::checkpoint(&mut t)?;
            Treasuries::<T>::insert(launch_id, &t);
            Self::deposit_event(Event::Staked { launch_id, amount: added, shares });

            Self::retarget_after_bond_change();
            Ok(())
        }

        /// The re-cooperate that follows every bond change. Failure is
        /// recorded, not returned: the bond stands (§6.2).
        fn retarget_after_bond_change() {
            match Self::do_retarget() {
                Ok(_) => {},
                Err(reason) => {
                    CooperationStale::<T>::put(true);
                    Self::deposit_event(Event::CooperationStale { reason });
                },
            }
        }

        /// Filter targets, split `active` equally, `cooperate`. Returns the
        /// targets submitted.
        #[allow(clippy::type_complexity)]
        pub fn do_retarget() -> Result<Vec<(T::AccountId, BalanceOf<T>)>, DispatchError> {
            let vault = Self::vault();
            let active = T::Staking::active(&vault);
            ensure!(active >= T::Staking::min_cooperator_bond(), Error::<T>::NothingToDo);
            let survivors: Vec<T::AccountId> =
                Targets::<T>::get().into_iter().filter(T::Staking::is_cooperable).collect();
            ensure!(!survivors.is_empty(), Error::<T>::NoTargets);
            let n: u128 = survivors.len() as u128;
            let a: u128 = active.into();
            let each = a / n;
            let first_extra = a - each * n;
            let targets: Vec<(T::AccountId, BalanceOf<T>)> = survivors
                .into_iter()
                .enumerate()
                .map(|(i, v)| (v, if i == 0 { (each + first_extra).into() } else { each.into() }))
                .collect();
            T::Staking::cooperate(&vault, targets.clone())?;
            CooperationStale::<T>::put(false);
            Self::deposit_event(Event::Retargeted { targets: targets.clone() });
            Ok(targets)
        }

        // ---- §6.4 compound ---------------------------------------------

        /// Largest `x ≤ want` whose VTRS quote the broker can pay.
        fn sellable(want: BalanceOf<T>) -> Option<(BalanceOf<T>, BalanceOf<T>)> {
            let lnrg = T::LnrgAsset::get();
            let depth = T::Exchange::depth();
            // R8: the broker takes the input with `keep_alive`, so the vault
            // can part with its reducible LNRG and no more — balance minus
            // the asset's min balance. A claim that equals the whole balance
            // (the accumulator attributes without a remainder every so
            // often) must not ask for the whole balance.
            let can_part_with = <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<
                T::AccountId,
            >>::reducible_balance(
                lnrg.clone(), &Self::vault(), Preserve, Polite
            );
            let want = want.min(can_part_with);
            if depth.is_zero() || want.is_zero() {
                return None;
            }
            let quote = T::Exchange::quote;
            let mut x = want;
            let mut q = quote(x)?;
            // The broker's rate is linear, so one proportional step lands
            // inside the depth; a couple of 1 % shaves cover rounding.
            for _ in 0..3 {
                if q <= depth {
                    return (!q.is_zero()).then_some((x, q));
                }
                x = Self::mul_div(x, depth, q).ok()?;
                x = x.saturating_sub(x / 100u32.into());
                if x.is_zero() {
                    return None;
                }
                q = quote(x)?;
            }
            (q <= depth && !q.is_zero()).then_some((x, q))
        }

        pub fn do_compound(caller: &T::AccountId, launch_id: LaunchId) -> DispatchResult {
            let mut t = Treasuries::<T>::get(launch_id).ok_or(Error::<T>::NoTreasury)?;
            let vault = Self::vault();
            let terms = Terms::<T>::get();
            let now = frame_system::Pallet::<T>::block_number();
            let mut did_something = false;

            Self::do_harvest()?;
            Self::settle(&mut t)?;

            // Sell.
            let (mut lnrg_sold, mut realised, mut bounty) =
                (BalanceOf::<T>::zero(), BalanceOf::<T>::zero(), BalanceOf::<T>::zero());
            if let Some((x, q)) = Self::sellable(t.lnrg_accrued) {
                let min_out = q.saturating_sub(q / 1_000u32.into());
                let out = T::Exchange::sell(&vault, x, min_out)?;
                t.lnrg_accrued = t.lnrg_accrued.saturating_sub(x);
                // What left the vault was attributed LNRG: the accumulator's
                // baseline follows it down, so the next rewards are `balance −
                // accounted` again and not `balance − (accounted + x)`.
                LnrgAccounted::<T>::mutate(|a| *a = a.saturating_sub(x));
                lnrg_sold = x;
                realised = out;
                bounty = Self::mul_div(
                    out,
                    (terms.keeper_bounty_bps as u128).into(),
                    (BPS as u128).into(),
                )?;
                let ed = <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<
                    T::AccountId,
                >>::minimum_balance(Self::native());
                if bounty >= ed {
                    <<T as pallet_vitreus_dex::Config>::Assets as FungiblesMutate<T::AccountId>>::transfer(
                        Self::native(),
                        &vault,
                        caller,
                        bounty,
                        Preserve,
                    )?;
                } else {
                    bounty = Zero::zero();
                }
                t.pending_burn = t.pending_burn.saturating_add(out.saturating_sub(bounty));
                did_something = true;
            }

            // Burn one slice. Two things about the venue call are read back
            // rather than assumed: the buy routes this launch's own treasury
            // slice straight into `pending` through `note_fee` (in storage,
            // underneath this copy), and a curve buy that crosses the target
            // is a partial fill (`do_buy` takes `quote_used`, not the offer).
            // So what was spent is the vault's balance delta corrected for
            // what arrived, and `pending` is re-read.
            let (mut burned_in, mut tokens_burned) =
                (BalanceOf::<T>::zero(), BalanceOf::<T>::zero());
            if !t.pending_burn.is_zero()
                && now.saturating_sub(t.last_burn_block) >= terms.min_burn_interval
            {
                let before = Self::assets_balance(Self::native(), &vault);
                let pending_before = t.pending;
                if let Some(tokens) =
                    Self::burn_slice(&vault, launch_id, t.pending_burn, terms.max_burn_impact_bps)?
                {
                    t.pending = Treasuries::<T>::get(launch_id)
                        .map(|f| f.pending)
                        .unwrap_or(pending_before);
                    let arrived = t.pending.saturating_sub(pending_before);
                    let after = Self::assets_balance(Self::native(), &vault);
                    let spent = before.saturating_add(arrived).saturating_sub(after);
                    t.pending_burn = t.pending_burn.saturating_sub(spent);
                    t.last_burn_block = now;
                    burned_in = spent;
                    tokens_burned = tokens;
                    did_something = true;
                    // The slice pays its caller as the sale does — the same
                    // rate on what was burned, from `pending_burn`, above
                    // ED or not at all. A retired launch's principal goes
                    // out over many slices with nothing to sell, and the
                    // keeper that runs them is paid for each (§6.4).
                    let slice_bounty = Self::mul_div(
                        spent,
                        (terms.keeper_bounty_bps as u128).into(),
                        (BPS as u128).into(),
                    )?;
                    let ed = <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<
                        T::AccountId,
                    >>::minimum_balance(Self::native());
                    if slice_bounty >= ed && slice_bounty <= t.pending_burn {
                        <<T as pallet_vitreus_dex::Config>::Assets as FungiblesMutate<
                            T::AccountId,
                        >>::transfer(
                            Self::native(), &vault, caller, slice_bounty, Preserve
                        )?;
                        t.pending_burn = t.pending_burn.saturating_sub(slice_bounty);
                        bounty = bounty.saturating_add(slice_bounty);
                    }
                }
            }

            // A retired treasury closes once what is left cannot be quoted:
            // the remainder goes to the protocol recipient and the record
            // stays, as `Retired` with nothing in it. It is never removed —
            // a launch with no record is one that was never funded, and
            // `account_for` would fund it again on the next trade (R3).
            if t.status == TreasuryStatus::Retired && t.shares.is_zero() && t.lnrg_accrued.is_zero()
            {
                let ed = <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<
                    T::AccountId,
                >>::minimum_balance(Self::native());
                if !t.pending_burn.is_zero() && t.pending_burn < ed && t.pending.is_zero() {
                    <<T as pallet_vitreus_dex::Config>::Assets as FungiblesMutate<T::AccountId>>::transfer(
                        Self::native(),
                        &vault,
                        &Self::protocol_recipient(),
                        t.pending_burn,
                        Preserve,
                    )?;
                    Self::deposit_event(Event::DustSwept { launch_id, amount: t.pending_burn });
                    t.pending_burn = Zero::zero();
                    Treasuries::<T>::insert(launch_id, &t);
                    return Ok(());
                }
            }

            ensure!(did_something, Error::<T>::NothingToDo);
            Treasuries::<T>::insert(launch_id, &t);
            Self::deposit_event(Event::Compounded {
                launch_id,
                lnrg_sold,
                vtrs_realised: realised,
                bounty,
                vtrs_burned_in: burned_in,
                tokens_burned,
            });
            Ok(())
        }

        /// Buy on the launch's venue with `min(pending, cap)` VTRS and burn
        /// what came back; returns the tokens burned. `cap` is the amount
        /// whose constant-product impact is `impact_bps`:
        /// `reserve_quote × impact / (2 × BPS)`. `None` when there is no
        /// venue to buy on (a `Complete` curve waiting for its seed) or the
        /// cap rounds to nothing. The caller measures what was spent.
        fn burn_slice(
            vault: &T::AccountId,
            launch_id: LaunchId,
            pending: BalanceOf<T>,
            impact_bps: u16,
        ) -> Result<Option<BalanceOf<T>>, DispatchError> {
            type Venue<T> = pallet_launchpad::Pallet<T>;
            let asset = Self::asset_kind_of(launch_id).ok_or(Error::<T>::NoTreasury)?;
            let phase = <Venue<T> as CurveVenue<_, _, _, _>>::phase(launch_id)
                .ok_or(Error::<T>::NoTreasury)?;
            let quote_reserve = match phase {
                Phase::Graduated => {
                    <T as pallet_launchpad::Config>::Dex::native_reserves(asset.clone())
                        .map(|(q, _)| q)
                },
                Phase::Trading => <Venue<T> as CurveVenue<_, _, _, _>>::virtual_reserves(launch_id)
                    .map(|(q, _)| q),
                Phase::Complete => None,
            };
            let Some(quote_reserve) = quote_reserve else { return Ok(None) };
            // R7: a slice whose price impact reaches the venue's round-trip
            // fee is worth bracketing — the bracket pays the fee twice, the
            // slice moves the price once, and with no `min_out` the slice
            // takes whatever price the bracket set. The term is a ceiling;
            // the venue's own fee is the bound, strictly under twice it.
            let venue_fee_bps: u16 = match phase {
                Phase::Graduated => <T as pallet_launchpad::Config>::Dex::fee_bps(asset.clone()),
                _ => <Venue<T> as CurveVenue<_, _, _, _>>::fee_bps(launch_id),
            }
            .unwrap_or(0);
            let impact_bps = impact_bps.min(
                (venue_fee_bps as u32).saturating_mul(2).saturating_sub(1).min(u16::MAX as u32)
                    as u16,
            );
            if impact_bps == 0 {
                return Ok(None);
            }
            let cap = Self::mul_div(
                quote_reserve,
                (impact_bps as u128).into(),
                (2 * BPS as u128).into(),
            )?;
            let y = pending.min(cap);
            if y.is_zero() {
                return Ok(None);
            }
            let bought = match phase {
                Phase::Graduated => <T as pallet_launchpad::Config>::Dex::swap_for(
                    vault,
                    Self::native(),
                    asset.clone(),
                    y,
                    Zero::zero(),
                ),
                _ => {
                    <Venue<T> as CurveVenue<_, _, _, _>>::buy_for(vault, launch_id, y, Zero::zero())
                },
            };
            // A slice the venue cannot quote (dust under the curve's fee
            // rounding) is nothing to burn this call, not an error that
            // reverts the sale beside it (R4). The venue refuses before it
            // moves anything, so there is nothing to undo.
            let tokens = match bought {
                Ok(t) => t,
                Err(e)
                    if e == DispatchError::from(pallet_launchpad::Error::<T>::Unquotable)
                        || e == DispatchError::from(pallet_launchpad::Error::<T>::ZeroAmount)
                        || e == DispatchError::from(pallet_vitreus_dex::Error::<T>::ZeroAmount) =>
                {
                    return Ok(None)
                },
                Err(e) => return Err(e),
            };
            if !tokens.is_zero() {
                <<T as pallet_vitreus_dex::Config>::Assets as FungiblesMutate<T::AccountId>>::burn_from(
                    asset, vault, tokens, Expendable, Exact, Force,
                )?;
            }
            Ok(Some(tokens))
        }

        // ---- §6.5 / §6.6 retire ----------------------------------------

        /// The block the launch's venue last traded: the curve's record,
        /// or the later of it and the pool's once graduated (a pool that
        /// never traded reads as block 0, which must not count).
        pub fn last_trade_block(launch_id: LaunchId) -> Option<BlockNumberFor<T>> {
            type Venue<T> = pallet_launchpad::Pallet<T>;
            let curve = <Venue<T> as CurveVenue<_, _, _, _>>::last_trade_block(launch_id)?;
            let pool = Self::asset_kind_of(launch_id)
                .and_then(<T as pallet_launchpad::Config>::Dex::last_swap_block)
                .unwrap_or_default();
            Some(if pool > curve { pool } else { curve })
        }

        pub fn do_retire(launch_id: LaunchId) -> DispatchResult {
            let mut t = Treasuries::<T>::get(launch_id).ok_or(Error::<T>::NoTreasury)?;
            ensure!(t.status == TreasuryStatus::Active, Error::<T>::NotActive);
            let now = frame_system::Pallet::<T>::block_number();
            let last = Self::last_trade_block(launch_id).ok_or(Error::<T>::NoTreasury)?;
            ensure!(now.saturating_sub(last) >= t.dormancy_blocks, Error::<T>::NotDormant);

            let vault = Self::vault();
            Self::do_harvest()?;
            Self::settle(&mut t)?;

            let total = TotalShares::<T>::get();
            let active = T::Staking::active(&vault);
            let v = if t.shares.is_zero() || total.is_zero() {
                Zero::zero()
            } else {
                Self::mul_div(t.shares, active, total)?
            };
            // Unstaked fees retire with the rest.
            t.pending_burn = t.pending_burn.saturating_add(t.pending);
            t.pending = Zero::zero();

            if v.is_zero() {
                t.status = TreasuryStatus::Retired;
                Self::deposit_event(Event::Retired { launch_id, amount: Zero::zero() });
            } else {
                let mut queue = RetiringQueue::<T>::get();
                ensure!(queue.len() < T::MaxUnlockingChunks::get() as usize, Error::<T>::QueueFull);
                let remaining = active.saturating_sub(v);
                if remaining < T::Staking::min_cooperator_bond()
                    && T::Staking::is_cooperating(&vault)
                {
                    T::Staking::chill(&vault)?;
                }
                T::Staking::unbond(&vault, v)?;
                let chunk_era =
                    T::Staking::current_era().saturating_add(T::Staking::bonding_duration());
                queue.try_push((launch_id, chunk_era, v)).map_err(|_| Error::<T>::QueueFull)?;
                RetiringQueue::<T>::put(queue);
                t.status = TreasuryStatus::Retiring { chunk_era };
                Self::deposit_event(Event::Retiring { launch_id, amount: v, chunk_era });
            }
            TotalShares::<T>::put(total.saturating_sub(t.shares));
            t.shares = Zero::zero();
            t.lnrg_debt = 0;
            Treasuries::<T>::insert(launch_id, &t);

            if T::Staking::is_cooperating(&vault) {
                Self::retarget_after_bond_change();
            }
            Ok(())
        }

        pub fn do_finalize(launch_id: LaunchId) -> DispatchResult {
            let t = Treasuries::<T>::get(launch_id).ok_or(Error::<T>::NoTreasury)?;
            let TreasuryStatus::Retiring { chunk_era } = t.status else {
                return Err(Error::<T>::NotRetiring.into());
            };
            let era = T::Staking::current_era();
            ensure!(era >= chunk_era, Error::<T>::NotMatured);

            let vault = Self::vault();
            let withdrawn = T::Staking::withdraw_unbonded(&vault)?;

            // Every matured entry is credited, pro rata to what actually
            // came back (a slash during unbonding reduces the chunks too).
            let queue = RetiringQueue::<T>::get();
            let (matured, waiting): (Vec<_>, Vec<_>) =
                queue.into_iter().partition(|(_, e, _)| *e <= era);
            let expected =
                matured.iter().fold(BalanceOf::<T>::zero(), |a, (_, _, v)| a.saturating_add(*v));
            ensure!(!expected.is_zero(), Error::<T>::NothingToDo);
            let mut credited_total = BalanceOf::<T>::zero();
            let n = matured.len();
            for (i, (id, _, v)) in matured.into_iter().enumerate() {
                let credit = if i + 1 == n {
                    withdrawn.saturating_sub(credited_total)
                } else {
                    Self::mul_div(v, withdrawn, expected)?
                };
                credited_total = credited_total.saturating_add(credit);
                Treasuries::<T>::mutate(id, |maybe| {
                    if let Some(tr) = maybe {
                        tr.pending_burn = tr.pending_burn.saturating_add(credit);
                        tr.status = TreasuryStatus::Retired;
                    }
                });
                Self::deposit_event(Event::Retired { launch_id: id, amount: credit });
            }
            let waiting: BoundedVec<_, T::MaxUnlockingChunks> =
                waiting.try_into().map_err(|_| Error::<T>::QueueFull)?;
            RetiringQueue::<T>::put(waiting);
            Ok(())
        }

        // ---- views -------------------------------------------------------

        /// LNRG the launch could realise right now: what it has accrued plus
        /// its unsettled share of everything harvested since its checkpoint.
        /// Does not include LNRG that has arrived and not yet been harvested.
        pub fn claimable_lnrg(launch_id: LaunchId) -> Option<BalanceOf<T>> {
            let t = Treasuries::<T>::get(launch_id)?;
            let gross = Self::owed_gross(t.shares).ok()?;
            Some(t.lnrg_accrued.saturating_add(gross.saturating_sub(t.lnrg_debt).into()))
        }

        /// The launch's claim on the pooled stake in VTRS at the current
        /// share price, `shares × active / TotalShares`.
        pub fn staked_value(launch_id: LaunchId) -> Option<BalanceOf<T>> {
            let t = Treasuries::<T>::get(launch_id)?;
            let total = TotalShares::<T>::get();
            if t.shares.is_zero() || total.is_zero() {
                return Some(Zero::zero());
            }
            Self::mul_div(t.shares, T::Staking::active(&Self::vault()), total).ok()
        }

        /// The names of every dispatchable, for I-T3's "no exit path" assertion.
        pub fn call_names() -> Vec<&'static str> {
            <Call<T> as frame_support::traits::GetCallName>::get_call_names().to_vec()
        }

        // ---- try_state (I-T1, I-T2, I-T7) -----------------------------

        #[cfg(any(feature = "try-runtime", test))]
        pub fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
            let vault = Self::vault();
            let mut pending = BalanceOf::<T>::zero();
            let mut pending_burn = BalanceOf::<T>::zero();
            let mut shares = BalanceOf::<T>::zero();
            for (_, t) in Treasuries::<T>::iter() {
                pending = pending.saturating_add(t.pending);
                pending_burn = pending_burn.saturating_add(t.pending_burn);
                shares = shares.saturating_add(t.shares);
            }
            frame_support::ensure!(shares == TotalShares::<T>::get(), "I-T2: shares");
            // I-T2, the LNRG half: every launch's claim is backed by attributed
            // LNRG, and attributed LNRG is in the vault. A sale that lowered the
            // balance without lowering `LnrgAccounted` breaks the right-hand side.
            let mut claims = BalanceOf::<T>::zero();
            for (id, _) in Treasuries::<T>::iter() {
                claims = claims.saturating_add(Self::claimable_lnrg(id).unwrap_or_default());
            }
            let accounted = LnrgAccounted::<T>::get();
            let lnrg = Self::assets_balance(T::LnrgAsset::get(), &vault);
            frame_support::ensure!(claims <= accounted, "I-T2: claims exceed LnrgAccounted");
            frame_support::ensure!(
                accounted <= lnrg,
                "I-T2: LnrgAccounted exceeds the vault's LNRG"
            );
            // The ED buffer is there once the upgrade or the first fee put it
            // there (§9.6); before that the vault holds nothing unaccounted.
            let ed = if VaultFunded::<T>::get() {
                <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<T::AccountId>>::minimum_balance(Self::native())
            } else {
                Zero::zero()
            };
            let held = Self::assets_balance(Self::native(), &vault);
            let expected = ed
                .saturating_add(pending)
                .saturating_add(pending_burn)
                .saturating_add(T::Staking::total(&vault));
            // A floor, not an equality: anyone can send the vault VTRS, and
            // nothing accounts for it or moves it. What the invariant guards
            // is that the records never claim more than the vault holds (R5).
            frame_support::ensure!(
                held >= expected,
                "I-T1: vault VTRS < ED + pending + pending_burn + ledger.total"
            );
            if !CooperationStale::<T>::get() && T::Staking::is_cooperating(&vault) {
                // Exact after this pallet's own retarget; a slash in between
                // scales each target down with a floor (energy-generation's
                // `adjust_cooperator_targets`), one unit per target at most.
                let (c, a) = (T::Staking::cooperated(&vault), T::Staking::active(&vault));
                let k: BalanceOf<T> = (T::MaxTargets::get() as u128).into();
                frame_support::ensure!(
                    c <= a && a.saturating_sub(c) <= k,
                    "I-T7: cooperated != active"
                );
            }
            Ok(())
        }
    }

    // ---- the sink the DEX and the launchpad push into (§6.1) -------------

    impl<T: Config> TreasurySink<AssetKindOf<T>, T::AccountId, BalanceOf<T>> for Pallet<T> {
        fn account_for(asset: &AssetKindOf<T>) -> Option<T::AccountId> {
            let launch_id = Self::launch_of(asset)?;
            match Treasuries::<T>::get(launch_id) {
                // Not yet funded: the first fee creates it (see `note_fee`).
                None => Some(Self::vault()),
                Some(t) if t.status == TreasuryStatus::Active => Some(Self::vault()),
                Some(_) => None,
            }
        }

        fn note_fee(asset: &AssetKindOf<T>, amount: BalanceOf<T>) {
            let Some(launch_id) = Self::launch_of(asset) else { return };
            let now = frame_system::Pallet::<T>::block_number();
            // No upgrade funded the vault: this fee created it (the
            // transfer needed `amount ≥ ED` to), and its ED is the buffer
            // §4 assumes and I-T1 counts. Withhold it once, here; it is
            // never `pending` and outlives every launch (§9.6).
            let amount = if VaultFunded::<T>::get() {
                amount
            } else {
                VaultFunded::<T>::put(true);
                let ed = <<T as pallet_vitreus_dex::Config>::Assets as FungiblesInspect<
                    T::AccountId,
                >>::minimum_balance(Self::native());
                amount.saturating_sub(ed)
            };
            Treasuries::<T>::mutate(launch_id, |maybe| {
                let t = maybe.get_or_insert_with(|| TreasuryRecord {
                    pending: Zero::zero(),
                    shares: Zero::zero(),
                    lnrg_debt: 0,
                    lnrg_accrued: Zero::zero(),
                    pending_burn: Zero::zero(),
                    last_burn_block: now,
                    dormancy_blocks: Terms::<T>::get().dormancy_blocks,
                    status: TreasuryStatus::Active,
                });
                if t.status == TreasuryStatus::Active {
                    t.pending = t.pending.saturating_add(amount);
                }
            });
            Self::deposit_event(Event::FeeNoted { launch_id, amount });
        }
    }
}

/// Fork-only migrations (LAUNCH_TREASURY_SPEC §10.12): what a running chain
/// needs and the submission must not carry.
pub mod migrations {
    use super::*;
    use frame_support::{
        migrations::VersionedMigration, traits::UncheckedOnRuntimeUpgrade, weights::Weight,
    };
    use sp_std::marker::PhantomData;

    /// v0 → v1 (R1): recount `LnrgAccounted`. Under v0 a sale did not lower
    /// it, so it overstated the attributed LNRG in the vault by everything
    /// ever sold, and `harvest` attributed nothing until new rewards had
    /// covered that gap — those rewards were owned by no launch. Every
    /// launch's own claim (`lnrg_accrued` plus its unsettled share of the
    /// accumulator) was tracked correctly throughout, so their sum is the
    /// right value: attributed LNRG still in the vault, up to accumulator
    /// dust. Set it to that; the next `harvest` attributes every stranded
    /// era to the launches holding shares then. Nothing moves; one key.
    pub mod v1 {
        use super::*;

        pub struct VersionUncheckedMigrateToV1<T>(PhantomData<T>);
        impl<T: Config> UncheckedOnRuntimeUpgrade for VersionUncheckedMigrateToV1<T> {
            fn on_runtime_upgrade() -> Weight {
                let before = LnrgAccounted::<T>::get();
                let mut claims = BalanceOf::<T>::zero();
                let mut n = 0u64;
                for (id, _) in Treasuries::<T>::iter() {
                    claims =
                        claims.saturating_add(Pallet::<T>::claimable_lnrg(id).unwrap_or_default());
                    n += 1;
                }
                let held = Pallet::<T>::assets_balance(T::LnrgAsset::get(), &Pallet::<T>::vault());
                let after = claims.min(held);
                LnrgAccounted::<T>::put(after);
                log::info!(
                    target: "runtime::launch-treasury",
                    "R1 recount: LnrgAccounted {:?} -> {:?} over {} treasuries (vault holds {:?}); {:?} of stranded rewards become attributable at the next harvest",
                    before, after, n, held, held.saturating_sub(after),
                );
                T::DbWeight::get().reads_writes(n.saturating_add(3), 1)
            }

            #[cfg(feature = "try-runtime")]
            fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
                let held = Pallet::<T>::assets_balance(T::LnrgAsset::get(), &Pallet::<T>::vault());
                log::info!(target: "runtime::launch-treasury", "R1 pre_upgrade: LnrgAccounted {:?}, vault LNRG {:?}, {} treasuries", LnrgAccounted::<T>::get(), held, Treasuries::<T>::iter().count());
                Ok(held.encode())
            }

            #[cfg(feature = "try-runtime")]
            fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
                let held_before: BalanceOf<T> =
                    Decode::decode(&mut &state[..]).map_err(|_| "decode")?;
                let held = Pallet::<T>::assets_balance(T::LnrgAsset::get(), &Pallet::<T>::vault());
                frame_support::ensure!(
                    held == held_before,
                    "R1 post_upgrade: the vault's LNRG moved"
                );
                let accounted = LnrgAccounted::<T>::get();
                frame_support::ensure!(
                    accounted <= held,
                    "R1 post_upgrade: LnrgAccounted above the vault's LNRG"
                );
                let mut claims = BalanceOf::<T>::zero();
                for (id, _) in Treasuries::<T>::iter() {
                    claims =
                        claims.saturating_add(Pallet::<T>::claimable_lnrg(id).unwrap_or_default());
                }
                frame_support::ensure!(
                    claims <= accounted,
                    "R1 post_upgrade: claims above LnrgAccounted"
                );
                log::info!(target: "runtime::launch-treasury", "R1 post_upgrade: LnrgAccounted {:?}, claims {:?}, vault LNRG {:?}", accounted, claims, held);
                Ok(())
            }
        }

        pub type MigrateToV1<T> = VersionedMigration<
            0,
            1,
            VersionUncheckedMigrateToV1<T>,
            Pallet<T>,
            <T as frame_system::Config>::DbWeight,
        >;
    }
}
