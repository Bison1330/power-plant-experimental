//! Storage migrations.

use super::*;
use frame_support::{
    migrations::VersionedMigration, pallet_prelude::*, traits::UncheckedOnRuntimeUpgrade,
};
use sp_std::marker::PhantomData;

/// v1: endow the pallet's escrows with the native ED.
pub mod v1 {
    use super::*;

    /// Unversioned body; wrap in [`MigrateToV1`].
    ///
    /// Genesis does this for a chain built with the pallet; a chain that
    /// receives the pallet by upgrade runs no genesis, so without this its
    /// escrows would start with nothing. Each ED is paid by `Source` (the
    /// runtime binds its Treasury), keeping its own ED, so issuance does not
    /// change. A source that cannot pay is logged, and `post_upgrade` then
    /// fails try-runtime: fund the source before the upgrade.
    pub struct VersionUncheckedMigrateToV1<T, Source>(PhantomData<(T, Source)>);

    impl<T: Config, Source: Get<T::AccountId>> UncheckedOnRuntimeUpgrade
        for VersionUncheckedMigrateToV1<T, Source>
    {
        fn on_runtime_upgrade() -> Weight {
            let endowed = Pallet::<T>::endow_escrows(Some(&Source::get()));
            log::info!(
                target: "runtime::solver-marketplace",
                "v1 migration: endowed {endowed} escrow(s) with the ED"
            );
            // Per escrow: its balance, and on a transfer it and the source.
            T::DbWeight::get().reads_writes(4, 2u64.saturating_mul(endowed.into()))
        }

        #[cfg(feature = "try-runtime")]
        fn post_upgrade(_: sp_std::vec::Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
            let native = T::NativeAsset::get();
            let ed = T::Assets::minimum_balance(native.clone());
            for escrow in
                [Pallet::<T>::intent_escrow_account(), Pallet::<T>::protocol_treasury_account()]
            {
                ensure!(
                    T::Assets::balance(native.clone(), &escrow) >= ed,
                    "an escrow holds less than the ED"
                );
            }
            Ok(())
        }
    }

    /// Escrow endowment, gated on the pallet's on-chain storage version
    /// (0 on a chain that has just received the pallet).
    pub type MigrateToV1<T, Source> = VersionedMigration<
        0,
        1,
        VersionUncheckedMigrateToV1<T, Source>,
        Pallet<T>,
        <T as frame_system::Config>::DbWeight,
    >;
}
