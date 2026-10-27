//! # Vitreus DEX pallet
//!
//! A native AMM DEX pallet for the Vitreus blockchain. Scaffolded to mirror the
//! structure of `pallet-energy-broker`. Pallet index: 43. PalletId: `vtrs/dex`.
//!
//! Other pallets in the same runtime can create pools and provision liquidity
//! without dispatching extrinsics via the [`PoolManager`] trait (implemented
//! for [`Pallet`]), which wraps the origin-free `do_create_pool`,
//! `do_add_liquidity_for` and `do_lock_liquidity_for` helpers. The solver
//! marketplace, a pallet of its own, settles intents through
//! [`IntentSwap`], which exposes nothing but that one swap.
//!
//! # Part 3 integration notes (runtime wiring)
//!
//! When wiring this pallet into `runtime/vitreus/src/lib.rs`:
//!
//! 1. **Pallet index 43** — confirmed free between `DynamicEnergy = 42` and
//!    `Proxy = 44`. Use `VitreusDex: pallet_vitreus_dex = 43`.
//!
//! 2. **Energy fee routing** — `pallet_energy_fee` is the runtime's
//!    `OnChargeTransaction`. Add `RuntimeCall::VitreusDex(..)` to the
//!    `CallFee::Regular(Self::custom_fee())` match in
//!    `runtime/vitreus/src/lib.rs` (around the `CustomFee` impl) or leave
//!    it to the default `weight_fee` branch — decide per governance.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]
#![allow(clippy::result_unit_err, clippy::too_many_arguments)]

#[cfg(test)]
mod account_lifecycle_tests;
#[cfg(test)]
mod asset_conversion_tests;
#[cfg(test)]
mod core_tests;
#[cfg(test)]
mod fuzz;
#[cfg(test)]
mod mock;
#[cfg(any(test, feature = "test-utils"))]
pub mod testing;
#[cfg(test)]
mod tests;

pub mod base;
pub use base::{AmmBase, AssetConversionAdapter};

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;
pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
pub use benchmarking::BenchmarkHelper;
pub use weights::WeightInfo;

pub use pallet::*;

use frame_support::{
    dispatch::DispatchResult,
    traits::{
        fungibles::{Balanced, Credit, Inspect, Mutate},
        tokens::{
            Balance, DepositConsequence,
            Fortitude::Polite,
            Precision::Exact,
            Preservation::{self, Expendable, Preserve},
            Provenance,
        },
        AccountTouch, Contains, Get,
    },
    PalletId,
};
use frame_system::pallet_prelude::BlockNumberFor;
use parity_scale_codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_arithmetic::traits::Unsigned;
use sp_runtime::{
    traits::{
        AccountIdConversion, Bounded, CheckedAdd, CheckedDiv, CheckedMul, CheckedSub, Ensure,
        IntegerSquareRoot, One, Saturating, Zero,
    },
    DispatchError, PerThing, Permill, Rounding, RuntimeDebug,
};

/// The PalletId used to derive the DEX sovereign account.
pub const PALLET_ID: PalletId = PalletId(*b"vtrs/dex");

/// The DEX's own account as the only member of a set: with
/// [`EnsureDexAccount`], the base pallet's `AdminOrigin`. Only this pallet
/// signs as that account, so no runtime origin reaches the base pallet's
/// per-pool fee calls; a pool's fee is what this pallet set at creation.
pub struct DexAccount<T>(core::marker::PhantomData<T>);
impl<T: frame_system::Config> frame_support::traits::SortedMembers<T::AccountId> for DexAccount<T> {
    fn sorted_members() -> sp_std::vec::Vec<T::AccountId> {
        sp_std::vec![PALLET_ID.into_account_truncating()]
    }
}

/// The base pallet's `AdminOrigin`: signed by the DEX's own account.
pub type EnsureDexAccount<T> =
    frame_system::EnsureSignedBy<DexAccount<T>, <T as frame_system::Config>::AccountId>;

/// Denominator for the fee tier, expressed in 10ths of a percent.
pub const FEE_DENOMINATOR: u32 = 1_000;

/// Minimum liquidity permanently locked on first deposit to prevent first-depositor attacks.
pub const MINIMUM_LIQUIDITY: u32 = 1_000;

/// Basis-point denominator for fee routing.
pub const BPS: u32 = 10_000;

/// Smallest whitelisted fee tier (0.1 %). A `create_pool` pool may be this
/// tier, and it carries only the protocol slice (creator and treasury fold
/// into the pool), so `DefaultFeeRouting.protocol_bps ≤ MIN_FEE_TIER × 10`.
pub const MIN_FEE_TIER: u32 = 1;

/// Smallest tier the launchpad may seed a pool at (LAUNCH_TREASURY_SPEC §7.2
/// tightened it from 1 to 3). A seeded pool carries every routed slice.
pub const MIN_LAUNCH_FEE_TIER: u32 = 3;

/// D10: bps of every swap a pool keeps for whoever provided its liquidity,
/// whatever the routing says. Restores the floor §2.6 asked for and §10
/// item 4 dropped: a launch pool is the only venue its token trades on, so
/// routing the whole tier would leave anyone who adds liquidity after
/// graduation earning nothing, and external depth would never arrive.
pub const MIN_POOL_BPS: u16 = 10;

/// D10: what a pool of `fee_tier` must keep — [`MIN_POOL_BPS`], or half the
/// tier where the tier is too small to spare that much. Tier 1 (10 bps)
/// keeps 5, which is what the protocol slice at tier 1 was always sized
/// against; tiers 3 and 10 keep 10, leaving 20 and 90 bps routable.
pub fn pool_floor_bps(fee_tier: u32) -> u16 {
    let tier_bps = fee_tier.saturating_mul(10).min(u16::MAX as u32) as u16;
    MIN_POOL_BPS.min(tier_bps / 2)
}

/// D4 / D9: how a pool's swap fee is split. Snapshotted into [`PoolInfo`] when
/// the pool is created or seeded and never changed afterwards — the same
/// per-launch immutability the launchpad gives its curve terms. The pool
/// keeps `fee_tier × 10 − protocol_bps − creator_bps − treasury_bps` bps.
///
/// Routed slices are always denominated in the native asset (VTRS): taken
/// from the input when VTRS is `asset_in`, from the output when it is
/// `asset_out`. Pools with no native side route nothing.
///
/// D9: the bound is tier-relative and checked where the tier is known
/// (create / seed). D10: it is also floored — a pool keeps at least
/// [`pool_floor_bps`] — and the default is stored per tier, so a default
/// that no pool of that tier could honour cannot be stored (that would
/// fail every launchpad graduation at seed time, D4).
#[derive(
    Clone,
    Copy,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Default,
    Eq,
    PartialEq,
    RuntimeDebug,
    TypeInfo,
    MaxEncodedLen,
)]
pub struct FeeRouting {
    /// Share of every swap sent to the protocol fee recipient, in bps of the swap.
    pub protocol_bps: u16,
    /// Share of every swap accrued for the pool's creator, in bps of the swap.
    pub creator_bps: u16,
    /// D9: share of every swap pushed to the launch's treasury, in bps of the
    /// swap. Zero for pools that predate D9 and for pools with no treasury.
    pub treasury_bps: u16,
}

impl FeeRouting {
    /// `protocol_bps + creator_bps + treasury_bps`.
    pub fn routed_bps(&self) -> u16 {
        self.protocol_bps
            .saturating_add(self.creator_bps)
            .saturating_add(self.treasury_bps)
    }
    /// D10: whether a pool of `fee_tier` may be created with this split —
    /// the routed slices plus the pool's floor must fit the tier. This is
    /// the bound every creation path and the default setter check.
    pub fn is_valid_for(&self, fee_tier: u32) -> bool {
        u32::from(self.routed_bps())
            <= fee_tier.saturating_mul(10).saturating_sub(pool_floor_bps(fee_tier).into())
    }
    /// The arithmetic invariant alone: a pool cannot route more fee than it
    /// charges. Weaker than [`is_valid_for`], which adds the pool's floor;
    /// `try_state` checks this one, because a pool created under an earlier
    /// bound keeps its snapshot and must not trip a later policy.
    pub fn fits_tier(&self, fee_tier: u32) -> bool {
        u32::from(self.routed_bps()) <= fee_tier.saturating_mul(10)
    }
}

/// D4: resolves who may claim a pool's accrued creator share. The DEX has
/// no notion of a creator; the runtime binds this to the launchpad
/// (`AssetToLaunch → Launches[id].creator_fee_recipient`) so there is one
/// source of truth and `set_creator_fee_recipient` needs no propagation.
/// Consulted only at claim time, never inside a swap.
pub trait CreatorFeeRecipient<AssetKind, AccountId> {
    /// The account entitled to `asset`'s creator share, if any.
    fn creator_fee_recipient(asset: &AssetKind) -> Option<AccountId>;
}

impl<AssetKind, AccountId> CreatorFeeRecipient<AssetKind, AccountId> for () {
    fn creator_fee_recipient(_: &AssetKind) -> Option<AccountId> {
        None
    }
}

/// D9: where a launch asset's treasury slice goes. Unlike the protocol and
/// creator slices this one is *pushed* inside the swap, which D4 forbade for
/// those two: the recipient here is a pallet-owned account that always
/// exists and is never user-settable, so the two failures D4 guarded against
/// — a reaped recipient, a mis-set one — cannot occur. The runtime binds this
/// to `pallet_launch_treasury` on testnet and to `()` on mainnet, where every
/// treasury slice folds into the protocol share.
///
/// Both methods are consulted inside `do_swap` and must be O(1).
pub trait TreasurySink<AssetKind, AccountId, Balance> {
    /// The account that receives `asset`'s treasury slice, or `None` to fold
    /// the slice into the protocol share.
    fn account_for(asset: &AssetKind) -> Option<AccountId>;
    /// Called after the transfer so the sink can attribute it. Infallible.
    fn note_fee(asset: &AssetKind, amount: Balance);
}

impl<AssetKind, AccountId, Balance> TreasurySink<AssetKind, AccountId, Balance> for () {
    fn account_for(_: &AssetKind) -> Option<AccountId> {
        None
    }
    fn note_fee(_: &AssetKind, _: Balance) {}
}

/// Our record of a pool: what was fixed for it when governance created it
/// or a launch seeded it. The pool itself — its reserves, its LP tokens and
/// the account that holds them — is in the base pallet, which charges the LP
/// fee this record fixes ([`Pallet::lp_fee`]); this pallet takes the routed
/// slices outside it. The base pool comes into existence with the first
/// deposit, whose depositor pays the deposits that open its accounts.
///
/// **Quoters: price from the pool account's balances** (the base pallet's
/// reserves), and take the routed slices off as the swap does.
#[derive(
    Clone,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    RuntimeDebug,
    TypeInfo,
    MaxEncodedLen,
)]
pub struct PoolInfo<Balance> {
    /// Swap fee tier for this pool, expressed in 10ths of a percent: the
    /// whole fee a swap pays, routed slices included.
    pub fee_tier: u32,
    /// Cumulative fees collected over the pool's lifetime.
    pub total_fees_collected: Balance,
    /// Fee split snapshotted at creation / seeding. Zero for pools with
    /// no native side.
    pub routing: FeeRouting,
}

/// The routed slices of one swap, in native, and where the treasury slice
/// goes (`None`: folded into `protocol`).
struct Slices<Balance, AccountId> {
    protocol: Balance,
    creator: Balance,
    treasury: Balance,
    sink: Option<AccountId>,
}

impl<Balance: sp_runtime::traits::AtLeast32BitUnsigned + Copy, AccountId>
    Slices<Balance, AccountId>
{
    fn none() -> Self {
        Slices { protocol: Zero::zero(), creator: Zero::zero(), treasury: Zero::zero(), sink: None }
    }
    fn total(&self) -> Balance {
        self.protocol.saturating_add(self.creator).saturating_add(self.treasury)
    }
}

/// An exact-input swap, priced: the routed slices, what reaches the pool,
/// what the trader receives, and the pool account it moves through.
struct Priced<Balance, AccountId> {
    slices: Slices<Balance, AccountId>,
    to_pool: Balance,
    amount_out: Balance,
    pool_account: AccountId,
}

/// In-runtime interface for pallets that need to create DEX pools and seed
/// them with liquidity without going through the extrinsic layer.
///
/// None of these methods perform origin checks — the extrinsics in this
/// pallet gate the same logic on `ManageOrigin` / `ensure_signed`, and an
/// implementing pallet is expected to apply its own authorisation before
/// calling. Implemented for [`Pallet<T>`]; bind it in a dependent pallet's
/// `Config` as
/// `type Dex: PoolManager<Self::AccountId, AssetKind, Balance, BlockNumberFor<Self>>`.
pub trait PoolManager<AccountId, AssetKind, Balance, BlockNumber> {
    /// Whether a pool exists for the (unordered) asset pair.
    fn pool_exists(asset_a: AssetKind, asset_b: AssetKind) -> bool;

