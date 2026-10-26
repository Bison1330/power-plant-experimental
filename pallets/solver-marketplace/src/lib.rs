//! # Solver marketplace pallet
//!
//! Users post intents (sell exactly this much of one asset for at least
//! that much of another, by this block); bonded solvers bid what they will
//! deliver; the winning solver settles by swapping the intent's input
//! through the Vitreus DEX and keeps what the trade yields above its bid,
//! less a protocol share. A solver that does not settle in time is slashed
//! and the user refunded.
//!
//! The DEX is reached only through [`pallet_vitreus_dex::IntentSwap`], the
//! one swap settlement needs; this pallet cannot create pools or move
//! liquidity.
//!
//! # Runtime wiring
//!
//! 1. **`DefaultSolverBondAmount`** — do NOT use the mock value of
//!    `1_000_000_000_000` in production. VTRS has 18 decimals; use
//!    `1_000 * UNITS` (= 10^21) or the post-governance agreed value.
//!
//! 2. **Same-block bid races** — Vitreus runs BABE with rotational block
//!    authorship, and `pallet_energy_fee::pay_priority_fee` is a no-op so
//!    tips can't influence inclusion. Same-block overbidding is
//!    non-deterministic in ordering but economically sound: the current
//!    BABE author's mempool view decides which valid bid lands first;
//!    losers retry next block. No commit-reveal needed.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]

#[cfg(test)]
mod account_lifecycle_tests;
#[cfg(test)]
mod fuzz;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;
pub mod migrations;
pub mod settlement;
pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
pub use benchmarking::BenchmarkHelper;
pub use weights::WeightInfo;

pub use pallet::*;

use frame_support::{
    traits::{
        fungibles::{Inspect, Mutate},
        tokens::{
            Balance,
            Preservation::{Expendable, Preserve},
        },
        Get,
    },
    PalletId,
};
use frame_system::pallet_prelude::BlockNumberFor;
use pallet_vitreus_dex::IntentSwap;
use sp_runtime::{
    traits::{AccountIdConversion, Saturating, Zero},
    SaturatedConversion,
};

/// The PalletId the marketplace's escrows derive from.
pub const PALLET_ID: PalletId = PalletId(*b"vtrs/slv");

#[frame_support::pallet]
pub mod pallet {
    use super::*;
    use frame_support::pallet_prelude::*;
    use frame_system::pallet_prelude::*;

    /// v1: the escrows hold an ED (`migrations::v1`); a chain built from
    /// genesis starts there.
    const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

    #[pallet::pallet]
    #[pallet::storage_version(STORAGE_VERSION)]
    pub struct Pallet<T>(_);

    #[pallet::config]
    pub trait Config: frame_system::Config {
        /// The origin which can manage parameters of this pallet.
        type ManageOrigin: EnsureOrigin<Self::RuntimeOrigin>;

        /// The type in which assets are measured.
        type Balance: Balance;

        /// Type of asset class intents trade.
        type AssetKind: Parameter + MaxEncodedLen;

        /// Registry of the assets intents trade and bonds are posted in.
        type Assets: Inspect<Self::AccountId, AssetId = Self::AssetKind, Balance = Self::Balance>
            + Mutate<Self::AccountId>;

        /// Identifier of the native asset, which bonds are posted in.
        #[pallet::constant]
        type NativeAsset: Get<Self::AssetKind>;

        /// The DEX settlement swaps through.
        type Dex: IntentSwap<Self::AccountId, Self::AssetKind, Self::Balance>;

        /// Initial default for the bid window in blocks. Can be updated at
        /// runtime via `set_bid_window` (gated on `ManageOrigin`).
        #[pallet::constant]
        type DefaultBidWindowBlocks: Get<BlockNumberFor<Self>>;

        /// Initial default for the settlement window in blocks.
        #[pallet::constant]
        type DefaultSettlementWindowBlocks: Get<BlockNumberFor<Self>>;

        /// Initial default for the solver bond amount.
        ///
        /// Denominated in the native-asset sub-unit (VTRS uses 18 decimals
        /// in the production runtime, so 1 VTRS = 10^18 sub-units). The
        /// production runtime should bind this to something on the order of
        /// `1_000 * UNITS` (≈ 10^21 sub-units); the test mock binds it to
        /// `1_000_000_000_000` which is fine for tests but equals only
        /// 10^-6 VTRS in production denominations.
        #[pallet::constant]
        type DefaultSolverBondAmount: Get<Self::Balance>;

        /// Weight information for the extrinsics of this pallet.
        type WeightInfo: WeightInfo;

        /// Sets up what the benchmarks need from the DEX.
        #[cfg(feature = "runtime-benchmarks")]
        type BenchmarkHelper: BenchmarkHelper<Self::AssetKind, Self::AccountId, Self::Balance>;
    }

    // ---- Settlement: governance-adjustable parameters ----

    /// Number of blocks during which solvers may bid on an open intent.
    /// `None` means fall back to `T::DefaultBidWindowBlocks`.
    #[pallet::storage]
    pub type BidWindowBlocks<T: Config> = StorageValue<_, BlockNumberFor<T>, OptionQuery>;

