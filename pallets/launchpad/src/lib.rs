//! # pallet-launchpad
//!
//! Bonding-curve token launchpad graduating into VitreusDEX. Implements
//! `pallets/LAUNCHPAD_SPEC.md`; section numbers in comments refer to it.
//!
//! Each launch mints a fixed-supply `pallet_assets` token into a pallet-owned
//! escrow sub-account, sells `Sellable` of it along a constant-product curve
//! quoted in the native asset (VTRS), and on sell-out seeds a permanently
//! locked DEX pool with the raised quote and the remaining `Reserved` tokens
//! through `ReservedPoolSeeder`. No extrinsic — creator, governance, or the
//! pallet's own — moves curve or pool funds anywhere else.
//!
//! Escrow accounting is *tracked* in storage (`real_quote`,
//! `tokens_remaining`); balances are never read back from the escrow account
//! for pricing, so donations to it are inert (FM-04).

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
pub mod curve;
pub mod weights;

pub use weights::WeightInfo;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

use frame_support::{
    dispatch::DispatchResult,
    traits::{
        fungible::{self, Inspect as FungibleInspect, Mutate as FungibleMutate},
        fungibles::{
            metadata::Mutate as MetadataMutate, Create, Inspect as FungiblesInspect,
            Mutate as FungiblesMutate,
        },
        tokens::{
            Fortitude::Polite,
            Preservation::{Expendable, Preserve},
        },
        EnsureOrigin, Get,
    },
    BoundedVec, CloneNoBound, EqNoBound, PalletId, PartialEqNoBound, RuntimeDebugNoBound,
};
use frame_system::pallet_prelude::BlockNumberFor;
use pallet_vitreus_dex::{
    PoolManager, ReservedPoolSeeder, TreasurySink, MINIMUM_LIQUIDITY, MIN_LAUNCH_FEE_TIER,
};
use parity_scale_codec::{Decode, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_core::U256;
use sp_runtime::{
    traits::{
        AccountIdConversion, AtLeast32BitUnsigned, Bounded, CheckedSub, Convert, Hash, One,
        Saturating, UniqueSaturatedFrom, Zero,
    },
    DispatchError, RuntimeDebug,
};
use sp_std::vec::Vec;

/// Identifier of a launch. Monotone, never reused (I12).
pub type LaunchId = u64;

/// Quote/token balance type — the DEX's, so amounts never need converting at the seam.
pub type BalanceOf<T> = <T as pallet_vitreus_dex::Config>::Balance;
/// DEX asset-kind type (`NativeOrWithId<AssetId>` in the runtime).
pub type AssetKindOf<T> = <T as pallet_vitreus_dex::Config>::AssetKind;
pub type AssetIdOf<T> = <T as Config>::AssetId;

/// Basis-point denominator.
pub const BPS: u16 = 10_000;

/// Decimals every launch token is registered with.
pub const TOKEN_DECIMALS: u8 = 18;

/// FM-17: how far `create_launch` walks past squatted asset ids before it gives
/// up. A squatter must hold this many contiguous deposits ahead of the cursor to
/// force even a temporary failure, and the cursor walks past them as soon as they
/// stop; the bound only keeps one create's scan finite.
pub const MAX_ASSET_ID_SCAN: u32 = 256;

/// Governance parameters (§1.1). Live; snapshotted into each launch except
/// `creation_fee`, which is charged once at creation.
#[derive(Clone, Encode, Decode, Eq, PartialEq, RuntimeDebug, TypeInfo, MaxEncodedLen)]
pub struct LaunchParams<Balance> {
    /// Graduation target `T` in quote base units.
    pub graduation_target: Balance,
    /// Curve trading fee on the quote leg, both directions, in bps.
    pub curve_fee_bps: u16,
    /// Share of the curve fee that goes to the protocol; what neither this nor
    /// `treasury_share_bps` takes accrues to the creator.
    pub protocol_share_bps: u16,
    /// L1 (LAUNCH_TREASURY_SPEC §7.2): share of the curve fee pushed to the
    /// launch's treasury sink, or to the protocol when the runtime binds none.
    pub treasury_share_bps: u16,
    /// DEX fee tier for the graduated pool (tenths of a percent); `3 | 10`.
    pub pool_fee_tier: u32,
    /// One-off creation fee, paid to treasury after funding escrow deposits.
    pub creation_fee: Balance,
}

/// Everything that determines pricing and fee routing for one launch (§1.2).
#[derive(Clone, Encode, Decode, Eq, PartialEq, RuntimeDebug, TypeInfo, MaxEncodedLen)]
pub struct CurveParams<Balance> {
    pub graduation_target: Balance,
    /// `V_q = T / 3`.
    pub virtual_quote: Balance,
    pub curve_fee_bps: u16,
    pub protocol_share_bps: u16,
    /// L1: snapshotted like every other term.
    pub treasury_share_bps: u16,
    pub pool_fee_tier: u32,
}

#[derive(Clone, Copy, Encode, Decode, Eq, PartialEq, RuntimeDebug, TypeInfo, MaxEncodedLen)]
pub enum Phase {
    Trading,
    Complete,
    Graduated,
}

/// Cold, write-once launch record (§1.2). `creator_fee_recipient` is the only
/// mutable field.
#[derive(Clone, Encode, Decode, Eq, PartialEq, RuntimeDebug, TypeInfo, MaxEncodedLen)]
#[scale_info(skip_type_params(T))]
pub struct Launch<T: Config> {
    pub asset_id: AssetIdOf<T>,
    pub creator: T::AccountId,
    pub creator_fee_recipient: T::AccountId,
    pub escrow: T::AccountId,
    pub created_at: BlockNumberFor<T>,
    pub curve: CurveParams<BalanceOf<T>>,
    pub params_hash: T::Hash,
}

/// Off-curve presentation data for a launch (§1.2). Cold: read on a page
/// view, never on a trade, which is why it is stored apart from [`Launch`].
/// Every field is a bounded byte string the chain stores and returns
/// verbatim — nothing here is validated or verified on chain (see §2.9): the
/// image is expected to be a URI the frontend resolves, not image bytes.
#[derive(
    CloneNoBound,
    Encode,
    Decode,
    EqNoBound,
    PartialEqNoBound,
    RuntimeDebugNoBound,
    TypeInfo,
    MaxEncodedLen,
)]
#[scale_info(skip_type_params(UriLimit, DescriptionLimit))]
pub struct LaunchMetadata<UriLimit: Get<u32>, DescriptionLimit: Get<u32>> {
    /// Image URI (https://…, ipfs://…, data:…). Resolved and sandboxed by the frontend.
    pub image: BoundedVec<u8, UriLimit>,
    /// Free text.
    pub description: BoundedVec<u8, DescriptionLimit>,
    pub website: BoundedVec<u8, UriLimit>,
    pub twitter: BoundedVec<u8, UriLimit>,
    pub telegram: BoundedVec<u8, UriLimit>,
}

impl<UriLimit: Get<u32>, DescriptionLimit: Get<u32>> LaunchMetadata<UriLimit, DescriptionLimit> {
    /// `(description length, longest URI length)` — the two dimensions the
    /// weight of writing this record varies with.
    pub fn dims(&self) -> (u32, u32) {
        let u = [&self.image, &self.website, &self.twitter, &self.telegram]
            .into_iter()
            .map(|b| b.len())
            .max()
            .unwrap_or(0);
        (self.description.len() as u32, u as u32)
    }
}

/// [`LaunchMetadata`] with this pallet's bounds.
pub type LaunchMetadataOf<T> =
    LaunchMetadata<<T as Config>::UriLimit, <T as Config>::DescriptionLimit>;

/// Hot per-launch curve state (§1.3).
#[derive(Clone, Encode, Decode, Eq, PartialEq, RuntimeDebug, TypeInfo, MaxEncodedLen)]
#[scale_info(skip_type_params(T))]
pub struct CurveState<T: Config> {
    pub phase: Phase,
    /// Quote held for the curve, excluding fees. Tracked, never read from balances.
    pub real_quote: BalanceOf<T>,
    /// Sellable tokens still in escrow. `Sellable − tokens_remaining` = sold.
    pub tokens_remaining: BalanceOf<T>,
    pub creator_fees_unclaimed: BalanceOf<T>,
    pub protocol_fees_paid: BalanceOf<T>,
    /// L1: curve fee pushed to the launch treasury so far.
    pub treasury_fees_paid: BalanceOf<T>,
    /// L1: the last block a buy or sell ran on this curve (`created_at`
    /// until the first), for the treasury's dormancy rule.
    pub last_trade_block: BlockNumberFor<T>,
    pub completed_at: Option<BlockNumberFor<T>>,
    pub graduated_at: Option<BlockNumberFor<T>>,
    pub lp_shares: BalanceOf<T>,
}

