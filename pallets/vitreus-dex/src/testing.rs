//! The runtime-like test world, shared by this pallet's tests and by any
//! pallet whose mock runs on top of the DEX (the solver marketplace's), so
//! both judge the same chain: the runtime's existential deposit and asset
//! deposits, every asset non-sufficient but one, twenty-byte accounts.
//!
//! Each mock still owns its `parameter_types! pub static` values; it sets
//! them from the constants here before building storage with
//! [`runtime_like_storage`].

use frame_support::traits::fungibles::Inspect;
use sp_runtime::BuildStorage;

/// Twenty-byte account ids, as on the runtime (`AccountId20`): sub-accounts
/// derived from a pallet id truncate exactly as they do there.
pub type AccountId = sp_core::H160;
/// The account whose twenty bytes are all `b`.
pub const fn acc(b: u8) -> AccountId {
    sp_core::H160([b; 20])
}

/// Actors.
pub const ALICE: AccountId = acc(1);
#[allow(missing_docs)]
pub const BOB: AccountId = acc(2);
#[allow(missing_docs)]
pub const CHARLIE: AccountId = acc(3);
/// Neither a user nor a solver in the runtime-like tests.
pub const DAVE: AccountId = acc(4);
/// Account that receives pre-seed sweeps (stands in for the treasury).
pub const TREASURY: AccountId = acc(99);
/// The creator on record for [`LAUNCH_ID`].
pub const CREATOR: AccountId = acc(77);
/// The launch asset's treasury vault, when a test installs one.
pub const VAULT: AccountId = acc(88);

frame_support::parameter_types! {
    /// Who pays the endowments an upgrade's migrations make, as the
    /// runtime's Treasury does there: DAVE, who holds plenty.
    pub const UpgradeSource: AccountId = DAVE;
}

/// Asset ids.
pub const USDC_ID: u32 = 1;
#[allow(missing_docs)]
pub const VNRG_ID: u32 = 2;
/// A sufficient asset; on the runtime no asset is, so only tests about the
/// sufficient case use it.
pub const SUFFICIENT_ID: u32 = 3;
/// Asset ids at or above this are "reserved" (the launchpad's range).
pub const RESERVED_ASSET_BASE: u32 = 1_000;
/// The reserved asset with a creator on record.
pub const LAUNCH_ID: u32 = RESERVED_ASSET_BASE + 1;

/// 10^18 sub-units per whole unit, VTRS and tokens alike.
pub const UNIT: u128 = 1_000_000_000_000_000_000;
/// The runtime's existential deposit (100 µVTRS).
pub const RUNTIME_ED: u128 = 100_000_000_000_000;
/// The runtime's `AssetDeposit` and `AssetAccountDeposit`.
pub const RUNTIME_ASSET_DEPOSIT: u128 = 100;
#[allow(missing_docs)]
pub const RUNTIME_ASSET_ACCOUNT_DEPOSIT: u128 = 10;
/// The runtime's default solver bond.
pub const RUNTIME_SOLVER_BOND: u128 = 1_000 * UNIT;
/// Native and token balance of each actor in the runtime-like tests.
pub const RUNTIME_LIKE_BALANCE: u128 = 1_000_000 * UNIT;

/// Storage for the runtime-like world: ALICE..DAVE each hold
/// [`RUNTIME_LIKE_BALANCE`] of native and of USDC, VNRG and the sufficient
/// asset; the treasury holds exactly its ED. The caller's mock must have
/// set its ED to [`RUNTIME_ED`] first.
pub fn runtime_like_storage<T>() -> sp_runtime::Storage
where
    T: pallet_balances::Config<Balance = u128>
        + pallet_assets::Config<AssetId = u32, Balance = u128>
        + frame_system::Config<AccountId = AccountId>,
{
    let mut t = frame_system::GenesisConfig::<T>::default().build_storage().unwrap();
    let actors = [ALICE, BOB, CHARLIE, DAVE];
    pallet_balances::GenesisConfig::<T> {
        dev_accounts: None,
        balances: actors
            .iter()
            .map(|a| (*a, RUNTIME_LIKE_BALANCE))
            .chain(core::iter::once((TREASURY, RUNTIME_ED)))
            .collect(),
    }
    .assimilate_storage(&mut t)
    .unwrap();
    pallet_assets::GenesisConfig::<T> {
        assets: vec![
            (USDC_ID, ALICE, false, 1),
            (VNRG_ID, ALICE, false, 1),
            (SUFFICIENT_ID, ALICE, true, 1),
        ],
        accounts: [USDC_ID, VNRG_ID, SUFFICIENT_ID]
            .iter()
            .flat_map(|id| actors.iter().map(move |a| (*id, *a, RUNTIME_LIKE_BALANCE)))
            .collect(),
        ..Default::default()
    }
    .assimilate_storage(&mut t)
    .unwrap();
    t
}

/// The first asset id at or above `from` and below the reserved range that
/// no asset has.
pub fn next_free_asset_id<T>(from: u32) -> u32
where
    T: pallet_assets::Config<AssetId = u32>,
{
    (from..RESERVED_ASSET_BASE)
        .find(|id| !pallet_assets::Pallet::<T>::asset_exists(*id))
        .expect("free asset id below the reserved range")
}

