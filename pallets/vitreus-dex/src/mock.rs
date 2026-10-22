//! Test environment for the Vitreus DEX pallet.

use super::*;
use crate as pallet_vitreus_dex;

use frame_support::{
    construct_runtime, derive_impl, parameter_types,
    traits::{AsEnsureOriginWithArg, ConstU128, ConstU32},
};
use frame_system::{EnsureRoot, EnsureSigned};
use sp_runtime::{traits::IdentityLookup, BuildStorage};

type Block = frame_system::mocking::MockBlock<Test>;

pub use crate::testing::*;

construct_runtime!(
    pub enum Test
    {
        System: frame_system,
        Balances: pallet_balances,
        Assets: pallet_assets,
        VitreusDex: pallet_vitreus_dex,
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
    // defaults are the unit-test values the arithmetic tests are written
    // against; `new_runtime_like_ext` switches to the runtime's.
    pub static ExistentialDeposit: u128 = 1;
    pub static AssetDeposit: u128 = 0;
    pub static AssetAccountDeposit: u128 = 0;
}

parameter_types! {
    pub const NativeAsset: NativeOrAssetId = NativeOrAssetId::Native;
    pub const USDC: u32 = USDC_ID;
    pub const VNRG: u32 = VNRG_ID;
    pub EnergyAsset: NativeOrAssetId = NativeOrAssetId::WithId(VNRG_ID);
    pub const ExcessRecipient: AccountId = TREASURY;
}

/// D4: the creator on record for a reserved asset in tests (stands in for
/// the launchpad's `creator_fee_recipient`). `LAUNCH_ID` → `CREATOR`;
/// everything else has no creator.
pub struct MockCreators;
impl crate::CreatorFeeRecipient<NativeOrAssetId, AccountId> for MockCreators {
    fn creator_fee_recipient(asset: &NativeOrAssetId) -> Option<AccountId> {
        match asset {
            NativeOrAssetId::WithId(id) if *id == LAUNCH_ID => Some(CREATOR),
            NativeOrAssetId::WithId(id) => PRIMED_CREATORS.with(|c| c.borrow().get(id).copied()),
            NativeOrAssetId::Native => None,
        }
    }
}

/// D9: a recording treasury sink. `SINK_VAULT` is the account the launch
/// asset's treasury slice is pushed to (`None` = no treasury, fold into
/// protocol); `SINK_NOTED` is every `(asset id, amount)` the DEX reported.
pub struct MockSink;
impl crate::TreasurySink<NativeOrAssetId, AccountId, u128> for MockSink {
    fn account_for(asset: &NativeOrAssetId) -> Option<AccountId> {
        match asset {
            NativeOrAssetId::WithId(id) if *id == LAUNCH_ID => SINK_VAULT.with(|v| *v.borrow()),
            _ => None,
        }
    }
    fn note_fee(asset: &NativeOrAssetId, amount: u128) {
        if let NativeOrAssetId::WithId(id) = asset {
            SINK_NOTED.with(|n| n.borrow_mut().push((*id, amount)));
        }
    }
}

thread_local! {
    pub static SINK_VAULT: std::cell::RefCell<Option<AccountId>> = const { std::cell::RefCell::new(None) };
    pub static SINK_NOTED: std::cell::RefCell<Vec<(u32, u128)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Creators primed by `MockBenchHelper::set_creator` (benchmarks only).
    static PRIMED_CREATORS: std::cell::RefCell<std::collections::BTreeMap<u32, AccountId>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// Benchmark helper for the mock: `WithId(seed)` assets, and creators primed
/// through the thread-local `MockCreators` consults.
#[cfg(feature = "runtime-benchmarks")]
pub struct MockBenchHelper;
#[cfg(feature = "runtime-benchmarks")]
impl crate::BenchmarkHelper<NativeOrAssetId, AccountId> for MockBenchHelper {
    fn asset_kind(seed: u32) -> NativeOrAssetId {
        NativeOrAssetId::WithId(seed)
    }
    fn set_creator(asset: &NativeOrAssetId, who: &AccountId) -> bool {
        match asset {
            NativeOrAssetId::WithId(id) => {
                PRIMED_CREATORS.with(|c| c.borrow_mut().insert(*id, *who));
                true
            },
            NativeOrAssetId::Native => false,
        }
    }
}

/// Mock reserved range: every `WithId(id)` with `id >= RESERVED_ASSET_BASE`.
pub struct ReservedAssets;
impl frame_support::traits::Contains<NativeOrAssetId> for ReservedAssets {
    fn contains(a: &NativeOrAssetId) -> bool {
        matches!(a, NativeOrAssetId::WithId(id) if *id >= RESERVED_ASSET_BASE)
    }
}

impl Config for Test {
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
    type TreasurySink = MockSink;
    type WeightInfo = ();
    #[cfg(feature = "runtime-benchmarks")]
    type BenchmarkHelper = MockBenchHelper;
}

pub(crate) fn new_test_ext() -> sp_io::TestExternalities {
    // Tests share a thread under `--test-threads=1`.
    ExistentialDeposit::set(1);
    AssetDeposit::set(0);
    AssetAccountDeposit::set(0);

    let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

    pallet_balances::GenesisConfig::<Test> {
        dev_accounts: None,
        // Enough for every flow here without hitting ED or balance limits.
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
    ext.execute_with(|| {
        System::set_block_number(1);
        SINK_VAULT.with(|v| *v.borrow_mut() = None);
        SINK_NOTED.with(|n| n.borrow_mut().clear());
    });
    ext
}

/// Externalities shaped like the runtime the DEX runs on: its ED and asset
/// deposits, every asset non-sufficient except `SUFFICIENT_ID`, and the
/// treasury holding exactly its ED (see [`crate::testing`]). The DEX's
/// genesis build does not run, as on a chain that received the pallet by
/// upgrade; that chain then runs this version's upgrade, so its migrations
/// have run. `new_runtime_like_ext_before_upgrade` is the same chain before
/// it, and `new_runtime_like_ext_from_genesis` one built with the pallet.
pub(crate) fn new_runtime_like_ext() -> sp_io::TestExternalities {
    build_runtime_like_ext(false, true)
}

pub(crate) fn new_runtime_like_ext_before_upgrade() -> sp_io::TestExternalities {
    build_runtime_like_ext(false, false)
}

pub(crate) fn new_runtime_like_ext_from_genesis() -> sp_io::TestExternalities {
    build_runtime_like_ext(true, false)
}

fn build_runtime_like_ext(dex_genesis: bool, upgraded: bool) -> sp_io::TestExternalities {
    ExistentialDeposit::set(RUNTIME_ED);
    AssetDeposit::set(RUNTIME_ASSET_DEPOSIT);
    AssetAccountDeposit::set(RUNTIME_ASSET_ACCOUNT_DEPOSIT);

    let mut t = runtime_like_storage::<Test>();
    if dex_genesis {
        pallet_vitreus_dex::GenesisConfig::<Test>::default()
            .assimilate_storage(&mut t)
            .unwrap();
    }

    let mut ext = sp_io::TestExternalities::new(t);
    ext.execute_with(|| {
        System::set_block_number(1);
        SINK_VAULT.with(|v| *v.borrow_mut() = None);
        SINK_NOTED.with(|n| n.borrow_mut().clear());
        if upgraded {
            use frame_support::traits::UncheckedOnRuntimeUpgrade;
            crate::migrations::v6::VersionUncheckedMigrateToV6::<Test, UpgradeSource>::on_runtime_upgrade();
            System::reset_events();
        }
    });
    ext
}