    /// Create a pool for the pair. `fee_tier` is in 10ths of a percent and
    /// must be one of the whitelisted tiers (1, 3, 10).
    fn create_pool(asset_a: AssetKind, asset_b: AssetKind, fee_tier: u32) -> DispatchResult;

    /// Add liquidity from `who`'s balances, crediting the LP position to
    /// `who`. Returns the LP shares minted. `amount_a`/`amount_a_min` belong
    /// to `asset_a` and `amount_b`/`amount_b_min` to `asset_b` in whatever
    /// order the caller passes them (D5); the pool's canonical order is an
    /// implementation detail.
    fn add_liquidity_for(
        who: &AccountId,
        asset_a: AssetKind,
        asset_b: AssetKind,
        amount_a: Balance,
        amount_b: Balance,
        amount_a_min: Balance,
        amount_b_min: Balance,
    ) -> Result<Balance, DispatchError>;

    /// Extend the lock on `who`'s LP position in the pool to `lock_until`.
    /// A lock can only ever be extended: passing a block earlier than the
    /// current lock fails with `LockCannotBeShortened`.
    fn lock_liquidity_for(
        who: &AccountId,
        asset_a: AssetKind,
        asset_b: AssetKind,
        lock_until: BlockNumber,
    ) -> DispatchResult;

    /// D9: swap exactly `amount_in` of `asset_in` for `asset_out` on behalf
    /// of `who`, delivering to `who`. The extrinsic's body without the
    /// origin check, for a pallet that owns `who` (the launch treasury's
    /// buy-and-burn). Returns the amount out. Not a trade for
    /// [`PoolManager::last_swap_block`]: that clock is for people (R2).
    fn swap_for(
        who: &AccountId,
        asset_in: AssetKind,
        asset_out: AssetKind,
        amount_in: Balance,
        amount_out_min: Balance,
    ) -> Result<Balance, DispatchError>;

    /// D9: the pool's live native-side reserve and the other asset's, as
    /// `(native, other)`, read from balances (the quoting rule on
    /// [`PoolInfo`]); `None` if there is no such pool or no native side.
    fn native_reserves(asset: AssetKind) -> Option<(Balance, Balance)>;

    /// The native pool's total swap fee in bps (tier × 10), for a caller
    /// sizing a trade against the round-trip cost of bracketing it (R7);
    /// `None` if the pool does not exist.
    fn fee_bps(asset: AssetKind) -> Option<u16>;

    /// D9: the last block a person's swap ran against `asset`'s native
    /// pool (`swap_for` does not count), for the treasury's dormancy rule;
    /// `None` if the pool does not exist.
    fn last_swap_block(asset: AssetKind) -> Option<BlockNumber>;
}

/// The one swap the solver marketplace needs to settle an intent, and
/// nothing else: a pallet that binds this cannot create pools, move
/// liquidity or swap in a way that leaves the dormancy clock alone.
///
/// Implemented for [`Pallet<T>`]; bind it in the marketplace's `Config` as
/// `type Dex: IntentSwap<Self::AccountId, AssetKind, Balance>`.
pub trait IntentSwap<AccountId, AssetKind, Balance> {
    /// Swap exactly `amount_in` of `asset_in` held by `who` for at least
    /// `amount_out_min` of `asset_out`, delivered back to `who`. Returns
    /// the amount out. `who` may be spent to zero (it is an escrow, not a
    /// person keeping their ED). Unlike [`PoolManager::swap_for`] this is a
    /// trade for [`PoolManager::last_swap_block`]: a settled intent is
    /// someone trading the token.
    fn swap_exact_for_intent(
        who: &AccountId,
        asset_in: AssetKind,
        asset_out: AssetKind,
        amount_in: Balance,
        amount_out_min: Balance,
    ) -> Result<Balance, DispatchError>;
}

/// What a launch's seed put into its pool: the seeder's LP tokens, frozen
/// for good, and the amounts deposited.
#[derive(Clone, Copy, Eq, PartialEq, RuntimeDebug)]
pub struct SeedOutcome<Balance> {
    /// LP tokens minted to the seeder (the base pallet's locked minimum is
    /// the pool account's, not these).
    pub shares: Balance,
    /// The reserved asset deposited: the amount given, less what the seeder
    /// keeps as its minimum balance where it holds no more than that amount.
    pub asset_seeded: Balance,
    /// The quote deposited.
    pub quote_seeded: Balance,
}

/// The single entry point through which a pool for a *reserved* asset (see
/// [`Config::ReservedAssets`]) can come into existence. Reserved assets are
/// rejected by every other pool-creation path, including `ManageOrigin`.
///
/// Intended for the launchpad pallet's graduation step. The implementation
/// is transactional and, in order:
///
/// 1. requires `asset` to be reserved and `quote` not to be;
/// 2. creates the pool's record, or adopts one whose pool has never held
///    liquidity; a pool with LP tokens is rejected with `PoolAlreadySeeded`;
/// 3. clears whatever the pool account already holds of either asset, so a
///    donation parked at the (predictable) pool address cannot set the
///    opening price: the reserved asset is burned, and the quote is swept to
///    [`Config::ExcessRecipient`]. If the recipient cannot receive it the
///    whole seed fails with `ExcessRecipientCannotReceive` — loudly, so a
///    mis-wired recipient is noticed and fixed, after which seeding can be
///    retried. The reserved asset is not swept: crediting it needs a free
///    consumer reference at the recipient, which anyone can use up, so a
///    sweep there could be made to block every graduation;
/// 4. creates the base pool, `who` paying its accounts' deposits, and makes
///    the first deposit from `who`: `amount_quote` and `amount_asset`, less
///    at most each asset's minimum balance, which the base pallet leaves
///    with a depositor that would otherwise give its last unit;
/// 5. freezes `who`'s LP tokens for good.
///
/// No origin check is performed; only the launchpad binds this trait and the
/// DEX exposes no extrinsic that reaches it.
pub trait ReservedPoolSeeder<AccountId, AssetKind, Balance, BlockNumber> {
    /// The LP tokens credited to `who`, and what was deposited.
    fn seed_reserved_pool_for(
        who: &AccountId,
        asset: AssetKind,
        quote: AssetKind,
        amount_asset: Balance,
        amount_quote: Balance,
        fee_tier: u32,
    ) -> Result<SeedOutcome<Balance>, DispatchError>;
}

#[frame_support::pallet]
pub mod pallet {
    use super::*;
    use frame_support::pallet_prelude::*;
    use frame_system::pallet_prelude::*;

    /// v7: the core on the base pallet. A chain at v4 reaches it through
    /// `migrations::v7::MigrateToV7`, which clears the single-pool core's
    /// storage and carries v5's and v6's steps.
    const STORAGE_VERSION: StorageVersion = StorageVersion::new(7);

    #[pallet::pallet]
    #[pallet::storage_version(STORAGE_VERSION)]
    pub struct Pallet<T>(_);

    #[pallet::config]
    pub trait Config: frame_system::Config {
        /// The origin which can manage parameters of this pallet.
        type ManageOrigin: EnsureOrigin<Self::RuntimeOrigin>;

        /// The type in which the assets for swapping are measured.
        type Balance: Balance;

        /// Wide integer used for every multiply-then-divide in the pool math so
        /// reserve products cannot overflow. Balances are 18-decimal `u128`s, so
        /// a product of two realistic reserves (10^22 × 2·10^26) is ~10^48 and
        /// does not fit `u128`. Bind to `sp_core::U256`, as `pallet_energy_broker`
        /// does. Results are narrowed back with a checked conversion; a value
        /// that does not fit `Balance` is an [`Error::Overflow`], never a
        /// truncation.
        type HigherPrecisionBalance: IntegerSquareRoot
            + One
            + Ensure
            + Unsigned
            + From<u32>
            + From<Self::Balance>
            + TryInto<Self::Balance>;

        /// Type of asset class used to provide liquidity.
        type AssetKind: Parameter + MaxEncodedLen;

        /// Registry of assets utilized for providing liquidity.
        ///
        /// `AccountTouch` opens an asset account on a deposit for a holder
        /// whose consumer references are used up (see
        /// [`Pallet::open_for_credit`]); the runtime's `NativeAndAssets`
        /// (`fungible::UnionOf` of Balances and pallet-assets) provides it.
        type Assets: Inspect<Self::AccountId, AssetId = Self::AssetKind, Balance = Self::Balance>
            + Mutate<Self::AccountId>
            + Balanced<Self::AccountId>
            + AccountTouch<Self::AssetKind, Self::AccountId, Balance = Self::Balance>;

        /// Identifier of native asset.
        #[pallet::constant]
        type NativeAsset: Get<Self::AssetKind>;

        /// Identifier of energy asset.
        #[pallet::constant]
        type EnergyAsset: Get<Self::AssetKind>;

        /// Assets for which pools may only be created through
        /// [`ReservedPoolSeeder::seed_reserved_pool_for`]. `do_create_pool`
        /// rejects them for every caller, `ManageOrigin` included. The
        /// runtime binds this to the launchpad's asset-id range.
        type ReservedAssets: Contains<Self::AssetKind>;

        /// Recipient of any balance found in a reserved pool's sub-account
        /// before it is seeded (see [`ReservedPoolSeeder`]).
        type ExcessRecipient: Get<Self::AccountId>;

        /// D4: where routed protocol fees go while [`ProtocolFeeRecipient`]
        /// is unset. The runtime binds this to the runtime Treasury. The
        /// destination is deliberately storage-settable rather than a
        /// constant: the Treasury is not a store (its extension recycles a
        /// fraction of the balance to staking every spend period), so
        /// governance must be able to redirect revenue without a runtime
        /// upgrade.
        type DefaultProtocolFeeRecipient: Get<Self::AccountId>;

        /// D4: who may claim a pool's creator share. `()` means nobody (no
        /// creators; creator slices then fold into the pool at snapshot).
        type CreatorFeeRecipient: CreatorFeeRecipient<Self::AssetKind, Self::AccountId>;

        /// D9: where a launch asset's treasury slice is pushed. `()` folds
        /// every treasury slice into the protocol share.
        type TreasurySink: TreasurySink<Self::AssetKind, Self::AccountId, Self::Balance>;

        /// The base pallet: pools, pricing, liquidity and LP tokens (see
        /// [`crate::base`]). The runtime binds it to
        /// [`AssetConversionAdapter`] over pallet-asset-conversion configured
        /// with these `Assets`, and that pallet's `AdminOrigin` to
        /// [`EnsureDexAccount`].
        type Base: AmmBase<
            Self::AccountId,
            Self::AssetKind,
            Self::Balance,
            Assets = Self::Assets,
            FreezeId = Self::RuntimeFreezeReason,
        >;

        /// The runtime's freeze reasons, which include this pallet's: the
        /// names of the freezes on LP tokens.
        type RuntimeFreezeReason: From<FreezeReason>;

        /// Weight information for the extrinsics of this pallet.
        type WeightInfo: WeightInfo;

        /// Supplies non-native asset identifiers for benchmarks.
        #[cfg(feature = "runtime-benchmarks")]
        type BenchmarkHelper: BenchmarkHelper<Self::AssetKind, Self::AccountId>;
    }

    /// All known pools keyed by their canonical ordered asset pair.
    #[pallet::storage]
    pub type Pools<T: Config> =
        StorageMap<_, Blake2_128Concat, (T::AssetKind, T::AssetKind), PoolInfo<T::Balance>>;

    /// D4: fee split applied to pools created or seeded from now on, keyed
    /// by fee tier (D10). Changing it never touches an existing pool (each
    /// pool carries its own snapshot).
    ///
    /// Absent means *not configured for that tier*, which is not the same as
    /// zero: a launchpad seed at an unconfigured tier is refused
    /// ([`Error::NoDefaultFeeRouting`]) rather than graduating a pool that
    /// silently routes nothing to its treasury. A `create_pool` pool at an
    /// unconfigured tier routes nothing, as it did before any default was set.
    #[pallet::storage]
    pub type DefaultFeeRouting<T: Config> =
        StorageMap<_, Twox64Concat, u32, FeeRouting, OptionQuery>;

    /// D4: where protocol fees are paid on `withdraw_protocol_fees`. `None`
    /// means `T::DefaultProtocolFeeRecipient` (the runtime Treasury).
    #[pallet::storage]
    pub type ProtocolFeeRecipient<T: Config> = StorageValue<_, T::AccountId, OptionQuery>;

    /// D4: native-asset creator fees accrued per pool, held in the fee
    /// escrow sub-account until `claim_pool_creator_fees`.
    #[pallet::storage]
    pub type CreatorFeesUnclaimed<T: Config> =
        StorageMap<_, Blake2_128Concat, (T::AssetKind, T::AssetKind), T::Balance, ValueQuery>;

    /// D4: native-asset protocol fees accrued across all pools, held in the
    /// fee escrow sub-account until `withdraw_protocol_fees`.
    #[pallet::storage]
    pub type ProtocolFeesUnclaimed<T: Config> = StorageValue<_, T::Balance, ValueQuery>;