/// L2 (LAUNCH_TREASURY_SPEC §7.2): the curve as a venue for a pallet that
/// owns the buyer — the launch treasury's buy-and-burn before graduation.
/// Read-only accessors plus `buy_for`, which is `do_buy` for an in-runtime
/// caller: the same path a user's `buy` takes, anti-snipe hook included, so a
/// treasury's buys are ordinary buys and can graduate the launch.
pub trait CurveVenue<AccountId, AssetId, Balance, BlockNumber> {
    fn launch_of_asset(asset: AssetId) -> Option<LaunchId>;
    fn asset_of(launch_id: LaunchId) -> Option<AssetId>;
    fn phase(launch_id: LaunchId) -> Option<Phase>;
    /// `created_at` until the first trade, then the last buy or sell block.
    fn last_trade_block(launch_id: LaunchId) -> Option<BlockNumber>;
    /// The curve's virtual reserves `(quote, token)` — what its price is
    /// quoted on — while it is `Trading`; `None` otherwise.
    fn virtual_reserves(launch_id: LaunchId) -> Option<(Balance, Balance)>;
    /// The curve's trading fee in bps, for a caller sizing a trade against
    /// the round-trip cost of bracketing it (R7); `None` for no launch.
    fn fee_bps(launch_id: LaunchId) -> Option<u16>;
    /// Buy with exactly `quote_in` from `who`, who receives the tokens.
    /// Returns the tokens received.
    fn buy_for(
        who: &AccountId,
        launch_id: LaunchId,
        quote_in: Balance,
        min_tokens_out: Balance,
    ) -> Result<Balance, DispatchError>;
}

/// Anti-snipe hook (§2.7). v1 binds `()`. Every buy — the `buy` extrinsic, the
/// initial buy inside `create_launch`, any future precompile — reaches
/// `do_buy` and therefore this hook: the single choke point FM-05 requires.
pub trait OnCurveBuy<AccountId, Balance, BlockNumber> {
    /// Returns `(amount that reaches the curve, extra amount routed to the treasury)`.
    /// Block numbers only — never timestamps.
    fn on_buy(
        launch_id: LaunchId,
        launch_created_at: BlockNumber,
        now: BlockNumber,
        who: &AccountId,
        is_creator: bool,
        quote_in: Balance,
    ) -> Result<(Balance, Balance), DispatchError>;
}

impl<A, B: Zero, N> OnCurveBuy<A, B, N> for () {
    fn on_buy(
        _: LaunchId,
        _: N,
        _: N,
        _: &A,
        _: bool,
        quote_in: B,
    ) -> Result<(B, B), DispatchError> {
        Ok((quote_in, B::zero()))
    }
}

#[frame_support::pallet]
pub mod pallet {
    use super::*;
    use frame_support::pallet_prelude::*;
    use frame_system::pallet_prelude::*;

    /// v1 (L1): `treasury_share_bps` on the params, `treasury_fees_paid` and
    /// `last_trade_block` on the curve state. No chain the submission targets
    /// has a v0 launch, so there is no migration here; the fork carries its own.
    const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

    #[pallet::pallet]
    #[pallet::storage_version(STORAGE_VERSION)]
    pub struct Pallet<T>(_);

    /// Bound on `pallet_vitreus_dex::Config` so DEX errors can be matched by
    /// type (the rescue path maps `SlippageExceeded` → `PriceOutOfTolerance`);
    /// `Dex` stays trait-typed so the launchpad only ever calls the
    /// `PoolManager` / `ReservedPoolSeeder` surface.
    #[pallet::config]
    pub trait Config:
        frame_system::Config + pallet_vitreus_dex::Config<Balance: From<u128> + Into<u128>>
    {
        type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

        /// Governance origin for `set_params`, `set_creation_paused`, rescue.
        /// Named `LaunchManageOrigin` because the DEX Config has its own `ManageOrigin`.
        type LaunchManageOrigin: EnsureOrigin<Self::RuntimeOrigin>;

        /// `pallet_assets` id type. Launch ids map to `LaunchAssetBase + launch_id`.
        type AssetId: Parameter + Member + Copy + MaxEncodedLen + AtLeast32BitUnsigned;

        /// The quote asset (VTRS).
        type Currency: fungible::Inspect<Self::AccountId, Balance = BalanceOf<Self>>
            + fungible::Mutate<Self::AccountId>;

        /// `pallet_assets` main instance (named `LaunchAssets` because the DEX
        /// Config already has an `Assets`: its Native-or-asset union).
        type LaunchAssets: FungiblesInspect<Self::AccountId, AssetId = Self::AssetId, Balance = BalanceOf<Self>>
            + FungiblesMutate<Self::AccountId>
            + Create<Self::AccountId>
            + MetadataMutate<Self::AccountId>;

        /// The DEX's identifier for the quote asset.
        type NativeAssetKind: Get<AssetKindOf<Self>>;
        /// Launch asset id → DEX asset kind.
        type IntoAssetKind: Convert<Self::AssetId, AssetKindOf<Self>>;
        /// VitreusDEX. Trait-typed so the launchpad only calls the
        /// `PoolManager` / `ReservedPoolSeeder` surface.
        type Dex: PoolManager<Self::AccountId, AssetKindOf<Self>, BalanceOf<Self>, BlockNumberFor<Self>>
            + ReservedPoolSeeder<
                Self::AccountId,
                AssetKindOf<Self>,
                BalanceOf<Self>,
                BlockNumberFor<Self>,
            >;

        /// Receives protocol fees and rescue leftovers.
        type Treasury: Get<Self::AccountId>;

        /// L1: where a launch's `treasury_share_bps` of the curve fee is
        /// pushed. `()` folds it into the protocol share. Named apart from
        /// the DEX Config's `TreasurySink` (a supertrait here); the runtime
        /// binds both to the same pallet.
        type CurveTreasurySink: TreasurySink<AssetKindOf<Self>, Self::AccountId, BalanceOf<Self>>;

        #[pallet::constant]
        type PalletId: Get<PalletId>;
        /// `S`, total supply of every launch token, base units.
        #[pallet::constant]
        type TotalSupply: Get<BalanceOf<Self>>;
        /// `SELLABLE`. `Reserved = TotalSupply − Sellable`.
        #[pallet::constant]
        type Sellable: Get<BalanceOf<Self>>;
        /// `VT_FLOOR`, virtual token reserve remaining at sell-out.
        #[pallet::constant]
        type VirtualTokenFloor: Get<BalanceOf<Self>>;
        /// First asset id of the reserved range.
        #[pallet::constant]
        type LaunchAssetBase: Get<Self::AssetId>;
        #[pallet::constant]
        type MinGraduationTarget: Get<BalanceOf<Self>>;
        #[pallet::constant]
        type MaxGraduationTarget: Get<BalanceOf<Self>>;
        #[pallet::constant]
        type MaxCurveFeeBps: Get<u16>;
        #[pallet::constant]
        type MinProtocolShareBps: Get<u16>;
        #[pallet::constant]
        type MinCreationFee: Get<BalanceOf<Self>>;
        /// Blocks a launch must sit in `Complete` before governance may
        /// `force_seed_into_existing_pool` (§4.4).
        #[pallet::constant]
        type RescueDelay: Get<BlockNumberFor<Self>>;
        /// Name / symbol length cap (≤ `pallet_assets` StringLimit).
        #[pallet::constant]
        type StringLimit: Get<u32>;
        /// Byte cap on each URI field of [`LaunchMetadata`] (image, website, twitter, telegram).
        #[pallet::constant]
        type UriLimit: Get<u32>;
        /// Byte cap on [`LaunchMetadata::description`].
        #[pallet::constant]
        type DescriptionLimit: Get<u32>;
        /// Initial `Params`.
        #[pallet::constant]
        type DefaultLaunchParams: Get<LaunchParams<BalanceOf<Self>>>;

        /// Anti-snipe hook; `()` in v1.
        type BuyHook: OnCurveBuy<Self::AccountId, BalanceOf<Self>, BlockNumberFor<Self>>;

        /// Weight information for the extrinsics of this pallet.
        type WeightInfo: WeightInfo;
    }