/// The base pallet as a runtime wires it under the DEX, for a mock built on
/// the DEX: pallet-asset-conversion with its `AdminOrigin` the DEX's own
/// account, the LP tokens in pallet-assets `Instance2` with no deposits and
/// no origin for its calls, and pallet-assets-freezer on them. The mock's
/// `construct_runtime!` must name `AssetConversion`, `PoolAssets` (as
/// `pallet_assets::<Instance2>`) and `PoolAssetsFreezer`, and its DEX
/// `Config` set `type Base = pallet_vitreus_dex::AssetConversionAdapter<Self>`.
///
/// Asset kinds are `NativeOrWithId<$asset_id>`; `$native` is a `Get` of the
/// native one, `$assets` the DEX's `Config::Assets`.
#[macro_export]
macro_rules! impl_base_pallet_for_mock {
    ($runtime:ty, $account:ty, $asset_id:ty, $assets:ty, $native:ty) => {
        $crate::impl_base_pallet_for_mock!(
            @inner $runtime, $account, $asset_id,
            frame_support::traits::fungible::NativeOrWithId<$asset_id>, $assets, $native
        );
    };
    (@inner $runtime:ty, $account:ty, $asset_id:ty, $asset_kind:ty, $assets:ty, $native:ty) => {
        impl pallet_assets::Config<frame_support::instances::Instance2> for $runtime {
            type RuntimeEvent = RuntimeEvent;
            type Holder = ();
            type ReserveData = ();
            type Balance = u128;
            type RemoveItemsLimit = frame_support::traits::ConstU32<1000>;
            type AssetId = u32;
            type AssetIdParameter = u32;
            type Currency = Balances;
            type CreateOrigin = frame_support::traits::AsEnsureOriginWithArg<
                frame_support::traits::NeverEnsureOrigin<$account>,
            >;
            type ForceOrigin = frame_system::EnsureRoot<$account>;
            type AssetDeposit = frame_support::traits::ConstU128<0>;
            type AssetAccountDeposit = frame_support::traits::ConstU128<0>;
            type MetadataDepositBase = frame_support::traits::ConstU128<0>;
            type MetadataDepositPerByte = frame_support::traits::ConstU128<0>;
            type ApprovalDeposit = frame_support::traits::ConstU128<0>;
            type StringLimit = frame_support::traits::ConstU32<50>;
            type Freezer = PoolAssetsFreezer;
            type Extra = ();
            type WeightInfo = ();
            type CallbackHandle = ();
            pallet_assets::runtime_benchmarks_enabled! {
                type BenchmarkHelper = ();
            }
        }

        impl pallet_assets_freezer::Config<frame_support::instances::Instance2> for $runtime {
            type RuntimeFreezeReason = RuntimeFreezeReason;
            type RuntimeEvent = RuntimeEvent;
        }

        frame_support::parameter_types! {
            pub const AssetConversionPalletId: frame_support::PalletId =
                frame_support::PalletId(*b"vtrs/amm");
            pub const BaseLpFee: sp_runtime::Permill = sp_runtime::Permill::from_parts(3_000);
            pub const MaxSwapFee: sp_runtime::Permill = sp_runtime::Permill::from_percent(1);
            pub const LiquidityWithdrawalFee: sp_runtime::Permill = sp_runtime::Permill::zero();
        }

        pub type PoolIdToAccountId = pallet_asset_conversion::AccountIdConverter<
            AssetConversionPalletId,
            ($asset_kind, $asset_kind),
        >;
        pub type PoolLocator = pallet_asset_conversion::Chain<
            pallet_asset_conversion::WithFirstAsset<
                $native,
                $account,
                $asset_kind,
                PoolIdToAccountId,
            >,
            pallet_asset_conversion::Ascending<$account, $asset_kind, PoolIdToAccountId>,
        >;

        impl pallet_asset_conversion::Config for $runtime {
            type RuntimeEvent = RuntimeEvent;
            type Balance = u128;
            type HigherPrecisionBalance = sp_core::U256;
            type AssetKind = $asset_kind;
            type Assets = $assets;
            type PoolId = ($asset_kind, $asset_kind);
            type PoolLocator = PoolLocator;
            type PoolAssetId = u32;
            type PoolAssets = PoolAssets;
            type LPFee = BaseLpFee;
            type AdminOrigin = $crate::EnsureDexAccount<$runtime>;
            type MaxSwapFee = MaxSwapFee;
            type PoolSetupFee = frame_support::traits::ConstU128<0>;
            type PoolSetupFeeAsset = $native;
            type PoolSetupFeeTarget = ();
            type LiquidityWithdrawalFee = LiquidityWithdrawalFee;
            type MintMinLiquidity =
                frame_support::traits::ConstU128<{ $crate::MINIMUM_LIQUIDITY as u128 }>;
            type MaxSwapPathLength = frame_support::traits::ConstU32<2>;
            type PalletId = AssetConversionPalletId;
            type WeightInfo = ();
            #[cfg(feature = "runtime-benchmarks")]
            type BenchmarkHelper = pallet_asset_conversion::NativeOrWithIdFactory<$asset_id>;
        }
    };
}