    /// Why this pallet freezes LP tokens.
    #[pallet::composite_enum]
    pub enum FreezeReason {
        /// A lock taken with `lock_liquidity`, until its block
        /// ([`LiquidityLocks`]). It covers the whole position, liquidity
        /// added while it is in force included.
        LiquidityLock,
        /// A launch's seed: frozen for good. Nothing lifts it.
        LaunchSeed,
    }

    /// The block until which an account's position in a pool (canonical
    /// pair) is locked. While in force, its LP tokens in the pool are frozen
    /// ([`FreezeReason::LiquidityLock`]); once past, the next add or remove
    /// lifts the freeze and drops the record.
    #[pallet::storage]
    pub type LiquidityLocks<T: Config> = StorageDoubleMap<
        _,
        Blake2_128Concat,
        (T::AssetKind, T::AssetKind),
        Blake2_128Concat,
        T::AccountId,
        BlockNumberFor<T>,
        OptionQuery,
    >;

    /// D9: the last block a swap ran against each pool (canonical pair), for
    /// the launch treasury's dormancy rule. Kept beside `PoolInfo` rather
    /// than in it so the pool record's shape — which every quoter decodes —
    /// does not change for a field only one reader wants.
    #[pallet::storage]
    pub type LastSwapBlock<T: Config> = StorageMap<
        _,
        Blake2_128Concat,
        (T::AssetKind, T::AssetKind),
        BlockNumberFor<T>,
        OptionQuery,
    >;

    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        /// A new pool has been created.
        PoolCreated {
            /// First asset in the pair.
            asset_a: T::AssetKind,
            /// Second asset in the pair.
            asset_b: T::AssetKind,
            /// Fee tier for the new pool (10ths of a percent).
            fee_tier: u32,
        },
        /// Liquidity has been added to a pool.
        LiquidityAdded {
            /// Account providing the liquidity.
            provider: T::AccountId,
            /// First asset in the pair.
            asset_a: T::AssetKind,
            /// Second asset in the pair.
            asset_b: T::AssetKind,
            /// Amount of `asset_a` deposited.
            amount_a: T::Balance,
            /// Amount of `asset_b` deposited.
            amount_b: T::Balance,
            /// LP shares minted to the provider.
            shares_minted: T::Balance,
        },
        /// Liquidity has been removed from a pool.
        LiquidityRemoved {
            /// Account withdrawing the liquidity.
            provider: T::AccountId,
            /// First asset in the pair.
            asset_a: T::AssetKind,
            /// Second asset in the pair.
            asset_b: T::AssetKind,
            /// Amount of `asset_a` returned.
            amount_a: T::Balance,
            /// Amount of `asset_b` returned.
            amount_b: T::Balance,
            /// LP shares burned from the provider.
            shares_burned: T::Balance,
        },
        /// A swap has been executed against a pool.
        SwapExecuted {
            /// Originator of the swap.
            who: T::AccountId,
            /// Input asset.
            asset_in: T::AssetKind,
            /// Output asset.
            asset_out: T::AssetKind,
            /// Amount of `asset_in` consumed.
            amount_in: T::Balance,
            /// Amount of `asset_out` produced.
            amount_out: T::Balance,
            /// Fee charged on this swap.
            fee: T::Balance,
        },
        /// Accumulated fees have been collected from a pool.
        FeesCollected {
            /// Pool whose fees were collected.
            pool: (T::AssetKind, T::AssetKind),
            /// Total fee amount paid out.
            amount: T::Balance,
            /// Beneficiary of the collected fees.
            recipient: T::AccountId,
        },
        /// D4: routed slices of a swap fee were moved to the fee escrow.
        FeesRouted {
            /// Pool the swap ran against (canonical order).
            pool: (T::AssetKind, T::AssetKind),
            /// Native amount accrued for the protocol.
            protocol: T::Balance,
            /// Native amount accrued for the pool's creator.
            creator: T::Balance,
            /// D9: native amount pushed to the launch treasury (or folded
            /// into `protocol` when there is no sink for the asset).
            treasury: T::Balance,
        },
        /// D4: a creator claimed their accrued share for a pool.
        CreatorFeesClaimed {
            /// The pool (canonical order).
            pool: (T::AssetKind, T::AssetKind),
            /// Who was paid.
            recipient: T::AccountId,
            /// Native amount paid.
            amount: T::Balance,
        },
        /// D4: accrued protocol fees were paid to the current recipient.
        ProtocolFeesWithdrawn {
            /// Who was paid.
            recipient: T::AccountId,
            /// Native amount paid.
            amount: T::Balance,
        },
        /// D4: governance changed the split for pools created from now on.
        DefaultFeeRoutingSet {
            /// D10: the fee tier this default applies to.
            fee_tier: u32,
            /// The new default for that tier.
            routing: FeeRouting,
        },
        /// D4: governance changed where protocol fees are paid.
        ProtocolFeeRecipientSet {
            /// The new recipient; `None` restores the runtime default.
            recipient: Option<T::AccountId>,
        },
        /// A liquidity position has been locked until a given block.
        LiquidityLocked {
            /// The account that locked the position.
            who: T::AccountId,
            /// The pool pair.
            pool: (T::AssetKind, T::AssetKind),
            /// The block until which the position is locked.
            locked_until: BlockNumberFor<T>,
        },
        /// A balance found in a reserved pool's sub-account before seeding was
        /// moved to `ExcessRecipient` so it could not affect the opening price.
        PreSeedBalanceSwept {
            /// The pool pair (canonical order).
            pool: (T::AssetKind, T::AssetKind),
            /// The asset that was swept.
            asset: T::AssetKind,
            /// Amount moved out of the pool sub-account.
            amount: T::Balance,
            /// Where it went.
            to: T::AccountId,
        },
        /// A reserved-asset pool was seeded through `ReservedPoolSeeder` and
        /// its first position locked permanently.
        ReservedPoolSeeded {
            /// Account whose balances funded the seed and who owns the locked position.
            who: T::AccountId,
            /// The reserved asset.
            asset: T::AssetKind,
            /// The quote asset.
            quote: T::AssetKind,
            /// Reserved-asset amount deposited.
            amount_asset: T::Balance,
            /// Quote amount deposited.
            amount_quote: T::Balance,
            /// LP shares credited to `who`.
            shares: T::Balance,
        },
        /// A pre-seed balance of the reserved asset in its pool's
        /// sub-account was burned, so it could not affect the opening price.
        /// Unlike other assets it is not swept to `ExcessRecipient`: that
        /// credit can be made to fail, and it would block the graduation.
        PreSeedBalanceBurned {
            /// The pool pair (canonical order).
            pool: (T::AssetKind, T::AssetKind),
            /// The reserved asset.
            asset: T::AssetKind,
            /// Amount burned.
            amount: T::Balance,
        },
    }

    #[pallet::error]
    pub enum Error<T> {
        /// A pool already exists for this asset pair.
        PoolAlreadyExists,
        /// No pool exists for this asset pair.
        PoolNotFound,
        /// The pool does not hold enough liquidity to satisfy the operation.
        InsufficientLiquidity,
        /// The caller does not own enough LP shares.
        InsufficientShares,
        /// Calculated output falls outside the caller's slippage bounds.
        SlippageExceeded,
        /// The position or pool is currently locked.
        PoolLocked,
        /// The provided fee tier is not accepted.
        InvalidFeeTier,
        /// Amount can't be zero.
        ZeroAmount,
        /// An overflow happened.
        Overflow,
        /// Initial liquidity deposit is too small to exceed MINIMUM_LIQUIDITY.
        InsufficientInitialLiquidity,
        /// The pair contains a reserved asset; such pools can only be created
        /// through `ReservedPoolSeeder::seed_reserved_pool_for`.
        ReservedAsset,
        /// `seed_reserved_pool_for` was called for an asset that is not
        /// reserved, or with a reserved asset as the quote.
        NotReservedAsset,
        /// The reserved pool already holds liquidity and cannot be seeded again.
        PoolAlreadySeeded,
        /// A liquidity lock can be extended but never shortened.
        LockCannotBeShortened,
        /// A pre-seed balance in the pool sub-account could not be delivered to
        /// `ExcessRecipient` (for example, the recipient has no provider and the
        /// asset is not sufficient). Fix the recipient and retry the seed.
        ExcessRecipientCannotReceive,
        /// D4 / D9 / D10: the split is more than the pool's tier can carry
        /// once the pool's own floor is kept.
        InvalidFeeRouting,
        /// D10: no default routing is configured for this tier, so a
        /// launchpad seed cannot snapshot one. Governance must set the tier
        /// before a launch at it can graduate (FM-11: a deferred graduation,
        /// retried by `graduate` once the default exists).
        NoDefaultFeeRouting,
        /// D4: no creator is known for this asset, so it has no creator share.
        NoCreatorForAsset,
        /// D4: the caller is not the asset's creator fee recipient.
        NotCreatorFeeRecipient,

        /// Swap output did not meet the user's slippage bound.
        SlippageProtectionFailed,
        /// D11: `swap_exact_tokens_for_tokens` pays out to the signer only.
        RecipientNotSigner,
        /// The withdrawal or swap would leave the pool holding less of an
        /// asset than its minimum balance; the asset's pallet would pay that
        /// remainder out with it. Take a little less.
        ReserveBelowMinimum,
        /// The recipient cannot take what the swap or withdrawal pays it:
        /// less than the asset's minimum balance for an account that does
        /// not hold it, or no consumer reference left to open one.
        RecipientCannotReceive,
    }

    #[pallet::hooks]
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        #[cfg(feature = "try-runtime")]
        fn try_state(_n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
            Self::do_try_state()
        }
    }

    /// Finding 14, from-genesis path: endow the fee escrow with the native ED so
    /// it exists before the first swap and every routed protocol/creator slice —
    /// even one below ED — reaches it rather than being left in the pool. On a
    /// chain that receives this pallet by upgrade, `migrations::v6` does the
    /// same; the `do_swap` guard makes correctness independent of either.
    #[pallet::genesis_config]
    #[derive(frame_support::DefaultNoBound)]
    pub struct GenesisConfig<T: Config> {
        /// No configurable fields; the build endows the fee escrow with the ED.
        #[serde(skip)]
        pub _marker: core::marker::PhantomData<T>,
    }

    #[pallet::genesis_build]
    impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
        fn build(&self) {
            Pallet::<T>::mint_ed_at_genesis(&Pallet::<T>::fee_escrow_account());
        }
    }

    /// Quotes, routing included, from the swap's own pricing. The base
    /// pallet's quotes know only the LP fee and overstate what our pools pay;
    /// the runtime refuses those two and serves these.
    #[pallet::view_functions]
    impl<T: Config> Pallet<T> {
        /// What `swap_exact_tokens_for_tokens` with exactly `amount_in` of
        /// `asset_in` would deliver of `asset_out` now, after the routed
        /// slices and the pool's fee. `None` where the swap would fail: no
        /// pool or liquidity, an output that rounds to nothing, or more than
        /// the pool can pay.
        pub fn quote_exact_tokens_for_tokens(
            asset_in: T::AssetKind,
            asset_out: T::AssetKind,
            amount_in: T::Balance,
        ) -> Option<T::Balance> {
            Self::quote_exact_in(asset_in, asset_out, amount_in)
        }

        /// The input of `asset_in` for which `swap_exact_tokens_for_tokens`
        /// would deliver at least `amount_out` of `asset_out` now: the
        /// smallest one, in that one unit less delivers less. `None` where no
        /// input can (more than the pool can pay) or there is no pool.
        pub fn quote_tokens_for_exact_tokens(
            asset_in: T::AssetKind,
            asset_out: T::AssetKind,
            amount_out: T::Balance,
        ) -> Option<T::Balance> {
            Self::quote_exact_out(asset_in, asset_out, amount_out)
        }
    }

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        /// Create a new AMM pool for the given asset pair and fee tier.
        #[pallet::call_index(0)]
        #[pallet::weight(<T as Config>::WeightInfo::create_pool())]
        pub fn create_pool(
            origin: OriginFor<T>,
            asset_a: T::AssetKind,
            asset_b: T::AssetKind,
            fee_tier: u32,
        ) -> DispatchResult {
            T::ManageOrigin::ensure_origin(origin)?;
            Self::do_create_pool(asset_a, asset_b, fee_tier)
        }

        /// Add liquidity to an existing pool.
        #[pallet::call_index(1)]
        #[pallet::weight(<T as Config>::WeightInfo::add_liquidity())]
        pub fn add_liquidity(
            origin: OriginFor<T>,
            asset_a: T::AssetKind,
            asset_b: T::AssetKind,
            amount_a: T::Balance,
            amount_b: T::Balance,
            amount_a_min: T::Balance,
            amount_b_min: T::Balance,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::do_add_liquidity_for(
                &who,
                asset_a,
                asset_b,
                amount_a,
                amount_b,
                amount_a_min,
                amount_b_min,
            )?;
            Ok(())
        }

        /// Remove liquidity from an existing pool: burn `shares` of the
        /// signer's LP tokens for their share of the reserves.
        #[pallet::call_index(2)]
        #[pallet::weight(<T as Config>::WeightInfo::remove_liquidity())]
        pub fn remove_liquidity(
            origin: OriginFor<T>,
            asset_a: T::AssetKind,
            asset_b: T::AssetKind,
            shares: T::Balance,
            amount_a_min: T::Balance,
            amount_b_min: T::Balance,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::do_remove_liquidity_for(
                &who,
                asset_a,
                asset_b,
                shares,
                amount_a_min,
                amount_b_min,
            )?;
            Ok(())
        }

        /// Swap an exact amount of `asset_in` for as much `asset_out` as the pool yields.
        ///
        /// D11: `recipient` must be the signer (`RecipientNotSigner` otherwise).
        /// The argument stays for encoding stability; dropping it belongs in a
        /// planned breaking release. A swap that pays out elsewhere let a
        /// delegate (a proxy) send the account's funds to itself, which a
        /// runtime proxy filter cannot prevent — it never sees the account it
        /// acts for — and `SwapExecuted` carries no recipient, so such a payout
        /// was already invisible to anything reading events.
        #[pallet::call_index(3)]
        #[pallet::weight(<T as Config>::WeightInfo::swap_exact_tokens_for_tokens())]
        pub fn swap_exact_tokens_for_tokens(
            origin: OriginFor<T>,
            asset_in: T::AssetKind,
            asset_out: T::AssetKind,
            amount_in: T::Balance,
            amount_out_min: T::Balance,
            recipient: T::AccountId,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            ensure!(recipient == who, Error::<T>::RecipientNotSigner);
            // A person whose consumer references are used up can still buy
            // a token they do not hold, on their own deposit.
            Self::open_for_credit(asset_out.clone(), &who, &who)?;
            Self::do_swap(
                &who,
                asset_in,
                asset_out,
                amount_in,
                amount_out_min,
                &recipient,
                true,
                Preserve,
            )?;
            Ok(())
        }

        /// Lock a liquidity position until a given block, or extend an
        /// existing lock. A lock can never be shortened (D3): passing a block
        /// earlier than the current lock fails with `LockCannotBeShortened`.
        ///
        /// Finding 10: activates the previously dead `locked_until` field.
        #[pallet::call_index(4)]
        #[pallet::weight(<T as Config>::WeightInfo::lock_liquidity())]
        pub fn lock_liquidity(
            origin: OriginFor<T>,
            asset_a: T::AssetKind,
            asset_b: T::AssetKind,
            lock_until: BlockNumberFor<T>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::do_lock_liquidity_for(&who, asset_a, asset_b, lock_until)
        }

        // ---- D4: fee routing ------------------------------------------------

        /// D4: set the fee split for pools created or seeded from now on.
        /// Gated on `ManageOrigin`. Never changes an existing pool's split.
        /// D10: per tier, and checked against *that* tier
        /// ([`FeeRouting::is_valid_for`]), so a 1 % pool may route what a
        /// 0.3 % pool cannot. Tiers below [`MIN_LAUNCH_FEE_TIER`] can carry
        /// no creator or treasury slice: the launchpad cannot seed them, so
        /// no pool at that tier has either party.
        #[pallet::call_index(16)]
        #[pallet::weight(<T as Config>::WeightInfo::set_default_fee_routing())]
        pub fn set_default_fee_routing(
            origin: OriginFor<T>,
            fee_tier: u32,
            protocol_bps: u16,
            creator_bps: u16,
            treasury_bps: u16,
        ) -> DispatchResult {
            T::ManageOrigin::ensure_origin(origin)?;
            Self::ensure_valid_fee_tier(fee_tier)?;
            let routing = FeeRouting { protocol_bps, creator_bps, treasury_bps };
            ensure!(routing.is_valid_for(fee_tier), Error::<T>::InvalidFeeRouting);
            ensure!(
                fee_tier >= MIN_LAUNCH_FEE_TIER
                    || (routing.creator_bps == 0 && routing.treasury_bps == 0),
                Error::<T>::InvalidFeeRouting
            );
            DefaultFeeRouting::<T>::insert(fee_tier, routing);
            Self::deposit_event(Event::DefaultFeeRoutingSet { fee_tier, routing });
            Ok(())
        }

        /// D4: set where protocol fees are paid; `None` restores the runtime
        /// default (the Treasury). Gated on `ManageOrigin`. Takes effect on the
        /// next `withdraw_protocol_fees`; fees already accrued follow it.
        #[pallet::call_index(17)]
        #[pallet::weight(<T as Config>::WeightInfo::set_protocol_fee_recipient())]
        pub fn set_protocol_fee_recipient(
            origin: OriginFor<T>,
            recipient: Option<T::AccountId>,
        ) -> DispatchResult {
            T::ManageOrigin::ensure_origin(origin)?;
            match &recipient {
                Some(r) => ProtocolFeeRecipient::<T>::put(r.clone()),
                None => ProtocolFeeRecipient::<T>::kill(),
            }
            Self::deposit_event(Event::ProtocolFeeRecipientSet { recipient });
            Ok(())
        }

        /// D4: pay the accrued creator share of `asset`'s native-quoted pool
        /// to its creator fee recipient. Only that recipient, as resolved by
        /// `T::CreatorFeeRecipient` at call time, may call.
        #[pallet::call_index(18)]
        #[pallet::weight(<T as Config>::WeightInfo::claim_pool_creator_fees())]
        pub fn claim_pool_creator_fees(
            origin: OriginFor<T>,
            asset: T::AssetKind,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let recipient = T::CreatorFeeRecipient::creator_fee_recipient(&asset)
                .ok_or(Error::<T>::NoCreatorForAsset)?;
            ensure!(who == recipient, Error::<T>::NotCreatorFeeRecipient);
            let pair = Self::canonical_pair(asset, T::NativeAsset::get());
            let amount = CreatorFeesUnclaimed::<T>::take(&pair);
            ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
            T::Assets::transfer(
                T::NativeAsset::get(),
                &Self::fee_escrow_account(),
                &who,
                amount,
                Expendable,
            )?;
            Self::deposit_event(Event::CreatorFeesClaimed { pool: pair, recipient: who, amount });
            Ok(())
        }

        /// D4: pay every accrued protocol fee to the current protocol fee
        /// recipient. Permissionless: revenue is pulled, so a recipient that
        /// cannot receive can never block a swap.
        #[pallet::call_index(19)]
        #[pallet::weight(<T as Config>::WeightInfo::withdraw_protocol_fees())]
        pub fn withdraw_protocol_fees(origin: OriginFor<T>) -> DispatchResult {
            let _ = ensure_signed(origin)?;
            let amount = ProtocolFeesUnclaimed::<T>::take();
            ensure!(!amount.is_zero(), Error::<T>::ZeroAmount);
            let recipient = Self::protocol_fee_recipient();
            T::Assets::transfer(
                T::NativeAsset::get(),
                &Self::fee_escrow_account(),
                &recipient,
                amount,
                Expendable,
            )?;
            Self::deposit_event(Event::ProtocolFeesWithdrawn { recipient, amount });
            Ok(())
        }
    }

    impl<T: Config> Pallet<T> {
        /// What the pallet promised, checked against what it holds:
        ///
        /// - the fee escrow holds its ED and, above it, every unclaimed
        ///   protocol and creator fee;
        /// - every pool routes no more than its tier charges;
        /// - every pool the base pallet holds for a record of ours charges
        ///   the LP fee that record fixes. Nothing can change it there: the
        ///   base pallet's fee calls answer only to this pallet's account.
        #[cfg(any(feature = "try-runtime", feature = "test-utils", test))]
        pub fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
            use frame_support::ensure;

            let native = T::NativeAsset::get();
            let fees = CreatorFeesUnclaimed::<T>::iter_values()
                .fold(ProtocolFeesUnclaimed::<T>::get(), |a, f| a.saturating_add(f));
            let ed = Self::native_ed();
            ensure!(
                T::Assets::balance(native, &Self::fee_escrow_account()) >= fees.saturating_add(ed),
                "the fee escrow holds less than its ED and the fees it owes"
            );

            for (pair, pool) in Pools::<T>::iter() {
                ensure!(pool.routing.fits_tier(pool.fee_tier), "a pool routes more than its tier");
                if let Some(fee) = T::Base::pool_fee(&pair.0, &pair.1) {
                    ensure!(
                        fee == Self::lp_fee(pool.fee_tier, &pool.routing),
                        "a pool's LP fee differs from the one its record fixes"
                    );
                }
            }
            Ok(())
        }

        /// The account ID of the Vitreus DEX.
        pub fn account_id() -> T::AccountId {
            AccountIdConversion::<T::AccountId>::into_account_truncating(&PALLET_ID)
        }

        /// D4: the sub-account that holds routed fees until they are claimed
        /// or withdrawn. Separate from every pool account, so no routed fee
        /// is ever counted as depth.
        pub fn fee_escrow_account() -> T::AccountId {
            PALLET_ID.into_sub_account_truncating(b"fees")
        }

        /// D4: where protocol fees are paid right now — the storage override
        /// or the runtime default. Also read by the launchpad's `Treasury`
        /// binding so all protocol revenue lands in one place.
        pub fn protocol_fee_recipient() -> T::AccountId {
            ProtocolFeeRecipient::<T>::get().unwrap_or_else(T::DefaultProtocolFeeRecipient::get)
        }

        /// D10: the split a new pool of `fee_tier` snapshots. A pool with no
        /// native side routes nothing. A seeded pool takes the tier's default
        /// whole and **fails** if that tier has none — governance setting one
        /// tier and forgetting another must not graduate a launch whose
        /// treasury would then be fed nothing, for the life of the pool. A
        /// `create_pool` pool takes the protocol slice alone (nobody could
        /// claim a creator or treasury slice), and nothing at all
        /// where the tier is unconfigured.
        fn routing_for_new_pool(
            pair: &(T::AssetKind, T::AssetKind),
            fee_tier: u32,
            seeded: bool,
        ) -> Result<FeeRouting, Error<T>> {
            let native = T::NativeAsset::get().encode();
            if pair.0.encode() != native && pair.1.encode() != native {
                return Ok(FeeRouting::default());
            }
            match (DefaultFeeRouting::<T>::get(fee_tier), seeded) {
                (Some(d), true) => Ok(d),
                (Some(d), false) => {
                    Ok(FeeRouting { protocol_bps: d.protocol_bps, creator_bps: 0, treasury_bps: 0 })
                },
                (None, true) => Err(Error::<T>::NoDefaultFeeRouting),
                (None, false) => Ok(FeeRouting::default()),
            }
        }

        /// The fee the base pool charges on every swap, which its LPs keep.
        /// The routed slices are taken outside the pool, in native, so the
        /// pool charges what the tier leaves once they are out:
        /// `(t − r) / (1 − r)` for tier `t` and routed share `r`. A native-in
        /// swap then reaches the curve with `x · (1 − t)`, as when one
        /// formula charged the whole tier. Rounded up to the next part per
        /// million, so LPs never keep less than the tier leaves them; the
        /// trader gives up at most that part per million of the output.
        pub fn lp_fee(fee_tier: u32, routing: &FeeRouting) -> Permill {
            let tier = fee_tier.saturating_mul(10).min(BPS);
            let routed = u32::from(routing.routed_bps()).min(tier);
            Permill::from_rational_with_rounding(tier - routed, BPS - routed, Rounding::Up)
                .unwrap_or_else(|_| Permill::one())
        }

        /// `floor(amount × bps / BPS)`.
        fn bps_of(amount: T::Balance, bps: u16) -> Result<T::Balance, Error<T>> {
            if bps == 0 {
                return Ok(Zero::zero());
            }
            Self::mul_div_floor(amount, (bps as u32).into(), BPS.into())
        }

        /// Finding 5: canonicalize a pair so that the lexicographically smaller
        /// encoded asset comes first. Our records are keyed by it; the base
        /// pallet orders a pair its own way, and takes either.
        pub fn canonical_pair(a: T::AssetKind, b: T::AssetKind) -> (T::AssetKind, T::AssetKind) {
            if a.encode() <= b.encode() {
                (a, b)
            } else {
                (b, a)
            }
        }

        /// D5: canonicalize a pair together with two per-asset values given in
        /// the caller's order (amounts, minimums). Returns the canonical pair
        /// and the values in that same order, so `.1` always belongs to `.0`'s
        /// asset.
        pub fn canonical_pair_with<V>(
            a: T::AssetKind,
            b: T::AssetKind,
            x_a: V,
            x_b: V,
        ) -> ((T::AssetKind, T::AssetKind), V, V) {
            if a.encode() <= b.encode() {
                ((a, b), x_a, x_b)
            } else {
                ((b, a), x_b, x_a)
            }
        }

        /// The native existential deposit.
        pub(crate) fn native_ed() -> T::Balance {
            <T::Assets as Inspect<T::AccountId>>::minimum_balance(T::NativeAsset::get())
        }

        /// Whether a credit of `amount` of `asset` would land on `who`, as
        /// the asset's own pallet judges it (`can_deposit`). For native that
        /// is the ED rule, which does not care whether the account exists:
        /// one kept alive by a sufficient asset has no native and still
        /// cannot take less than an ED. For a token it is the minimum balance
        /// and the consumer references a new holder needs.
        pub(crate) fn can_credit(
            asset: T::AssetKind,
            who: &T::AccountId,
            amount: T::Balance,
        ) -> bool {
            T::Assets::can_deposit(asset, who, amount, Provenance::Extant)
                == DepositConsequence::Success
        }

        /// Make sure `who` can be credited `asset` without a free consumer
        /// reference: if pallet-assets would refuse to open the account the
        /// usual way (no room for two more references, or no provider),
        /// open it on a deposit paid by `payer`. A deposit-backed account
        /// needs no consumer reference or provider and stays open at zero
        /// balance. Native and sufficient assets never need it.
        ///
        /// `payer` must be the signer of the extrinsic being served, never
        /// an account the pallet owns: a deposit reserved there would no
        /// longer match what the pallet records it holds.
        pub fn open_for_credit(
            asset: T::AssetKind,
            who: &T::AccountId,
            payer: &T::AccountId,
        ) -> DispatchResult {
            if T::Assets::should_touch(asset.clone(), who) {
                T::Assets::touch(asset, who, payer)?;
            }
            Ok(())
        }

        /// Mint the native ED into `who` if it holds less, at genesis only:
        /// on a live chain an endowment is paid by a source account, so
        /// issuance does not change. Returns whether it minted.
        pub(crate) fn mint_ed_at_genesis(who: &T::AccountId) -> bool {
            let native = T::NativeAsset::get();
            if T::Assets::balance(native.clone(), who) >= Self::native_ed() {
                return false;
            }
            match T::Assets::mint_into(native, who, Self::native_ed()) {
                Ok(_) => true,
                Err(e) => {
                    log::error!(target: "runtime::vitreus-dex", "could not endow {who:?}: {e:?}");
                    false
                },
            }
        }

        /// `floor(a * b / c)` in wide precision. Errors with `Overflow`
        /// if the result does not fit `Balance`, and with
        /// `InsufficientLiquidity` if `c == 0`.
        fn mul_div_floor(
            a: T::Balance,
            b: T::Balance,
            c: T::Balance,
        ) -> Result<T::Balance, Error<T>> {
            let wide = T::HigherPrecisionBalance::from;
            let q = wide(a)
                .checked_mul(&wide(b))
                .ok_or(Error::<T>::Overflow)?
                .checked_div(&wide(c))
                .ok_or(Error::<T>::InsufficientLiquidity)?;
            q.try_into().map_err(|_| Error::<T>::Overflow)
        }

        /// Whether a pool exists for the (unordered) asset pair: our record,
        /// whether or not its first deposit has created it in the base pallet.
        pub fn pool_exists(asset_a: T::AssetKind, asset_b: T::AssetKind) -> bool {
            Pools::<T>::contains_key(Self::canonical_pair(asset_a, asset_b))
        }

        /// Create a new AMM pool for the given asset pair and fee tier.
        ///
        /// Origin-free body of the `create_pool` extrinsic. The extrinsic gates
        /// this on `ManageOrigin`; in-runtime callers (see [`PoolManager`]) are
        /// trusted to apply their own authorisation before calling. Writes
        /// our record; the base pool follows with the first deposit.
        pub fn do_create_pool(
            asset_a: T::AssetKind,
            asset_b: T::AssetKind,
            fee_tier: u32,
        ) -> DispatchResult {
            // D2: a reserved asset can only get a pool through
            // `seed_reserved_pool_for`. No caller is exempt.
            ensure!(
                !T::ReservedAssets::contains(&asset_a) && !T::ReservedAssets::contains(&asset_b),
                Error::<T>::ReservedAsset
            );
            ensure!(asset_a.encode() != asset_b.encode(), Error::<T>::PoolNotFound);
            Self::ensure_valid_fee_tier(fee_tier)?;

            let pair = Self::canonical_pair(asset_a, asset_b);
            ensure!(!Pools::<T>::contains_key(&pair), Error::<T>::PoolAlreadyExists);

            // D4: no creator can exist for a governance-created pool.
            let routing = Self::routing_for_new_pool(&pair, fee_tier, false)?;
            Self::insert_new_pool(&pair, fee_tier, routing)?;
            Ok(())
        }

        /// Finding 4: whitelist allowed fee tiers (0.1%, 0.3%, 1.0%).
        fn ensure_valid_fee_tier(fee_tier: u32) -> DispatchResult {
            ensure!(fee_tier == 1 || fee_tier == 3 || fee_tier == 10, Error::<T>::InvalidFeeTier);
            Ok(())
        }

        /// The account that holds the pair's reserves, whether or not the
        /// pool exists yet: the base pallet's address for it, a hash of its
        /// pallet id and the pair. `None` for a pair of one asset.
        pub fn pool_account_for(
            asset_a: T::AssetKind,
            asset_b: T::AssetKind,
        ) -> Option<T::AccountId> {
            T::Base::pool_account(&asset_a, &asset_b)
        }

        /// Write a fresh pool record for an already-canonical `pair`. Callers
        /// must have checked that no pool exists. The snapshotted split
        /// must fit the tier (`InvalidFeeRouting` otherwise — at seed time
        /// that surfaces as a deferred graduation).
        fn insert_new_pool(
            pair: &(T::AssetKind, T::AssetKind),
            fee_tier: u32,
            routing: FeeRouting,
        ) -> DispatchResult {
            ensure!(routing.is_valid_for(fee_tier), Error::<T>::InvalidFeeRouting);
            Pools::<T>::insert(
                pair,
                PoolInfo { fee_tier, total_fees_collected: Zero::zero(), routing },
            );
            Self::deposit_event(Event::PoolCreated {
                asset_a: pair.0.clone(),
                asset_b: pair.1.clone(),
                fee_tier,
            });
            Ok(())
        }

        /// The base pool for `pair`, created if it does not exist yet at the
        /// LP fee `pool` fixes, with `creator` paying the deposits that open
        /// its accounts. The base pallet's calls are not in the runtime, so
        /// no base pool exists without a record of ours.
        fn ensure_base_pool(
            pair: &(T::AssetKind, T::AssetKind),
            pool: &PoolInfo<T::Balance>,
            creator: &T::AccountId,
        ) -> DispatchResult {
            if !T::Base::pool_exists(&pair.0, &pair.1) {
                T::Base::create_pool(
                    creator,
                    pair.0.clone(),
                    pair.1.clone(),
                    Self::lp_fee(pool.fee_tier, &pool.routing),
                )?;
            }
            Ok(())
        }

        /// Body of [`ReservedPoolSeeder::seed_reserved_pool_for`]; see the
        /// trait docs for the contract. Transactional: any failure leaves no
        /// trace, including the sweep.
        #[frame_support::transactional]
        pub fn do_seed_reserved_pool_for(
            who: &T::AccountId,
            asset: T::AssetKind,
            quote: T::AssetKind,
            amount_asset: T::Balance,
            amount_quote: T::Balance,
            fee_tier: u32,
        ) -> Result<SeedOutcome<T::Balance>, DispatchError> {
            ensure!(
                amount_asset > Zero::zero() && amount_quote > Zero::zero(),
                Error::<T>::ZeroAmount
            );
            // Only reserved assets come through here, and only against a
            // non-reserved quote, so this path cannot be used to make
            // arbitrary frozen pools around `ManageOrigin`.
            ensure!(T::ReservedAssets::contains(&asset), Error::<T>::NotReservedAsset);
            ensure!(!T::ReservedAssets::contains(&quote), Error::<T>::NotReservedAsset);
            ensure!(asset.encode() != quote.encode(), Error::<T>::NotReservedAsset);
            Self::ensure_valid_fee_tier(fee_tier)?;

            // 1. Create the record, or adopt one whose pool never held liquidity.
            let pair = Self::canonical_pair(asset.clone(), quote.clone());
            match Pools::<T>::get(&pair) {
                // D4: a seeded pool has a creator (the launch's fee recipient),
                // so it snapshots the full default split. An adopted record
                // keeps whatever split it carries.
                None => Self::insert_new_pool(
                    &pair,
                    fee_tier,
                    Self::routing_for_new_pool(&pair, fee_tier, true)?,
                )?,
                Some(existing) => {
                    ensure!(existing.fee_tier == fee_tier, Error::<T>::InvalidFeeTier)
                },
            }
            ensure!(T::Base::lp_issuance(&asset, &quote).is_zero(), Error::<T>::PoolAlreadySeeded);
            let pool = Pools::<T>::get(&pair).ok_or(Error::<T>::PoolNotFound)?;
            let pool_account =
                T::Base::pool_account(&asset, &quote).ok_or(Error::<T>::PoolNotFound)?;

            // 2. Clear the pool account: it keeps nothing of its own
            //    (the base pool counts its whole balance as reserve), so
            //    everything in it is a donation. The reserved asset goes first:
            //    for a non-sufficient asset its balance holds a consumer
            //    reference on the account, which must be released before the
            //    native balance can leave in full.
            let excess_to = T::ExcessRecipient::get();
            for swept in [asset.clone(), quote.clone()] {
                let held =
                    T::Assets::reducible_balance(swept.clone(), &pool_account, Expendable, Polite);
                if held.is_zero() {
                    continue;
                }
                // The reserved asset is burned. A credit of it to the
                // recipient needs a free consumer reference there, which
                // anyone can use up beforehand, and failing the seed would
                // block the graduation of every launch whose pool address
                // received a donation.
                if T::ReservedAssets::contains(&swept) {
                    T::Assets::burn_from(
                        swept.clone(),
                        &pool_account,
                        held,
                        Expendable,
                        Exact,
                        frame_support::traits::tokens::Fortitude::Force,
                    )?;
                    Self::deposit_event(Event::PreSeedBalanceBurned {
                        pool: pair.clone(),
                        asset: swept,
                        amount: held,
                    });
                    continue;
                }
                // Deliver to the recipient, or fail the seed with a distinct
                // error: a silent burn would make a mis-wired recipient look
                // like correct operation, whereas a loud failure is fixed by
                // re-wiring and retrying. In a storage layer of its own, so
                // the underlying error leaves nothing half-applied.
                frame_support::storage::with_storage_layer(|| {
                    T::Assets::transfer(swept.clone(), &pool_account, &excess_to, held, Expendable)
                })
                .map_err(|_| Error::<T>::ExcessRecipientCannotReceive)?;
                Self::deposit_event(Event::PreSeedBalanceSwept {
                    pool: pair.clone(),
                    asset: swept,
                    amount: held,
                    to: excess_to.clone(),
                });
            }

            // 3–4. The base pool, and the first deposit. The base pallet
            //      leaves a depositor its minimum of each asset rather than
            //      take its last unit, so a seeder holding exactly what it
            //      seeds deposits that minimum less; more than that missing
            //      is a seeder short of funds.
            Self::ensure_base_pool(&pair, &pool, who)?;
            T::Base::open_lp_account(&asset, &quote, who)?;
            let spare = |a: &T::AssetKind, amount: T::Balance| -> Result<T::Balance, Error<T>> {
                let can =
                    T::Assets::reducible_balance(a.clone(), who, Preserve, Polite).min(amount);
                ensure!(
                    amount.saturating_sub(can) <= T::Assets::minimum_balance(a.clone())
                        && !can.is_zero(),
                    Error::<T>::InsufficientInitialLiquidity
                );
                Ok(can)
            };
            let asset_in = spare(&asset, amount_asset)?;
            let quote_in = spare(&quote, amount_quote)?;
            let (quote_seeded, asset_seeded, shares) = T::Base::add_liquidity(
                who,
                quote.clone(),
                asset.clone(),
                quote_in,
                asset_in,
                quote_in,
                asset_in,
            )?;

            // 5. Frozen for good: no code path lifts a `LaunchSeed` freeze.
            T::Base::set_lp_freeze(&asset, &quote, who, &FreezeReason::LaunchSeed.into(), shares)?;
            // And locked to the last block, as a seed position always was: a
            // lock to an earlier block is refused (`LockCannotBeShortened`),
            // and anything reading locks sees this one. The freeze above is
            // what holds it.
            LiquidityLocks::<T>::insert(&pair, who, BlockNumberFor::<T>::max_value());
            T::Base::set_lp_freeze(
                &asset,
                &quote,
                who,
                &FreezeReason::LiquidityLock.into(),
                shares,
            )?;

            let (canonical, amount_a, amount_b) =
                Self::canonical_pair_with(quote.clone(), asset.clone(), quote_seeded, asset_seeded);
            Self::deposit_event(Event::LiquidityAdded {
                provider: who.clone(),
                asset_a: canonical.0,
                asset_b: canonical.1,
                amount_a,
                amount_b,
                shares_minted: shares,
            });
            Self::deposit_event(Event::LiquidityLocked {
                who: who.clone(),
                pool: pair,
                locked_until: Bounded::max_value(),
            });
            Self::deposit_event(Event::ReservedPoolSeeded {
                who: who.clone(),
                asset,
                quote,
                amount_asset: asset_seeded,
                amount_quote: quote_seeded,
                shares,
            });
            Ok(SeedOutcome { shares, asset_seeded, quote_seeded })
        }

        /// `who`'s lock on `pair`, settled: one whose block has come is lifted
        /// (its freeze thawed, its record dropped), so what is left is a lock
        /// in force. Returns its block, if there is one.
        fn lock_in_force(
            pair: &(T::AssetKind, T::AssetKind),
            who: &T::AccountId,
        ) -> Result<Option<BlockNumberFor<T>>, DispatchError> {
            let Some(until) = LiquidityLocks::<T>::get(pair, who) else { return Ok(None) };
            if frame_system::Pallet::<T>::block_number() < until {
                return Ok(Some(until));
            }
            T::Base::thaw_lp(&pair.0, &pair.1, who, &FreezeReason::LiquidityLock.into())?;
            LiquidityLocks::<T>::remove(pair, who);
            Ok(None)
        }

        /// Add liquidity to an existing pool on behalf of `who`.
        ///
        /// Body of the `add_liquidity` extrinsic with the signer resolved by
        /// the caller. Up to the desired amounts are pulled from `who` at the
        /// pool's ratio, and the LP tokens minted to `who`. The first deposit
        /// creates the base pool, `who` paying its accounts' deposits, and
        /// locks the base pallet's minimum of LP tokens in the pool account.
        /// Returns the LP tokens minted to `who`.
        ///
        /// `who`'s LP-token account is opened first, deposit-backed, so a
        /// holder needs no free consumer reference to add.
        #[frame_support::transactional]
        pub fn do_add_liquidity_for(
            who: &T::AccountId,
            asset_a: T::AssetKind,
            asset_b: T::AssetKind,
            amount_a: T::Balance,
            amount_b: T::Balance,
            amount_a_min: T::Balance,
            amount_b_min: T::Balance,
        ) -> Result<T::Balance, DispatchError> {
            ensure!(amount_a > Zero::zero() && amount_b > Zero::zero(), Error::<T>::ZeroAmount);

            let pair = Self::canonical_pair(asset_a.clone(), asset_b.clone());
            let pool = Pools::<T>::get(&pair).ok_or(Error::<T>::PoolNotFound)?;

            // The amounts the base pallet will take, worked out here so its
            // refusals are ours (the minimums apply to what is
            // actually taken). It leaves a depositor its minimum of each
            // asset rather than take the last unit, so a depositor offering
            // its whole balance gives that minimum less; one short by more
            // fails on its funds.
            let offered = |a: &T::AssetKind, amount: T::Balance| -> T::Balance {
                let can = T::Assets::reducible_balance(a.clone(), who, Preserve, Polite);
                if can < amount
                    && amount.saturating_sub(can) <= T::Assets::minimum_balance(a.clone())
                {
                    can
                } else {
                    amount
                }
            };
            let (amount_a, amount_b) = (offered(&asset_a, amount_a), offered(&asset_b, amount_b));
            let (take_a, take_b) = match T::Base::reserves(&asset_a, &asset_b) {
                None => (amount_a, amount_b),
                Some((reserve_a, reserve_b)) => {
                    // Floor, so the depositor never over-contributes.
                    let optimal_b = Self::mul_div_floor(amount_a, reserve_b, reserve_a)?;
                    if optimal_b <= amount_b {
                        (amount_a, optimal_b)
                    } else {
                        (Self::mul_div_floor(amount_b, reserve_a, reserve_b)?, amount_b)
                    }
                },
            };
            ensure!(!take_a.is_zero() && !take_b.is_zero(), Error::<T>::ZeroAmount);
            ensure!(take_a >= amount_a_min && take_b >= amount_b_min, Error::<T>::SlippageExceeded);
            // What it mints, as the base pallet reckons it, which refuses a
            // deposit minting no more than its locked minimum: a first deposit
            // that small is too small to open the pool, a later one too small
            // to record.
            let min = T::Base::min_liquidity();
            match T::Base::reserves(&asset_a, &asset_b) {
                None => {
                    let wide = T::HigherPrecisionBalance::from;
                    let root: T::Balance = wide(take_a)
                        .checked_mul(&wide(take_b))
                        .ok_or(Error::<T>::Overflow)?
                        .integer_sqrt()
                        .try_into()
                        .map_err(|_| Error::<T>::Overflow)?;
                    ensure!(
                        root.saturating_sub(min) > min,
                        Error::<T>::InsufficientInitialLiquidity
                    );
                },
                Some((reserve_a, reserve_b)) => {
                    let issuance = T::Base::lp_issuance(&asset_a, &asset_b);
                    let minted = Self::mul_div_floor(take_a, issuance, reserve_a)?
                        .min(Self::mul_div_floor(take_b, issuance, reserve_b)?);
                    ensure!(minted > min, Error::<T>::ZeroAmount);
                },
            }

            Self::ensure_base_pool(&pair, &pool, who)?;
            T::Base::open_lp_account(&asset_a, &asset_b, who)?;
            // Offered as desired, taken as minimums: the base pallet's own
            // reckoning from the same reserves comes to the same amounts.
            let (added_a, added_b, lp) = T::Base::add_liquidity(
                who,
                asset_a.clone(),
                asset_b.clone(),
                amount_a,
                amount_b,
                take_a,
                take_b,
            )?;

            // A lock covers the position: liquidity added while it is in
            // force is frozen with the rest.
            if Self::lock_in_force(&pair, who)?.is_some() {
                let all = T::Base::lp_balance(&asset_a, &asset_b, who);
                T::Base::set_lp_freeze(
                    &asset_a,
                    &asset_b,
                    who,
                    &FreezeReason::LiquidityLock.into(),
                    all,
                )?;
            }

            let (pair, amount_a, amount_b) =
                Self::canonical_pair_with(asset_a, asset_b, added_a, added_b);
            Self::deposit_event(Event::LiquidityAdded {
                provider: who.clone(),
                asset_a: pair.0,
                asset_b: pair.1,
                amount_a,
                amount_b,
                shares_minted: lp,
            });
            Ok(lp)
        }

        /// Burn `lp` of `who`'s LP tokens and pay their share of the reserves
        /// to `who`. The base pallet floors each amount, leaves each reserve
        /// at least its asset's minimum, and burns only tokens that are not
        /// frozen. Returns the amounts in the caller's order.
        #[frame_support::transactional]
        pub fn do_remove_liquidity_for(
            who: &T::AccountId,
            asset_a: T::AssetKind,
            asset_b: T::AssetKind,
            lp: T::Balance,
            amount_a_min: T::Balance,
            amount_b_min: T::Balance,
        ) -> Result<(T::Balance, T::Balance), DispatchError> {
            ensure!(lp > Zero::zero(), Error::<T>::ZeroAmount);
            let pair = Self::canonical_pair(asset_a.clone(), asset_b.clone());
            ensure!(Pools::<T>::contains_key(&pair), Error::<T>::PoolNotFound);
            let held = T::Base::lp_balance(&asset_a, &asset_b, who);
            ensure!(held >= lp, Error::<T>::InsufficientShares);
            // A lock in force holds the whole position, and
            // a launch's seed is frozen for good.
            ensure!(Self::lock_in_force(&pair, who)?.is_none(), Error::<T>::PoolLocked);
            let seed =
                T::Base::lp_frozen(&asset_a, &asset_b, who, &FreezeReason::LaunchSeed.into());
            ensure!(lp <= held.saturating_sub(seed), Error::<T>::PoolLocked);
            // What the base pallet will pay, worked out here so its refusals
            // are ours: each amount floored (in the pool's favour), at
            // least the caller's minimum, more than nothing, and leaving each
            // reserve at least its asset's minimum balance, which belongs to
            // the remaining LPs.
            let (reserve_a, reserve_b) =
                T::Base::reserves(&asset_a, &asset_b).ok_or(Error::<T>::InsufficientLiquidity)?;
            let issuance = T::Base::lp_issuance(&asset_a, &asset_b);
            let pay_a = Self::mul_div_floor(lp, reserve_a, issuance)?;
            let pay_b = Self::mul_div_floor(lp, reserve_b, issuance)?;
            ensure!(pay_a >= amount_a_min && pay_b >= amount_b_min, Error::<T>::SlippageExceeded);
            ensure!(!pay_a.is_zero() && !pay_b.is_zero(), Error::<T>::ZeroAmount);
            ensure!(
                reserve_a.saturating_sub(pay_a) >= T::Assets::minimum_balance(asset_a.clone())
                    && reserve_b.saturating_sub(pay_b)
                        >= T::Assets::minimum_balance(asset_b.clone()),
                Error::<T>::ReserveBelowMinimum
            );
            // And what leaves must land: a payout the LP's own account cannot
            // take (less than an ED of native on an account holding none,
            // say) is refused as such, not as whatever the asset's pallet
            // says halfway through.
            ensure!(
                Self::can_credit(asset_a.clone(), who, pay_a)
                    && Self::can_credit(asset_b.clone(), who, pay_b),
                Error::<T>::RecipientCannotReceive
            );

            let (out_a, out_b) = T::Base::remove_liquidity(
                who,
                asset_a.clone(),
                asset_b.clone(),
                lp,
                amount_a_min,
                amount_b_min,
            )?;

            let (pair, amount_a, amount_b) =
                Self::canonical_pair_with(asset_a, asset_b, out_a, out_b);
            Self::deposit_event(Event::LiquidityRemoved {
                provider: who.clone(),
                asset_a: pair.0,
                asset_b: pair.1,
                amount_a,
                amount_b,
                shares_burned: lp,
            });
            Ok((out_a, out_b))
        }

        /// Lock `who`'s position in the pool until `lock_until`, or extend
        /// its lock. Body of the `lock_liquidity` extrinsic with the signer
        /// resolved by the caller. The lock covers the whole position — its
        /// LP tokens now and any added while it is in force — by a freeze.
        /// Fails with `InsufficientShares` if `who` holds no LP tokens there.
        /// D3: a lock is monotone — re-locking to the same block is a no-op,
        /// an earlier block fails with `LockCannotBeShortened`, so a position
        /// locked to `BlockNumber::max_value()` stays locked for good.
        pub fn do_lock_liquidity_for(
            who: &T::AccountId,
            asset_a: T::AssetKind,
            asset_b: T::AssetKind,
            lock_until: BlockNumberFor<T>,
        ) -> DispatchResult {
            let pair = Self::canonical_pair(asset_a, asset_b);
            let lp = T::Base::lp_balance(&pair.0, &pair.1, who);
            ensure!(!lp.is_zero(), Error::<T>::InsufficientShares);
            LiquidityLocks::<T>::try_mutate(&pair, who, |lock| -> DispatchResult {
                if let Some(existing) = lock {
                    ensure!(lock_until >= *existing, Error::<T>::LockCannotBeShortened);
                }
                *lock = Some(lock_until);
                Ok(())
            })?;
            T::Base::set_lp_freeze(&pair.0, &pair.1, who, &FreezeReason::LiquidityLock.into(), lp)?;
            Self::deposit_event(Event::LiquidityLocked {
                who: who.clone(),
                pool: pair,
                locked_until: lock_until,
            });
            Ok(())
        }

        /// The routed slices of `amount` in native: protocol, creator and
        /// treasury, each floored on its own. The treasury slice folds
        /// into the protocol's where `other` (the pool's non-native asset)
        /// has no treasury sink. Returns the slices and the sink.
        fn slices(
            routing: &FeeRouting,
            amount: T::Balance,
            other: &T::AssetKind,
        ) -> Result<Slices<T::Balance, T::AccountId>, Error<T>> {
            let mut protocol = Self::bps_of(amount, routing.protocol_bps)?;
            let creator = Self::bps_of(amount, routing.creator_bps)?;
            let mut treasury = Self::bps_of(amount, routing.treasury_bps)?;
            let sink = if treasury.is_zero() { None } else { T::TreasurySink::account_for(other) };
            if sink.is_none() && !treasury.is_zero() {
                protocol = protocol.checked_add(&treasury).ok_or(Error::<T>::Overflow)?;
                treasury = Zero::zero();
            }
            Ok(Slices { protocol, creator, treasury, sink })
        }

        /// What swapping exactly `amount_in` of `asset_in` for `asset_out`
        /// pays, priced from the pool as it stands: the routed slices, what
        /// reaches the pool, and what the trader receives. The swap and the
        /// quotes both price here, so a quote is what the swap would pay now.
        /// Fails as the swap would: no base pool or no liquidity, an output
        /// that rounds to nothing, or one the pool cannot pay out and
        /// keep its own minimum of the asset.
        fn price_exact_in(
            pool: &PoolInfo<T::Balance>,
            asset_in: &T::AssetKind,
            asset_out: &T::AssetKind,
            amount_in: T::Balance,
        ) -> Result<Priced<T::Balance, T::AccountId>, DispatchError> {
            ensure!(amount_in > Zero::zero(), Error::<T>::ZeroAmount);
            let pool_account =
                T::Base::pool_account(asset_in, asset_out).ok_or(Error::<T>::PoolNotFound)?;
            ensure!(
                T::Base::reserves(asset_in, asset_out).is_some(),
                Error::<T>::InsufficientLiquidity
            );
            let native = T::NativeAsset::get().encode();
            let native_in = asset_in.encode() == native;
            let native_out = asset_out.encode() == native;
            let other = if native_in { asset_out } else { asset_in };

            let (slices, to_pool) = if native_in {
                let slices = Self::slices(&pool.routing, amount_in, other)?;
                let to_pool = amount_in.checked_sub(&slices.total()).ok_or(Error::<T>::Overflow)?;
                (slices, to_pool)
            } else {
                (Slices::none(), amount_in)
            };
            let gross = T::Base::amount_out(asset_in, asset_out, to_pool)
                .ok_or(Error::<T>::InsufficientLiquidity)?;
            ensure!(!gross.is_zero(), Error::<T>::ZeroAmount);
            // The base pool pays out keeping its own minimum of the asset.
            ensure!(
                gross
                    <= T::Assets::reducible_balance(
                        asset_out.clone(),
                        &pool_account,
                        Preserve,
                        Polite
                    ),
                Error::<T>::InsufficientLiquidity
            );
            let (slices, amount_out) = if native_out {
                let slices = Self::slices(&pool.routing, gross, other)?;
                let net = gross.checked_sub(&slices.total()).ok_or(Error::<T>::Overflow)?;
                (slices, net)
            } else {
                (slices, gross)
            };
            ensure!(!amount_out.is_zero(), Error::<T>::ZeroAmount);
            Ok(Priced { slices, to_pool, amount_out, pool_account })
        }

        /// [`Pallet::quote_exact_tokens_for_tokens`].
        fn quote_exact_in(
            asset_in: T::AssetKind,
            asset_out: T::AssetKind,
            amount_in: T::Balance,
        ) -> Option<T::Balance> {
            let pool = Pools::<T>::get(Self::canonical_pair(asset_in.clone(), asset_out.clone()))?;
            Self::price_exact_in(&pool, &asset_in, &asset_out, amount_in)
                .ok()
                .map(|p| p.amount_out)
        }

        /// [`Pallet::quote_tokens_for_exact_tokens`]: a search over the swap's
        /// own pricing. The routed slices floor one by one, so the output can
        /// dip by a unit as the input grows by one; the search keeps an input
        /// that pays enough and one a unit below it that does not.
        fn quote_exact_out(
            asset_in: T::AssetKind,
            asset_out: T::AssetKind,
            amount_out: T::Balance,
        ) -> Option<T::Balance> {
            if amount_out.is_zero() {
                return None;
            }
            let pool = Pools::<T>::get(Self::canonical_pair(asset_in.clone(), asset_out.clone()))?;
            // What `x` pays: nothing when it is too small to pay anything,
            // `None` when the pool cannot price it at all.
            let pays = |x: T::Balance| -> Option<T::Balance> {
                match Self::price_exact_in(&pool, &asset_in, &asset_out, x) {
                    Ok(p) => Some(p.amount_out),
                    Err(e) if e == Error::<T>::ZeroAmount.into() => Some(Zero::zero()),
                    Err(_) => None,
                }
            };
            // `lo` pays less than `amount_out`, `hi` at least that much.
            let two: T::Balance = 2u32.into();
            let mut lo: T::Balance = Zero::zero();
            let mut hi: T::Balance = One::one();
            while pays(hi)? < amount_out {
                lo = hi;
                hi = hi.checked_mul(&two)?;
            }
            while hi - lo > One::one() {
                let mid = lo + (hi - lo) / two;
                if pays(mid)? >= amount_out {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            Some(hi)
        }

        /// Execute a swap on behalf of `who`, delivering the output to
        /// `recipient`. Returns the amount delivered.
        ///
        /// The pool charges its LP fee ([`Self::lp_fee`]); the routed slices
        /// are taken here, in native, outside it: off the input when
        /// native goes in, off the output when native comes out, and not at
        /// all for a pair with no native side. The swap runs on credits, so
        /// nothing moves until everything has been priced and checked, and
        /// the whole of it happens or none of it does.
        ///
        /// `is_trade`: whether this swap counts as market activity for
        /// `LastSwapBlock`. A person's swap and a settled intent do; the
        /// launch treasury's buyback through [`PoolManager::swap_for`] does
        /// not — the dormancy rule that reads it asks whether anyone still
        /// trades the token, and the pallet buying it back is not an
        /// answer (R2).
        ///
        /// `input`: how native leaves `who`. A person keeps their ED
        /// (`Preserve`): a swap of one's whole native balance used to kill
        /// the account and then fail to deliver a non-sufficient asset to it
        /// (`CannotCreate`). A settled intent swaps from the
        /// marketplace's intent escrow through [`IntentSwap`], which spends it
        /// to zero (`Expendable`). A token always leaves `Expendable`: on a
        /// pallet-assets balance `Preserve` refuses the last unit, and every
        /// holder could then never sell their whole position.
        #[frame_support::transactional]
        pub(crate) fn do_swap(
            who: &T::AccountId,
            asset_in: T::AssetKind,
            asset_out: T::AssetKind,
            amount_in: T::Balance,
            amount_out_min: T::Balance,
            recipient: &T::AccountId,
            is_trade: bool,
            input: Preservation,
        ) -> Result<T::Balance, DispatchError> {
            ensure!(amount_in > Zero::zero(), Error::<T>::ZeroAmount);

            let pair = Self::canonical_pair(asset_in.clone(), asset_out.clone());
            let mut pool = Pools::<T>::get(&pair).ok_or(Error::<T>::PoolNotFound)?;
            let native = T::NativeAsset::get();
            let native_in = asset_in.encode() == native.encode();
            let native_out = asset_out.encode() == native.encode();
            let other = if native_in { asset_out.clone() } else { asset_in.clone() };
            let lp_fee = Self::lp_fee(pool.fee_tier, &pool.routing);

            // ---- Price and check everything before anything moves ----------
            let Priced { slices, to_pool, amount_out, pool_account } =
                Self::price_exact_in(&pool, &asset_in, &asset_out, amount_in)?;
            let Slices { protocol, creator, treasury, sink } = slices;
            ensure!(amount_out >= amount_out_min, Error::<T>::SlippageExceeded);
            // The output must land where it is going, as the asset's pallet
            // judges it: below the asset's minimum balance for a new holder,
            // or no consumer reference left to open one, it cannot.
            ensure!(
                Self::can_credit(asset_out.clone(), recipient, amount_out),
                Error::<T>::RecipientCannotReceive
            );

            // ---- Move ---------------------------------------------------------
            let input = if native_in { input } else { Expendable };
            // What `who` cannot spend is refused up front, with the answer
            // a transfer gives — the last ED of native (`NotExpendable`) or
            // more than is there (`FundsUnavailable`) — not partway through.
            if amount_in > T::Assets::reducible_balance(asset_in.clone(), who, input, Polite) {
                return Err(if amount_in <= T::Assets::balance(asset_in, who) {
                    sp_runtime::TokenError::NotExpendable
                } else {
                    sp_runtime::TokenError::FundsUnavailable
                }
                .into());
            }
            let credit_in =
                T::Assets::withdraw(asset_in.clone(), who, amount_in, Exact, input, Polite)?;
            let routed_total = protocol.saturating_add(creator).saturating_add(treasury);
            let (mut routed, out) = if native_in {
                let (routed, to_pool) = credit_in.split(routed_total);
                (routed, T::Base::swap_credit(asset_out.clone(), to_pool).map_err(|(_, e)| e)?)
            } else {
                let gross =
                    T::Base::swap_credit(asset_out.clone(), credit_in).map_err(|(_, e)| e)?;
                if native_out {
                    gross.split(routed_total)
                } else {
                    (Credit::<T::AccountId, T::Assets>::zero(native.clone()), gross)
                }
            };
            ensure!(out.peek() == amount_out, Error::<T>::Overflow);
            T::Assets::resolve(recipient, out).map_err(|_| Error::<T>::RecipientCannotReceive)?;

            // A routed slice that would leave its recipient below
            // the native ED cannot create that account, and must not fail the
            // swap; it goes to the pool, where it accrues to LPs as the
            // pool's own fee does. `ProtocolFeesUnclaimed`,
            // `CreatorFeesUnclaimed` and the sink's tally then do not count
            // it. The window is "before the recipient holds an ED of native":
            // genesis and the upgrade fund the fee escrow, so it never opens
            // for that on a real chain.
            let fee_escrow = Self::fee_escrow_account();
            let escrowed = protocol.saturating_add(creator);
            if !escrowed.is_zero() && Self::can_credit(native.clone(), &fee_escrow, escrowed) {
                let (to_escrow, rest) = routed.split(escrowed);
                routed = rest;
                T::Assets::resolve(&fee_escrow, to_escrow).map_err(|_| Error::<T>::Overflow)?;
                if !protocol.is_zero() {
                    ProtocolFeesUnclaimed::<T>::mutate(|t| *t = t.saturating_add(protocol));
                }
                if !creator.is_zero() {
                    CreatorFeesUnclaimed::<T>::mutate(&pair, |t| *t = t.saturating_add(creator));
                }
            }
            // D9: the treasury slice is pushed to the sink (see `TreasurySink`
            // for why a push is safe here), under the same Finding-14 guard.
            if let Some(vault) = sink {
                if !treasury.is_zero() && Self::can_credit(native.clone(), &vault, treasury) {
                    let (to_vault, rest) = routed.split(treasury);
                    routed = rest;
                    T::Assets::resolve(&vault, to_vault).map_err(|_| Error::<T>::Overflow)?;
                    T::TreasurySink::note_fee(&other, treasury);
                }
            }
            if !routed.peek().is_zero() {
                T::Assets::resolve(&pool_account, routed).map_err(|_| Error::<T>::Overflow)?;
            }

            // The fee this swap charged, in the input asset as before: the
            // routed slices and the pool's LP fee when native goes in, the LP
            // fee alone otherwise (the routed slices then come off the output).
            let fee = lp_fee.mul_floor(to_pool).saturating_add(if native_in {
                routed_total
            } else {
                Zero::zero()
            });
            pool.total_fees_collected = pool.total_fees_collected.saturating_add(fee);
            Pools::<T>::insert(&pair, pool);
            if is_trade {
                LastSwapBlock::<T>::insert(&pair, frame_system::Pallet::<T>::block_number());
            }

            Self::deposit_event(Event::SwapExecuted {
                who: who.clone(),
                asset_in,
                asset_out,
                amount_in,
                amount_out,
                fee,
            });
            Self::deposit_event(Event::FeesCollected {
                pool: pair.clone(),
                amount: fee,
                recipient: pool_account,
            });
            if !routed_total.is_zero() {
                Self::deposit_event(Event::FeesRouted { pool: pair, protocol, creator, treasury });
            }
            Ok(amount_out)
        }
    }
}

