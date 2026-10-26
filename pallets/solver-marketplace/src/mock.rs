//! Test environment for the solver marketplace: the marketplace on top of
//! the real Vitreus DEX, which it settles through.

use super::*;
use crate as pallet_solver_marketplace;

use frame_support::{
    construct_runtime, derive_impl, parameter_types,
    traits::{AsEnsureOriginWithArg, ConstU128, ConstU32, ConstU64},
};
use frame_system::{EnsureRoot, EnsureSigned};
use sp_runtime::{traits::IdentityLookup, BuildStorage};

type Block = frame_system::mocking::MockBlock<Test>;

pub use pallet_vitreus_dex::testing::*;

construct_runtime!(
    pub enum Test
    {
        System: frame_system,
        Balances: pallet_balances,
        Assets: pallet_assets,
        VitreusDex: pallet_vitreus_dex,
        AssetConversion: pallet_asset_conversion,
        PoolAssets: pallet_assets::<Instance2>,
        PoolAssetsFreezer: pallet_assets_freezer::<Instance2>,
        Marketplace: pallet_solver_marketplace,
    }
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
    type AccountId = sp_core::H160;
    type Lookup = IdentityLookup<Self::AccountId>;
    type Block = Block;
    type AccountData = pallet_balances::AccountData<u128>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
    type Balance = u128;
    // The DEX freezes LP tokens; balances shares the runtime's freeze reasons.
    type MaxFreezes = frame_support::traits::VariantCountOf<RuntimeFreezeReason>;
    type ExistentialDeposit = ExistentialDeposit;
    type AccountStore = System;
}

impl pallet_assets::Config for Test {
    type RuntimeEvent = RuntimeEvent;
    type Holder = ();
    type ReserveData = ();
    type Balance = u128;
    type RemoveItemsLimit = ConstU32<1000>;
    type AssetId = u32;
    type AssetIdParameter = u32;
    type Currency = Balances;
    type CreateOrigin = AsEnsureOriginWithArg<EnsureSigned<Self::AccountId>>;
    type ForceOrigin = EnsureRoot<Self::AccountId>;
    type AssetDeposit = AssetDeposit;
    type AssetAccountDeposit = AssetAccountDeposit;
    type MetadataDepositBase = ConstU128<0>;
    type MetadataDepositPerByte = ConstU128<0>;
    type ApprovalDeposit = ConstU128<0>;
    type StringLimit = ConstU32<50>;
    type Freezer = ();
    type Extra = ();
    type WeightInfo = ();
    type CallbackHandle = ();
    pallet_assets::runtime_benchmarks_enabled! {
        type BenchmarkHelper = ();
    }
}

pub type NativeOrAssetId = frame_support::traits::fungible::NativeOrWithId<u32>;

type NativeAndAssets = frame_support::traits::fungible::UnionOf<
    Balances,
    Assets,
    frame_support::traits::fungible::NativeFromLeft,
    NativeOrAssetId,
    AccountId,
>;

parameter_types! {
    // Per-thread, so each test sees the values its externalities set. The
    // defaults are the unit-test values `tests.rs` is written against;
    // `new_runtime_like_ext` switches to the runtime's.
    pub static ExistentialDeposit: u128 = 1;
    pub static AssetDeposit: u128 = 0;
    pub static AssetAccountDeposit: u128 = 0;
    pub static DefaultSolverBondAmount: u128 = 1_000_000_000_000;
}

parameter_types! {
    pub const NativeAsset: NativeOrAssetId = NativeOrAssetId::Native;
    pub EnergyAsset: NativeOrAssetId = NativeOrAssetId::WithId(VNRG_ID);
    pub const ExcessRecipient: AccountId = TREASURY;
}

/// `LAUNCH_ID`'s creator is `CREATOR`; nothing else has one.
pub struct MockCreators;
impl pallet_vitreus_dex::CreatorFeeRecipient<NativeOrAssetId, AccountId> for MockCreators {
    fn creator_fee_recipient(asset: &NativeOrAssetId) -> Option<AccountId> {
        match asset {
            NativeOrAssetId::WithId(id) if *id == LAUNCH_ID => Some(CREATOR),
            _ => None,
        }
    }
}

/// Every `WithId(id)` with `id >= RESERVED_ASSET_BASE`.
pub struct ReservedAssets;
impl frame_support::traits::Contains<NativeOrAssetId> for ReservedAssets {
    fn contains(a: &NativeOrAssetId) -> bool {
        matches!(a, NativeOrAssetId::WithId(id) if *id >= RESERVED_ASSET_BASE)
    }
}

pallet_vitreus_dex::impl_base_pallet_for_mock!(Test, AccountId, u32, NativeAndAssets, NativeAsset);

impl pallet_vitreus_dex::Config for Test {
    type ManageOrigin = EnsureRoot<AccountId>;
    type Balance = u128;
    type HigherPrecisionBalance = sp_core::U256;
    type AssetKind = NativeOrAssetId;
    type Assets = NativeAndAssets;
    type NativeAsset = NativeAsset;
    type EnergyAsset = EnergyAsset;
    type ReservedAssets = ReservedAssets;
    type ExcessRecipient = ExcessRecipient;
    type DefaultProtocolFeeRecipient = ExcessRecipient;
    type CreatorFeeRecipient = MockCreators;
    // No launch has a treasury here: every treasury slice folds into the
    // protocol share.
    type TreasurySink = ();
    type Base = pallet_vitreus_dex::AssetConversionAdapter<Self, PoolAssetsFreezer>;
    type RuntimeFreezeReason = RuntimeFreezeReason;
    type WeightInfo = ();
    #[cfg(feature = "runtime-benchmarks")]
    type BenchmarkHelper = ();
}

