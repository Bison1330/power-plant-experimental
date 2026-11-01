//! Storage migrations.

use super::*;
use frame_support::{pallet_prelude::*, traits::OnRuntimeUpgrade};
use sp_std::marker::PhantomData;

/// Endows the pallet's escrows with the native ED.
///
/// Genesis does this for a chain built with the pallet; a chain that
/// receives the pallet by upgrade runs no genesis, so without this its
/// escrows would start with nothing. Each ED is paid by `Source` (the
/// runtime binds its Treasury), keeping its own ED, so issuance does not
/// change. A source that cannot pay is logged, and `post_upgrade` then
/// fails try-runtime: fund the source before the upgrade.
///
/// Not versioned, and must not be: when an upgrade adds the pallet, FRAME
/// sets its on-chain storage version to the in-code one before any
/// migration runs, so a version gate would always skip this. What makes a
/// second run harmless is that an escrow already holding the ED is skipped.
pub struct EndowEscrows<T, Source>(PhantomData<(T, Source)>);

impl<T: Config, Source: Get<T::AccountId>> OnRuntimeUpgrade for EndowEscrows<T, Source> {
    fn on_runtime_upgrade() -> Weight {
        let endowed = Pallet::<T>::endow_escrows(Some(&Source::get()));
        log::info!(
            target: "runtime::solver-marketplace",
            "EndowEscrows: endowed {endowed} escrow(s) with the ED"
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