impl<T: Config> IntentSwap<T::AccountId, T::AssetKind, T::Balance> for Pallet<T> {
    fn swap_exact_for_intent(
        who: &T::AccountId,
        asset_in: T::AssetKind,
        asset_out: T::AssetKind,
        amount_in: T::Balance,
        amount_out_min: T::Balance,
    ) -> Result<T::Balance, DispatchError> {
        Self::do_swap(who, asset_in, asset_out, amount_in, amount_out_min, who, true, Expendable)
    }
}

impl<T: Config> PoolManager<T::AccountId, T::AssetKind, T::Balance, BlockNumberFor<T>>
    for Pallet<T>
{
    fn pool_exists(asset_a: T::AssetKind, asset_b: T::AssetKind) -> bool {
        Self::pool_exists(asset_a, asset_b)
    }

    fn create_pool(asset_a: T::AssetKind, asset_b: T::AssetKind, fee_tier: u32) -> DispatchResult {
        Self::do_create_pool(asset_a, asset_b, fee_tier)
    }

    fn add_liquidity_for(
        who: &T::AccountId,
        asset_a: T::AssetKind,
        asset_b: T::AssetKind,
        amount_a: T::Balance,
        amount_b: T::Balance,
        amount_a_min: T::Balance,
        amount_b_min: T::Balance,
    ) -> Result<T::Balance, DispatchError> {
        Self::do_add_liquidity_for(
            who,
            asset_a,
            asset_b,
            amount_a,
            amount_b,
            amount_a_min,
            amount_b_min,
        )
    }

    fn lock_liquidity_for(
        who: &T::AccountId,
        asset_a: T::AssetKind,
        asset_b: T::AssetKind,
        lock_until: BlockNumberFor<T>,
    ) -> DispatchResult {
        Self::do_lock_liquidity_for(who, asset_a, asset_b, lock_until)
    }

    fn swap_for(
        who: &T::AccountId,
        asset_in: T::AssetKind,
        asset_out: T::AssetKind,
        amount_in: T::Balance,
        amount_out_min: T::Balance,
    ) -> Result<T::Balance, DispatchError> {
        // An in-runtime swap on a token's behalf is not a trade for the
        // dormancy clock (R2).
        Self::do_swap(who, asset_in, asset_out, amount_in, amount_out_min, who, false, Preserve)
    }

    fn native_reserves(asset: T::AssetKind) -> Option<(T::Balance, T::Balance)> {
        let native = <T::NativeAsset as Get<T::AssetKind>>::get();
        Pools::<T>::get(Self::canonical_pair(native.clone(), asset.clone()))?;
        T::Base::reserves(&native, &asset)
    }

    fn fee_bps(asset: T::AssetKind) -> Option<u16> {
        let pair = Self::canonical_pair(<T::NativeAsset as Get<T::AssetKind>>::get(), asset);
        Pools::<T>::get(&pair).map(|p| (p.fee_tier as u16).saturating_mul(10))
    }

    fn last_swap_block(asset: T::AssetKind) -> Option<BlockNumberFor<T>> {
        let pair = Self::canonical_pair(<T::NativeAsset as Get<T::AssetKind>>::get(), asset);
        Pools::<T>::contains_key(&pair).then(|| LastSwapBlock::<T>::get(&pair).unwrap_or_default())
    }
}

