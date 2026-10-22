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
mod fuzz;
#[cfg(test)]
mod mock;
#[cfg(any(test, feature = "test-utils"))]
pub mod testing;
#[cfg(test)]
mod tests;

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
        fungibles::{Balanced, Inspect, Mutate},
        tokens::{
            Balance, DepositConsequence,
            Fortitude::Polite,
            Preservation::{self, Expendable, Preserve},
            Provenance, WithdrawConsequence,
        },
        Contains, Get,
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
    DispatchError, RuntimeDebug,
};

/// The PalletId used to derive the DEX sovereign account.
pub const PALLET_ID: PalletId = PalletId(*b"vtrs/dex");

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

/// On-chain record of a trading pair's reserves, fee tier and dedicated sub-account.
///
/// **Quoters: price from the pool account's live balances, not from
/// `reserve_a` / `reserve_b`.** `do_swap` calls `sync_reserves` before it
/// prices, which re-reads the balances, so the stored reserves are only a
/// snapshot as of the last sync: they lag by the pool's share of every fee
/// since (it stays in the account uncounted, Finding 3) and by any direct
/// transfer. A quote computed from the stored fields overstates the output
/// and the chain then fails the swap with `SlippageExceeded` at a tight
/// `amount_out_min`. The frontend's swap page reads balances for this reason.
///
/// **What a pool account keeps as its own is not a reserve.** From its
/// first deposit, every pool account holds the native existential deposit,
/// as its own provider, and one less than each token's minimum balance —
/// paid by that depositor on top of its amounts, or by a seed out of what it
/// cannot spare (see `Pallet::pool_buffer`). The ED lets a pool with no
/// native side hold non-sufficient tokens; and since a withdrawal or swap
/// only ever takes reserves and leaves at least a unit of each, the account
/// never drops below any asset's minimum, where the asset's pallet would pay
/// the rest out with the debit and leave the remaining LPs with nothing. A
/// reserve is the account's balance less what it keeps
/// ([`PoolManager::native_reserves`] already subtracts it).
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
pub struct PoolInfo<Balance, AccountId> {
    /// Reserve of `asset_a` as of the last `sync_reserves`; see the struct docs
    /// before pricing from this.
    pub reserve_a: Balance,
    /// Reserve of `asset_b` as of the last `sync_reserves`; see the struct docs
    /// before pricing from this.
    pub reserve_b: Balance,
    /// Swap fee tier for this pool, expressed in 10ths of a percent.
    pub fee_tier: u32,
    /// Cumulative fees collected over the pool's lifetime.
    pub total_fees_collected: Balance,
    /// Sub-account that physically holds the pool's reserves.
    pub pool_account: AccountId,
    /// D4: fee split snapshotted at creation / seeding. Zero for pools that
    /// predate D4 (migration v1) and for pools with no native side.
    pub routing: FeeRouting,
}

/// On-chain record of a single liquidity provider's position in a pool.
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
pub struct LiquidityPosition<Balance, BlockNumber> {
    /// LP shares owned by the provider.
    pub shares: Balance,
    /// Block at which the position was opened.
    pub entry_block: BlockNumber,
    /// Block until which the position is locked, if any.
    pub locked_until: Option<BlockNumber>,
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

/// The single entry point through which a pool for a *reserved* asset (see
/// [`Config::ReservedAssets`]) can come into existence. Reserved assets are
/// rejected by every other pool-creation path, including `ManageOrigin`.
///
/// Intended for the launchpad pallet's graduation step. The implementation
/// is transactional and, in order:
///
/// 1. requires `asset` to be reserved and `quote` not to be;
/// 2. creates the pool, or adopts an existing pool that has never received
///    liquidity (`TotalLiquidity == 0`); a pool that already holds shares is
///    rejected with `PoolAlreadySeeded`;
/// 3. sweeps any balance the pool's sub-account already holds in either asset
///    to [`Config::ExcessRecipient`], so a donation parked at the (predictable)
///    pool address before seeding cannot be absorbed by `sync_reserves` into
///    the opening price. If the recipient cannot receive the asset the whole
///    seed fails with `ExcessRecipientCannotReceive` — loudly, so a mis-wired
///    recipient is noticed and fixed, after which seeding can be retried;
/// 4. pulls exactly `(amount_quote, amount_asset)` from `who` — quote first, so
///    the pool account has a provider before it receives a possibly
///    non-sufficient asset — as the pool's first deposit;
/// 5. locks `who`'s position until `BlockNumber::max_value()`.
///
/// No origin check is performed; only the launchpad binds this trait and the
/// DEX exposes no extrinsic that reaches it.
pub trait ReservedPoolSeeder<AccountId, AssetKind, Balance, BlockNumber> {
    /// Returns the LP shares credited to `who` (net of `MINIMUM_LIQUIDITY`).
    fn seed_reserved_pool_for(
        who: &AccountId,
        asset: AssetKind,
        quote: AssetKind,
        amount_asset: Balance,
        amount_quote: Balance,
        fee_tier: u32,
    ) -> Result<Balance, DispatchError>;
}

#[frame_support::pallet]
pub mod pallet {
    use super::*;
    use frame_support::pallet_prelude::*;
    use frame_system::pallet_prelude::*;

    /// v2 on the fork is D8 (hash-derived pool accounts, `migrations::v2`).
    /// v5 removes the solver marketplace's storage, which moved to a pallet
    /// of its own (`migrations::v5`); v6 gives the fee escrow and every pool
    /// account their ED (`migrations::v6`).
    /// D9 — `FeeRouting.treasury_bps`, `LastSwapBlock` — changes the shape
    /// of every stored `PoolInfo`, so on the fork it is v3 with a migration
    /// (`migrations::v3`); the submission branch, which no chain with v1
    /// pools targets, carries D9 as its v2 without one.
    const STORAGE_VERSION: StorageVersion = StorageVersion::new(6);

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
        type Assets: Inspect<Self::AccountId, AssetId = Self::AssetKind, Balance = Self::Balance>
            + Mutate<Self::AccountId>
            + Balanced<Self::AccountId>;

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

        /// Weight information for the extrinsics of this pallet.
        type WeightInfo: WeightInfo;

        /// Supplies non-native asset identifiers for benchmarks.
        #[cfg(feature = "runtime-benchmarks")]
        type BenchmarkHelper: BenchmarkHelper<Self::AssetKind, Self::AccountId>;
    }

    /// All known pools keyed by their canonical ordered asset pair.
    #[pallet::storage]
    pub type Pools<T: Config> = StorageMap<
        _,
        Blake2_128Concat,
        (T::AssetKind, T::AssetKind),
        PoolInfo<T::Balance, T::AccountId>,
    >;

    /// Per-provider liquidity positions keyed by account and pool.
    #[pallet::storage]
    pub type LiquidityPositions<T: Config> = StorageDoubleMap<
        _,
        Blake2_128Concat,
        T::AccountId,
        Blake2_128Concat,
        (T::AssetKind, T::AssetKind),
        LiquidityPosition<T::Balance, BlockNumberFor<T>>,
    >;

    /// Total LP shares outstanding for each pool.
    #[pallet::storage]
    pub type TotalLiquidity<T: Config> =
        StorageMap<_, Blake2_128Concat, (T::AssetKind, T::AssetKind), T::Balance>;

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

        /// Remove liquidity from an existing pool.
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

            ensure!(shares > Zero::zero(), Error::<T>::ZeroAmount);

            // Finding 5 + D5: canonicalize the pair AND the minimums, which the
            // caller gave in (asset_a, asset_b) order.
            let (pair, amount_a_min, amount_b_min) =
                Self::canonical_pair_with(asset_a, asset_b, amount_a_min, amount_b_min);
            let mut pool = Pools::<T>::get(&pair).ok_or(Error::<T>::PoolNotFound)?;

            // Finding 2: sync reserves from actual balances before computing withdrawal.
            Self::sync_reserves(&pair, &mut pool);

            let total_shares = TotalLiquidity::<T>::get(&pair).ok_or(Error::<T>::PoolNotFound)?;
            ensure!(!total_shares.is_zero(), Error::<T>::InsufficientLiquidity);

            let mut position =
                LiquidityPositions::<T>::get(&who, &pair).ok_or(Error::<T>::InsufficientShares)?;
            ensure!(position.shares >= shares, Error::<T>::InsufficientShares);

            // Finding 10: enforce lock check.
            if let Some(until) = position.locked_until {
                let current = frame_system::Pallet::<T>::block_number();
                ensure!(current >= until, Error::<T>::PoolLocked);
            }

            // D1: wide precision; floor rounds the withdrawal DOWN, in the pool's favour.
            let amount_a = Self::mul_div_floor(shares, pool.reserve_a, total_shares)?;
            let amount_b = Self::mul_div_floor(shares, pool.reserve_b, total_shares)?;

            ensure!(amount_a >= amount_a_min, Error::<T>::SlippageExceeded);
            ensure!(amount_b >= amount_b_min, Error::<T>::SlippageExceeded);
            // What stays belongs to the other LPs and the locked minimum; a
            // withdrawal that would leave less than an asset's minimum
            // balance would take that remainder with it.
            ensure!(
                Self::can_debit_pool(pair.0.clone(), &pool.pool_account, amount_a)
                    && Self::can_debit_pool(pair.1.clone(), &pool.pool_account, amount_b),
                Error::<T>::ReserveBelowMinimum
            );
            // And what leaves must land: a payout the LP's own account
            // cannot take (less than an ED of native on an account holding
            // none, say) is refused as such.
            ensure!(
                Self::can_credit(pair.0.clone(), &who, amount_a)
                    && Self::can_credit(pair.1.clone(), &who, amount_b),
                Error::<T>::RecipientCannotReceive
            );

