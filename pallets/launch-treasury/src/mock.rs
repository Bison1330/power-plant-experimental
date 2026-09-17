//! Test runtime for pallet-launch-treasury: the real `pallet_vitreus_dex`
//! (D9) and `pallet_launchpad` (L1/L2), `pallet_assets`, `pallet_balances`,
//! and two stand-ins for the Foundation's pallets:
//!
//! - `MockStaking` — a ledger that enforces the rules of `energy-generation`
//!   this pallet depends on, read from its source at `423740e`: `bond`
//!   needs `value ≥ ED`; `bond_extra` bonds `min(free − total, extra)` and
//!   does not touch cooperation targets; `cooperate` needs `active ≥
//!   MinCooperatorBond`, the stash's reputation, and every target
//!   cooperable, all-or-nothing; `unbond` refuses to leave a cooperator
//!   under `MinCooperatorBond` (chill first), merges chunks of one era and
//!   caps them at `MaxUnlockingChunks`; `withdraw_unbonded` releases every
//!   matured chunk and kills an empty ledger; a slash hits active and
//!   unlocking alike. The bond is a `Balances` lock, as on chain.
//! - `MockBroker` — the energy broker's `LNRG → VTRS` path at a fixed rate
//!   with a 1 % fee, paying from `BROKER`'s VTRS and burning the LNRG.
//!   `InsufficientLiquidity` when `BROKER` is short, like the real one.
//!
//! `new_test_ext` is the upgrade path (the vault holds its ED and
//! `VaultFunded` is set, as `FundLaunchTreasuryVault` leaves it);
//! `new_test_ext_from_genesis` is the chain that ships the pallet at genesis
//! (§9.6: nothing funds the vault, the first fee creates it).

use super::*;
use crate as pallet_launch_treasury;

use frame_support::{
    construct_runtime, derive_impl, parameter_types,
    traits::{
        fungibles::Mutate as _,
        tokens::{Fortitude, Precision, Preservation},
        AsEnsureOriginWithArg, ConstU128, ConstU16, ConstU32, ConstU64, Contains, LockIdentifier,
        LockableCurrency, WithdrawReasons,
    },
    PalletId,
};
use frame_system::{EnsureRoot, EnsureSigned};
use pallet_launchpad::{LaunchParams, OnCurveBuy};
use sp_runtime::{traits::IdentityLookup, BuildStorage};
use std::{cell::RefCell, collections::BTreeMap};

type Block = frame_system::mocking::MockBlock<Test>;

pub const UNIT: u128 = 1_000_000_000_000_000_000;
pub const ED: u128 = 1_000_000_000_000; // 10^-6 VTRS

/// Twenty bytes, as on the chain (`fp_account::AccountId20`): every
/// `PalletId`-derived account is truncated to twenty bytes here as it is
/// there, so a derivation that only collides at that width (D8) collides
/// in these tests too. `sp_core::H160` is the same twenty bytes without
/// frontier in the dev-dependencies.
pub type Acc = sp_core::H160;
pub const fn acc(b: u8) -> Acc {
    sp_core::H160([b; 20])
}
pub const ALICE: Acc = acc(1);
pub const BOB: Acc = acc(2);
pub const CHARLIE: Acc = acc(3);
pub const KEEPER: Acc = acc(7);
pub const TREASURY: Acc = acc(99);
pub const EXCESS: Acc = acc(98);
pub const BROKER: Acc = acc(97);
pub const VAL_A: Acc = acc(0xA0);
pub const VAL_B: Acc = acc(0xB0);
pub const VAL_C: Acc = acc(0xC0);

pub const VNRG_ID: u128 = 0;
pub const LNRG_ID: u128 = 2;
pub const ASSET_BASE: u128 = 1u128 << 64;
pub const T_DEFAULT: u128 = 3_000 * UNIT;
pub const CREATION_FEE: u128 = 10 * ED;

pub const MIN_COOP_BOND: u128 = UNIT;
pub const BONDING_DURATION: u32 = 42;
pub const MAX_CHUNKS: u32 = 64;
pub const DORMANCY: u64 = 100;
pub const BURN_INTERVAL: u64 = 10;
pub const IMPACT_BPS: u16 = 50;
pub const BOUNTY_BPS: u16 = 50;