    // ---- storage (§1) ----------------------------------------------------

    #[pallet::type_value]
    pub fn DefaultParams<T: Config>() -> LaunchParams<BalanceOf<T>> {
        T::DefaultLaunchParams::get()
    }

    /// Live governance parameters. Read at `create_launch` and snapshotted.
    #[pallet::storage]
    pub type Params<T: Config> =
        StorageValue<_, LaunchParams<BalanceOf<T>>, ValueQuery, DefaultParams<T>>;

    /// Blocks `create_launch` only. Never affects buy/sell/graduate.
    #[pallet::storage]
    pub type CreationPaused<T: Config> = StorageValue<_, bool, ValueQuery>;

    #[pallet::storage]
    pub type NextLaunchId<T: Config> = StorageValue<_, LaunchId, ValueQuery>;

    /// FM-17: monotonic cursor for the next launch asset id to try. Unset on a
    /// chain that predates the fix; then it starts at `LaunchAssetBase +
    /// NextLaunchId` (asset ids were `base + launch_id` before), so the first
    /// create after the upgrade resumes exactly where the old scheme left off
    /// and skips any already-squatted slot — no migration.
    #[pallet::storage]
    pub type NextAssetId<T: Config> = StorageValue<_, AssetIdOf<T>, OptionQuery>;

    #[pallet::storage]
    pub type Launches<T: Config> = StorageMap<_, Blake2_128Concat, LaunchId, Launch<T>>;

    #[pallet::storage]
    pub type Curves<T: Config> = StorageMap<_, Blake2_128Concat, LaunchId, CurveState<T>>;

    /// Reverse index asset id → launch id.
    #[pallet::storage]
    pub type AssetToLaunch<T: Config> = StorageMap<_, Blake2_128Concat, AssetIdOf<T>, LaunchId>;

    /// Presentation metadata per launch (§1.2). Absent when the creator gave
    /// none. Replaced whole by `set_launch_metadata`; never read on a trade.
    #[pallet::storage]
    pub type Metadata<T: Config> = StorageMap<_, Blake2_128Concat, LaunchId, LaunchMetadataOf<T>>;