impl<T: Config> ReservedPoolSeeder<T::AccountId, T::AssetKind, T::Balance, BlockNumberFor<T>>
    for Pallet<T>
{
    fn seed_reserved_pool_for(
        who: &T::AccountId,
        asset: T::AssetKind,
        quote: T::AssetKind,
        amount_asset: T::Balance,
        amount_quote: T::Balance,
        fee_tier: u32,
    ) -> Result<SeedOutcome<T::Balance>, DispatchError> {
        Self::do_seed_reserved_pool_for(who, asset, quote, amount_asset, amount_quote, fee_tier)
    }
}

/// Storage migrations.
pub mod migrations {
    use super::*;
    use frame_support::{
        migrations::VersionedMigration,
        traits::{Get, UncheckedOnRuntimeUpgrade},
        weights::Weight,
    };
    use sp_std::marker::PhantomData;

    /// v4 → v7: the DEX core moves onto the base pallet. The single-pool
    /// core's storage is cleared, not converted, so it may only run on a
    /// chain with no pools: a pool that did exist would lose its record
    /// while its pool account kept the funds, with no call left that moves
    /// them. What is dropped is logged, each pool with its id and account.
    ///
    /// Also carried: v5's removal of the solver marketplace's items (a chain
    /// at v4 never ran it; list this migration alone, not v5 as well), and
    /// the fee escrow's ED from `Source`, which v6 gave it. Kept:
    /// `DefaultFeeRouting`, `ProtocolFeeRecipient`, and the fees owed —
    /// `ProtocolFeesUnclaimed` and `CreatorFeesUnclaimed`, whose native the
    /// fee escrow holds and the new core pays out under the same keys.
    pub mod v7 {
        use super::*;
        use frame_support::{
            storage::{storage_prefix, unhashed},
            traits::PalletInfoAccess,
            Blake2_128Concat,
        };