construct_runtime!(
    pub enum Test {
        System: frame_system,
        Balances: pallet_balances,
        Assets: pallet_assets,
        VitreusDex: pallet_vitreus_dex,
        Launchpad: pallet_launchpad,
        LaunchTreasury: pallet_launch_treasury,
    }
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
    type AccountId = Acc;
    type Lookup = IdentityLookup<Self::AccountId>;
    type Block = Block;
    type AccountData = pallet_balances::AccountData<u128>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
    type Balance = u128;
    type ExistentialDeposit = ConstU128<ED>;
    type AccountStore = System;
    type MaxLocks = ConstU32<4>;
}

parameter_types! {
    pub const AssetDeposit: u128 = 100;
    pub const MetadataDepositBase: u128 = 100;
    pub const MetadataDepositPerByte: u128 = 2;
}

impl pallet_assets::Config for Test {
    type RuntimeEvent = RuntimeEvent;
    type Balance = u128;
    type RemoveItemsLimit = ConstU32<1000>;
    type AssetId = u128;
    type AssetIdParameter = parity_scale_codec::Compact<u128>;
    type Currency = Balances;
    type CreateOrigin = AsEnsureOriginWithArg<EnsureSigned<Self::AccountId>>;
    type ForceOrigin = EnsureRoot<Self::AccountId>;
    type AssetDeposit = AssetDeposit;
    type AssetAccountDeposit = ConstU128<10>;
    type MetadataDepositBase = MetadataDepositBase;
    type MetadataDepositPerByte = MetadataDepositPerByte;
    type ApprovalDeposit = ConstU128<ED>;
    type StringLimit = ConstU32<50>;
    type Freezer = ();
    type Extra = ();
    type WeightInfo = ();
    type CallbackHandle = ();
    pallet_assets::runtime_benchmarks_enabled! {
        type BenchmarkHelper = AssetsBenchmarkHelper;
    }
}

pallet_assets::runtime_benchmarks_enabled! {
    pub struct AssetsBenchmarkHelper;
    impl pallet_assets::BenchmarkHelper<parity_scale_codec::Compact<u128>> for AssetsBenchmarkHelper {
        fn create_asset_id_parameter(id: u32) -> parity_scale_codec::Compact<u128> {
            parity_scale_codec::Compact(id.into())
        }
    }
}

pub type NativeOrAssetId = frame_support::traits::fungible::NativeOrWithId<u128>;
pub type NativeAndAssets = frame_support::traits::fungible::UnionOf<
    Balances,
    Assets,
    frame_support::traits::fungible::NativeFromLeft,
    NativeOrAssetId,
    Acc,
>;

parameter_types! {
    pub const NativeAsset: NativeOrAssetId = NativeOrAssetId::Native;
    pub EnergyAsset: NativeOrAssetId = NativeOrAssetId::WithId(VNRG_ID);
    pub LnrgAsset: NativeOrAssetId = NativeOrAssetId::WithId(LNRG_ID);
    pub const ExcessRecipient: Acc = EXCESS;
    pub const TreasuryAccount: Acc = TREASURY;
}

pub struct ReservedAssets;
impl Contains<NativeOrAssetId> for ReservedAssets {
    fn contains(a: &NativeOrAssetId) -> bool {
        matches!(a, NativeOrAssetId::WithId(id) if *id >= ASSET_BASE && *id < (ASSET_BASE << 1))
    }
}

pub struct LaunchpadCreators;
impl pallet_vitreus_dex::CreatorFeeRecipient<NativeOrAssetId, Acc> for LaunchpadCreators {
    fn creator_fee_recipient(asset: &NativeOrAssetId) -> Option<Acc> {
        match asset {
            NativeOrAssetId::WithId(id) => Launchpad::creator_fee_recipient_for(*id),
            NativeOrAssetId::Native => None,
        }
    }
}