    // ---- events / errors (§7) --------------------------------------------

    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        LaunchCreated {
            id: LaunchId,
            asset_id: AssetIdOf<T>,
            creator: T::AccountId,
            params_hash: T::Hash,
        },
        Bought {
            launch_id: LaunchId,
            who: T::AccountId,
            quote_used: BalanceOf<T>,
            fee: BalanceOf<T>,
            tokens_out: BalanceOf<T>,
        },
        Sold {
            launch_id: LaunchId,
            who: T::AccountId,
            tokens_in: BalanceOf<T>,
            fee: BalanceOf<T>,
            quote_out: BalanceOf<T>,
        },
        CurveCompleted {
            launch_id: LaunchId,
            raised: BalanceOf<T>,
        },
        /// Seeding failed inside the crossing buy; the buy stands, the launch
        /// stays `Complete`, and `graduate` may be retried by anyone.
        GraduationDeferred {
            launch_id: LaunchId,
            error: DispatchError,
        },
        Graduated {
            launch_id: LaunchId,
            quote_seeded: BalanceOf<T>,
            tokens_seeded: BalanceOf<T>,
            shares: BalanceOf<T>,
        },
        CreatorFeesClaimed {
            launch_id: LaunchId,
            recipient: T::AccountId,
            amount: BalanceOf<T>,
        },
        CreatorFeeRecipientChanged {
            launch_id: LaunchId,
            old: T::AccountId,
            new: T::AccountId,
        },
        /// Metadata was written for a launch (at creation or by `set_launch_metadata`).
        LaunchMetadataSet {
            launch_id: LaunchId,
        },
        ParamsUpdated {
            params: LaunchParams<BalanceOf<T>>,
        },
        CreationPausedSet {
            paused: bool,
        },
        ForceSeeded {
            launch_id: LaunchId,
            deviation_bps: u16,
            shares: BalanceOf<T>,
        },
    }

    #[pallet::error]
    pub enum Error<T> {
        LaunchNotFound,
        WrongPhase,
        ZeroAmount,
        SlippageExceeded,
        ArithmeticOverflow,
        /// The trade would deliver nothing to the trader (or is too small to price).
        Unquotable,
        NotFeeRecipient,
        CreationPaused,
        /// The reserved asset id for the next launch already exists (FM-14).
        AssetIdTaken,
        /// `expected_params_hash` did not match the current terms (FM-10).
        ParamsMismatch,
        ParamsOutOfBounds,
        /// A DEX pool for this launch already holds liquidity.
        PoolAlreadySeeded,
        /// The launch has not sat in `Complete` for `RescueDelay` yet.
        RescueNotDue,
        /// Rescue: the existing pool's price is outside the allowed deviation.
        PriceOutOfTolerance,
        InvalidMetadata,
        /// The seed this launch implies would be refused by the DEX (FM-11 preflight).
        Unseedable,
        /// Selling more than the curve has sold; a broken invariant (I3), not a user error.
        SellExceedsSold,
        /// `force_seed_into_existing_pool` needs an existing pool; use `graduate` otherwise.
        PoolNotFound,
    }

    // ---- calls (§2) ------------------------------------------------------

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        /// §2.1
        #[pallet::call_index(0)]
        #[pallet::weight({
            let (d, u) = metadata.as_ref().map(|m| m.dims()).unwrap_or((0, 0));
            let w = <T as Config>::WeightInfo::create_launch(name.len() as u32, symbol.len() as u32, d, u);
            if initial_buy.is_zero() { w } else { w.saturating_add(<T as Config>::WeightInfo::buy_crossing()) }
        })]
        #[allow(clippy::too_many_arguments)] // the call's signature is the interface
        pub fn create_launch(
            origin: OriginFor<T>,
            name: BoundedVec<u8, T::StringLimit>,
            symbol: BoundedVec<u8, T::StringLimit>,
            creator_fee_recipient: Option<T::AccountId>,
            initial_buy: BalanceOf<T>,
            min_tokens_out: BalanceOf<T>,
            expected_params_hash: Option<T::Hash>,
            metadata: Option<LaunchMetadataOf<T>>,
        ) -> DispatchResultWithPostInfo {
            let creator = ensure_signed(origin)?;
            ensure!(!CreationPaused::<T>::get(), Error::<T>::CreationPaused);
            ensure!(!name.is_empty() && !symbol.is_empty(), Error::<T>::InvalidMetadata);

            let id = NextLaunchId::<T>::get();
            // FM-17 (asset-id squatting): the reserved id `LaunchAssetBase + id`
            // is public and anyone can `pallet_assets::create` it for a deposit,
            // which used to fail every `create_launch` with `AssetIdTaken` until a
            // runtime migration bumped the counter — a whole pad bricked for the
            // price of one asset. Instead, walk from a monotonic cursor to the
            // first free id and use that; a squatter now only forces the cursor
            // forward at a deposit per id, and creation always succeeds. The
            // cursor defaults to `LaunchAssetBase + NextLaunchId` on a chain that
            // predates this (asset ids were exactly `base + launch_id` then), so
            // no migration is needed even where a slot is already squatted — the
            // next create skips it. (This is a different finding from vitreus-dex
            // SECURITY_AUDIT Finding 14, the sub-ED fee-routing one.)
            let asset_id = Self::next_free_asset_id()?;

            let params = Params::<T>::get();
            let curve = Self::snapshot(&params);
            let params_hash = Self::params_hash(&curve);
            if let Some(expected) = expected_params_hash {
                ensure!(expected == params_hash, Error::<T>::ParamsMismatch);
            }

            let asset_kind = T::IntoAssetKind::convert(asset_id);
            ensure!(
                !T::Dex::pool_exists(T::NativeAssetKind::get(), asset_kind),
                Error::<T>::PoolAlreadySeeded
            );
            Self::ensure_seedable(curve.graduation_target.into(), Self::reserved().into())?;

            let escrow = Self::escrow_account(id);
            let treasury = T::Treasury::get();

            // 1. creation fee into escrow (funds ED + asset deposits)
            T::Currency::transfer(&creator, &escrow, params.creation_fee, Preserve)?;
            // 2–4. asset, metadata (deposits reserved from escrow), full mint
            T::LaunchAssets::create(asset_id, escrow.clone(), false, One::one())?;
            <T::LaunchAssets as MetadataMutate<T::AccountId>>::set(
                asset_id,
                &escrow,
                name.to_vec(),
                symbol.to_vec(),
                TOKEN_DECIMALS,
            )?;
            T::LaunchAssets::mint_into(asset_id, &escrow, T::TotalSupply::get())?;
            // 5. whatever the escrow can spare above ED (and above reserved deposits) goes to treasury
            let spare = T::Currency::reducible_balance(&escrow, Preserve, Polite);
            if !spare.is_zero() {
                T::Currency::transfer(&escrow, &treasury, spare, Preserve)?;
            }

            // 6. records
            let now = frame_system::Pallet::<T>::block_number();
            let recipient = creator_fee_recipient.unwrap_or_else(|| creator.clone());
            Launches::<T>::insert(
                id,
                Launch::<T> {
                    asset_id,
                    creator: creator.clone(),
                    creator_fee_recipient: recipient,
                    escrow,
                    created_at: now,
                    curve,
                    params_hash,
                },
            );
            Curves::<T>::insert(
                id,
                CurveState::<T> {
                    phase: Phase::Trading,
                    real_quote: Zero::zero(),
                    tokens_remaining: T::Sellable::get(),
                    creator_fees_unclaimed: Zero::zero(),
                    protocol_fees_paid: Zero::zero(),
                    treasury_fees_paid: Zero::zero(),
                    last_trade_block: now,
                    completed_at: None,
                    graduated_at: None,
                    lp_shares: Zero::zero(),
                },
            );
            AssetToLaunch::<T>::insert(asset_id, id);
            NextLaunchId::<T>::put(id.checked_add(1).ok_or(Error::<T>::ArithmeticOverflow)?);
            // Advance the FM-17 cursor past the id just used, so the next create
            // never rescans it (and the walk stays O(1) unless squatting is active).
            NextAssetId::<T>::put(asset_id.saturating_add(One::one()));
            Self::deposit_event(Event::LaunchCreated {
                id,
                asset_id,
                creator: creator.clone(),
                params_hash,
            });

            // 7. presentation metadata, stored apart from the launch record (§2.9).
            let (d, u) = metadata.as_ref().map(|m| m.dims()).unwrap_or((0, 0));
            if let Some(m) = metadata {
                Metadata::<T>::insert(id, m);
                Self::deposit_event(Event::LaunchMetadataSet { launch_id: id });
            }

            // 8. optional atomic first buy. Charged as a crossing buy up front;
            // refunded to a plain buy when the curve was not exhausted.
            if !initial_buy.is_zero() {
                let (crossed, _) =
                    Self::do_buy(&creator, id, initial_buy, min_tokens_out, true, true)?;
                if !crossed {
                    let w = <T as Config>::WeightInfo::create_launch(
                        name.len() as u32,
                        symbol.len() as u32,
                        d,
                        u,
                    )
                    .saturating_add(<T as Config>::WeightInfo::buy());
                    return Ok(Some(w).into());
                }
            }
            Ok(().into())
        }

        /// §2.2. Whether the buy crosses is state-dependent, so the crossing
        /// weight (partial fill + pool creation + seed + lock) is charged up
        /// front and refunded to `buy()` when the curve was not exhausted.
        #[pallet::call_index(1)]
        #[pallet::weight(<T as Config>::WeightInfo::buy_crossing())]
        pub fn buy(
            origin: OriginFor<T>,
            launch_id: LaunchId,
            quote_in: BalanceOf<T>,
            min_tokens_out: BalanceOf<T>,
        ) -> DispatchResultWithPostInfo {
            let who = ensure_signed(origin)?;
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            let (crossed, _) = Self::do_buy(
                &who,
                launch_id,
                quote_in,
                min_tokens_out,
                who == launch.creator,
                true,
            )?;
            Ok(if crossed { None } else { Some(<T as Config>::WeightInfo::buy()) }.into())
        }

        /// §2.3
        #[pallet::call_index(2)]
        #[pallet::weight(<T as Config>::WeightInfo::sell())]
        pub fn sell(
            origin: OriginFor<T>,
            launch_id: LaunchId,
            tokens_in: BalanceOf<T>,
            min_quote_out: BalanceOf<T>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::do_sell(&who, launch_id, tokens_in, min_quote_out)
        }

        /// §2.4 — permissionless retry of seeding. Not nested: a failure is
        /// the extrinsic's error, visible to the caller.
        #[pallet::call_index(3)]
        #[pallet::weight(<T as Config>::WeightInfo::graduate())]
        pub fn graduate(origin: OriginFor<T>, launch_id: LaunchId) -> DispatchResult {
            let _ = ensure_signed(origin)?;
            Self::do_seed(launch_id)
        }

        /// §2.5
        #[pallet::call_index(4)]
        #[pallet::weight(<T as Config>::WeightInfo::claim_creator_fees())]
        pub fn claim_creator_fees(origin: OriginFor<T>, launch_id: LaunchId) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            ensure!(who == launch.creator_fee_recipient, Error::<T>::NotFeeRecipient);
            let amount = Curves::<T>::try_mutate(
                launch_id,
                |maybe| -> Result<BalanceOf<T>, DispatchError> {
                    let c = maybe.as_mut().ok_or(Error::<T>::LaunchNotFound)?;
                    let amount = c.creator_fees_unclaimed;
                    ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
                    c.creator_fees_unclaimed = Zero::zero();
                    Ok(amount)
                },
            )?;
            T::Currency::transfer(&launch.escrow, &who, amount, Preserve)?;
            Self::deposit_event(Event::CreatorFeesClaimed { launch_id, recipient: who, amount });
            Ok(())
        }

        /// §2.6 — only the current recipient; no governance override.
        #[pallet::call_index(5)]
        #[pallet::weight(<T as Config>::WeightInfo::set_creator_fee_recipient())]
        pub fn set_creator_fee_recipient(
            origin: OriginFor<T>,
            launch_id: LaunchId,
            new: T::AccountId,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Launches::<T>::try_mutate(launch_id, |maybe| -> DispatchResult {
                let l = maybe.as_mut().ok_or(Error::<T>::LaunchNotFound)?;
                ensure!(who == l.creator_fee_recipient, Error::<T>::NotFeeRecipient);
                let old = core::mem::replace(&mut l.creator_fee_recipient, new.clone());
                Self::deposit_event(Event::CreatorFeeRecipientChanged {
                    launch_id,
                    old,
                    new: new.clone(),
                });
                Ok(())
            })
        }

        /// §2.9 — replace a launch's presentation metadata. Same authority as
        /// `set_creator_fee_recipient`: only the current fee recipient, no
        /// governance override. Allowed in every phase. Nothing is validated.
        #[pallet::call_index(9)]
        #[pallet::weight({
            let (d, u) = metadata.dims();
            <T as Config>::WeightInfo::set_launch_metadata(d, u)
        })]
        pub fn set_launch_metadata(
            origin: OriginFor<T>,
            launch_id: LaunchId,
            metadata: LaunchMetadataOf<T>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            ensure!(who == launch.creator_fee_recipient, Error::<T>::NotFeeRecipient);
            Metadata::<T>::insert(launch_id, metadata);
            Self::deposit_event(Event::LaunchMetadataSet { launch_id });
            Ok(())
        }

        /// §2.8 — affects launches created afterwards only (FM-10).
        #[pallet::call_index(6)]
        #[pallet::weight(<T as Config>::WeightInfo::set_params())]
        pub fn set_params(origin: OriginFor<T>, new: LaunchParams<BalanceOf<T>>) -> DispatchResult {
            T::LaunchManageOrigin::ensure_origin(origin)?;
            Self::validate_params(&new)?;
            Params::<T>::put(new.clone());
            Self::deposit_event(Event::ParamsUpdated { params: new });
            Ok(())
        }

        /// §2.8
        #[pallet::call_index(7)]
        #[pallet::weight(<T as Config>::WeightInfo::set_creation_paused())]
        pub fn set_creation_paused(origin: OriginFor<T>, paused: bool) -> DispatchResult {
            T::LaunchManageOrigin::ensure_origin(origin)?;
            CreationPaused::<T>::put(paused);
            Self::deposit_event(Event::CreationPausedSet { paused });
            Ok(())
        }

        /// §4.4 — governance rescue for a launch whose DEX pool already holds
        /// liquidity (unreachable through any DEX call since D2; only a
        /// runtime-level bypass could create it). Deposits the stored amounts
        /// into that pool at its price, requiring the realised amounts to stay
        /// within `max_price_deviation_bps` of the stored ones, locks the
        /// position forever, sweeps the unused remainder to the treasury.
        /// The only fund movement is escrow → pool / treasury (FM-03).
        #[pallet::call_index(8)]
        #[pallet::weight(<T as Config>::WeightInfo::force_seed_into_existing_pool())]
        pub fn force_seed_into_existing_pool(
            origin: OriginFor<T>,
            launch_id: LaunchId,
            max_price_deviation_bps: u16,
        ) -> DispatchResult {
            T::LaunchManageOrigin::ensure_origin(origin)?;
            ensure!(max_price_deviation_bps <= BPS, Error::<T>::ParamsOutOfBounds);
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            let curve = Curves::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            ensure!(curve.phase == Phase::Complete, Error::<T>::WrongPhase);
            let completed_at = curve.completed_at.ok_or(Error::<T>::WrongPhase)?;
            let now = frame_system::Pallet::<T>::block_number();
            ensure!(
                now >= completed_at.saturating_add(T::RescueDelay::get()),
                Error::<T>::RescueNotDue
            );

            let asset = T::IntoAssetKind::convert(launch.asset_id);
            let native = T::NativeAssetKind::get();
            ensure!(T::Dex::pool_exists(native.clone(), asset.clone()), Error::<T>::PoolNotFound);

            let reserved = Self::reserved();
            let quote = curve.real_quote;
            let keep = |amount: BalanceOf<T>| -> BalanceOf<T> {
                let a: u128 = amount.into();
                let kept =
                    a.saturating_mul((BPS - max_price_deviation_bps) as u128) / (BPS as u128);
                kept.into()
            };

            let escrow = launch.escrow.clone();
            let quote_before = T::Currency::balance(&escrow);
            let tokens_before = T::LaunchAssets::balance(launch.asset_id, &escrow);

            let shares = T::Dex::add_liquidity_for(
                &escrow,
                asset.clone(),
                native.clone(),
                reserved,
                quote,
                keep(reserved),
                keep(quote),
            )
            .map_err(|e| {
                if e == DispatchError::from(pallet_vitreus_dex::Error::<T>::SlippageExceeded) {
                    Error::<T>::PriceOutOfTolerance.into()
                } else {
                    e
                }
            })?;
            T::Dex::lock_liquidity_for(&escrow, asset, native, Bounded::max_value())?;

            // Sweep what the pool did not take (only escrow → treasury).
            let quote_spent = quote_before.saturating_sub(T::Currency::balance(&escrow));
            let tokens_spent =
                tokens_before.saturating_sub(T::LaunchAssets::balance(launch.asset_id, &escrow));
            let quote_left = quote.saturating_sub(quote_spent);
            let tokens_left = reserved.saturating_sub(tokens_spent);
            let treasury = T::Treasury::get();
            if !quote_left.is_zero() {
                T::Currency::transfer(&escrow, &treasury, quote_left, Preserve)?;
            }
            if !tokens_left.is_zero() {
                T::LaunchAssets::transfer(
                    launch.asset_id,
                    &escrow,
                    &treasury,
                    tokens_left,
                    Expendable,
                )?;
            }

            Curves::<T>::mutate(launch_id, |maybe| {
                if let Some(c) = maybe {
                    c.real_quote = Zero::zero();
                    c.lp_shares = shares;
                    c.phase = Phase::Graduated;
                    c.graduated_at = Some(now);
                }
            });
            Self::deposit_event(Event::ForceSeeded {
                launch_id,
                deviation_bps: max_price_deviation_bps,
                shares,
            });
            Self::deposit_event(Event::Graduated {
                launch_id,
                quote_seeded: quote_spent,
                tokens_seeded: tokens_spent,
                shares,
            });
            Ok(())
        }
    }

    // ---- internals -------------------------------------------------------

    impl<T: Config> Pallet<T> {
        /// `PalletId::into_sub_account_truncating(id)`. The sub-seed is the bare
        /// 8-byte id so that on a 20-byte AccountId ("modl" + 8-byte PalletId +
        /// 8 bytes of seed) every launch keeps a distinct escrow; a longer seed
        /// such as `("launch", id)` would be truncated to two bytes of the id.
        pub fn escrow_account(id: LaunchId) -> T::AccountId {
            T::PalletId::get().into_sub_account_truncating(id)
        }

        pub fn asset_id_for(id: LaunchId) -> AssetIdOf<T> {
            T::LaunchAssetBase::get().saturating_add(AssetIdOf::<T>::unique_saturated_from(id))
        }

        /// FM-17: the first asset id at or above the cursor that no asset and no
        /// launch already claims. Bounded so a squatter cannot make one create
        /// scan without end; `MAX_ASSET_ID_SCAN` contiguous squats ahead of the
        /// cursor cost that many standing deposits and only delay, since the
        /// cursor never advances on a failed create and any create in a gap
        /// moves it past.
        fn next_free_asset_id() -> Result<AssetIdOf<T>, Error<T>> {
            let mut candidate = NextAssetId::<T>::get().unwrap_or_else(|| {
                T::LaunchAssetBase::get()
                    .saturating_add(AssetIdOf::<T>::unique_saturated_from(NextLaunchId::<T>::get()))
            });
            for _ in 0..MAX_ASSET_ID_SCAN {
                if !T::LaunchAssets::asset_exists(candidate)
                    && !AssetToLaunch::<T>::contains_key(candidate)
                {
                    return Ok(candidate);
                }
                candidate = candidate.saturating_add(One::one());
            }
            Err(Error::<T>::AssetIdTaken)
        }

        /// D4: who may claim the DEX creator share of `asset`'s graduated
        /// pool — the launch's current `creator_fee_recipient`, so a
        /// `set_creator_fee_recipient` moves the DEX stream with it. The
        /// runtime binds `pallet_vitreus_dex::Config::CreatorFeeRecipient`
        /// to this through an adapter; the DEX itself knows no creators.
        pub fn creator_fee_recipient_for(asset: AssetIdOf<T>) -> Option<T::AccountId> {
            AssetToLaunch::<T>::get(asset)
                .and_then(Launches::<T>::get)
                .map(|l| l.creator_fee_recipient)
        }

        /// `Reserved = TotalSupply − Sellable`.
        pub fn reserved() -> BalanceOf<T> {
            T::TotalSupply::get().saturating_sub(T::Sellable::get())
        }

        fn terms(curve: &CurveParams<BalanceOf<T>>) -> curve::Terms {
            curve::Terms {
                virtual_quote: curve.virtual_quote.into(),
                token_floor: T::VirtualTokenFloor::get().into(),
                fee_bps: curve.curve_fee_bps as u128,
            }
        }

        fn snapshot(p: &LaunchParams<BalanceOf<T>>) -> CurveParams<BalanceOf<T>> {
            let t: u128 = p.graduation_target.into();
            CurveParams {
                graduation_target: p.graduation_target,
                virtual_quote: (t / 3).into(),
                curve_fee_bps: p.curve_fee_bps,
                protocol_share_bps: p.protocol_share_bps,
                treasury_share_bps: p.treasury_share_bps,
                pool_fee_tier: p.pool_fee_tier,
            }
        }

        /// `blake2(encode((curve, S, SELLABLE, RESERVED, VT_FLOOR)))` via `T::Hashing`.
        pub fn params_hash(curve: &CurveParams<BalanceOf<T>>) -> T::Hash {
            T::Hashing::hash_of(&(
                curve,
                T::TotalSupply::get(),
                T::Sellable::get(),
                Self::reserved(),
                T::VirtualTokenFloor::get(),
            ))
        }

        /// Hash of the current live `Params` as a launch would snapshot them —
        /// what a creator passes as `expected_params_hash`.
        pub fn current_params_hash() -> T::Hash {
            Self::params_hash(&Self::snapshot(&Params::<T>::get()))
        }

        pub fn validate_params(p: &LaunchParams<BalanceOf<T>>) -> DispatchResult {
            ensure!(
                p.graduation_target >= T::MinGraduationTarget::get()
                    && p.graduation_target <= T::MaxGraduationTarget::get(),
                Error::<T>::ParamsOutOfBounds
            );
            ensure!(p.curve_fee_bps <= T::MaxCurveFeeBps::get(), Error::<T>::ParamsOutOfBounds);
            // L1: the non-creator share (protocol + treasury) is what
            // `MinProtocolShareBps` bounds from below; the creator gets the rest.
            let non_creator = p.protocol_share_bps.saturating_add(p.treasury_share_bps);
            ensure!(
                non_creator >= T::MinProtocolShareBps::get() && non_creator <= BPS,
                Error::<T>::ParamsOutOfBounds
            );
            // L1: every launch pool must carry the three routed slices (D9).
            ensure!(
                matches!(p.pool_fee_tier, 3 | 10) && p.pool_fee_tier >= MIN_LAUNCH_FEE_TIER,
                Error::<T>::ParamsOutOfBounds
            );
            ensure!(p.creation_fee >= T::MinCreationFee::get(), Error::<T>::ParamsOutOfBounds);
            Ok(())
        }

        /// FM-11 preflight: the first deposit the DEX would mint from
        /// `(target, reserved)` must exceed its permanently burned minimum.
        pub fn ensure_seedable(target: u128, reserved: u128) -> DispatchResult {
            let shares = (U256::from(target) * U256::from(reserved)).integer_sqrt();
            ensure!(shares > U256::from(MINIMUM_LIQUIDITY), Error::<T>::Unseedable);
            Ok(())
        }

        fn map_math(e: curve::MathError) -> DispatchError {
            match e {
                curve::MathError::ZeroAmount => Error::<T>::ZeroAmount.into(),
                curve::MathError::Overflow => Error::<T>::ArithmeticOverflow.into(),
                curve::MathError::Unquotable => Error::<T>::Unquotable.into(),
                curve::MathError::BadState => Error::<T>::ArithmeticOverflow.into(),
            }
        }

        /// Split a fee: protocol and treasury parts floor, creator gets the
        /// rest. Returns `(protocol, treasury, creator)`.
        fn split_fee(
            fee: u128,
            protocol_share_bps: u16,
            treasury_share_bps: u16,
        ) -> (u128, u128, u128) {
            let protocol = fee.saturating_mul(protocol_share_bps as u128) / (BPS as u128);
            let treasury = fee.saturating_mul(treasury_share_bps as u128) / (BPS as u128);
            (protocol, treasury, fee - protocol - treasury)
        }

        /// L1: pay the protocol and treasury parts of a curve fee out of the
        /// escrow. The treasury part goes to the sink's account for the
        /// launch asset if there is one, else it joins the protocol part.
        /// Returns `(protocol paid, treasury paid)`.
        fn pay_fee_parts(
            launch: &Launch<T>,
            protocol: u128,
            treasury: u128,
            treasury_recipient: &T::AccountId,
        ) -> Result<(BalanceOf<T>, BalanceOf<T>), DispatchError> {
            let asset = T::IntoAssetKind::convert(launch.asset_id);
            let sink = if treasury > 0 { T::CurveTreasurySink::account_for(&asset) } else { None };
            // Finding 14 (SECURITY_AUDIT): a sub-ED treasury slice cannot create
            // the vault before it is funded, and the transfer below would fail the
            // whole buy with `Token(BelowMinimum)`. Fold such a slice into the
            // protocol share — the same redirection the retired-treasury case
            // (`account_for == None`) already does. So, as on the DEX side, a
            // treasury total (`treasury_fees_paid`) does NOT count a sub-ED slice
            // taken before the vault existed; the window closes at the vault's
            // first ≥ED credit (LAUNCH_TREASURY_SPEC §9.6).
            let ed = <T::Currency as FungibleInspect<T::AccountId>>::minimum_balance();
            let treasury_bal: BalanceOf<T> = treasury.into();
            let sink = match sink {
                Some(v) if treasury_bal >= ed || frame_system::Pallet::<T>::account_exists(&v) => {
                    Some(v)
                },
                _ => None,
            };
            let (protocol, treasury) = match sink {
                Some(_) => (protocol, treasury),
                None => (protocol + treasury, 0),
            };
            if protocol > 0 {
                T::Currency::transfer(
                    &launch.escrow,
                    treasury_recipient,
                    protocol.into(),
                    Preserve,
                )?;
            }
            if let Some(vault) = sink {
                T::Currency::transfer(&launch.escrow, &vault, treasury.into(), Preserve)?;
                T::CurveTreasurySink::note_fee(&asset, treasury.into());
            }
            Ok((protocol.into(), treasury.into()))
        }

        /// §2.2 body. Single choke point for every buy. Returns whether the
        /// buy exhausted the curve (and therefore attempted the seed), and
        /// the tokens delivered. `is_trade` says whether this buy counts as
        /// market activity for `last_trade_block`: a person's buy does, the
        /// treasury's own buyback through [`CurveVenue::buy_for`] does not —
        /// the dormancy rule that reads the field asks whether anyone is
        /// still interested in the token, and the pallet buying it back is
        /// not an answer (R2).
        pub fn do_buy(
            who: &T::AccountId,
            launch_id: LaunchId,
            quote_in: BalanceOf<T>,
            min_tokens_out: BalanceOf<T>,
            is_creator: bool,
            is_trade: bool,
        ) -> Result<(bool, BalanceOf<T>), DispatchError> {
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            let mut state = Curves::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            ensure!(state.phase == Phase::Trading, Error::<T>::WrongPhase);
            ensure!(!quote_in.is_zero(), Error::<T>::ZeroAmount);

            let now = frame_system::Pallet::<T>::block_number();
            let (to_curve, extra) =
                T::BuyHook::on_buy(launch_id, launch.created_at, now, who, is_creator, quote_in)?;
            let treasury = T::Treasury::get();
            if !extra.is_zero() {
                T::Currency::transfer(who, &treasury, extra, Preserve)?;
            }

            let terms = Self::terms(&launch.curve);
            let q = curve::quote_buy(
                &terms,
                &curve::State {
                    real_quote: state.real_quote.into(),
                    tokens_remaining: state.tokens_remaining.into(),
                },
                to_curve.into(),
            )
            .map_err(Self::map_math)?;
            let tokens_out: BalanceOf<T> = q.tokens_out.into();
            ensure!(tokens_out >= min_tokens_out, Error::<T>::SlippageExceeded);

            // funds in
            T::Currency::transfer(who, &launch.escrow, q.quote_used.into(), Preserve)?;
            // fee split
            let (protocol, treasury_part, creator) = Self::split_fee(
                q.fee,
                launch.curve.protocol_share_bps,
                launch.curve.treasury_share_bps,
            );
            let (protocol, treasury_part) =
                Self::pay_fee_parts(&launch, protocol, treasury_part, &treasury)?;
            state.creator_fees_unclaimed =
                state.creator_fees_unclaimed.saturating_add(creator.into());
            state.protocol_fees_paid = state.protocol_fees_paid.saturating_add(protocol);
            state.treasury_fees_paid = state.treasury_fees_paid.saturating_add(treasury_part);
            if is_trade {
                state.last_trade_block = now;
            }
            // curve state
            state.real_quote = state.real_quote.saturating_add(q.quote_net_used.into());
            state.tokens_remaining = state
                .tokens_remaining
                .checked_sub(&tokens_out)
                .ok_or(Error::<T>::ArithmeticOverflow)?;
            // tokens out
            T::LaunchAssets::transfer(
                launch.asset_id,
                &launch.escrow,
                who,
                tokens_out,
                Expendable,
            )?;

            let crossed = state.tokens_remaining.is_zero();
            if crossed {
                state.phase = Phase::Complete;
                state.completed_at = Some(now);
            }
            Curves::<T>::insert(launch_id, &state);
            Self::deposit_event(Event::Bought {
                launch_id,
                who: who.clone(),
                quote_used: q.quote_used.into(),
                fee: q.fee.into(),
                tokens_out,
            });

            if crossed {
                Self::deposit_event(Event::CurveCompleted { launch_id, raised: state.real_quote });
                // §4.3: seed in a nested storage layer so a DEX failure defers
                // graduation instead of reverting the buy (FM-08).
                let res = frame_support::storage::with_storage_layer(|| Self::do_seed(launch_id));
                if let Err(error) = res {
                    Self::deposit_event(Event::GraduationDeferred { launch_id, error });
                }
            }
            Ok((crossed, tokens_out))
        }

        /// §2.3 body.
        pub fn do_sell(
            who: &T::AccountId,
            launch_id: LaunchId,
            tokens_in: BalanceOf<T>,
            min_quote_out: BalanceOf<T>,
        ) -> DispatchResult {
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            let mut state = Curves::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            ensure!(state.phase == Phase::Trading, Error::<T>::WrongPhase);
            ensure!(!tokens_in.is_zero(), Error::<T>::ZeroAmount);
            let sold = T::Sellable::get().saturating_sub(state.tokens_remaining);
            ensure!(tokens_in <= sold, Error::<T>::SellExceedsSold);

            let terms = Self::terms(&launch.curve);
            let q = curve::quote_sell(
                &terms,
                &curve::State {
                    real_quote: state.real_quote.into(),
                    tokens_remaining: state.tokens_remaining.into(),
                },
                tokens_in.into(),
            )
            .map_err(Self::map_math)?;
            let quote_out: BalanceOf<T> = q.quote_out.into();
            ensure!(quote_out >= min_quote_out, Error::<T>::SlippageExceeded);

            T::LaunchAssets::transfer(launch.asset_id, who, &launch.escrow, tokens_in, Expendable)?;
            let treasury = T::Treasury::get();
            let (protocol, treasury_part, creator) = Self::split_fee(
                q.fee,
                launch.curve.protocol_share_bps,
                launch.curve.treasury_share_bps,
            );
            let (protocol, treasury_part) =
                Self::pay_fee_parts(&launch, protocol, treasury_part, &treasury)?;
            state.creator_fees_unclaimed =
                state.creator_fees_unclaimed.saturating_add(creator.into());
            state.protocol_fees_paid = state.protocol_fees_paid.saturating_add(protocol);
            state.treasury_fees_paid = state.treasury_fees_paid.saturating_add(treasury_part);
            state.last_trade_block = frame_system::Pallet::<T>::block_number();
            state.real_quote = state
                .real_quote
                .checked_sub(&q.quote_gross.into())
                .ok_or(Error::<T>::ArithmeticOverflow)?;
            state.tokens_remaining = state.tokens_remaining.saturating_add(tokens_in);
            T::Currency::transfer(&launch.escrow, who, quote_out, Preserve)?;
            Curves::<T>::insert(launch_id, &state);
            Self::deposit_event(Event::Sold {
                launch_id,
                who: who.clone(),
                tokens_in,
                fee: q.fee.into(),
                quote_out,
            });
            Ok(())
        }

        /// §4.3 — the only path that moves curve funds to the pool.
        pub fn do_seed(launch_id: LaunchId) -> DispatchResult {
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            let state = Curves::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            ensure!(state.phase == Phase::Complete, Error::<T>::WrongPhase);
            let reserved = Self::reserved();
            let shares = T::Dex::seed_reserved_pool_for(
                &launch.escrow,
                T::IntoAssetKind::convert(launch.asset_id),
                T::NativeAssetKind::get(),
                reserved,
                state.real_quote,
                launch.curve.pool_fee_tier,
            )?;
            let now = frame_system::Pallet::<T>::block_number();
            Curves::<T>::mutate(launch_id, |maybe| {
                if let Some(c) = maybe {
                    c.real_quote = Zero::zero();
                    c.lp_shares = shares;
                    c.phase = Phase::Graduated;
                    c.graduated_at = Some(now);
                }
            });
            Self::deposit_event(Event::Graduated {
                launch_id,
                quote_seeded: state.real_quote,
                tokens_seeded: reserved,
                shares,
            });
            Ok(())
        }

        /// Read-only quotes for a frontend / runtime API (§3.5). Never reimplement the math elsewhere.
        pub fn quote_buy(
            launch_id: LaunchId,
            quote_in: BalanceOf<T>,
        ) -> Result<curve::BuyQuote, DispatchError> {
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            let s = Curves::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            ensure!(s.phase == Phase::Trading, Error::<T>::WrongPhase);
            curve::quote_buy(
                &Self::terms(&launch.curve),
                &curve::State {
                    real_quote: s.real_quote.into(),
                    tokens_remaining: s.tokens_remaining.into(),
                },
                quote_in.into(),
            )
            .map_err(Self::map_math)
        }

        pub fn quote_sell(
            launch_id: LaunchId,
            tokens_in: BalanceOf<T>,
        ) -> Result<curve::SellQuote, DispatchError> {
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            let s = Curves::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            ensure!(s.phase == Phase::Trading, Error::<T>::WrongPhase);
            curve::quote_sell(
                &Self::terms(&launch.curve),
                &curve::State {
                    real_quote: s.real_quote.into(),
                    tokens_remaining: s.tokens_remaining.into(),
                },
                tokens_in.into(),
            )
            .map_err(Self::map_math)
        }

        /// Marginal price, quote per token, as an 18-decimal fixed point (§3.5).
        pub fn spot_price(launch_id: LaunchId) -> Option<u128> {
            let launch = Launches::<T>::get(launch_id)?;
            let s = Curves::<T>::get(launch_id)?;
            let q: u128 = launch.curve.virtual_quote.into();
            let q = q.checked_add(s.real_quote.into())?;
            let tk: u128 = T::VirtualTokenFloor::get().into();
            let tk = tk.checked_add(s.tokens_remaining.into())?;
            u128::try_from(U256::from(q) * U256::from(10u128.pow(18)) / U256::from(tk)).ok()
        }

        /// Terms of a launch in the pure-math form (tests, try_state).
        pub fn curve_terms(launch_id: LaunchId) -> Option<curve::Terms> {
            Launches::<T>::get(launch_id).map(|l| Self::terms(&l.curve))
        }

        /// `k` for a launch's current state (I4).
        pub fn invariant_k(launch_id: LaunchId) -> Option<U256> {
            let terms = Self::curve_terms(launch_id)?;
            let s = Curves::<T>::get(launch_id)?;
            curve::invariant_k(
                &terms,
                &curve::State {
                    real_quote: s.real_quote.into(),
                    tokens_remaining: s.tokens_remaining.into(),
                },
            )
        }

        /// The names of every dispatchable, for FM-03's "no withdraw path" assertion.
        pub fn call_names() -> Vec<&'static str> {
            <Call<T> as frame_support::traits::GetCallName>::get_call_names().to_vec()
        }
    }

    impl<T: Config> CurveVenue<T::AccountId, T::AssetId, BalanceOf<T>, BlockNumberFor<T>>
        for Pallet<T>
    {
        fn launch_of_asset(asset: T::AssetId) -> Option<LaunchId> {
            AssetToLaunch::<T>::get(asset)
        }
        fn asset_of(launch_id: LaunchId) -> Option<T::AssetId> {
            Launches::<T>::get(launch_id).map(|l| l.asset_id)
        }
        fn phase(launch_id: LaunchId) -> Option<Phase> {
            Curves::<T>::get(launch_id).map(|c| c.phase)
        }
        fn last_trade_block(launch_id: LaunchId) -> Option<BlockNumberFor<T>> {
            Curves::<T>::get(launch_id).map(|c| c.last_trade_block)
        }
        fn virtual_reserves(launch_id: LaunchId) -> Option<(BalanceOf<T>, BalanceOf<T>)> {
            let launch = Launches::<T>::get(launch_id)?;
            let s = Curves::<T>::get(launch_id)?;
            if s.phase != Phase::Trading {
                return None;
            }
            let q: u128 = launch.curve.virtual_quote.into();
            let q = q.checked_add(s.real_quote.into())?;
            let tk: u128 = T::VirtualTokenFloor::get().into();
            let tk = tk.checked_add(s.tokens_remaining.into())?;
            Some((q.into(), tk.into()))
        }
        fn fee_bps(launch_id: LaunchId) -> Option<u16> {
            Launches::<T>::get(launch_id).map(|l| l.curve.curve_fee_bps)
        }
        fn buy_for(
            who: &T::AccountId,
            launch_id: LaunchId,
            quote_in: BalanceOf<T>,
            min_tokens_out: BalanceOf<T>,
        ) -> Result<BalanceOf<T>, DispatchError> {
            let launch = Launches::<T>::get(launch_id).ok_or(Error::<T>::LaunchNotFound)?;
            // An in-runtime buy on the token's behalf is not a trade for the
            // dormancy clock (R2).
            let (_, tokens_out) = Self::do_buy(
                who,
                launch_id,
                quote_in,
                min_tokens_out,
                *who == launch.creator,
                false,
            )?;
            Ok(tokens_out)
        }
    }
}