/// Benchmarks' pools, made through the DEX's `PoolManager`.
#[cfg(feature = "runtime-benchmarks")]
pub struct MockBenchHelper;
#[cfg(feature = "runtime-benchmarks")]
impl crate::BenchmarkHelper<NativeOrAssetId, AccountId, u128> for MockBenchHelper {
    fn asset_kind(seed: u32) -> NativeOrAssetId {
        NativeOrAssetId::WithId(seed)
    }
    fn create_pool(provider: &AccountId, asset: NativeOrAssetId, liquidity: u128) {
        use pallet_vitreus_dex::PoolManager;
        type Dex = pallet_vitreus_dex::Pallet<Test>;
        <Dex as PoolManager<_, _, _, u64>>::create_pool(NativeAsset::get(), asset.clone(), 3)
            .expect("create pool");
        <Dex as PoolManager<_, _, _, u64>>::add_liquidity_for(
            provider,
            NativeAsset::get(),
            asset,
            liquidity,
            liquidity,
            0,
            0,
        )
        .expect("seed liquidity");
    }
}

impl Config for Test {
    type ManageOrigin = EnsureRoot<AccountId>;
    type Balance = u128;
    type AssetKind = NativeOrAssetId;
    type Assets = NativeAndAssets;
    type NativeAsset = NativeAsset;
    type Dex = VitreusDex;
    type DefaultBidWindowBlocks = ConstU64<10>;
    type DefaultSettlementWindowBlocks = ConstU64<5>;
    type DefaultSolverBondAmount = DefaultSolverBondAmount;
    type WeightInfo = ();
    #[cfg(feature = "runtime-benchmarks")]
    type BenchmarkHelper = MockBenchHelper;
}

pub(crate) fn new_test_ext() -> sp_io::TestExternalities {
    // Tests share a thread under `--test-threads=1`.
    ExistentialDeposit::set(1);
    AssetDeposit::set(0);
    AssetAccountDeposit::set(0);
    DefaultSolverBondAmount::set(1_000_000_000_000);

    let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

    pallet_balances::GenesisConfig::<Test> {
        dev_accounts: None,
        // 10× the default solver bond (1e12) so tests can run register +
        // deregister + re-register flows without hitting ED or balance limits.
        balances: vec![
            (ALICE, 10_000_000_000_000),
            (BOB, 10_000_000_000_000),
            (CHARLIE, 10_000_000_000_000),
        ],
    }
    .assimilate_storage(&mut t)
    .unwrap();

    pallet_assets::GenesisConfig::<Test> {
        assets: vec![(USDC_ID, ALICE, true, 1), (VNRG_ID, ALICE, true, 1)],
        accounts: vec![
            (USDC_ID, ALICE, 1_000_000),
            (USDC_ID, BOB, 1_000_000),
            (USDC_ID, CHARLIE, 1_000_000),
            (VNRG_ID, ALICE, 1_000_000),
            (VNRG_ID, BOB, 1_000_000),
            (VNRG_ID, CHARLIE, 1_000_000),
        ],
        ..Default::default()
    }
    .assimilate_storage(&mut t)
    .unwrap();

    let mut ext = sp_io::TestExternalities::new(t);
    ext.execute_with(|| System::set_block_number(1));
    ext
}

/// Externalities shaped like the runtime (see
/// [`pallet_vitreus_dex::testing`]), the bond at its default. Neither
/// pallet's genesis build runs, as on a chain that receives them by
/// upgrade, and the DEX's upgrade migrations have run;
/// `new_runtime_like_ext_from_genesis` runs both genesis builds instead.
pub(crate) fn new_runtime_like_ext() -> sp_io::TestExternalities {
    build_runtime_like_ext(false)
}

pub(crate) fn new_runtime_like_ext_from_genesis() -> sp_io::TestExternalities {
    build_runtime_like_ext(true)
}

fn build_runtime_like_ext(genesis: bool) -> sp_io::TestExternalities {
    ExistentialDeposit::set(RUNTIME_ED);
    AssetDeposit::set(RUNTIME_ASSET_DEPOSIT);
    AssetAccountDeposit::set(RUNTIME_ASSET_ACCOUNT_DEPOSIT);
    DefaultSolverBondAmount::set(RUNTIME_SOLVER_BOND);

    let mut t = runtime_like_storage::<Test>();
    if genesis {
        pallet_vitreus_dex::GenesisConfig::<Test>::default()
            .assimilate_storage(&mut t)
            .unwrap();
        pallet_solver_marketplace::GenesisConfig::<Test>::default()
            .assimilate_storage(&mut t)
            .unwrap();
    }

    let mut ext = sp_io::TestExternalities::new(t);
    ext.execute_with(|| {
        System::set_block_number(1);
        if !genesis {
            // What the upgrade does for the DEX's fee escrow: its ED, from
            // the treasury's stand-in.
            use frame_support::traits::fungible::Mutate as _;
            Balances::transfer(
                &UpgradeSource::get(),
                &VitreusDex::fee_escrow_account(),
                RUNTIME_ED,
                frame_support::traits::tokens::Preservation::Preserve,
            )
            .unwrap();
            System::reset_events();
        }
    });
    ext
}