impl pallet_vitreus_dex::Config for Test {
    type RuntimeEvent = RuntimeEvent;
    type ManageOrigin = EnsureRoot<Acc>;
    type Balance = u128;
    type HigherPrecisionBalance = sp_core::U256;
    type AssetKind = NativeOrAssetId;
    type Assets = NativeAndAssets;
    type NativeAsset = NativeAsset;
    type EnergyAsset = EnergyAsset;
    type ReservedAssets = ReservedAssets;
    type ExcessRecipient = ExcessRecipient;
    type DefaultProtocolFeeRecipient = TreasuryAccount;
    type CreatorFeeRecipient = LaunchpadCreators;
    // D9: the DEX pushes the treasury slice into this pallet.
    type TreasurySink = LaunchTreasury;
    type DefaultBidWindowBlocks = ConstU64<10>;
    type DefaultSettlementWindowBlocks = ConstU64<5>;
    type DefaultSolverBondAmount = ConstU128<1_000_000_000_000>;
    type WeightInfo = ();
    #[cfg(feature = "runtime-benchmarks")]
    type BenchmarkHelper = ();
}

parameter_types! {
    pub const LaunchpadPalletId: PalletId = PalletId(*b"vtrs/lpd");
    pub const TotalSupply: u128 = 1_000_000_000 * UNIT;
    pub const Sellable: u128 = 800_000_000 * UNIT;
    pub const VirtualTokenFloor: u128 = 266_666_667 * UNIT;
    pub const LaunchAssetBase: u128 = ASSET_BASE;
    pub const MinGraduationTarget: u128 = 3 * UNIT;
    pub const MaxGraduationTarget: u128 = 3_000_000_000 * UNIT;
    pub const MinCreationFee: u128 = 2 * ED + 100 + 100 + 2 * 50 * 2;
    /// Spec §2.6: creator 50 / protocol 25 / treasury 25 on the curve.
    pub DefaultLaunchParams: LaunchParams<u128> = LaunchParams {
        graduation_target: T_DEFAULT,
        curve_fee_bps: 100,
        protocol_share_bps: 2_500,
        treasury_share_bps: 2_500,
        pool_fee_tier: 3,
        creation_fee: CREATION_FEE,
    };
}

pub struct IntoAssetKind;
impl sp_runtime::traits::Convert<u128, NativeOrAssetId> for IntoAssetKind {
    fn convert(id: u128) -> NativeOrAssetId {
        NativeOrAssetId::WithId(id)
    }
}
pub struct AssetIdOfKind;
impl sp_runtime::traits::Convert<NativeOrAssetId, Option<u128>> for AssetIdOfKind {
    fn convert(k: NativeOrAssetId) -> Option<u128> {
        match k {
            NativeOrAssetId::WithId(id) => Some(id),
            NativeOrAssetId::Native => None,
        }
    }
}

pub struct PassHook;
impl OnCurveBuy<Acc, u128, u64> for PassHook {
    fn on_buy(
        _: LaunchId,
        _: u64,
        _: u64,
        _: &Acc,
        _: bool,
        quote_in: u128,
    ) -> Result<(u128, u128), DispatchError> {
        Ok((quote_in, 0))
    }
}

impl pallet_launchpad::Config for Test {
    type RuntimeEvent = RuntimeEvent;
    type LaunchManageOrigin = EnsureRoot<Acc>;
    type AssetId = u128;
    type Currency = Balances;
    type LaunchAssets = Assets;
    type NativeAssetKind = NativeAsset;
    type IntoAssetKind = IntoAssetKind;
    type Dex = VitreusDex;
    type Treasury = TreasuryAccount;
    // L1: the launchpad pushes the curve's treasury share into this pallet.
    type CurveTreasurySink = LaunchTreasury;
    type PalletId = LaunchpadPalletId;
    type TotalSupply = TotalSupply;
    type Sellable = Sellable;
    type VirtualTokenFloor = VirtualTokenFloor;
    type LaunchAssetBase = LaunchAssetBase;
    type MinGraduationTarget = MinGraduationTarget;
    type MaxGraduationTarget = MaxGraduationTarget;
    type MaxCurveFeeBps = ConstU16<500>;
    type MinProtocolShareBps = ConstU16<5_000>;
    type MinCreationFee = MinCreationFee;
    type RescueDelay = ConstU64<100_800>;
    type StringLimit = ConstU32<50>;
    type UriLimit = ConstU32<256>;
    type DescriptionLimit = ConstU32<1024>;
    type DefaultLaunchParams = DefaultLaunchParams;
    type BuyHook = PassHook;
    type WeightInfo = ();
}