/// Fork-only storage migrations. The submission branch ships these shapes
/// as its v1 with no migration, since no chain it targets has a v0 launch;
/// the fork's dev chain has five.
pub mod migrations {
    use super::*;
    use frame_support::{
        migrations::VersionedMigration,
        traits::{Get, UncheckedOnRuntimeUpgrade},
        weights::Weight,
    };
    use sp_std::marker::PhantomData;

    /// L1 (v0 → v1): `LaunchParams` and `CurveParams` gain
    /// `treasury_share_bps`; `CurveState` gains `treasury_fees_paid` and
    /// `last_trade_block`. Every stored `Params`, `Launches` and `Curves`
    /// record is re-encoded.
    ///
    /// Existing launches get `treasury_share_bps = 0` — a launch's terms are
    /// its snapshot (LAUNCHPAD_SPEC §1.4), and the treasury's share is a term.
    /// The governance `Params` get 0 too, for governance to raise with
    /// `set_params`. `last_trade_block` becomes the curve's last known
    /// event — `graduated_at`, else `completed_at`, else the block this runs
    /// in — so a dormancy clock (LAUNCH_TREASURY_SPEC §6.5) can only start
    /// from the upgrade, never earlier.
    pub mod v1 {
        use super::*;

        /// `LaunchParams` as stored before L1.
        #[derive(Encode, Decode)]
        #[allow(missing_docs)]
        pub struct OldLaunchParams<Balance> {
            pub graduation_target: Balance,
            pub curve_fee_bps: u16,
            pub protocol_share_bps: u16,
            pub pool_fee_tier: u32,
            pub creation_fee: Balance,
        }