        /// A pool record as the single-pool core stored it (v3 to v6).
        #[derive(Encode, Decode)]
        #[allow(missing_docs)]
        pub struct OldPoolInfo<Balance, AccountId> {
            pub reserve_a: Balance,
            pub reserve_b: Balance,
            pub fee_tier: u32,
            pub total_fees_collected: Balance,
            pub pool_account: AccountId,
            pub routing: FeeRouting,
        }

        #[frame_support::storage_alias]
        type OldPools<T: Config> = StorageMap<
            Pallet<T>,
            Blake2_128Concat,
            (<T as Config>::AssetKind, <T as Config>::AssetKind),
            OldPoolInfo<<T as Config>::Balance, <T as frame_system::Config>::AccountId>,
        >;

        /// The single-pool core's items this upgrade clears. `Pools` keeps
        /// its name with the new record shape, so it is cleared too.
        pub const CLEARED: [&str; 4] =
            ["Pools", "LiquidityPositions", "TotalLiquidity", "LastSwapBlock"];

        fn prefix<T: Config>(item: &str) -> [u8; 32] {
            storage_prefix(Pallet::<T>::name().as_bytes(), item.as_bytes())
        }

        /// The routed fees the escrow owes: protocol and every creator's.
        pub fn fees_owed<T: Config>() -> T::Balance {
            CreatorFeesUnclaimed::<T>::iter_values()
                .fold(ProtocolFeesUnclaimed::<T>::get(), |a, f| a.saturating_add(f))
        }