// ---- MockStaking --------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Ledger {
    pub active: u128,
    /// `(era, amount)`, one entry per era, FIFO.
    pub unlocking: Vec<(u32, u128)>,
    pub cooperating: bool,
    pub targets: Vec<(Acc, u128)>,
}
impl Ledger {
    pub fn total(&self) -> u128 {
        self.active + self.unlocking.iter().map(|(_, v)| v).sum::<u128>()
    }
}

thread_local! {
    pub static LEDGER: RefCell<BTreeMap<Acc, Ledger>> = const { RefCell::new(BTreeMap::new()) };
    pub static ERA: RefCell<u32> = const { RefCell::new(10) };
    /// Validators the vault may target: `account → cooperable`.
    pub static VALIDATORS: RefCell<BTreeMap<Acc, bool>> = const { RefCell::new(BTreeMap::new()) };
    /// Whether the vault's reputation clears the targets' `min_coop_reputation` (§2.2; a fresh account does, a slashed one may not).
    pub static REPUTATION_OK: RefCell<bool> = const { RefCell::new(true) };
    /// Every `cooperate` the mock accepted: the targets as submitted.
    pub static COOPERATE_CALLS: RefCell<Vec<Vec<(Acc, u128)>>> = const { RefCell::new(Vec::new()) };
}

const STAKING_LOCK: LockIdentifier = *b"stakelck";

pub fn err(s: &'static str) -> DispatchError {
    DispatchError::Other(s)
}

/// `a × b / c` without the u128 overflow a full vault's stake would hit.
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    u128::try_from(U256::from(a) * U256::from(b) / U256::from(c)).expect("fits: result ≤ a")
}

pub struct MockStaking;
impl MockStaking {
    fn with<R>(f: impl FnOnce(&mut BTreeMap<Acc, Ledger>) -> R) -> R {
        LEDGER.with(|l| f(&mut l.borrow_mut()))
    }
    fn relock(stash: &Acc, ledger: &Ledger) {
        let total = ledger.total();
        if total == 0 {
            <Balances as LockableCurrency<Acc>>::remove_lock(STAKING_LOCK, stash);
        } else {
            <Balances as LockableCurrency<Acc>>::set_lock(
                STAKING_LOCK,
                stash,
                total,
                WithdrawReasons::all(),
            );
        }
    }
    pub fn ledger(stash: &Acc) -> Option<Ledger> {
        Self::with(|m| m.get(stash).cloned())
    }
    /// A slash of `bps` on the whole ledger: active and unlocking alike, as
    /// `energy-generation` applies a validator's slash to its cooperators.
    pub fn slash(stash: &Acc, bps: u128) {
        Self::with(|m| {
            if let Some(l) = m.get_mut(stash) {
                let before = l.total();
                let cut = |v: u128| v - v * bps / 10_000;
                l.active = cut(l.active);
                for c in l.unlocking.iter_mut() {
                    c.1 = cut(c.1);
                }
                // The slashed VTRS leaves the stash (to the Treasury on chain), lock or no lock.
                let _ = <Balances as frame_support::traits::Currency<Acc>>::slash(
                    stash,
                    before - l.total(),
                );
                let total = l.active;
                let old: u128 = l.targets.iter().map(|(_, s)| s).sum();
                if old > 0 && total < old {
                    for t in l.targets.iter_mut() {
                        t.1 = mul_div(t.1, total, old);
                    }
                }
                Self::relock(stash, l);
            }
        });
    }
    pub fn set_era(era: u32) {
        ERA.with(|e| *e.borrow_mut() = era);
    }
    pub fn advance_eras(n: u32) {
        ERA.with(|e| *e.borrow_mut() += n);
    }
    pub fn set_validator(v: Acc, cooperable: bool) {
        VALIDATORS.with(|m| m.borrow_mut().insert(v, cooperable));
    }
}