        /// `CurveParams` as stored before L1.
        #[derive(Encode, Decode)]
        #[allow(missing_docs)]
        pub struct OldCurveParams<Balance> {
            pub graduation_target: Balance,
            pub virtual_quote: Balance,
            pub curve_fee_bps: u16,
            pub protocol_share_bps: u16,
            pub pool_fee_tier: u32,
        }

        /// `Launch` as stored before L1.
        #[derive(Encode, Decode)]
        #[allow(missing_docs)]
        pub struct OldLaunch<T: Config> {
            pub asset_id: AssetIdOf<T>,
            pub creator: T::AccountId,
            pub creator_fee_recipient: T::AccountId,
            pub escrow: T::AccountId,
            pub created_at: BlockNumberFor<T>,
            pub curve: OldCurveParams<BalanceOf<T>>,
            pub params_hash: T::Hash,
        }

        /// `CurveState` as stored before L1.
        #[derive(Encode, Decode)]
        #[allow(missing_docs)]
        pub struct OldCurveState<T: Config> {
            pub phase: Phase,
            pub real_quote: BalanceOf<T>,
            pub tokens_remaining: BalanceOf<T>,
            pub creator_fees_unclaimed: BalanceOf<T>,
            pub protocol_fees_paid: BalanceOf<T>,
            pub completed_at: Option<BlockNumberFor<T>>,
            pub graduated_at: Option<BlockNumberFor<T>>,
            pub lp_shares: BalanceOf<T>,
        }