        /// How many keys each of [`CLEARED`] holds.
        pub fn counts<T: Config>() -> sp_std::vec::Vec<u32> {
            CLEARED
                .iter()
                .map(|item| {
                    let p = prefix::<T>(item);
                    let mut n = 0u32;
                    let mut key = p.to_vec();
                    while let Some(next) = sp_io::storage::next_key(&key) {
                        if !next.starts_with(&p) {
                            break;
                        }
                        n = n.saturating_add(1);
                        key = next;
                    }
                    n
                })
                .collect()
        }

        /// Unversioned body; wrap in [`MigrateToV7`]. `Source` pays the fee
        /// escrow's ED where it holds less, as the runtime's Treasury.
        pub struct VersionUncheckedMigrateToV7<T, Source>(PhantomData<(T, Source)>);

        impl<T: Config, Source: Get<T::AccountId>> UncheckedOnRuntimeUpgrade
            for VersionUncheckedMigrateToV7<T, Source>
        {
            fn on_runtime_upgrade() -> Weight {
                let (mut reads, mut writes) = (0u64, 0u64);
                // Name every pool that is lost, before its record goes.
                for (pair, pool) in OldPools::<T>::iter() {
                    reads = reads.saturating_add(1);
                    log::warn!(
                        target: "runtime::vitreus-dex",
                        "v7 migration: dropping pool {pair:?} (account {:?}, reserves {:?} / {:?}); its account keeps what it holds",
                        pool.pool_account,
                        pool.reserve_a,
                        pool.reserve_b,
                    );
                }
                for item in CLEARED {
                    let r = unhashed::clear_prefix(&prefix::<T>(item), None, None);
                    reads = reads.saturating_add(r.loops as u64);
                    writes = writes.saturating_add(r.unique as u64);
                    if r.unique > 0 {
                        log::warn!(
                            target: "runtime::vitreus-dex",
                            "v7 migration: dropped {} key(s) of {item}",
                            r.unique
                        );
                    }
                }
                let w = v5::VersionUncheckedMigrateToV5::<T>::on_runtime_upgrade();

                // The fee escrow's ED above the fees it owes, paid by `Source`
                // (nothing is minted).
                let escrow = Pallet::<T>::fee_escrow_account();
                let native = T::NativeAsset::get();
                let short = Pallet::<T>::native_ed()
                    .saturating_add(fees_owed::<T>())
                    .saturating_sub(T::Assets::balance(native.clone(), &escrow));
                if !short.is_zero() {
                    match T::Assets::transfer(native, &Source::get(), &escrow, short, Preserve) {
                        Ok(_) => writes = writes.saturating_add(2),
                        Err(e) => log::error!(
                            target: "runtime::vitreus-dex",
                            "v7 migration: the fee escrow's ED could not be paid ({e:?}); routed slices below it go to the pool until it holds one"
                        ),
                    }
                }
                T::DbWeight::get()
                    .reads_writes(reads.saturating_add(2), writes)
                    .saturating_add(w)
            }

            /// Records what the upgrade drops; never fails, so a chain
            /// holding pools still passes try-runtime and its log shows them.
            #[cfg(feature = "try-runtime")]
            fn pre_upgrade() -> Result<sp_std::vec::Vec<u8>, sp_runtime::TryRuntimeError> {
                let counts = counts::<T>();
                for (item, n) in CLEARED.iter().zip(counts.iter()) {
                    if *n > 0 {
                        log::warn!(
                            target: "runtime::vitreus-dex",
                            "v7 pre_upgrade: {item} holds {n} key(s); the upgrade drops them"
                        );
                    }
                }
                Ok(counts.encode())
            }

            #[cfg(feature = "try-runtime")]
            fn post_upgrade(_: sp_std::vec::Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
                frame_support::ensure!(
                    counts::<T>().iter().all(|n| *n == 0),
                    "the single-pool core's storage is cleared"
                );
                frame_support::ensure!(!v5::any_left::<T>(), "no marketplace key is left");
                let escrow = Pallet::<T>::fee_escrow_account();
                frame_support::ensure!(
                    T::Assets::balance(T::NativeAsset::get(), &escrow)
                        >= Pallet::<T>::native_ed().saturating_add(fees_owed::<T>()),
                    "the fee escrow holds its ED and the fees it owes"
                );
                Ok(())
            }
        }