impl TreasuryStaking<Acc, u128> for MockStaking {
    fn is_bonded(stash: &Acc) -> bool {
        Self::with(|m| m.contains_key(stash))
    }
    fn active(stash: &Acc) -> u128 {
        Self::ledger(stash).map(|l| l.active).unwrap_or(0)
    }
    fn total(stash: &Acc) -> u128 {
        Self::ledger(stash).map(|l| l.total()).unwrap_or(0)
    }
    fn is_cooperating(stash: &Acc) -> bool {
        Self::ledger(stash).map(|l| l.cooperating).unwrap_or(false)
    }
    fn cooperated(stash: &Acc) -> u128 {
        Self::ledger(stash).map(|l| l.targets.iter().map(|(_, s)| s).sum()).unwrap_or(0)
    }
    fn min_cooperator_bond() -> u128 {
        MIN_COOP_BOND
    }
    fn current_era() -> u32 {
        ERA.with(|e| *e.borrow())
    }
    fn bonding_duration() -> u32 {
        BONDING_DURATION
    }
    fn is_cooperable(validator: &Acc) -> bool {
        VALIDATORS.with(|m| m.borrow().get(validator).copied().unwrap_or(false))
    }
    fn bond(stash: &Acc, value: u128) -> DispatchResult {
        if Self::is_bonded(stash) {
            return Err(err("AlreadyBonded"));
        }
        if value < ED {
            return Err(err("InsufficientBond"));
        }
        let free = Balances::free_balance(stash);
        let value = value.min(free);
        let l = Ledger { active: value, ..Default::default() };
        Self::relock(stash, &l);
        Self::with(|m| m.insert(*stash, l));
        Ok(())
    }
    fn bond_extra(stash: &Acc, max_additional: u128) -> DispatchResult {
        let mut l = Self::ledger(stash).ok_or(err("NotStash"))?;
        let free = Balances::free_balance(stash);
        if let Some(extra) = free.checked_sub(l.total()) {
            let extra = extra.min(max_additional);
            l.active += extra;
            if l.active < ED {
                return Err(err("InsufficientBond"));
            }
            Self::relock(stash, &l);
            Self::with(|m| m.insert(*stash, l));
        }
        Ok(())
    }
    fn cooperate(stash: &Acc, targets: Vec<(Acc, u128)>) -> DispatchResult {
        let mut l = Self::ledger(stash).ok_or(err("NotController"))?;
        if l.active < MIN_COOP_BOND {
            return Err(err("InsufficientBond"));
        }
        let total: u128 = targets.iter().map(|(_, s)| s).sum();
        if total > l.active {
            return Err(err("InsufficientBond"));
        }
        if targets.is_empty() {
            return Err(err("EmptyTargets"));
        }
        if !REPUTATION_OK.with(|r| *r.borrow()) {
            return Err(err("ReputationTooLow"));
        }
        for (v, _) in &targets {
            if !Self::is_cooperable(v) {
                return Err(err("BadTarget"));
            }
        }
        l.cooperating = true;
        l.targets = targets.clone();
        Self::with(|m| m.insert(*stash, l));
        COOPERATE_CALLS.with(|c| c.borrow_mut().push(targets));
        Ok(())
    }
    fn chill(stash: &Acc) -> DispatchResult {
        let mut l = Self::ledger(stash).ok_or(err("NotController"))?;
        l.cooperating = false;
        l.targets.clear();
        Self::with(|m| m.insert(*stash, l));
        Ok(())
    }
    fn unbond(stash: &Acc, value: u128) -> DispatchResult {
        let mut l = Self::ledger(stash).ok_or(err("NotController"))?;
        if l.unlocking.len() >= MAX_CHUNKS as usize {
            return Err(err("NoMoreChunks"));
        }
        let mut value = value.min(l.active);
        if value == 0 {
            return Ok(());
        }
        l.active -= value;
        if l.active < ED {
            value += l.active;
            l.active = 0;
        }
        if l.cooperating && l.active < MIN_COOP_BOND {
            return Err(err("InsufficientBond"));
        }
        let era = Self::current_era() + BONDING_DURATION;
        match l.unlocking.last_mut() {
            Some(c) if c.0 == era => c.1 += value,
            _ => l.unlocking.push((era, value)),
        }
        // Targets scale down with active, as `adjust_cooperator_targets`.
        let old: u128 = l.targets.iter().map(|(_, s)| s).sum();
        if old > l.active && old > 0 {
            for t in l.targets.iter_mut() {
                t.1 = mul_div(t.1, l.active, old);
            }
        }
        Self::relock(stash, &l);
        Self::with(|m| m.insert(*stash, l));
        Ok(())
    }
    fn withdraw_unbonded(stash: &Acc) -> Result<u128, DispatchError> {
        let mut l = Self::ledger(stash).ok_or(err("NotController"))?;
        let era = Self::current_era();
        let (matured, waiting): (Vec<_>, Vec<_>) = l.unlocking.iter().partition(|(e, _)| *e <= era);
        let withdrawn: u128 = matured.iter().map(|(_, v)| v).sum();
        l.unlocking = waiting;
        Self::relock(stash, &l);
        if l.total() == 0 {
            Self::with(|m| m.remove(stash));
        } else {
            Self::with(|m| m.insert(*stash, l));
        }
        Ok(withdrawn)
    }
}