            T::Assets::transfer(pair.0.clone(), &pool.pool_account, &who, amount_a, Expendable)?;
            T::Assets::transfer(pair.1.clone(), &pool.pool_account, &who, amount_b, Expendable)?;

            pool.reserve_a =
                pool.reserve_a.checked_sub(&amount_a).ok_or(Error::<T>::InsufficientLiquidity)?;
            pool.reserve_b =
                pool.reserve_b.checked_sub(&amount_b).ok_or(Error::<T>::InsufficientLiquidity)?;
            Pools::<T>::insert(&pair, pool);

            let new_total = total_shares.checked_sub(&shares).ok_or(Error::<T>::Overflow)?;
            TotalLiquidity::<T>::insert(&pair, new_total);

            position.shares = position.shares.checked_sub(&shares).ok_or(Error::<T>::Overflow)?;
            if position.shares.is_zero() {
                LiquidityPositions::<T>::remove(&who, &pair);
            } else {
                LiquidityPositions::<T>::insert(&who, &pair, position);
            }

            Self::deposit_event(Event::LiquidityRemoved {
                provider: who,
                asset_a: pair.0,
                asset_b: pair.1,
                amount_a,
                amount_b,
                shares_burned: shares,
            });
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
        /// What every account the pallet owns must hold, and how its records
        /// must agree with one another. Each check is a balance the pallet
        /// promised someone or a count two maps must share:
        ///
        /// - the fee escrow holds its ED and, above it, every unclaimed
        ///   protocol and creator fee;
        /// - a pool with liquidity holds what it keeps of each asset (the ED,
        ///   a token's minimum less one) and, above it, at least its recorded
        ///   reserves, and
        ///   the pool's shares are its positions plus the locked minimum.
        #[cfg(any(feature = "try-runtime", feature = "test-utils", test))]
        pub fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
            use frame_support::ensure;

            let native = T::NativeAsset::get();
            let fees = CreatorFeesUnclaimed::<T>::iter_values()
                .fold(ProtocolFeesUnclaimed::<T>::get(), |a, f| a.saturating_add(f));
            let ed = Self::native_ed();
            ensure!(
                T::Assets::balance(native.clone(), &Self::fee_escrow_account())
                    >= fees.saturating_add(ed),
                "the fee escrow holds less than its ED and the fees it owes"
            );