        /// v4 → v7, gated on the pallet's on-chain storage version.
        pub type MigrateToV7<T, Source> = VersionedMigration<
            4,
            7,
            VersionUncheckedMigrateToV7<T, Source>,
            Pallet<T>,
            <T as frame_system::Config>::DbWeight,
        >;
    }

    /// v5: the solver marketplace's storage leaves with it.
    pub mod v5 {
        use super::*;
        use frame_support::{
            storage::{storage_prefix, unhashed},
            traits::PalletInfoAccess,
        };

        /// The solver marketplace's storage items, as this pallet held them
        /// before the marketplace became a pallet of its own. The first
        /// three are governance's settings; the rest are its records.
        pub const ITEMS: [&str; 10] = [
            "BidWindowBlocks",
            "SettlementWindowBlocks",
            "SolverBondAmount",
            "NextIntentId",
            "NextSolverId",
            "Intents",
            "SolverAccountToId",
            "Solvers",
            "FillCommitments",
            "IntentEscrowBalances",
        ];

        fn prefix<T: Config>(item: &str) -> [u8; 32] {
            storage_prefix(Pallet::<T>::name().as_bytes(), item.as_bytes())
        }

        /// Whether any of [`ITEMS`] still holds a value here.
        pub fn any_left<T: Config>() -> bool {
            ITEMS.iter().any(|item| {
                let p = prefix::<T>(item);
                sp_io::storage::next_key(&p).is_some_and(|k| k.starts_with(&p))
            })
        }

        /// Unversioned body; wrap in [`MigrateToV5`].
        ///
        /// Nothing is carried over and nothing is refused: every item is
        /// cleared, whatever it holds, and each one that held anything is
        /// logged with how many keys it lost. A chain with open intents or
        /// registered solvers loses them; one whose governance set the three
        /// settings starts the marketplace from its defaults again.
        pub struct VersionUncheckedMigrateToV5<T>(PhantomData<T>);

        impl<T: Config> UncheckedOnRuntimeUpgrade for VersionUncheckedMigrateToV5<T> {
            fn on_runtime_upgrade() -> Weight {
                let (mut reads, mut writes) = (0u64, 0u64);
                for item in ITEMS {
                    let r = unhashed::clear_prefix(&prefix::<T>(item), None, None);
                    reads = reads.saturating_add(r.loops as u64);
                    writes = writes.saturating_add(r.unique as u64);
                    if r.unique > 0 {
                        log::warn!(
                            target: "runtime::vitreus-dex",
                            "v5 migration: dropped {} key(s) of {item}",
                            r.unique
                        );
                    }
                }
                T::DbWeight::get().reads_writes(reads, writes)
            }

            /// Reports what the upgrade will drop; never fails, so a chain
            /// with open intents still passes try-runtime.
            #[cfg(feature = "try-runtime")]
            fn pre_upgrade() -> Result<sp_std::vec::Vec<u8>, sp_runtime::TryRuntimeError> {
                for item in ITEMS {
                    let p = prefix::<T>(item);
                    if sp_io::storage::next_key(&p).is_some_and(|k| k.starts_with(&p)) {
                        log::warn!(
                            target: "runtime::vitreus-dex",
                            "v5 pre_upgrade: {item} holds values; the upgrade drops them"
                        );
                    }
                }
                Ok(sp_std::vec::Vec::new())
            }

            #[cfg(feature = "try-runtime")]
            fn post_upgrade(_: sp_std::vec::Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
                frame_support::ensure!(!any_left::<T>(), "no marketplace key is left");
                Ok(())
            }
        }

        /// Removal of the marketplace's storage, gated on the pallet's
        /// on-chain storage version.
        pub type MigrateToV5<T> = VersionedMigration<
            4,
            5,
            VersionUncheckedMigrateToV5<T>,
            Pallet<T>,
            <T as frame_system::Config>::DbWeight,
        >;
    }
}