// ---- MockBroker ---------------------------------------------------------

thread_local! {
    /// VTRS per LNRG as `(num, den)`; the broker rate is linear.
    pub static RATE: RefCell<(u128, u128)> = const { RefCell::new((1, 1)) };
}
pub const BROKER_FEE_PERMILLE: u128 = 10; // 1 %

pub struct MockBroker;
impl MockBroker {
    fn out_for(amount_in: u128) -> u128 {
        let (n, d) = RATE.with(|r| *r.borrow());
        let after_fee = amount_in * (1000 - BROKER_FEE_PERMILLE) / 1000;
        after_fee * n / d
    }
    pub fn set_rate(num: u128, den: u128) {
        RATE.with(|r| *r.borrow_mut() = (num, den));
    }
}
impl TreasuryExchange<Acc, u128> for MockBroker {
    fn quote(lnrg: u128) -> Option<u128> {
        Some(Self::out_for(lnrg))
    }
    fn depth() -> u128 {
        Balances::free_balance(BROKER).saturating_sub(ED)
    }
    fn sell(who: &Acc, lnrg: u128, min_native: u128) -> Result<u128, DispatchError> {
        // The real broker (energy-broker `do_swap` with `keep_alive`): the
        // sender's reducible balance under `Preserve` must cover the input,
        // and for a pallet-assets asset that is balance − min_balance.
        // Selling an account's whole LNRG is `NotExpendable` (R8, seen live
        // on 222).
        let reducible =
            <Assets as frame_support::traits::fungibles::Inspect<Acc>>::reducible_balance(
                LNRG_ID,
                who,
                Preservation::Preserve,
                Fortitude::Polite,
            );
        if reducible < lnrg {
            return Err(sp_runtime::TokenError::NotExpendable.into());
        }
        let out = Self::out_for(lnrg);
        if out == 0 {
            return Err(err("ZeroAmount"));
        }
        if out < min_native {
            return Err(err("ProvidedMinimumNotSufficientForSwap"));
        }
        if Self::depth() < out {
            return Err(err("InsufficientLiquidity"));
        }
        // LNRG sold is burned (the real converter drops the credit); VTRS comes from the broker.
        Assets::burn_from(
            LNRG_ID,
            who,
            lnrg,
            Preservation::Preserve,
            Precision::Exact,
            Fortitude::Polite,
        )?;
        <Balances as frame_support::traits::fungible::Mutate<Acc>>::transfer(
            &BROKER,
            who,
            out,
            Preservation::Preserve,
        )?;
        Ok(out)
    }
}

// ---- the pallet under test ---------------------------------------------

parameter_types! {
    pub const TreasuryPalletId: PalletId = PalletId(*b"vtrs/lpt");
    pub DefaultTerms: TreasuryTerms<u128, u64> = TreasuryTerms {
        dormancy_blocks: DORMANCY,
        min_stake: UNIT,
        max_burn_impact_bps: IMPACT_BPS,
        min_burn_interval: BURN_INTERVAL,
        keeper_bounty_bps: BOUNTY_BPS,
    };
}