        /// Unversioned body; wrap in [`MigrateToV1`].
        pub struct VersionUncheckedMigrateToV1<T>(PhantomData<T>);

        impl<T: Config> UncheckedOnRuntimeUpgrade for VersionUncheckedMigrateToV1<T> {
            fn on_runtime_upgrade() -> Weight {
                let now = frame_system::Pallet::<T>::block_number();
                // Absent (a chain still on the runtime default) stays absent:
                // `get()` then answers the new default, treasury share included.
                let params_set = Params::<T>::exists();
                let _ = Params::<T>::translate::<OldLaunchParams<BalanceOf<T>>, _>(|old| {
                    old.map(|o| LaunchParams {
                        graduation_target: o.graduation_target,
                        curve_fee_bps: o.curve_fee_bps,
                        protocol_share_bps: o.protocol_share_bps,
                        treasury_share_bps: 0,
                        pool_fee_tier: o.pool_fee_tier,
                        creation_fee: o.creation_fee,
                    })
                });
                let mut launches = 0u64;
                Launches::<T>::translate::<OldLaunch<T>, _>(|_id, old| {
                    launches = launches.saturating_add(1);
                    Some(Launch {
                        asset_id: old.asset_id,
                        creator: old.creator,
                        creator_fee_recipient: old.creator_fee_recipient,
                        escrow: old.escrow,
                        created_at: old.created_at,
                        curve: CurveParams {
                            graduation_target: old.curve.graduation_target,
                            virtual_quote: old.curve.virtual_quote,
                            curve_fee_bps: old.curve.curve_fee_bps,
                            protocol_share_bps: old.curve.protocol_share_bps,
                            treasury_share_bps: 0,
                            pool_fee_tier: old.curve.pool_fee_tier,
                        },
                        params_hash: old.params_hash,
                    })
                });
                let mut curves = 0u64;
                Curves::<T>::translate::<OldCurveState<T>, _>(|_id, old| {
                    curves = curves.saturating_add(1);
                    Some(CurveState {
                        phase: old.phase,
                        real_quote: old.real_quote,
                        tokens_remaining: old.tokens_remaining,
                        creator_fees_unclaimed: old.creator_fees_unclaimed,
                        protocol_fees_paid: old.protocol_fees_paid,
                        treasury_fees_paid: Zero::zero(),
                        last_trade_block: old.graduated_at.or(old.completed_at).unwrap_or(now),
                        completed_at: old.completed_at,
                        graduated_at: old.graduated_at,
                        lp_shares: old.lp_shares,
                    })
                });
                log::info!(
                    target: "runtime::launchpad",
                    "L1 migration: {launches} launches and {curves} curves re-encoded with treasury_share_bps = 0; governance params {}",
                    if params_set { "re-encoded with treasury_share_bps = 0" } else { "not set: the runtime default applies, treasury share included" }
                );
                T::DbWeight::get().reads_writes(
                    launches.saturating_add(curves).saturating_add(1),
                    launches.saturating_add(curves).saturating_add(1),
                )
            }