    /// Number of blocks a committed solver has to settle before becoming
    /// slashable. `None` means fall back to `T::DefaultSettlementWindowBlocks`.
    #[pallet::storage]
    pub type SettlementWindowBlocks<T: Config> = StorageValue<_, BlockNumberFor<T>, OptionQuery>;

    /// Amount of VTRS a solver must bond to register.
    /// `None` means fall back to `T::DefaultSolverBondAmount`.
    #[pallet::storage]
    pub type SolverBondAmount<T: Config> = StorageValue<_, T::Balance, OptionQuery>;

    // ---- Settlement: id counters ----

    /// Monotonic id for the next submitted intent.
    #[pallet::storage]
    pub type NextIntentId<T: Config> = StorageValue<_, u64, ValueQuery>;

    /// Monotonic id for the next registered solver.
    #[pallet::storage]
    pub type NextSolverId<T: Config> = StorageValue<_, u64, ValueQuery>;

    // ---- Settlement: data maps ----

    /// All intents by id.
    #[pallet::storage]
    pub type Intents<T: Config> = StorageMap<
        _,
        Blake2_128Concat,
        u64,
        crate::settlement::Intent<T::AccountId, T::AssetKind, T::Balance, BlockNumberFor<T>>,
        OptionQuery,
    >;

    /// Index from solver account to solver id, for uniqueness enforcement
    /// and fast lookup at register time.
    #[pallet::storage]
    pub type SolverAccountToId<T: Config> =
        StorageMap<_, Blake2_128Concat, T::AccountId, u64, OptionQuery>;

    /// All solvers by id.
    #[pallet::storage]
    pub type Solvers<T: Config> = StorageMap<
        _,
        Blake2_128Concat,
        u64,
        crate::settlement::SolverInfo<T::AccountId, T::Balance, BlockNumberFor<T>>,
        OptionQuery,
    >;

    /// Active commitments keyed by intent id. Removed on settle/cancel/slash.
    #[pallet::storage]
    pub type FillCommitments<T: Config> = StorageMap<
        _,
        Blake2_128Concat,
        u64,
        crate::settlement::FillCommitment<T::AccountId, T::Balance, BlockNumberFor<T>>,
        OptionQuery,
    >;

    /// Amount held in the shared intent escrow per-intent. Tracks how much
    /// `token_in` the escrow account owes back to each intent owner.
    /// Insert on `submit_intent`, remove on settle / cancel / refund.
    #[pallet::storage]
    pub type IntentEscrowBalances<T: Config> =
        StorageMap<_, Blake2_128Concat, u64, (T::AssetKind, T::Balance), OptionQuery>;

    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        /// A new solver registered and posted a bond.
        SolverRegistered {
            /// Id assigned to the new solver.
            solver_id: u64,
            /// Solver's on-chain account.
            account: T::AccountId,
            /// Amount of bond posted.
            bond: T::Balance,
        },

        /// A solver voluntarily deregistered; bond refunded.
        SolverDeregistered {
            /// Id of the solver.
            solver_id: u64,
            /// Solver's on-chain account.
            account: T::AccountId,
            /// Amount refunded from escrow.
            bond_refunded: T::Balance,
        },

        /// A new intent was submitted.
        IntentSubmitted {
            /// Id assigned to the new intent.
            intent_id: u64,
            /// Submitting user.
            user: T::AccountId,
            /// Input asset.
            token_in: T::AssetKind,
            /// Desired output asset.
            token_out: T::AssetKind,
            /// Amount of input provided.
            amount_in: T::Balance,
            /// Minimum acceptable output.
            min_amount_out: T::Balance,
            /// Block by which the intent must settle or be refundable.
            deadline: BlockNumberFor<T>,
        },

        /// A user cancelled an intent before any commitment.
        IntentCancelled {
            /// Id of the cancelled intent.
            intent_id: u64,
            /// User who cancelled.
            user: T::AccountId,
        },

        /// A solver committed to fill an intent.
        FillCommitted {
            /// Intent being filled.
            intent_id: u64,
            /// Solver making the commitment.
            solver_id: u64,
            /// Amount the solver promises to deliver to the user.
            committed_amount_out: T::Balance,
            /// Deadline for settlement.
            settle_by: BlockNumberFor<T>,
        },

        /// An intent was successfully settled.
        IntentSettled {
            /// Intent that was settled.
            intent_id: u64,
            /// Solver that settled it.
            solver_id: u64,
            /// User who submitted the intent.
            user: T::AccountId,
            /// Amount delivered to the user.
            amount_out_to_user: T::Balance,
            /// Solver's net profit after protocol fee.
            solver_net_profit: T::Balance,
            /// Protocol fee taken from solver profit.
            protocol_fee: T::Balance,
        },

        /// A solver was slashed for failing to settle within the window.
        SolverSlashed {
            /// Solver that was slashed.
            solver_id: u64,
            /// Intent whose non-settlement triggered the slash.
            intent_id: u64,
            /// Total amount slashed from the solver's bond.
            slashed_amount: T::Balance,
            /// Portion sent to the protocol treasury.
            to_treasury: T::Balance,
            /// Portion awarded to the slasher.
            to_slasher: T::Balance,
            /// Account that triggered the slash.
            slasher: T::AccountId,
        },