impl pallet_launch_treasury::Config for Test {
    type RuntimeEvent = RuntimeEvent;
    type TreasuryManageOrigin = EnsureRoot<Acc>;
    type Staking = MockStaking;
    type Exchange = MockBroker;
    type LnrgAsset = LnrgAsset;
    type AssetIdOf = AssetIdOfKind;
    type PalletId = TreasuryPalletId;
    type MaxTargets = ConstU32<16>;
    type MaxUnlockingChunks = ConstU32<MAX_CHUNKS>;
    type DefaultTerms = DefaultTerms;
    type WeightInfo = ();
    #[cfg(feature = "runtime-benchmarks")]
    type BenchmarkHelper = MockBenchmarkHelper;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct MockBenchmarkHelper;
#[cfg(feature = "runtime-benchmarks")]
impl pallet_launch_treasury::BenchmarkHelper<Acc> for MockBenchmarkHelper {
    fn cooperable_validator(i: u32) -> Acc {
        let v: Acc = frame_benchmarking::account("validator", i, 0);
        MockStaking::set_validator(v.clone(), true);
        v
    }
    fn clear_cooperator_gate(_vault: &Acc) {
        REPUTATION_OK.with(|r| *r.borrow_mut() = true);
    }
    fn set_current_era(era: u32) {
        MockStaking::set_era(era);
    }
    fn prepare_exchange() {}
    fn set_exchange_depth(native: u128) {
        <Balances as frame_support::traits::fungible::Mutate<Acc>>::set_balance(
            &BROKER,
            native + ED,
        );
    }
}

pub const RICH: u128 = 1_000_000_000 * UNIT;

pub fn vault() -> Acc {
    LaunchTreasury::vault()
}

/// Simulate `payout_stakers`: LNRG minted to the vault (the payee).
pub fn pay_rewards(amount: u128) {
    Assets::mint_into(LNRG_ID, &vault(), amount).unwrap();
}

/// The upgrade path (§4, §7.4): `FundLaunchTreasuryVault` gave the vault
/// its ED and set `VaultFunded` before any fee reached it.
pub fn new_test_ext() -> sp_io::TestExternalities {
    build_ext(true)
}

/// The from-genesis path (§9.6, §10.11): no migration ran, so nothing
/// funds the vault and the first fee creates it.
pub fn new_test_ext_from_genesis() -> sp_io::TestExternalities {
    build_ext(false)
}

fn build_ext(vault_funded: bool) -> sp_io::TestExternalities {
    let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
    let mut balances = vec![
        (ALICE, RICH),
        (BOB, RICH),
        (CHARLIE, RICH),
        (KEEPER, UNIT),
        (TREASURY, ED),
        (EXCESS, ED),
        // The broker holds the gas market's VTRS float (§3.2).
        (BROKER, 1_000_000 * UNIT),
    ];
    if vault_funded {
        balances.push((vault(), ED));
    }
    pallet_balances::GenesisConfig::<Test> { balances }
        .assimilate_storage(&mut t)
        .unwrap();
    pallet_assets::GenesisConfig::<Test> {
        assets: vec![(VNRG_ID, ALICE, false, 1), (LNRG_ID, ALICE, false, 1)],
        metadata: vec![
            (VNRG_ID, b"VNRG".to_vec(), b"VNRG".to_vec(), 18),
            (LNRG_ID, b"LNRG".to_vec(), b"LNRG".to_vec(), 18),
        ],
        accounts: vec![],
        ..Default::default()
    }
    .assimilate_storage(&mut t)
    .unwrap();
    let mut ext = sp_io::TestExternalities::new(t);
    ext.execute_with(|| {
        System::set_block_number(1);
        LEDGER.with(|l| l.borrow_mut().clear());
        ERA.with(|e| *e.borrow_mut() = 10);
        REPUTATION_OK.with(|r| *r.borrow_mut() = true);
        COOPERATE_CALLS.with(|c| c.borrow_mut().clear());
        VALIDATORS.with(|m| {
            let mut m = m.borrow_mut();
            m.clear();
            m.insert(VAL_A, true);
            m.insert(VAL_B, true);
            m.insert(VAL_C, true);
        });
        RATE.with(|r| *r.borrow_mut() = (1, 1));
        if vault_funded {
            VaultFunded::<Test>::put(true);
        }
        // Spec §2.6 pool split: 5 protocol / 5 creator / 10 treasury / 10 pool.
        frame_support::assert_ok!(VitreusDex::set_default_fee_routing(
            RuntimeOrigin::root(),
            5,
            5,
            10
        ));
        frame_support::assert_ok!(LaunchTreasury::set_targets(
            RuntimeOrigin::root(),
            vec![VAL_A, VAL_B]
        ));
    });
    ext
}