            for (pair, pool) in Pools::<T>::iter() {
                let total = TotalLiquidity::<T>::get(&pair).unwrap_or_else(Zero::zero);
                ensure!(
                    total.is_zero()
                        || (T::Assets::balance(native.clone(), &pool.pool_account) >= ed
                            && Self::buffer_short(pair.0.clone(), &pool.pool_account).is_zero()
                            && Self::buffer_short(pair.1.clone(), &pool.pool_account).is_zero()),
                    "a pool with liquidity holds less than it keeps of an asset"
                );
                ensure!(
                    Self::reserve_balance(pair.0.clone(), &pool.pool_account) >= pool.reserve_a
                        && Self::reserve_balance(pair.1.clone(), &pool.pool_account)
                            >= pool.reserve_b,
                    "a pool account holds less than its reserves"
                );
                if !total.is_zero() {
                    let positions = LiquidityPositions::<T>::iter()
                        .filter(|(_, p, _)| *p == pair)
                        .fold(T::Balance::zero(), |a, (_, _, l)| a.saturating_add(l.shares));
                    ensure!(
                        positions.saturating_add(MINIMUM_LIQUIDITY.into()) == total,
                        "a pool's shares differ from its positions"
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
        /// or withdrawn. Separate from every pool account so `sync_reserves`
        /// never counts accrued fees as depth.
        pub fn fee_escrow_account() -> T::AccountId {
            PALLET_ID.into_sub_account_truncating(b"fees")
        }

        /// D4: where protocol fees are paid right now — the storage override
        /// or the runtime default. Also read by the launchpad's `Treasury`
        /// binding so all protocol revenue lands in one place.
        pub fn protocol_fee_recipient() -> T::AccountId {
            ProtocolFeeRecipient::<T>::get().unwrap_or_else(T::DefaultProtocolFeeRecipient::get)
        }

        /// D4: the split a pool gets at creation. Snapshotted from the live
        /// default; a pool with no native side can route nothing and gets
        /// zero; `seeded = false` (plain `create_pool`) folds the creator
        /// and treasury slices into the pool since nobody could claim them
        /// (D9: a treasury exists only for a launch asset).
        /// D10: the split a new pool of `fee_tier` snapshots. A pool with no
        /// native side routes nothing. A seeded pool takes the tier's default
        /// whole and **fails** if that tier has none — governance setting one
        /// tier and forgetting another must not graduate a launch whose
        /// treasury would then be fed nothing, for the life of the pool. A
        /// `create_pool` pool takes the protocol slice alone, and nothing at
        /// all where the tier is unconfigured.
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

        /// `floor(amount × bps / BPS)`.
        fn bps_of(amount: T::Balance, bps: u16) -> Result<T::Balance, Error<T>> {
            if bps == 0 {
                return Ok(Zero::zero());
            }
            Self::mul_div_floor(amount, (bps as u32).into(), BPS.into())
        }

        /// Finding 5: canonicalize a pair so that the lexicographically smaller
        /// encoded asset comes first. Prevents duplicate pools for (A,B) vs (B,A).
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
        /// asset. Every entry point that accepts `(asset_a, asset_b, x_a, x_b)`
        /// must go through this rather than binding `x_a` to `pair.0` blindly.
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

        /// Finding 2: sync pool reserves from actual on-chain asset balances.
        /// Absorbs any direct transfers or previously uncounted fees into the
        /// reserve tracking so the AMM math operates on accurate figures.
        /// Runs at the top of `do_swap`, which is why an off-chain quote must
        /// start from the same balances (see [`PoolInfo`]).
        fn sync_reserves(
            pair: &(T::AssetKind, T::AssetKind),
            pool: &mut PoolInfo<T::Balance, T::AccountId>,
        ) {
            pool.reserve_a = Self::reserve_balance(pair.0.clone(), &pool.pool_account);
            pool.reserve_b = Self::reserve_balance(pair.1.clone(), &pool.pool_account);
        }

        /// What a pool account keeps of `asset` as its own (see
        /// [`PoolInfo`]): the ED of native, which is also the account's
        /// provider, and one less than a token's minimum balance. That much
        /// is enough for a token: a swap or a withdrawal always leaves at
        /// least one unit of the reserve (a swap takes less than the reserve,
        /// and the locked minimum's share is at least a unit), so the account
        /// keeps at least the minimum. A token whose minimum is one unit, as
        /// every launch token's is, needs nothing kept.
        pub(crate) fn pool_buffer(asset: &T::AssetKind) -> T::Balance {
            let min = T::Assets::minimum_balance(asset.clone());
            if asset.encode() == T::NativeAsset::get().encode() {
                min
            } else {
                min.saturating_sub(One::one())
            }
        }

        /// What `pool_account` holds of `asset` as a reserve: its balance,
        /// less what it keeps as its own ([`Self::pool_buffer`]).
        pub(crate) fn reserve_balance(
            asset: T::AssetKind,
            pool_account: &T::AccountId,
        ) -> T::Balance {
            let buffer = Self::pool_buffer(&asset);
            T::Assets::balance(asset, pool_account).saturating_sub(buffer)
        }

        /// How much of `asset` `pool_account` lacks of what it keeps.
        fn buffer_short(asset: T::AssetKind, pool_account: &T::AccountId) -> T::Balance {
            Self::pool_buffer(&asset).saturating_sub(T::Assets::balance(asset, pool_account))
        }

        /// How a payer may spend `asset`: keeping its ED of native, and all
        /// of a token (see `do_swap`).
        fn keep_of(asset: &T::AssetKind) -> Preservation {
            if asset.encode() == T::NativeAsset::get().encode() {
                Preserve
            } else {
                Expendable
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

        /// Whether `amount` of `asset` can leave `pool_account` and leave
        /// what stays there intact. pallet-assets pays out with a debit any
        /// remainder below the asset's minimum balance (`ReducedToZero`),
        /// and a pool's remainder belongs to its other LPs.
        pub(crate) fn can_debit_pool(
            asset: T::AssetKind,
            pool_account: &T::AccountId,
            amount: T::Balance,
        ) -> bool {
            matches!(
                T::Assets::can_withdraw(asset, pool_account, amount),
                WithdrawConsequence::Success
            )
        }

        /// For a pool with no native side, give `pool_account` its ED of
        /// native — its provider — paid by `payer`, before any token lands.
        /// A pool with a native side gets it with that side's deposit.
        fn endow_native_side(
            pair: &(T::AssetKind, T::AssetKind),
            pool_account: &T::AccountId,
            payer: &T::AccountId,
        ) -> DispatchResult {
            let native = T::NativeAsset::get();
            if pair.0.encode() == native.encode() || pair.1.encode() == native.encode() {
                return Ok(());
            }
            let short = Self::buffer_short(native.clone(), pool_account);
            if !short.is_zero() {
                T::Assets::transfer(native, payer, pool_account, short, Preserve)?;
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

        // ---- Wide-precision arithmetic helpers (D1) --------------------------
        //
        // All pool math that multiplies two `Balance`s goes through these. The
        // rounding direction of every division is the same as the original
        // `u128` code (floor), so nothing here changes which side a rounding
        // favours; it only removes the overflow.

        /// Lift a `Balance` into the wide type.
        fn hp(b: T::Balance) -> T::HigherPrecisionBalance {
            T::HigherPrecisionBalance::from(b)
        }

        /// Narrow a wide value back to `Balance`, failing with `Overflow` rather
        /// than truncating.
        fn narrow(x: T::HigherPrecisionBalance) -> Result<T::Balance, Error<T>> {
            x.try_into().map_err(|_| Error::<T>::Overflow)
        }

        /// `floor(a * b / c)` computed in wide precision. Errors with `Overflow`
        /// if the product overflows the wide type or the result does not fit
        /// `Balance`, and with `InsufficientLiquidity` if `c == 0`.
        fn mul_div_floor(
            a: T::Balance,
            b: T::Balance,
            c: T::Balance,
        ) -> Result<T::Balance, Error<T>> {
            let q = Self::hp(a)
                .checked_mul(&Self::hp(b))
                .ok_or(Error::<T>::Overflow)?
                .checked_div(&Self::hp(c))
                .ok_or(Error::<T>::InsufficientLiquidity)?;
            Self::narrow(q)
        }

        /// `floor(sqrt(a * b))` computed in wide precision.
        fn sqrt_of_product(a: T::Balance, b: T::Balance) -> Result<T::Balance, Error<T>> {
            let p = Self::hp(a).checked_mul(&Self::hp(b)).ok_or(Error::<T>::Overflow)?;
            Self::narrow(p.integer_sqrt())
        }

        /// Whether a pool exists for the (unordered) asset pair.
        pub fn pool_exists(asset_a: T::AssetKind, asset_b: T::AssetKind) -> bool {
            Pools::<T>::contains_key(Self::canonical_pair(asset_a, asset_b))
        }

        /// Create a new AMM pool for the given asset pair and fee tier.
        ///
        /// Origin-free body of the `create_pool` extrinsic. The extrinsic gates
        /// this on `ManageOrigin`; in-runtime callers (see [`PoolManager`]) are
        /// trusted to apply their own authorisation before calling.
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
            Self::ensure_valid_fee_tier(fee_tier)?;

            // Finding 5: canonicalize pair to prevent duplicate pools.
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

        /// The sub-account that holds a pool's reserves, for the (unordered) pair.
        ///
        /// Deterministic in the pair, so it can be computed before the pool
        /// exists — which is exactly why `seed_reserved_pool_for` sweeps it.
        ///
        /// D8 (Finding 13): the seed is `blake2_256` of the pair key, not the
        /// key itself. `into_sub_account_truncating` keeps only the first
        /// `size_of::<AccountId>()` bytes of `"modl" ++ PalletId ++ seed`;
        /// with the raw key as the seed an AccountId20 runtime kept eight
        /// bytes of it — `04 00 44 01` and the low four bytes of the second
        /// asset id for a native pair, so chain asset `n` and launch asset
        /// `2^64 + n` shared one pool account, and for a `(WithId, WithId)`
        /// pair the second asset never featured at all. Eight bytes of a
        /// hash do not collide. (Finding 6's length prefixes addressed
        /// ambiguity *within* the key; the key was then truncated anyway.)
        pub fn pool_account_for(asset_a: T::AssetKind, asset_b: T::AssetKind) -> T::AccountId {
            let pair = Self::canonical_pair(asset_a, asset_b);
            let pair_key = (pair.0.encode(), pair.1.encode());
            let seed = sp_io::hashing::blake2_256(&pair_key.encode());
            PALLET_ID.into_sub_account_truncating(seed)
        }

        /// Write a fresh, empty pool record for an already-canonical `pair`.
        /// Callers must have checked that no pool exists. D9: the snapshotted
        /// split must fit the tier (`InvalidFeeRouting` otherwise — at seed
        /// time that surfaces as a deferred graduation, FM-11).
        fn insert_new_pool(
            pair: &(T::AssetKind, T::AssetKind),
            fee_tier: u32,
            routing: FeeRouting,
        ) -> DispatchResult {
            ensure!(routing.is_valid_for(fee_tier), Error::<T>::InvalidFeeRouting);
            let pool = PoolInfo {
                reserve_a: Zero::zero(),
                reserve_b: Zero::zero(),
                fee_tier,
                total_fees_collected: Zero::zero(),
                pool_account: Self::pool_account_for(pair.0.clone(), pair.1.clone()),
                routing,
            };

            Pools::<T>::insert(pair, pool);
            TotalLiquidity::<T>::insert(pair, T::Balance::zero());

            Self::deposit_event(Event::PoolCreated {
                asset_a: pair.0.clone(),
                asset_b: pair.1.clone(),
                fee_tier,
            });
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
        ) -> Result<T::Balance, DispatchError> {
            ensure!(
                amount_asset > Zero::zero() && amount_quote > Zero::zero(),
                Error::<T>::ZeroAmount
            );
            // Only reserved assets come through here, and only against a
            // non-reserved quote, so this path cannot be used to mint arbitrary
            // permanently-locked pools around `ManageOrigin`.
            ensure!(T::ReservedAssets::contains(&asset), Error::<T>::NotReservedAsset);
            ensure!(!T::ReservedAssets::contains(&quote), Error::<T>::NotReservedAsset);
            ensure!(asset.encode() != quote.encode(), Error::<T>::NotReservedAsset);
            Self::ensure_valid_fee_tier(fee_tier)?;

            // 1. Create, or adopt an empty pool.
            let pair = Self::canonical_pair(asset.clone(), quote.clone());
            match Pools::<T>::get(&pair) {
                // D4: a seeded pool has a creator (the launch's fee recipient),
                // so it snapshots the full default split. An adopted empty
                // record keeps whatever split it carries.
                None => Self::insert_new_pool(
                    &pair,
                    fee_tier,
                    Self::routing_for_new_pool(&pair, fee_tier, true)?,
                )?,
                Some(existing) => {
                    let shares = TotalLiquidity::<T>::get(&pair).unwrap_or_else(Zero::zero);
                    ensure!(shares.is_zero(), Error::<T>::PoolAlreadySeeded);
                    ensure!(existing.fee_tier == fee_tier, Error::<T>::InvalidFeeTier);
                },
            }
            let mut pool = Pools::<T>::get(&pair).ok_or(Error::<T>::PoolNotFound)?;
            ensure!(
                pool.reserve_a.is_zero() && pool.reserve_b.is_zero(),
                Error::<T>::PoolAlreadySeeded
            );

            // 2. Sweep whatever already sits in the pool sub-account (FM-02).
            //    The reserved asset goes first: for a non-sufficient asset the
            //    pool account's asset balance holds a consumer reference on its
            //    native account, which must be released before the native
            //    balance can be swept to zero.
            let excess_to = T::ExcessRecipient::get();
            let sweep_order = if T::ReservedAssets::contains(&pair.0) {
                [pair.0.clone(), pair.1.clone()]
            } else {
                [pair.1.clone(), pair.0.clone()]
            };
            for swept in sweep_order {
                // Everything but the pool account's own ED (see `PoolInfo`).
                let keep = if swept.encode() == T::NativeAsset::get().encode() {
                    Preserve
                } else {
                    Expendable
                };
                let held =
                    T::Assets::reducible_balance(swept.clone(), &pool.pool_account, keep, Polite);
                if held.is_zero() {
                    continue;
                }
                // Deliver to the recipient. If it cannot take the asset (for
                // example a treasury with no provider cannot hold a
                // non-sufficient asset) fail the seed with a distinct error
                // rather than burning: a silent burn would make a mis-wired
                // recipient look like correct operation, whereas a loud failure
                // is fixed by re-wiring and retrying. The attempt runs in its
                // own storage layer so the underlying error leaves nothing
                // half-applied before the outer transactional rollback.
                frame_support::storage::with_storage_layer(|| {
                    T::Assets::transfer(swept.clone(), &pool.pool_account, &excess_to, held, keep)
                })
                .map_err(|_| Error::<T>::ExcessRecipientCannotReceive)?;
                Self::deposit_event(Event::PreSeedBalanceSwept {
                    pool: pair.clone(),
                    asset: swept,
                    amount: held,
                    to: excess_to.clone(),
                });
            }

            // The pool account's own minimum of each asset (see `PoolInfo`),
            // where it holds less: paid by the seeder on top of its deposit,
            // as any first depositor pays it. A launch sets the native one
            // aside at creation; what the seeder cannot spare on top — the
            // launch token, which it holds exactly as much of as it deposits,
            // or the ED for a launch created before launches set it aside —
            // comes out of the deposit instead, so a graduation never fails
            // on it. Native first: it is the pool account's provider.
            Self::endow_native_side(&pair, &pool.pool_account, who)?;
            // For each side: the reserve it leaves, and what is paid on top.
            let split = |a: &T::AssetKind,
                         amount: T::Balance|
             -> Result<(T::Balance, T::Balance), DispatchError> {
                let short = Self::buffer_short(a.clone(), &pool.pool_account);
                let keep = Self::keep_of(a);
                if short.is_zero() {
                    Ok((amount, Zero::zero()))
                } else if T::Assets::reducible_balance(a.clone(), who, keep, Polite)
                    >= amount.saturating_add(short)
                {
                    Ok((amount, short))
                } else {
                    Ok((
                        amount
                            .checked_sub(&short)
                            .ok_or(Error::<T>::InsufficientInitialLiquidity)?,
                        Zero::zero(),
                    ))
                }
            };
            let (quote_reserve, quote_on_top) = split(&quote, amount_quote)?;
            let (asset_reserve, asset_on_top) = split(&asset, amount_asset)?;

            let asset_is_a = pair.0.encode() == asset.encode();
            let (amount_a, amount_b) = if asset_is_a {
                (asset_reserve, quote_reserve)
            } else {
                (quote_reserve, asset_reserve)
            };

            // Finding 1: first deposit — burn MINIMUM_LIQUIDITY shares permanently.
            let raw_shares = Self::sqrt_of_product(amount_a, amount_b)?;
            let min_liq: T::Balance = MINIMUM_LIQUIDITY.into();
            ensure!(raw_shares > min_liq, Error::<T>::InsufficientInitialLiquidity);
            let shares_to_mint = raw_shares.checked_sub(&min_liq).ok_or(Error::<T>::Overflow)?;
            ensure!(shares_to_mint > Zero::zero(), Error::<T>::ZeroAmount);

            // Quote before asset (it is the provider), each side's minimum
            // with it where paid on top.
            T::Assets::transfer(
                quote.clone(),
                who,
                &pool.pool_account,
                amount_quote.saturating_add(quote_on_top),
                Expendable,
            )?;
            T::Assets::transfer(
                asset.clone(),
                who,
                &pool.pool_account,
                amount_asset.saturating_add(asset_on_top),
                Expendable,
            )?;

            pool.reserve_a = amount_a;
            pool.reserve_b = amount_b;
            Pools::<T>::insert(&pair, &pool);
            TotalLiquidity::<T>::insert(&pair, raw_shares);

            // 4. Position, locked forever. `who` never has an existing position
            //    here (the pool had no shares), so this is an insert.
            let current_block = frame_system::Pallet::<T>::block_number();
            let forever: BlockNumberFor<T> = Bounded::max_value();
            LiquidityPositions::<T>::insert(
                who,
                &pair,
                LiquidityPosition {
                    shares: shares_to_mint,
                    entry_block: current_block,
                    locked_until: Some(forever),
                },
            );

            Self::deposit_event(Event::LiquidityAdded {
                provider: who.clone(),
                asset_a: pair.0.clone(),
                asset_b: pair.1.clone(),
                amount_a,
                amount_b,
                shares_minted: shares_to_mint,
            });
            Self::deposit_event(Event::LiquidityLocked {
                who: who.clone(),
                pool: pair,
                locked_until: forever,
            });
            Self::deposit_event(Event::ReservedPoolSeeded {
                who: who.clone(),
                asset,
                quote,
                amount_asset,
                amount_quote,
                shares: shares_to_mint,
            });
            Ok(shares_to_mint)
        }

        /// Add liquidity to an existing pool on behalf of `who`.
        ///
        /// Body of the `add_liquidity` extrinsic with the signer resolved by the
        /// caller. Tokens are pulled from `who`, and the LP position is credited
        /// to `who`. Returns the LP shares minted to `who` (excluding the
        /// permanently locked `MINIMUM_LIQUIDITY` on a first deposit).
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

            // Finding 5 + D5: canonicalize the pair AND the amounts/minimums,
            // which the caller gave in (asset_a, asset_b) order. From here on
            // `amount_a` belongs to `pair.0` and `amount_b` to `pair.1`.
            let (pair, (amount_a, amount_a_min), (amount_b, amount_b_min)) =
                Self::canonical_pair_with(
                    asset_a,
                    asset_b,
                    (amount_a, amount_a_min),
                    (amount_b, amount_b_min),
                );
            let mut pool = Pools::<T>::get(&pair).ok_or(Error::<T>::PoolNotFound)?;

            // D6: price the deposit against what the pool actually holds. The
            // pool's share of every swap fee sits in the account uncounted
            // until the next sync (Finding 3); pricing against the stale
            // recorded reserves minted a depositor shares against a smaller
            // pool than the one `remove_liquidity` (which syncs) pays out of,
            // so swap → add → remove skimmed existing LPs' fees.
            Self::sync_reserves(&pair, &mut pool);

            let total_shares = TotalLiquidity::<T>::get(&pair).unwrap_or_else(T::Balance::zero);

            // Determine actual deposit amounts and shares to mint.
            let (actual_a, actual_b, total_new_shares, shares_to_mint) = if total_shares.is_zero() {
                // Finding 1: first deposit — burn MINIMUM_LIQUIDITY shares permanently.
                // D1: wide precision; integer sqrt rounds DOWN, in the pool's favour.
                let raw_shares = Self::sqrt_of_product(amount_a, amount_b)?;
                let min_liq: T::Balance = MINIMUM_LIQUIDITY.into();
                ensure!(raw_shares > min_liq, Error::<T>::InsufficientInitialLiquidity);
                let shares_to_mint =
                    raw_shares.checked_sub(&min_liq).ok_or(Error::<T>::Overflow)?;
                // total includes the locked minimum; user only receives the remainder.
                (amount_a, amount_b, raw_shares, shares_to_mint)
            } else {
                // Finding 8: calculate optimal amounts — don't donate excess tokens.
                // D1: wide precision; floor rounds the matched amount DOWN so the
                // depositor never over-contributes relative to the pool ratio.
                let optimal_b = Self::mul_div_floor(amount_a, pool.reserve_b, pool.reserve_a)?;

                let (actual_a, actual_b) = if optimal_b <= amount_b {
                    (amount_a, optimal_b)
                } else {
                    let optimal_a = Self::mul_div_floor(amount_b, pool.reserve_a, pool.reserve_b)?;
                    (optimal_a, amount_b)
                };

                // D1: floor rounds minted shares DOWN, in the pool's favour.
                let share_a = Self::mul_div_floor(actual_a, total_shares, pool.reserve_a)?;
                let share_b = Self::mul_div_floor(actual_b, total_shares, pool.reserve_b)?;
                let shares = if share_a < share_b { share_a } else { share_b };

                (actual_a, actual_b, shares, shares)
            };

            ensure!(shares_to_mint > Zero::zero(), Error::<T>::ZeroAmount);

            // Finding 7: enforce slippage on the actual (possibly adjusted) amounts.
            ensure!(actual_a >= amount_a_min, Error::<T>::SlippageExceeded);
            ensure!(actual_b >= amount_b_min, Error::<T>::SlippageExceeded);

            // What the pool account keeps of each asset (see `PoolInfo`),
            // where it lacks it — on the first deposit — from the depositor,
            // on top of the amounts and in the same credit (a token's part
            // alone could be below its minimum). A pool with no native side
            // gets its ED first, as a provider for the tokens.
            Self::endow_native_side(&pair, &pool.pool_account, who)?;
            let extra_a = Self::buffer_short(pair.0.clone(), &pool.pool_account);
            let extra_b = Self::buffer_short(pair.1.clone(), &pool.pool_account);
            T::Assets::transfer(
                pair.0.clone(),
                who,
                &pool.pool_account,
                actual_a.saturating_add(extra_a),
                Expendable,
            )?;
            T::Assets::transfer(
                pair.1.clone(),
                who,
                &pool.pool_account,
                actual_b.saturating_add(extra_b),
                Expendable,
            )?;

            pool.reserve_a = pool.reserve_a.checked_add(&actual_a).ok_or(Error::<T>::Overflow)?;
            pool.reserve_b = pool.reserve_b.checked_add(&actual_b).ok_or(Error::<T>::Overflow)?;
            Pools::<T>::insert(&pair, &pool);

            let new_total =
                total_shares.checked_add(&total_new_shares).ok_or(Error::<T>::Overflow)?;
            TotalLiquidity::<T>::insert(&pair, new_total);

            let current_block = frame_system::Pallet::<T>::block_number();
            LiquidityPositions::<T>::try_mutate(who, &pair, |maybe_pos| -> DispatchResult {
                match maybe_pos {
                    Some(pos) => {
                        // Finding 9: keep original entry_block on top-up.
                        pos.shares =
                            pos.shares.checked_add(&shares_to_mint).ok_or(Error::<T>::Overflow)?;
                    },
                    None => {
                        *maybe_pos = Some(LiquidityPosition {
                            shares: shares_to_mint,
                            entry_block: current_block,
                            locked_until: None,
                        });
                    },
                }
                Ok(())
            })?;

            Self::deposit_event(Event::LiquidityAdded {
                provider: who.clone(),
                asset_a: pair.0,
                asset_b: pair.1,
                amount_a: actual_a,
                amount_b: actual_b,
                shares_minted: shares_to_mint,
            });
            Ok(shares_to_mint)
        }

        /// Extend the lock on `who`'s liquidity position in the given pool to
        /// `lock_until`.
        ///
        /// Body of the `lock_liquidity` extrinsic with the signer resolved by
        /// the caller. Fails with `InsufficientShares` if `who` has no position.
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

            LiquidityPositions::<T>::try_mutate(who, &pair, |maybe_pos| -> DispatchResult {
                let pos = maybe_pos.as_mut().ok_or(Error::<T>::InsufficientShares)?;
                if let Some(existing) = pos.locked_until {
                    ensure!(lock_until >= existing, Error::<T>::LockCannotBeShortened);
                }
                pos.locked_until = Some(lock_until);
                Ok(())
            })?;

            Self::deposit_event(Event::LiquidityLocked {
                who: who.clone(),
                pool: pair,
                locked_until: lock_until,
            });

            Ok(())
        }

        /// Execute a swap on behalf of `who`, depositing output to `recipient`.
        ///
        /// Returns the actual `amount_out` transferred to `recipient`. Callers use
        /// this for slippage-capture economics (e.g., [`IntentSwap`]).
        ///
        /// Behaviorally identical to the body of `swap_exact_tokens_for_tokens`:
        /// loads the pool, syncs reserves, computes constant-product output, applies
        /// fee, performs transfers, updates storage, emits `SwapExecuted` and
        /// `FeesCollected`.
        /// `is_trade`: whether this swap counts as market activity for
        /// `LastSwapBlock`. A person's swap and a settled intent do; the
        /// launch treasury's buyback through [`PoolManager::swap_for`] does
        /// not — the dormancy rule that reads it asks whether anyone still
        /// trades the token, and the pallet buying it back is not an
        /// answer (R2).
        ///
        /// `input`: how `amount_in` leaves `who`. A person keeps their ED
        /// (`Preserve`): a swap of one's whole native balance used to kill
        /// the account and then fail to deliver a non-sufficient asset to it
        /// (`CannotCreate`, R9 — reachable, since fees are paid in energy).
        /// A settled intent swaps from the marketplace's intent escrow
        /// through [`IntentSwap`], which spends it to zero (`Expendable`).
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

            // Finding 5: canonicalize pair for lookup.
            let pair = Self::canonical_pair(asset_in.clone(), asset_out.clone());
            let mut pool = Pools::<T>::get(&pair).ok_or(Error::<T>::PoolNotFound)?;

            // Finding 2: sync reserves from actual on-chain balances.
            Self::sync_reserves(&pair, &mut pool);

            let flipped = pair.0.encode() != asset_in.encode();
            let (reserve_in, reserve_out) = if flipped {
                (pool.reserve_b, pool.reserve_a)
            } else {
                (pool.reserve_a, pool.reserve_b)
            };

            ensure!(
                !reserve_in.is_zero() && !reserve_out.is_zero(),
                Error::<T>::InsufficientLiquidity
            );

            // ---- D4: fee split ----------------------------------------------
            //
            // The tier is the total fee (0.1% / 0.3% / 1.0%). The routed part
            // (`protocol_bps + creator_bps`, snapshotted per pool) is always
            // taken in the native asset; the pool keeps the rest:
            //
            //   native in : fee   = tier of input (as before)
            //               routed slices come out of that fee
            //               reserves get input − fee (unchanged output)
            //   native out: input fee = (tier − routed) of input, stays in pool
            //               output is priced on that; routed slices come
            //               out of the gross output, trader gets the net
            //   neither   : nothing is routed (fee stays in the pool)
            //
            // Every division floors, in the pool's favour; k never decreases.
            let native = T::NativeAsset::get();
            let native_in = asset_in.encode() == native.encode();
            let native_out = asset_out.encode() == native.encode();
            let routing =
                if native_in || native_out { pool.routing } else { FeeRouting::default() };
            let tier_bps: u16 = (pool.fee_tier as u16).saturating_mul(10);
            let pool_bps = tier_bps.saturating_sub(routing.routed_bps());

            let fee_tier_bal: T::Balance = pool.fee_tier.into();
            let denominator_bal: T::Balance = FEE_DENOMINATOR.into();

            // Fee taken from the input. With native in (or nothing routed)
            // this is the full tier, computed exactly as before D4.
            let fee = if native_out && routing.routed_bps() > 0 {
                Self::bps_of(amount_in, pool_bps)?
            } else {
                // D1: wide precision. Fee keeps its pre-existing floor (rounds
                // the fee DOWN by < 1 unit). Direction unchanged by this change.
                Self::mul_div_floor(amount_in, fee_tier_bal, denominator_bal)?
            };
            let amount_in_after_fee = amount_in.checked_sub(&fee).ok_or(Error::<T>::Overflow)?;

            // D1: constant-product output in wide precision; floor rounds
            // amount_out DOWN, in the pool's favour, so k never decreases.
            let denom = reserve_in.checked_add(&amount_in_after_fee).ok_or(Error::<T>::Overflow)?;
            let amount_out_gross = Self::mul_div_floor(reserve_out, amount_in_after_fee, denom)?;

            // Routed slices, always native.
            let (mut protocol, creator, mut treasury) = if native_in {
                (
                    Self::bps_of(amount_in, routing.protocol_bps)?,
                    Self::bps_of(amount_in, routing.creator_bps)?,
                    Self::bps_of(amount_in, routing.treasury_bps)?,
                )
            } else if native_out {
                (
                    Self::bps_of(amount_out_gross, routing.protocol_bps)?,
                    Self::bps_of(amount_out_gross, routing.creator_bps)?,
                    Self::bps_of(amount_out_gross, routing.treasury_bps)?,
                )
            } else {
                (Zero::zero(), Zero::zero(), Zero::zero())
            };
            // D9: the treasury slice goes to the launch's treasury sink if
            // the asset has one, else it folds into the protocol share. The
            // non-native side of the pair is the launch asset.
            let other = if native_in { asset_out.clone() } else { asset_in.clone() };
            let sink = if treasury.is_zero() { None } else { T::TreasurySink::account_for(&other) };
            if sink.is_none() && !treasury.is_zero() {
                protocol = protocol.checked_add(&treasury).ok_or(Error::<T>::Overflow)?;
                treasury = Zero::zero();
            }
            let routed = protocol
                .checked_add(&creator)
                .ok_or(Error::<T>::Overflow)?
                .checked_add(&treasury)
                .ok_or(Error::<T>::Overflow)?;
            // With native in the routed part is a sub-slice of `fee` (bounded
            // by `routed_bps ≤ tier × 10`, D9); with native out it comes off
            // the gross output.
            let amount_out = if native_out {
                amount_out_gross.checked_sub(&routed).ok_or(Error::<T>::Overflow)?
            } else {
                amount_out_gross
            };

            ensure!(amount_out >= amount_out_min, Error::<T>::SlippageExceeded);
            // R10: an input so small its output rounds to nothing is refused
            // here, not by pallet-assets declining to open the recipient's
            // token account with a zero balance (`BelowMinimum`).
            ensure!(!amount_out.is_zero(), Error::<T>::ZeroAmount);
            // The same for an output the recipient cannot take, as the
            // asset's pallet judges it: below the asset's minimum balance for
            // a new holder, or no consumer reference left to open one.
            ensure!(
                Self::can_credit(asset_out.clone(), recipient, amount_out),
                Error::<T>::RecipientCannotReceive
            );
            ensure!(amount_out_gross < reserve_out, Error::<T>::InsufficientLiquidity);
            // The pool's remainder of `asset_out` must stay intact: a debit
            // leaving less than the asset's minimum balance would pay the
            // remainder out with it, and it belongs to the LPs.
            ensure!(
                Self::can_debit_pool(asset_out.clone(), &pool.pool_account, amount_out_gross),
                Error::<T>::ReserveBelowMinimum
            );

            // `input` is about the native ED: a person keeps theirs (R9). A
            // token has no such floor for its holder — `Preserve` on a
            // pallet-assets balance refuses to take the last unit
            // (`NotExpendable`), which would leave every holder unable to
            // sell their whole position (R11). Only the native side keeps.
            let input = if native_in { input } else { Expendable };
            T::Assets::transfer(asset_in.clone(), who, &pool.pool_account, amount_in, input)?;
            T::Assets::transfer(
                asset_out.clone(),
                &pool.pool_account,
                recipient,
                amount_out,
                Expendable,
            )?;
            // Finding 14 (SECURITY_AUDIT): a routed slice below the native ED
            // cannot create a recipient account that does not exist yet — the
            // fee escrow before its first fee, or the treasury vault before it is
            // funded — and pallet-balances fails the transfer, which used to fail
            // the whole swap with an unreadable `Token(BelowMinimum)`. When a
            // slice would leave its recipient's native below the ED, leave that
            // slice in the pool: it accrues to LPs at the next `sync_reserves`,
            // exactly as the pool's own fee share does (D7). The consequence,
            // stated plainly so no later reader treats a routing total as exact:
            // `ProtocolFeesUnclaimed`, `CreatorFeesUnclaimed` and the sink's
            // tally do NOT count a sub-ED slice that was redirected to LPs this
            // way. The window is only "before the recipient holds an ED of
            // native" — not "before its account exists": an account kept alive
            // by a sufficient asset holds none. Once it does, every later slice
            // of any size routes in full, and genesis or `migrations::v6` funds
            // the fee escrow so the window never opens for it on a real chain.
            let fee_escrow = Self::fee_escrow_account();
            // D4: move the routed slices out of the pool account into the fee
            // escrow, so neither reserves nor `sync_reserves` ever see them.
            let escrowed = protocol.checked_add(&creator).ok_or(Error::<T>::Overflow)?;
            let route_escrowed =
                !escrowed.is_zero() && Self::can_credit(native.clone(), &fee_escrow, escrowed);
            if route_escrowed {
                T::Assets::transfer(
                    native.clone(),
                    &pool.pool_account,
                    &fee_escrow,
                    escrowed,
                    Expendable,
                )?;
                if !protocol.is_zero() {
                    ProtocolFeesUnclaimed::<T>::mutate(|t| *t = t.saturating_add(protocol));
                }
                if !creator.is_zero() {
                    CreatorFeesUnclaimed::<T>::mutate(&pair, |t| *t = t.saturating_add(creator));
                }
            }
            // D9: the treasury slice is pushed to the sink (see `TreasurySink`
            // for why a push is safe here and was not for the other two), under
            // the same Finding-14 guard.
            if let Some(vault) = sink {
                if !treasury.is_zero() && Self::can_credit(native.clone(), &vault, treasury) {
                    T::Assets::transfer(
                        native.clone(),
                        &pool.pool_account,
                        &vault,
                        treasury,
                        Expendable,
                    )?;
                    T::TreasurySink::note_fee(&other, treasury);
                }
            }

            // Finding 3: only add amount_in_after_fee to reserves; the pool's
            // share of the fee stays in the pool account but is not counted in
            // reserves until the next sync. The output side drops by the gross
            // amount (net to the trader + routed slices).
            if flipped {
                pool.reserve_b =
                    pool.reserve_b.checked_add(&amount_in_after_fee).ok_or(Error::<T>::Overflow)?;
                pool.reserve_a = pool
                    .reserve_a
                    .checked_sub(&amount_out_gross)
                    .ok_or(Error::<T>::InsufficientLiquidity)?;
            } else {
                pool.reserve_a =
                    pool.reserve_a.checked_add(&amount_in_after_fee).ok_or(Error::<T>::Overflow)?;
                pool.reserve_b = pool
                    .reserve_b
                    .checked_sub(&amount_out_gross)
                    .ok_or(Error::<T>::InsufficientLiquidity)?;
            }
            pool.total_fees_collected =
                pool.total_fees_collected.checked_add(&fee).ok_or(Error::<T>::Overflow)?;

            let pool_account_for_event = pool.pool_account.clone();
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

            // Finding 11: emit FeesCollected event.
            Self::deposit_event(Event::FeesCollected {
                pool: pair.clone(),
                amount: fee,
                recipient: pool_account_for_event,
            });
            if !routed.is_zero() {
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
        let pair = Self::canonical_pair(native.clone(), asset.clone());
        let pool = Pools::<T>::get(&pair)?;
        Some((
            Self::reserve_balance(native, &pool.pool_account),
            Self::reserve_balance(asset, &pool.pool_account),
        ))
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
    ) -> Result<T::Balance, DispatchError> {
        Self::do_seed_reserved_pool_for(who, asset, quote, amount_asset, amount_quote, fee_tier)
    }
}

/// Storage migrations.
pub mod migrations {
    use super::*;
    use frame_support::{
        migrations::VersionedMigration,
        pallet_prelude::OptionQuery,
        traits::{Get, UncheckedOnRuntimeUpgrade},
        weights::Weight,
    };
    use sp_std::marker::PhantomData;

    /// D4 (v0 → v1): `PoolInfo` gains `routing`. Every existing pool gets
    /// `FeeRouting::default()` — zero — and keeps 100% of its fee in the
    /// pool. That includes the launchpad pool that graduated before D4
    /// (DLNCH on the dev chain): it was seeded under the terms in force at
    /// the time, and changing a live pool's split retroactively is exactly
    /// the parameter mutation the per-pool snapshot exists to prevent.
    pub mod v1 {
        use super::*;

        /// `PoolInfo` as stored before D4.
        #[derive(Encode, Decode)]
        #[allow(missing_docs)]
        pub struct OldPoolInfo<Balance, AccountId> {
            pub reserve_a: Balance,
            pub reserve_b: Balance,
            pub fee_tier: u32,
            pub total_fees_collected: Balance,
            pub pool_account: AccountId,
        }

        /// Unversioned body; wrap in [`MigrateToV1`].
        pub struct VersionUncheckedMigrateToV1<T>(PhantomData<T>);

        impl<T: Config> UncheckedOnRuntimeUpgrade for VersionUncheckedMigrateToV1<T> {
            fn on_runtime_upgrade() -> Weight {
                let mut count = 0u64;
                Pools::<T>::translate::<OldPoolInfo<T::Balance, T::AccountId>, _>(|_pair, old| {
                    count = count.saturating_add(1);
                    Some(PoolInfo {
                        reserve_a: old.reserve_a,
                        reserve_b: old.reserve_b,
                        fee_tier: old.fee_tier,
                        total_fees_collected: old.total_fees_collected,
                        pool_account: old.pool_account,
                        routing: FeeRouting::default(),
                    })
                });
                log::info!(target: "runtime::vitreus-dex", "D4 migration: {count} pools now carry zero fee routing");
                T::DbWeight::get().reads_writes(count, count)
            }

            #[cfg(feature = "try-runtime")]
            fn post_upgrade(
                _state: sp_std::vec::Vec<u8>,
            ) -> Result<(), sp_runtime::TryRuntimeError> {
                for (_pair, pool) in Pools::<T>::iter() {
                    frame_support::ensure!(
                        pool.routing == FeeRouting::default(),
                        "every pre-D4 pool must carry zero routing"
                    );
                }
                Ok(())
            }
        }

        /// D4 migration, gated on the pallet's on-chain storage version.
        pub type MigrateToV1<T> = VersionedMigration<
            0,
            1,
            VersionUncheckedMigrateToV1<T>,
            Pallet<T>,
            <T as frame_system::Config>::DbWeight,
        >;
    }

    /// D8 (v1 → v2): pool accounts are derived from a hash of the pair key
    /// (Finding 13). Every pool's reserves move from the account the old
    /// derivation named to the one the new derivation names, and
    /// `PoolInfo.pool_account` is rewritten. Positions, shares and routing
    /// are untouched. Fork-only: no chain upstream has a pre-D8 pool.
    pub mod v2 {
        use super::*;
        use frame_support::traits::tokens::{
            Fortitude::Polite,
            Preservation::{Expendable, Preserve},
        };

        /// The pre-D8 derivation, kept here only to find where a pool's
        /// reserves are.
        pub fn old_pool_account_for<T: Config>(
            pair: &(T::AssetKind, T::AssetKind),
        ) -> T::AccountId {
            let pair_key = (pair.0.encode(), pair.1.encode());
            PALLET_ID.into_sub_account_truncating(&pair_key)
        }

        /// Move every unit of `asset` the old account holds to the new one.
        fn move_all<T: Config>(
            asset: T::AssetKind,
            from: &T::AccountId,
            to: &T::AccountId,
        ) -> Result<T::Balance, DispatchError> {
            let held = T::Assets::reducible_balance(asset.clone(), from, Expendable, Polite);
            if held.is_zero() {
                return Ok(held);
            }
            T::Assets::transfer(asset, from, to, held, Expendable)
        }

        /// Unversioned body; wrap in [`MigrateToV2`].
        pub struct VersionUncheckedMigrateToV2<T>(PhantomData<T>);

        impl<T: Config> UncheckedOnRuntimeUpgrade for VersionUncheckedMigrateToV2<T> {
            fn on_runtime_upgrade() -> Weight {
                let mut moved = 0u64;
                let mut reads = 0u64;
                let pools: sp_std::vec::Vec<_> = Pools::<T>::iter().collect();
                for (pair, mut pool) in pools {
                    reads = reads.saturating_add(1);
                    let old = old_pool_account_for::<T>(&pair);
                    let new = Pallet::<T>::pool_account_for(pair.0.clone(), pair.1.clone());
                    if pool.pool_account != old || old == new {
                        continue;
                    }
                    // A native-quoted pool: the new account must exist before
                    // it can hold a non-sufficient asset, and the old one
                    // must have dropped the asset's consumer reference before
                    // its last native unit can leave. So: most of the native
                    // (keeping the old account alive), then the asset, then
                    // the rest of the native. A pool with no native side has
                    // no such ordering constraint.
                    let native = T::NativeAsset::get();
                    let native_side = if pair.0 == native {
                        Some(&pair.0)
                    } else if pair.1 == native {
                        Some(&pair.1)
                    } else {
                        None
                    };
                    let result: Result<(), DispatchError> =
                        frame_support::storage::with_storage_layer(|| {
                            if let Some(n) = native_side {
                                let keep_alive =
                                    T::Assets::reducible_balance(n.clone(), &old, Preserve, Polite);
                                if !keep_alive.is_zero() {
                                    T::Assets::transfer(
                                        n.clone(),
                                        &old,
                                        &new,
                                        keep_alive,
                                        Preserve,
                                    )?;
                                }
                            }
                            for a in [&pair.0, &pair.1] {
                                if Some(a) != native_side {
                                    move_all::<T>(a.clone(), &old, &new)?;
                                }
                            }
                            if let Some(n) = native_side {
                                move_all::<T>(n.clone(), &old, &new)?;
                            }
                            Ok(())
                        });
                    match result {
                        Ok(()) => {
                            pool.pool_account = new;
                            Pools::<T>::insert(&pair, pool);
                            moved = moved.saturating_add(1);
                        },
                        Err(e) => {
                            log::error!(target: "runtime::vitreus-dex", "D8 migration: could not move a pool's reserves ({e:?}); its record still points at the old account");
                        },
                    }
                }
                log::info!(target: "runtime::vitreus-dex", "D8 migration: {moved} pools moved to hash-derived accounts");
                T::DbWeight::get().reads_writes(reads.saturating_mul(4), moved.saturating_mul(6))
            }

            #[cfg(feature = "try-runtime")]
            fn pre_upgrade() -> Result<sp_std::vec::Vec<u8>, sp_runtime::TryRuntimeError> {
                // Old accounts must be unique across pools, or the reserves in
                // a shared account cannot be attributed and this migration
                // must not run. (That state is the bug itself.)
                let mut seen = sp_std::vec::Vec::new();
                for (pair, _) in Pools::<T>::iter() {
                    let old = old_pool_account_for::<T>(&pair);
                    frame_support::ensure!(
                        !seen.contains(&old),
                        "two pools share a pre-D8 account"
                    );
                    seen.push(old);
                }
                log::info!(target: "runtime::vitreus-dex", "D8 pre_upgrade: {} pools, all pre-D8 accounts distinct", seen.len());
                Ok(sp_std::vec::Vec::new())
            }

            #[cfg(feature = "try-runtime")]
            fn post_upgrade(
                _state: sp_std::vec::Vec<u8>,
            ) -> Result<(), sp_runtime::TryRuntimeError> {
                let mut n = 0u32;
                for (pair, pool) in Pools::<T>::iter() {
                    let new = Pallet::<T>::pool_account_for(pair.0.clone(), pair.1.clone());
                    frame_support::ensure!(
                        pool.pool_account == new,
                        "a pool still points at its pre-D8 account"
                    );
                    let old = old_pool_account_for::<T>(&pair);
                    for a in [&pair.0, &pair.1] {
                        frame_support::ensure!(
                            T::Assets::reducible_balance(a.clone(), &old, Expendable, Polite)
                                .is_zero(),
                            "a pre-D8 account still holds reserves"
                        );
                        // The new account holds at least the recorded reserve
                        // (it also holds uncounted fees, D6).
                        let held = T::Assets::balance(a.clone(), &new);
                        let recorded = if *a == pair.0 { pool.reserve_a } else { pool.reserve_b };
                        frame_support::ensure!(
                            held >= recorded,
                            "a hash-derived account holds less than its pool's recorded reserve"
                        );
                    }
                    n += 1;
                }
                log::info!(target: "runtime::vitreus-dex", "D8 post_upgrade: {n} pools at hash-derived accounts holding their reserves; pre-D8 accounts empty");
                Ok(())
            }
        }

        /// D8 migration, gated on the pallet's on-chain storage version.
        pub type MigrateToV2<T> = VersionedMigration<
            1,
            2,
            VersionUncheckedMigrateToV2<T>,
            Pallet<T>,
            <T as frame_system::Config>::DbWeight,
        >;
    }

    /// D9 (v2 → v3): `FeeRouting` gains `treasury_bps`, which sits inside
    /// every stored `PoolInfo` and in `DefaultFeeRouting`, so every record
    /// is re-encoded. Existing pools and the default get `treasury_bps = 0`:
    /// a live pool's split is its snapshot (the D4 rule), and the default is
    /// governance's to set with `set_default_fee_routing`. `LastSwapBlock` is
    /// a new map and needs nothing. Fork-only: the submission's D9 is its v2
    /// and no chain it targets has a pre-D9 pool.
    #[allow(missing_docs)]
    pub mod v3 {
        use super::*;

        /// `FeeRouting` as stored before D9.
        #[derive(Encode, Decode, Default)]
        #[allow(missing_docs)]
        pub struct OldFeeRouting {
            pub protocol_bps: u16,
            pub creator_bps: u16,
        }

        /// `PoolInfo` as stored before D9.
        #[derive(Encode, Decode)]
        #[allow(missing_docs)]
        pub struct OldPoolInfo<Balance, AccountId> {
            pub reserve_a: Balance,
            pub reserve_b: Balance,
            pub fee_tier: u32,
            pub total_fees_collected: Balance,
            pub pool_account: AccountId,
            pub routing: OldFeeRouting,
        }

        /// `DefaultFeeRouting` as it was before D10 made it per tier: one
        /// value under the pallet's own prefix. v3 predates the map, so it
        /// addresses the key it actually migrated.
        #[frame_support::storage_alias]
        pub type DefaultFeeRouting<T: Config> = StorageValue<Pallet<T>, FeeRouting, OptionQuery>;

        fn widen(old: OldFeeRouting) -> FeeRouting {
            FeeRouting {
                protocol_bps: old.protocol_bps,
                creator_bps: old.creator_bps,
                treasury_bps: 0,
            }
        }

        /// Unversioned body; wrap in [`MigrateToV3`].
        pub struct VersionUncheckedMigrateToV3<T>(PhantomData<T>);

        impl<T: Config> UncheckedOnRuntimeUpgrade for VersionUncheckedMigrateToV3<T> {
            fn on_runtime_upgrade() -> Weight {
                let mut count = 0u64;
                Pools::<T>::translate::<OldPoolInfo<T::Balance, T::AccountId>, _>(|_pair, old| {
                    count = count.saturating_add(1);
                    Some(PoolInfo {
                        reserve_a: old.reserve_a,
                        reserve_b: old.reserve_b,
                        fee_tier: old.fee_tier,
                        total_fees_collected: old.total_fees_collected,
                        pool_account: old.pool_account,
                        routing: widen(old.routing),
                    })
                });
                // Absent stays absent: a pool with no default routes nothing.
                let default_set = DefaultFeeRouting::<T>::exists();
                let _ = DefaultFeeRouting::<T>::translate::<OldFeeRouting, _>(|old| old.map(widen));
                log::info!(
                    target: "runtime::vitreus-dex",
                    "D9 migration: {count} pools re-encoded with treasury_bps = 0; default routing {}",
                    if default_set { "re-encoded with treasury_bps = 0" } else { "not set (zero)" }
                );
                T::DbWeight::get().reads_writes(count.saturating_add(1), count.saturating_add(1))
            }

            #[cfg(feature = "try-runtime")]
            fn pre_upgrade() -> Result<sp_std::vec::Vec<u8>, sp_runtime::TryRuntimeError> {
                // Values do not decode as the new type yet; keys do.
                let pools = Pools::<T>::iter_keys().count() as u32;
                let default_set = DefaultFeeRouting::<T>::exists();
                log::info!(target: "runtime::vitreus-dex", "D9 pre_upgrade: {pools} pools to re-encode; default routing set: {default_set}");
                Ok((pools, default_set).encode())
            }

            #[cfg(feature = "try-runtime")]
            fn post_upgrade(
                state: sp_std::vec::Vec<u8>,
            ) -> Result<(), sp_runtime::TryRuntimeError> {
                let (pools, _): (u32, bool) =
                    Decode::decode(&mut &state[..]).map_err(|_| "pre_upgrade state")?;
                let mut n = 0u32;
                for (_pair, pool) in Pools::<T>::iter() {
                    n = n.saturating_add(1);
                    frame_support::ensure!(
                        pool.routing.treasury_bps == 0,
                        "every pre-D9 pool carries treasury_bps = 0"
                    );
                    frame_support::ensure!(
                        pool.routing.fits_tier(pool.fee_tier),
                        "routing still fits the tier"
                    );
                }
                frame_support::ensure!(n == pools, "every pool decodes after D9");
                frame_support::ensure!(
                    DefaultFeeRouting::<T>::get().map_or(true, |d| d.treasury_bps == 0),
                    "default routing carries treasury_bps = 0 until governance sets it"
                );
                log::info!(
                    target: "runtime::vitreus-dex",
                    "D9 post_upgrade: {n} pools decode with treasury_bps = 0; default routing {:?}",
                    DefaultFeeRouting::<T>::get()
                );
                Ok(())
            }
        }

        /// D9 migration, gated on the pallet's on-chain storage version.
        pub type MigrateToV3<T> = VersionedMigration<
            2,
            3,
            VersionUncheckedMigrateToV3<T>,
            Pallet<T>,
            <T as frame_system::Config>::DbWeight,
        >;
    }

    /// D10 (v3 → v4): `DefaultFeeRouting` becomes one entry per fee tier, so
    /// a 1 % pool can route what a 0.3 % pool cannot.
    ///
    /// The single pre-D10 value was validated against tier 3, so that is the
    /// tier it becomes — and only if it still fits under D10's floor
    /// (`routed ≤ 20` at tier 3). A value that does not fit is dropped with a
    /// warning rather than clamped: a split nobody chose is worse than none,
    /// and none is visible — a launch at that tier stops at
    /// [`Error::NoDefaultFeeRouting`] until governance sets it.
    ///
    /// Tiers 1 and 10 are left unset deliberately. Existing pools are not
    /// touched at all: routing is snapshotted at creation and immutable, so
    /// every pool keeps the split it was created with.
    #[allow(missing_docs)]
    pub mod v4 {
        use super::*;

        /// The pre-D10 single value; the same key v3 wrote.
        #[allow(missing_docs)]
        #[frame_support::storage_alias]
        pub type DefaultFeeRouting<T: Config> = StorageValue<Pallet<T>, FeeRouting, OptionQuery>;

        /// Unversioned body; wrap in [`MigrateToV4`].
        pub struct VersionUncheckedMigrateToV4<T>(PhantomData<T>);

        impl<T: Config> UncheckedOnRuntimeUpgrade for VersionUncheckedMigrateToV4<T> {
            fn on_runtime_upgrade() -> Weight {
                let old = DefaultFeeRouting::<T>::take();
                match old {
                    Some(routing) if routing.is_valid_for(MIN_LAUNCH_FEE_TIER) => {
                        super::super::DefaultFeeRouting::<T>::insert(MIN_LAUNCH_FEE_TIER, routing);
                        log::info!(
                            target: "runtime::vitreus-dex",
                            "D10 migration: default routing {routing:?} moved to tier {MIN_LAUNCH_FEE_TIER}; tiers 1 and 10 unset"
                        );
                    },
                    Some(routing) => {
                        log::warn!(
                            target: "runtime::vitreus-dex",
                            "D10 migration: default routing {routing:?} routes more than tier {MIN_LAUNCH_FEE_TIER} may under the pool floor; dropped, governance must set it"
                        );
                    },
                    None => {
                        log::info!(
                            target: "runtime::vitreus-dex",
                            "D10 migration: no default routing was set; every tier starts unset"
                        );
                    },
                }
                T::DbWeight::get().reads_writes(1, 2)
            }

            #[cfg(feature = "try-runtime")]
            fn pre_upgrade() -> Result<sp_std::vec::Vec<u8>, sp_runtime::TryRuntimeError> {
                let old = DefaultFeeRouting::<T>::get();
                let pools = Pools::<T>::iter().count() as u32;
                log::info!(
                    target: "runtime::vitreus-dex",
                    "D10 pre_upgrade: default routing {old:?}; {pools} pools, none of which is touched"
                );
                Ok((old, pools).encode())
            }

            #[cfg(feature = "try-runtime")]
            fn post_upgrade(
                state: sp_std::vec::Vec<u8>,
            ) -> Result<(), sp_runtime::TryRuntimeError> {
                let (old, pools): (Option<FeeRouting>, u32) =
                    Decode::decode(&mut &state[..]).map_err(|_| "pre_upgrade state")?;
                frame_support::ensure!(
                    !DefaultFeeRouting::<T>::exists(),
                    "the pre-D10 single value is gone"
                );
                let at_three = super::super::DefaultFeeRouting::<T>::get(MIN_LAUNCH_FEE_TIER);
                match old {
                    Some(r) if r.is_valid_for(MIN_LAUNCH_FEE_TIER) => frame_support::ensure!(
                        at_three == Some(r),
                        "a default that fits tier 3 is now tier 3's default"
                    ),
                    _ => frame_support::ensure!(
                        at_three.is_none(),
                        "no default that did not fit was invented"
                    ),
                }
                frame_support::ensure!(
                    super::super::DefaultFeeRouting::<T>::get(MIN_FEE_TIER).is_none()
                        && super::super::DefaultFeeRouting::<T>::get(10).is_none(),
                    "tiers 1 and 10 are left for governance"
                );
                let mut n = 0u32;
                for (_pair, pool) in Pools::<T>::iter() {
                    n = n.saturating_add(1);
                    frame_support::ensure!(
                        pool.routing.fits_tier(pool.fee_tier),
                        "every pool still routes no more than its tier"
                    );
                }
                frame_support::ensure!(n == pools, "no pool was added or lost");
                Ok(())
            }
        }

        /// D10 migration, gated on the pallet's on-chain storage version.
        pub type MigrateToV4<T> = VersionedMigration<
            3,
            4,
            VersionUncheckedMigrateToV4<T>,
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

    /// v6: the fee escrow and every pool with liquidity hold the native ED.
    pub mod v6 {
        use super::*;

        /// Unversioned body; wrap in [`MigrateToV6`].
        ///
        /// Genesis endows the fee escrow and a pool's first deposit endows
        /// the pool; a chain that received the pallet by upgrade has had
        /// neither for what it already holds. Each ED is paid by `Source`
        /// (the runtime binds its Treasury), keeping its own ED, so issuance
        /// does not change. A source that cannot pay is logged, and
        /// `post_upgrade` then fails try-runtime: fund the source first.
        ///
        /// - The fee escrow is topped up to its ED plus every unclaimed fee,
        ///   so withdrawing them all leaves it its ED.
        /// - Every pool with liquidity gets an ED of native on top of what it
        ///   holds, so its native reserve is unchanged now that reserves are
        ///   read net of what it keeps. What it keeps of a token has no
        ///   source to come from: it comes out of the pool's token reserve,
        ///   one unit less than the token's minimum balance at most. A pool with no liquidity is
        ///   endowed by its first deposit, as any new pool is.
        pub struct VersionUncheckedMigrateToV6<T, Source>(PhantomData<(T, Source)>);

        impl<T: Config, Source: Get<T::AccountId>> UncheckedOnRuntimeUpgrade
            for VersionUncheckedMigrateToV6<T, Source>
        {
            fn on_runtime_upgrade() -> Weight {
                let native = T::NativeAsset::get();
                let ed = Pallet::<T>::native_ed();
                let source = Source::get();
                let pay =
                    |to: &T::AccountId, amount: T::Balance, what: &str| match T::Assets::transfer(
                        native.clone(),
                        &source,
                        to,
                        amount,
                        Preserve,
                    ) {
                        Ok(_) => true,
                        Err(e) => {
                            log::error!(
                                target: "runtime::vitreus-dex",
                                "v6 migration: the source could not pay {amount:?} to {what}: {e:?}"
                            );
                            false
                        },
                    };
                let escrow = Pallet::<T>::fee_escrow_account();
                let owed = CreatorFeesUnclaimed::<T>::iter_values()
                    .fold(ProtocolFeesUnclaimed::<T>::get(), |a, f| a.saturating_add(f));
                let short = owed
                    .saturating_add(ed)
                    .saturating_sub(T::Assets::balance(native.clone(), &escrow));
                let mut reads =
                    3u64.saturating_add(CreatorFeesUnclaimed::<T>::iter_keys().count() as u64);
                let mut writes = 0u64;
                if !short.is_zero() && pay(&escrow, short, "the fee escrow") {
                    writes = writes.saturating_add(2);
                }
                let mut pools = 0u32;
                for (pair, pool) in Pools::<T>::iter() {
                    reads = reads.saturating_add(2);
                    if TotalLiquidity::<T>::get(&pair).unwrap_or_else(Zero::zero).is_zero() {
                        continue;
                    }
                    if pay(&pool.pool_account, ed, "a pool account") {
                        pools = pools.saturating_add(1);
                        writes = writes.saturating_add(2);
                    }
                }
                log::info!(
                    target: "runtime::vitreus-dex",
                    "v6 migration: fee escrow topped up by {short:?}; {pools} pool account(s) endowed"
                );
                T::DbWeight::get().reads_writes(reads, writes)
            }

            #[cfg(feature = "try-runtime")]
            fn pre_upgrade() -> Result<sp_std::vec::Vec<u8>, sp_runtime::TryRuntimeError> {
                let reserves: sp_std::vec::Vec<_> = Pools::<T>::iter()
                    .filter(|(pair, _)| {
                        !TotalLiquidity::<T>::get(pair).unwrap_or_else(Zero::zero).is_zero()
                    })
                    .map(|(pair, p)| {
                        (
                            pair.clone(),
                            T::Assets::balance(pair.0.clone(), &p.pool_account),
                            T::Assets::balance(pair.1, &p.pool_account),
                        )
                    })
                    .collect();
                Ok(reserves.encode())
            }

            #[cfg(feature = "try-runtime")]
            fn post_upgrade(
                state: sp_std::vec::Vec<u8>,
            ) -> Result<(), sp_runtime::TryRuntimeError> {
                let before: sp_std::vec::Vec<(
                    (T::AssetKind, T::AssetKind),
                    T::Balance,
                    T::Balance,
                )> = Decode::decode(&mut &state[..]).map_err(|_| "pre_upgrade state")?;
                let native = T::NativeAsset::get();
                for (pair, a, b) in before {
                    let pool = Pools::<T>::get(&pair).ok_or("a pool was lost")?;
                    for (asset, was) in [(pair.0.clone(), a), (pair.1.clone(), b)] {
                        let now = Pallet::<T>::reserve_balance(asset.clone(), &pool.pool_account);
                        let lost = if asset.encode() == native.encode() {
                            Zero::zero()
                        } else {
                            Pallet::<T>::pool_buffer(&asset)
                        };
                        frame_support::ensure!(
                            now <= was && was.saturating_sub(now) <= lost,
                            "a pool's reserves moved by more than a token's minimum, or it was not endowed"
                        );
                    }
                }
                Pallet::<T>::do_try_state()
            }
        }

        /// Endowment of the fee escrow and the pool accounts, gated on the
        /// pallet's on-chain storage version.
        pub type MigrateToV6<T, Source> = VersionedMigration<
            5,
            6,
            VersionUncheckedMigrateToV6<T, Source>,
            Pallet<T>,
            <T as frame_system::Config>::DbWeight,
        >;
    }
}