        /// An expired intent was refunded to its owner.
        IntentRefunded {
            /// Intent that was refunded.
            intent_id: u64,
            /// User who was refunded.
            user: T::AccountId,
            /// Amount of `token_in` returned to the user.
            amount_refunded: T::Balance,
        },

        /// Governance updated the bid window.
        BidWindowUpdated {
            /// New bid window value (in blocks).
            new_value: BlockNumberFor<T>,
        },

        /// Governance updated the settlement window.
        SettlementWindowUpdated {
            /// New settlement window value (in blocks).
            new_value: BlockNumberFor<T>,
        },

        /// Governance updated the solver bond amount.
        SolverBondAmountUpdated {
            /// New solver bond amount.
            new_value: T::Balance,
        },
    }

    #[pallet::error]
    pub enum Error<T> {
        /// Caller is not a registered solver.
        SolverNotRegistered,
        /// Account has already registered as a solver.
        SolverAlreadyRegistered,
        /// Solver exists but is not currently active.
        SolverNotActive,
        /// Caller does not hold enough VTRS to post the bond.
        InsufficientBondFunds,
        /// No intent with the given id.
        IntentNotFound,
        /// Operation requires the intent to be in `Open` status.
        IntentNotOpen,
        /// Operation requires the intent to be in `Committed` status.
        IntentNotCommitted,
        /// Intent's deadline has passed.
        IntentExpired,
        /// Caller is not the intent's original submitter.
        NotIntentOwner,
        /// Bid window has closed for this intent.
        BidWindowClosed,
        /// Committed amount out is below the intent's minimum.
        BelowMinAmountOut,
        /// Incoming bid is not strictly better than the existing one.
        BidNotBetter,
        /// Caller is not the solver that committed to this intent.
        NotCommittedSolver,
        /// Settlement deadline has already passed.
        SettlementWindowPassed,
        /// Settlement deadline has not yet passed (slashing requires it to).
        SettlementWindowNotPassed,
        /// Solver still has active commitments; cannot deregister yet.
        ActiveCommitmentsExist,
        /// Deadline is in the past or not far enough in the future.
        InvalidDeadline,
        /// Deadline has not yet passed; refund is not yet available.
        DeadlineNotPassed,
        /// Amount parameter is zero or otherwise invalid.
        InvalidAmount,
    }

    #[pallet::hooks]
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        #[cfg(feature = "try-runtime")]
        fn try_state(_n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
            Self::do_try_state()
        }
    }

    /// Endow the intent escrow and the fee treasury with the native ED, so
    /// both exist before the first intent or fee whatever they receive. On a
    /// chain that receives this pallet by upgrade, `migrations::v1` does
    /// the same.
    #[pallet::genesis_config]
    #[derive(frame_support::DefaultNoBound)]
    pub struct GenesisConfig<T: Config> {
        /// No configurable fields; the build endows the escrows with the ED.
        #[serde(skip)]
        pub _marker: core::marker::PhantomData<T>,
    }

    #[pallet::genesis_build]
    impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
        fn build(&self) {
            Pallet::<T>::endow_escrows(None);
        }
    }

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        // ---- Solver lifecycle ----

        /// Register as a solver. Transfers the required bond from the caller
        /// to a per-solver escrow sub-account derived from the assigned id.
        #[pallet::call_index(0)]
        #[pallet::weight(<T as Config>::WeightInfo::register_solver())]
        pub fn register_solver(origin: OriginFor<T>) -> DispatchResult {
            let who = ensure_signed(origin)?;

            // Reject duplicate active registration. If a prior registration
            // exists but is inactive (deregistered / slashed), allow a fresh
            // registration with a new solver_id.
            if let Some(existing_id) = SolverAccountToId::<T>::get(&who) {
                if let Some(solver) = Solvers::<T>::get(existing_id) {
                    if solver.active {
                        return Err(Error::<T>::SolverAlreadyRegistered.into());
                    }
                }
            }

            let bond_amount = Self::current_solver_bond();
            let native_asset = T::NativeAsset::get();

            let solver_id = NextSolverId::<T>::get();
            let escrow = Self::solver_escrow_account(solver_id);

            T::Assets::transfer(native_asset, &who, &escrow, bond_amount, Preserve)
                .map_err(|_| Error::<T>::InsufficientBondFunds)?;

            let now = frame_system::Pallet::<T>::block_number();
            let info = crate::settlement::SolverInfo {
                id: solver_id,
                account: who.clone(),
                bond: bond_amount,
                reputation: 0,
                fills_completed: 0,
                fills_slashed: 0,
                active_commitments: 0,
                registered_at: now,
                active: true,
            };

            Solvers::<T>::insert(solver_id, info);
            SolverAccountToId::<T>::insert(&who, solver_id);
            NextSolverId::<T>::put(solver_id.saturating_add(1));

            Self::deposit_event(Event::SolverRegistered {
                solver_id,
                account: who,
                bond: bond_amount,
            });

            Ok(())
        }

        /// Voluntarily deregister as a solver. Refunds the full bond.
        /// Fails if the solver has any active (committed but unsettled) fills.
        #[pallet::call_index(1)]
        #[pallet::weight(<T as Config>::WeightInfo::deregister_solver())]
        pub fn deregister_solver(origin: OriginFor<T>) -> DispatchResult {
            let who = ensure_signed(origin)?;

            let solver_id =
                SolverAccountToId::<T>::get(&who).ok_or(Error::<T>::SolverNotRegistered)?;
            let mut solver = Solvers::<T>::get(solver_id).ok_or(Error::<T>::SolverNotRegistered)?;

            ensure!(solver.active, Error::<T>::SolverNotActive);
            ensure!(solver.active_commitments == 0, Error::<T>::ActiveCommitmentsExist,);

            let escrow = Self::solver_escrow_account(solver_id);
            let native_asset = T::NativeAsset::get();
            let bond = solver.bond;

            T::Assets::transfer(native_asset, &escrow, &who, bond, Expendable)
                .map_err(|_| Error::<T>::InsufficientBondFunds)?;

            solver.active = false;
            solver.bond = Zero::zero();
            Solvers::<T>::insert(solver_id, solver);

            Self::deposit_event(Event::SolverDeregistered {
                solver_id,
                account: who,
                bond_refunded: bond,
            });

            Ok(())
        }

        // ---- intent lifecycle ----

        /// Submit a trading intent. Escrows `amount_in` of `token_in` to the
        /// shared intent escrow account. Solvers may bid during the next
        /// `current_bid_window()` blocks.
        #[pallet::call_index(2)]
        #[pallet::weight(<T as Config>::WeightInfo::submit_intent())]
        pub fn submit_intent(
            origin: OriginFor<T>,
            token_in: T::AssetKind,
            token_out: T::AssetKind,
            amount_in: T::Balance,
            min_amount_out: T::Balance,
            deadline: BlockNumberFor<T>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            ensure!(!amount_in.is_zero(), Error::<T>::InvalidAmount);
            ensure!(token_in != token_out, Error::<T>::InvalidAmount);

            let now = frame_system::Pallet::<T>::block_number();
            ensure!(deadline > now, Error::<T>::InvalidDeadline);

            let escrow = Self::intent_escrow_account();
            T::Assets::transfer(token_in.clone(), &who, &escrow, amount_in, Preserve)
                .map_err(|_| Error::<T>::InvalidAmount)?;

            let intent_id = NextIntentId::<T>::get();

            let intent = crate::settlement::Intent {
                id: intent_id,
                user: who.clone(),
                token_in: token_in.clone(),
                token_out: token_out.clone(),
                amount_in,
                min_amount_out,
                deadline,
                submitted_at: now,
                status: crate::settlement::IntentStatus::Open,
            };

            Intents::<T>::insert(intent_id, intent);
            IntentEscrowBalances::<T>::insert(intent_id, (token_in.clone(), amount_in));
            NextIntentId::<T>::put(intent_id.saturating_add(1));

            Self::deposit_event(Event::IntentSubmitted {
                intent_id,
                user: who,
                token_in,
                token_out,
                amount_in,
                min_amount_out,
                deadline,
            });

            Ok(())
        }

        /// Cancel an open intent. Only callable by the intent owner. Refunds
        /// `amount_in` of `token_in`. Only succeeds while the intent is
        /// still `Open` (no solver has committed).
        #[pallet::call_index(3)]
        #[pallet::weight(<T as Config>::WeightInfo::cancel_intent())]
        pub fn cancel_intent(origin: OriginFor<T>, intent_id: u64) -> DispatchResult {
            let who = ensure_signed(origin)?;

            let mut intent = Intents::<T>::get(intent_id).ok_or(Error::<T>::IntentNotFound)?;

            ensure!(intent.user == who, Error::<T>::NotIntentOwner);
            ensure!(
                intent.status == crate::settlement::IntentStatus::Open,
                Error::<T>::IntentNotOpen,
            );

            // Use the recorded escrow balance (defense in depth — future
            // changes could adjust escrow mid-life).
            let (escrow_asset, escrow_amount) =
                IntentEscrowBalances::<T>::get(intent_id).ok_or(Error::<T>::IntentNotFound)?;

            let escrow = Self::intent_escrow_account();
            T::Assets::transfer(escrow_asset, &escrow, &who, escrow_amount, Expendable)
                .map_err(|_| Error::<T>::InvalidAmount)?;

            intent.status = crate::settlement::IntentStatus::Cancelled;
            Intents::<T>::insert(intent_id, intent);
            IntentEscrowBalances::<T>::remove(intent_id);

            Self::deposit_event(Event::IntentCancelled { intent_id, user: who });

            Ok(())
        }

        // ---- fill lifecycle ----

        /// Commit to fill an open intent at `committed_amount_out`. Must be
        /// `>=` the intent's `min_amount_out`; if a prior commitment exists,
        /// the new bid must strictly exceed it (strict-replace overbidding).
        ///
        /// Must be called within the bid window
        /// (`intent.submitted_at + current_bid_window()`). Sets intent
        /// status to `Committed` and increments the solver's
        /// `active_commitments` counter. The solver's bond stays in the
        /// per-solver escrow regardless of commitment state; slashing on
        /// failed settlement draws against it.
        #[pallet::call_index(4)]
        #[pallet::weight(<T as Config>::WeightInfo::commit_fill())]
        pub fn commit_fill(
            origin: OriginFor<T>,
            intent_id: u64,
            committed_amount_out: T::Balance,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            let solver_id =
                SolverAccountToId::<T>::get(&who).ok_or(Error::<T>::SolverNotRegistered)?;
            let mut solver = Solvers::<T>::get(solver_id).ok_or(Error::<T>::SolverNotRegistered)?;
            ensure!(solver.active, Error::<T>::SolverNotActive);

            let mut intent = Intents::<T>::get(intent_id).ok_or(Error::<T>::IntentNotFound)?;
            ensure!(
                intent.status == crate::settlement::IntentStatus::Open
                    || intent.status == crate::settlement::IntentStatus::Committed,
                Error::<T>::IntentNotOpen,
            );

            ensure!(committed_amount_out >= intent.min_amount_out, Error::<T>::BelowMinAmountOut,);

            let now = frame_system::Pallet::<T>::block_number();
            let bid_deadline = intent.submitted_at.saturating_add(Self::current_bid_window());
            ensure!(now <= bid_deadline, Error::<T>::BidWindowClosed);

            ensure!(now < intent.deadline, Error::<T>::IntentExpired);

            // Strict-replace overbidding. Decrement the displaced solver's
            // active_commitments; missing lookup is treated as no-op since
            // the new commit is still valid. A solver raising its own bid
            // displaces itself: the count must not move, and the record
            // written below must be the one the decrement touched — R6 was
            // this very record, loaded above, written back stale plus one,
            // which locked the bond behind a count that never reached zero.
            let mut self_replaced = false;
            if let Some(prior) = FillCommitments::<T>::get(intent_id) {
                ensure!(
                    committed_amount_out > prior.committed_amount_out,
                    Error::<T>::BidNotBetter,
                );
                if prior.solver_id == solver_id {
                    self_replaced = true;
                } else if let Some(mut displaced) = Solvers::<T>::get(prior.solver_id) {
                    displaced.active_commitments = displaced.active_commitments.saturating_sub(1);
                    Solvers::<T>::insert(prior.solver_id, displaced);
                }
            }

            let settle_by = now.saturating_add(Self::current_settlement_window());
            let commitment = crate::settlement::FillCommitment {
                intent_id,
                solver_id,
                solver_account: who.clone(),
                committed_amount_out,
                committed_at: now,
                settle_by,
            };
            FillCommitments::<T>::insert(intent_id, commitment);

            if !self_replaced {
                solver.active_commitments = solver.active_commitments.saturating_add(1);
                Solvers::<T>::insert(solver_id, solver);
            }

            intent.status = crate::settlement::IntentStatus::Committed;
            Intents::<T>::insert(intent_id, intent);

            Self::deposit_event(Event::FillCommitted {
                intent_id,
                solver_id,
                committed_amount_out,
                settle_by,
            });

            Ok(())
        }

        /// Settle a committed intent by executing the swap and distributing
        /// slippage capture.
        ///
        /// Only callable by the committed solver, within the settlement
        /// window. Swaps through `T::Dex` with the solver's `committed_amount_out`
        /// as the minimum output; if the AMM can't deliver at least that,
        /// the whole call rolls back and the commitment remains (solver can
        /// retry or be slashed after the window).
        ///
        /// On success: user gets exactly `committed_amount_out`, treasury
        /// gets the protocol fee share of profit, solver pockets the rest.
        #[pallet::call_index(5)]
        #[pallet::weight(<T as Config>::WeightInfo::settle_intent())]
        pub fn settle_intent(origin: OriginFor<T>, intent_id: u64) -> DispatchResult {
            let who = ensure_signed(origin)?;

            let commitment =
                FillCommitments::<T>::get(intent_id).ok_or(Error::<T>::IntentNotCommitted)?;
            ensure!(commitment.solver_account == who, Error::<T>::NotCommittedSolver,);

            let now = frame_system::Pallet::<T>::block_number();
            ensure!(now <= commitment.settle_by, Error::<T>::SettlementWindowPassed,);

            let mut intent = Intents::<T>::get(intent_id).ok_or(Error::<T>::IntentNotFound)?;
            ensure!(
                intent.status == crate::settlement::IntentStatus::Committed,
                Error::<T>::IntentNotCommitted,
            );

            let intent_escrow = Self::intent_escrow_account();
            let treasury = Self::protocol_treasury_account();

            // Execute the swap through the DEX; its output lands back in
            // intent_escrow.
            let actual_out = T::Dex::swap_exact_for_intent(
                &intent_escrow,
                intent.token_in.clone(),
                intent.token_out.clone(),
                intent.amount_in,
                commitment.committed_amount_out,
            )?;

            // Slippage capture: the swap guarantees actual_out >= committed_amount_out.
            let profit = actual_out.saturating_sub(commitment.committed_amount_out);

            let profit_u128: u128 = profit.saturated_into::<u128>();
            let (net_profit_u128, protocol_fee_u128) = crate::settlement::split_solver_profit_u128(
                profit_u128,
                crate::settlement::SOLVER_PROFIT_FEE_BPS,
            );
            let net_profit: T::Balance = net_profit_u128.saturated_into();
            let protocol_fee: T::Balance = protocol_fee_u128.saturated_into();

            // Deliver committed amount to user.
            T::Assets::transfer(
                intent.token_out.clone(),
                &intent_escrow,
                &intent.user,
                commitment.committed_amount_out,
                Expendable,
            )
            .map_err(|_| Error::<T>::InvalidAmount)?;

            if !protocol_fee.is_zero() {
                T::Assets::transfer(
                    intent.token_out.clone(),
                    &intent_escrow,
                    &treasury,
                    protocol_fee,
                    Expendable,
                )
                .map_err(|_| Error::<T>::InvalidAmount)?;
            }

            if !net_profit.is_zero() {
                T::Assets::transfer(
                    intent.token_out.clone(),
                    &intent_escrow,
                    &who,
                    net_profit,
                    Expendable,
                )
                .map_err(|_| Error::<T>::InvalidAmount)?;
            }

            IntentEscrowBalances::<T>::remove(intent_id);
            intent.status = crate::settlement::IntentStatus::Settled;
            Intents::<T>::insert(intent_id, intent.clone());
            FillCommitments::<T>::remove(intent_id);

            if let Some(mut solver) = Solvers::<T>::get(commitment.solver_id) {
                solver.reputation =
                    solver.reputation.saturating_add(crate::settlement::REPUTATION_FILL_REWARD);
                solver.fills_completed = solver.fills_completed.saturating_add(1);
                solver.active_commitments = solver.active_commitments.saturating_sub(1);
                Solvers::<T>::insert(commitment.solver_id, solver);
            }

            Self::deposit_event(Event::IntentSettled {
                intent_id,
                solver_id: commitment.solver_id,
                user: intent.user,
                amount_out_to_user: commitment.committed_amount_out,
                solver_net_profit: net_profit,
                protocol_fee,
            });

            Ok(())
        }

        /// Slash a solver that failed to settle within the settlement window.
        /// Permissionless — anyone can call once `settle_by` has passed. The
        /// caller receives `SLASHER_REWARD_BPS` of the bond; the rest goes
        /// to the protocol treasury. The user's intent is refunded.
        ///
        /// After slashing, the solver is marked inactive and must re-register
        /// with a fresh bond to participate again.
        #[pallet::call_index(6)]
        #[pallet::weight(<T as Config>::WeightInfo::slash_solver())]
        pub fn slash_solver(origin: OriginFor<T>, intent_id: u64) -> DispatchResult {
            let slasher = ensure_signed(origin)?;

            let commitment =
                FillCommitments::<T>::get(intent_id).ok_or(Error::<T>::IntentNotCommitted)?;

            let now = frame_system::Pallet::<T>::block_number();
            ensure!(now > commitment.settle_by, Error::<T>::SettlementWindowNotPassed,);

            let mut intent = Intents::<T>::get(intent_id).ok_or(Error::<T>::IntentNotFound)?;
            ensure!(
                intent.status == crate::settlement::IntentStatus::Committed,
                Error::<T>::IntentNotCommitted,
            );

            let mut solver =
                Solvers::<T>::get(commitment.solver_id).ok_or(Error::<T>::SolverNotRegistered)?;
            let bond = solver.bond;

            let bond_u128: u128 = bond.saturated_into::<u128>();
            let (to_treasury_u128, to_slasher_u128) = crate::settlement::split_slashed_bond_u128(
                bond_u128,
                crate::settlement::SLASHER_REWARD_BPS,
            );
            let to_treasury: T::Balance = to_treasury_u128.saturated_into();
            let to_slasher: T::Balance = to_slasher_u128.saturated_into();

            let solver_escrow = Self::solver_escrow_account(commitment.solver_id);
            let treasury = Self::protocol_treasury_account();
            let native_asset = T::NativeAsset::get();

            if !to_treasury.is_zero() {
                T::Assets::transfer(
                    native_asset.clone(),
                    &solver_escrow,
                    &treasury,
                    to_treasury,
                    Expendable,
                )
                .map_err(|_| Error::<T>::InsufficientBondFunds)?;
            }

            if !to_slasher.is_zero() {
                T::Assets::transfer(
                    native_asset.clone(),
                    &solver_escrow,
                    &slasher,
                    to_slasher,
                    Expendable,
                )
                .map_err(|_| Error::<T>::InsufficientBondFunds)?;
            }

            // Refund the user.
            let (escrow_asset, escrow_amount) =
                IntentEscrowBalances::<T>::get(intent_id).ok_or(Error::<T>::IntentNotFound)?;
            let intent_escrow = Self::intent_escrow_account();

            T::Assets::transfer(
                escrow_asset,
                &intent_escrow,
                &intent.user,
                escrow_amount,
                Expendable,
            )
            .map_err(|_| Error::<T>::InvalidAmount)?;

            IntentEscrowBalances::<T>::remove(intent_id);
            intent.status = crate::settlement::IntentStatus::Expired;
            Intents::<T>::insert(intent_id, intent.clone());
            FillCommitments::<T>::remove(intent_id);

            solver.bond = Zero::zero();
            solver.reputation =
                solver.reputation.saturating_add(crate::settlement::REPUTATION_SLASH_PENALTY);
            solver.fills_slashed = solver.fills_slashed.saturating_add(1);
            solver.active_commitments = solver.active_commitments.saturating_sub(1);
            solver.active = false;
            Solvers::<T>::insert(commitment.solver_id, solver);

            Self::deposit_event(Event::SolverSlashed {
                solver_id: commitment.solver_id,
                intent_id,
                slashed_amount: bond,
                to_treasury,
                to_slasher,
                slasher,
            });

            Self::deposit_event(Event::IntentRefunded {
                intent_id,
                user: intent.user,
                amount_refunded: escrow_amount,
            });

            Ok(())
        }

        /// Refund an intent that expired without ever being committed.
        /// Callable by the intent owner once the deadline has passed, only
        /// while the intent is still `Open`. Intents that were committed but
        /// not settled go through `slash_solver` instead.
        #[pallet::call_index(7)]
        #[pallet::weight(<T as Config>::WeightInfo::refund_expired_intent())]
        pub fn refund_expired_intent(origin: OriginFor<T>, intent_id: u64) -> DispatchResult {
            let who = ensure_signed(origin)?;

            let mut intent = Intents::<T>::get(intent_id).ok_or(Error::<T>::IntentNotFound)?;

            ensure!(intent.user == who, Error::<T>::NotIntentOwner);
            ensure!(
                intent.status == crate::settlement::IntentStatus::Open,
                Error::<T>::IntentNotOpen,
            );

            let now = frame_system::Pallet::<T>::block_number();
            ensure!(now >= intent.deadline, Error::<T>::DeadlineNotPassed);

            let (escrow_asset, escrow_amount) =
                IntentEscrowBalances::<T>::get(intent_id).ok_or(Error::<T>::IntentNotFound)?;
            let intent_escrow = Self::intent_escrow_account();

            T::Assets::transfer(escrow_asset, &intent_escrow, &who, escrow_amount, Expendable)
                .map_err(|_| Error::<T>::InvalidAmount)?;

            IntentEscrowBalances::<T>::remove(intent_id);
            intent.status = crate::settlement::IntentStatus::Expired;
            Intents::<T>::insert(intent_id, intent);

            Self::deposit_event(Event::IntentRefunded {
                intent_id,
                user: who,
                amount_refunded: escrow_amount,
            });

            Ok(())
        }

        // ---- governance setters ----

        /// Update the bid window (in blocks). Gated on `ManageOrigin`.
        #[pallet::call_index(8)]
        #[pallet::weight(<T as Config>::WeightInfo::set_bid_window())]
        pub fn set_bid_window(
            origin: OriginFor<T>,
            new_value: BlockNumberFor<T>,
        ) -> DispatchResult {
            T::ManageOrigin::ensure_origin(origin)?;
            ensure!(!new_value.is_zero(), Error::<T>::InvalidAmount);
            BidWindowBlocks::<T>::put(new_value);
            Self::deposit_event(Event::BidWindowUpdated { new_value });
            Ok(())
        }

        /// Update the settlement window (in blocks). Gated on `ManageOrigin`.
        #[pallet::call_index(9)]
        #[pallet::weight(<T as Config>::WeightInfo::set_settlement_window())]
        pub fn set_settlement_window(
            origin: OriginFor<T>,
            new_value: BlockNumberFor<T>,
        ) -> DispatchResult {
            T::ManageOrigin::ensure_origin(origin)?;
            ensure!(!new_value.is_zero(), Error::<T>::InvalidAmount);
            SettlementWindowBlocks::<T>::put(new_value);
            Self::deposit_event(Event::SettlementWindowUpdated { new_value });
            Ok(())
        }

        /// Update the required solver bond amount. Gated on `ManageOrigin`.
        /// Does not retroactively affect solvers already bonded at the prior
        /// amount; only applies to new registrations.
        #[pallet::call_index(10)]
        #[pallet::weight(<T as Config>::WeightInfo::set_solver_bond_amount())]
        pub fn set_solver_bond_amount(
            origin: OriginFor<T>,
            new_value: T::Balance,
        ) -> DispatchResult {
            T::ManageOrigin::ensure_origin(origin)?;
            ensure!(!new_value.is_zero(), Error::<T>::InvalidAmount);
            SolverBondAmount::<T>::put(new_value);
            Self::deposit_event(Event::SolverBondAmountUpdated { new_value });
            Ok(())
        }
    }

    impl<T: Config> Pallet<T> {
        /// What every account the pallet owns must hold, and how its records
        /// must agree with one another:
        ///
        /// - the intent escrow holds, per asset, at least the sum of every
        ///   open or committed intent's recorded input;
        /// - an intent has a recorded input exactly while it is open or
        ///   committed, and a commitment exactly while it is committed;
        /// - an active solver's escrow holds at least its bond, an inactive
        ///   solver has none, and its open-commitment count is the number of
        ///   commitments naming it.
        #[cfg(any(feature = "try-runtime", test))]
        pub fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
            use crate::settlement::IntentStatus;
            use frame_support::ensure;
            use sp_std::vec::Vec;

            // Recorded intent inputs, summed per asset.
            let mut owed: Vec<(T::AssetKind, T::Balance)> = Vec::new();
            for (id, (asset, amount)) in IntentEscrowBalances::<T>::iter() {
                let intent = Intents::<T>::get(id).ok_or("an escrowed input has no intent")?;
                ensure!(
                    matches!(intent.status, IntentStatus::Open | IntentStatus::Committed),
                    "a closed intent still has an escrowed input"
                );
                ensure!(
                    intent.token_in == asset && intent.amount_in == amount,
                    "an escrowed input differs from its intent"
                );
                match owed.iter_mut().find(|(a, _)| *a == asset) {
                    Some((_, sum)) => *sum = sum.saturating_add(amount),
                    None => owed.push((asset, amount)),
                }
            }
            let intent_escrow = Self::intent_escrow_account();
            for (asset, sum) in owed {
                ensure!(
                    T::Assets::balance(asset, &intent_escrow) >= sum,
                    "the intent escrow holds less than its open intents"
                );
            }
            for (id, intent) in Intents::<T>::iter() {
                let live = matches!(intent.status, IntentStatus::Open | IntentStatus::Committed);
                ensure!(
                    IntentEscrowBalances::<T>::contains_key(id) == live,
                    "an open or committed intent has no escrowed input"
                );
                ensure!(
                    FillCommitments::<T>::contains_key(id)
                        == (intent.status == IntentStatus::Committed),
                    "a commitment does not match its intent's status"
                );
            }

            let native = T::NativeAsset::get();
            for (id, solver) in Solvers::<T>::iter() {
                if solver.active {
                    ensure!(
                        T::Assets::balance(native.clone(), &Self::solver_escrow_account(id))
                            >= solver.bond,
                        "a solver escrow holds less than its bond"
                    );
                } else {
                    ensure!(solver.bond.is_zero(), "an inactive solver still has a bond");
                }
                let named = FillCommitments::<T>::iter_values()
                    .filter(|c| c.solver_id == id)
                    .count() as u32;
                ensure!(
                    solver.active_commitments == named,
                    "a solver's commitment count differs from its commitments"
                );
            }
            Ok(())
        }

        /// Give each pallet-held escrow that holds less than the native ED
        /// its ED, so it exists by its own provider rather than by whatever
        /// else it holds: minted at genesis (`source` is `None`), and on a
        /// live chain paid by `source`, so issuance does not change. Returns
        /// how many it endowed. Solver escrows are not among them: each is
        /// created with its bond.
        pub(crate) fn endow_escrows(source: Option<&T::AccountId>) -> u32 {
            let native = T::NativeAsset::get();
            let ed = T::Assets::minimum_balance(native.clone());
            let mut endowed = 0;
            for escrow in [Self::intent_escrow_account(), Self::protocol_treasury_account()] {
                if T::Assets::balance(native.clone(), &escrow) >= ed {
                    continue;
                }
                let paid = match source {
                    None => T::Assets::mint_into(native.clone(), &escrow, ed).map(|_| ()),
                    Some(from) => {
                        T::Assets::transfer(native.clone(), from, &escrow, ed, Preserve).map(|_| ())
                    },
                };
                match paid {
                    Ok(()) => endowed += 1,
                    Err(e) => log::error!(
                        target: "runtime::solver-marketplace",
                        "could not endow {escrow:?} with the ED: {e:?}"
                    ),
                }
            }
            endowed
        }

        /// The marketplace's own account. Nothing is held here; the escrows
        /// below are its sub-accounts.
        pub fn account_id() -> T::AccountId {
            PALLET_ID.into_account_truncating()
        }

        /// Current bid window. Uses storage value if set, otherwise the
        /// genesis default from `T::DefaultBidWindowBlocks`.
        pub(crate) fn current_bid_window() -> BlockNumberFor<T> {
            BidWindowBlocks::<T>::get().unwrap_or_else(T::DefaultBidWindowBlocks::get)
        }

        /// Current settlement window.
        pub(crate) fn current_settlement_window() -> BlockNumberFor<T> {
            SettlementWindowBlocks::<T>::get().unwrap_or_else(T::DefaultSettlementWindowBlocks::get)
        }

        /// Current solver bond amount.
        pub(crate) fn current_solver_bond() -> T::Balance {
            SolverBondAmount::<T>::get().unwrap_or_else(T::DefaultSolverBondAmount::get)
        }

        /// Derive the escrow account holding a specific solver's bond.
        ///
        /// Deterministic function of `solver_id`. Each solver gets a unique
        /// sub-account so slashing and refunds cannot mix funds. The
        /// `"slvr"` tag leads the seed, so no id derives the pallet's own
        /// account (an all-zero seed) or another escrow's. On twenty-byte
        /// accounts the seed keeps the tag and the id's low four bytes:
        /// ids alias only 2^32 registrations apart.
        pub(crate) fn solver_escrow_account(solver_id: u64) -> T::AccountId {
            let mut seed = [0u8; 12];
            seed[..4].copy_from_slice(b"slvr");
            seed[4..].copy_from_slice(&solver_id.to_le_bytes());
            PALLET_ID.into_sub_account_truncating(seed)
        }

        /// Derive the shared escrow account holding all pending intent
        /// `token_in` balances. Per-intent accounting lives in the `Intents`
        /// storage map.
        pub fn intent_escrow_account() -> T::AccountId {
            PALLET_ID.into_sub_account_truncating(b"intents")
        }

        /// Derive the protocol fee treasury account (where the protocol's
        /// share of solver profits accrues). Downstream distribution happens
        /// off this account via a separate sweep extrinsic in a later phase.
        pub(crate) fn protocol_treasury_account() -> T::AccountId {
            PALLET_ID.into_sub_account_truncating(b"fee_trsy")
        }
    }
}