            #[cfg(feature = "try-runtime")]
            fn pre_upgrade() -> Result<sp_std::vec::Vec<u8>, sp_runtime::TryRuntimeError> {
                let launches = Launches::<T>::iter_keys().count() as u32;
                let curves = Curves::<T>::iter_keys().count() as u32;
                let next = NextLaunchId::<T>::get();
                let params_set = Params::<T>::exists();
                log::info!(target: "runtime::launchpad", "L1 pre_upgrade: {launches} launches, {curves} curves, next id {next}, governance params set: {params_set}");
                Ok((launches, curves, next, params_set).encode())
            }

            #[cfg(feature = "try-runtime")]
            fn post_upgrade(
                state: sp_std::vec::Vec<u8>,
            ) -> Result<(), sp_runtime::TryRuntimeError> {
                let (launches, curves, next, params_set): (u32, u32, LaunchId, bool) =
                    Decode::decode(&mut &state[..]).map_err(|_| "pre_upgrade state")?;
                let now = frame_system::Pallet::<T>::block_number();
                let mut nl = 0u32;
                for (_id, l) in Launches::<T>::iter() {
                    nl = nl.saturating_add(1);
                    frame_support::ensure!(
                        l.curve.treasury_share_bps == 0,
                        "every pre-L1 launch carries treasury_share_bps = 0"
                    );
                }
                let mut nc = 0u32;
                for (_id, c) in Curves::<T>::iter() {
                    nc = nc.saturating_add(1);
                    frame_support::ensure!(
                        c.treasury_fees_paid.is_zero(),
                        "no treasury fee was paid before L1"
                    );
                    frame_support::ensure!(
                        c.last_trade_block <= now,
                        "last_trade_block is in the past"
                    );
                }
                frame_support::ensure!(
                    nl == launches && nc == curves,
                    "every launch and curve decodes after L1"
                );
                frame_support::ensure!(NextLaunchId::<T>::get() == next, "next id untouched");
                let p = Params::<T>::get();
                frame_support::ensure!(
                    Params::<T>::exists() == params_set,
                    "params stay set or stay absent"
                );
                if params_set {
                    frame_support::ensure!(
                        p.treasury_share_bps == 0,
                        "stored governance params carry treasury_share_bps = 0 until set_params"
                    );
                }
                frame_support::ensure!(
                    Pallet::<T>::validate_params(&p).is_ok(),
                    "governance params still valid"
                );
                log::info!(target: "runtime::launchpad", "L1 post_upgrade: {nl} launches and {nc} curves decode; params ({}) protocol {} / treasury {}", if params_set { "stored" } else { "runtime default" }, p.protocol_share_bps, p.treasury_share_bps);
                Ok(())
            }
        }

        /// L1 migration, gated on the pallet's on-chain storage version.
        pub type MigrateToV1<T> = VersionedMigration<
            0,
            1,
            VersionUncheckedMigrateToV1<T>,
            Pallet<T>,
            <T as frame_system::Config>::DbWeight,
        >;
    }
}
