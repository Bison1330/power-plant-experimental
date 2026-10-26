//! The base pallet (pallet-asset-conversion) as this pallet sees it.
//!
//! The DEX keeps pools, pricing, liquidity and LP tokens in the base pallet,
//! and its own fee routing, seeding, locks and dormancy clock on top. It
//! reaches the base through [`AmmBase`], bound in its `Config` as
//! `type Base`, rather than by extending the base pallet's `Config`: the two
//! share item names (`Balance`, `AssetKind`, `Assets`, `WeightInfo`,
//! `BenchmarkHelper`), which would be ambiguous in this pallet and in every
//! pallet built on it. A runtime binds it to [`AssetConversionAdapter`].

use frame_support::traits::Get;
use frame_support::traits::{
    fungibles::{Balanced, Credit, Inspect, MutateFreeze, Refund},
    AccountTouch,
};
use pallet_asset_conversion::{MutateLiquidity, PoolLocator, Pools, SwapCredit};
use sp_runtime::{traits::AccountIdConversion, DispatchError, DispatchResult, Permill};
use sp_std::{boxed::Box, vec};

/// A credit swap's outcome: the credit out, or the credit in back with why.
pub type CreditSwap<AccountId, Assets> =
    Result<Credit<AccountId, Assets>, (Credit<AccountId, Assets>, DispatchError)>;

/// What the DEX needs of the base pallet. Pairs are unordered: every method
/// takes the two assets in the caller's order and answers in that order.
pub trait AmmBase<AccountId, AssetKind, Balance>
where
    AssetKind: frame_support::traits::tokens::AssetId,
    Balance: frame_support::traits::tokens::Balance,
{
    /// The registry the base pallet's pools hold; the DEX's `Config::Assets`.
    type Assets: Inspect<AccountId, AssetId = AssetKind, Balance = Balance> + Balanced<AccountId>;

    /// What a freeze on LP tokens is named by: the runtime's freeze reason.
    type FreezeId;

    /// Create the pool with LP fee `fee`. `creator` pays the deposits that
    /// open the pool account's token accounts. Nothing else can set or change
    /// a pool's fee (the base pallet's `AdminOrigin`).
    fn create_pool(
        creator: &AccountId,
        asset_a: AssetKind,
        asset_b: AssetKind,
        fee: Permill,
    ) -> DispatchResult;

    /// Whether the base pallet has a pool for the pair.
    fn pool_exists(asset_a: &AssetKind, asset_b: &AssetKind) -> bool;

    /// The pool's LP fee, if the pool exists.
    fn pool_fee(asset_a: &AssetKind, asset_b: &AssetKind) -> Option<Permill>;

    /// The account that holds the pair's reserves, whether or not the pool
    /// exists yet.
    fn pool_account(asset_a: &AssetKind, asset_b: &AssetKind) -> Option<AccountId>;

    /// The pool's reserves, in the order asked; `None` if there is no pool or
    /// one side is empty.
    fn reserves(asset_a: &AssetKind, asset_b: &AssetKind) -> Option<(Balance, Balance)>;

    /// `who`'s LP tokens in the pool; zero if there is no pool.
    fn lp_balance(asset_a: &AssetKind, asset_b: &AssetKind, who: &AccountId) -> Balance;

    /// Every LP token of the pool, the pool account's locked minimum included.
    fn lp_issuance(asset_a: &AssetKind, asset_b: &AssetKind) -> Balance;

    /// The LP tokens a first deposit locks in the pool account, and what
    /// every deposit must mint more than.
    fn min_liquidity() -> Balance;

    /// Open `who`'s LP-token account if it has none: deposit-backed, which
    /// takes no consumer reference. The base pallet mints into an unopened
    /// account on a consumer reference, so the DEX opens it before every add.
    fn open_lp_account(asset_a: &AssetKind, asset_b: &AssetKind, who: &AccountId)
        -> DispatchResult;

    /// Deposit up to the desired amounts from `who` at the pool's ratio, and
    /// mint the LP tokens to `who`. Returns `(amount_a, amount_b, lp)`.
    fn add_liquidity(
        who: &AccountId,
        asset_a: AssetKind,
        asset_b: AssetKind,
        amount_a_desired: Balance,
        amount_b_desired: Balance,
        amount_a_min: Balance,
        amount_b_min: Balance,
    ) -> Result<(Balance, Balance, Balance), DispatchError>;

    /// Burn `lp` of `who`'s LP tokens (not frozen ones) and pay their share
    /// of the reserves to `who`. Returns `(amount_a, amount_b)`.
    fn remove_liquidity(
        who: &AccountId,
        asset_a: AssetKind,
        asset_b: AssetKind,
        lp: Balance,
        amount_a_min: Balance,
        amount_b_min: Balance,
    ) -> Result<(Balance, Balance), DispatchError>;

    /// Swap all of `credit_in` for `asset_out` at the pool's fee. On failure
    /// the credit comes back with the error.
    fn swap_credit(
        asset_out: AssetKind,
        credit_in: Credit<AccountId, Self::Assets>,
    ) -> CreditSwap<AccountId, Self::Assets>;

    /// How much of `who`'s LP tokens in the pool the freeze `id` holds.
    fn lp_frozen(
        asset_a: &AssetKind,
        asset_b: &AssetKind,
        who: &AccountId,
        id: &Self::FreezeId,
    ) -> Balance;

    /// Set the freeze `id` on `who`'s LP tokens in the pool to `amount`.
    /// Frozen LP tokens cannot be burned, so they cannot be withdrawn; the
    /// LP instance has no calls, so nothing but this pallet sets or lifts a
    /// freeze.
    fn set_lp_freeze(
        asset_a: &AssetKind,
        asset_b: &AssetKind,
        who: &AccountId,
        id: &Self::FreezeId,
        amount: Balance,
    ) -> DispatchResult;

    /// Lift the freeze `id` from `who`'s LP tokens in the pool.
    fn thaw_lp(
        asset_a: &AssetKind,
        asset_b: &AssetKind,
        who: &AccountId,
        id: &Self::FreezeId,
    ) -> DispatchResult;

    /// What swapping `amount_in` of `asset_in` for `asset_out` pays at the
    /// pool's fee, from the live reserves.
    fn amount_out(
        asset_in: &AssetKind,
        asset_out: &AssetKind,
        amount_in: Balance,
    ) -> Option<Balance>;
}

/// [`AmmBase`] on pallet-asset-conversion in runtime `R`, with `F` freezing
/// its LP tokens (pallet-assets-freezer on the LP instance). Signs as the
/// DEX's own account where the base pallet asks for its `AdminOrigin`, which
/// the runtime binds to [`crate::EnsureDexAccount`].
pub struct AssetConversionAdapter<R, F>(core::marker::PhantomData<(R, F)>);

impl<R, F> AssetConversionAdapter<R, F>
where
    R: pallet_asset_conversion::Config,
{
    fn lp_token(asset_a: &R::AssetKind, asset_b: &R::AssetKind) -> Option<R::PoolAssetId> {
        let pool_id = R::PoolLocator::pool_id(asset_a, asset_b).ok()?;
        Pools::<R>::get(pool_id).map(|p| p.lp_token)
    }
}

impl<R, F> AmmBase<R::AccountId, R::AssetKind, R::Balance> for AssetConversionAdapter<R, F>
where
    R: pallet_asset_conversion::Config,
    F: MutateFreeze<R::AccountId, AssetId = R::PoolAssetId, Balance = R::Balance>,
{
    type Assets = R::Assets;
    type FreezeId = F::Id;

    fn lp_frozen(
        asset_a: &R::AssetKind,
        asset_b: &R::AssetKind,
        who: &R::AccountId,
        id: &F::Id,
    ) -> R::Balance {
        Self::lp_token(asset_a, asset_b)
            .map(|lp| F::balance_frozen(lp, id, who))
            .unwrap_or_default()
    }

    fn set_lp_freeze(
        asset_a: &R::AssetKind,
        asset_b: &R::AssetKind,
        who: &R::AccountId,
        id: &F::Id,
        amount: R::Balance,
    ) -> DispatchResult {
        let lp = Self::lp_token(asset_a, asset_b)
            .ok_or(pallet_asset_conversion::Error::<R>::PoolNotFound)?;
        F::set_freeze(lp, id, who, amount)
    }

    fn thaw_lp(
        asset_a: &R::AssetKind,
        asset_b: &R::AssetKind,
        who: &R::AccountId,
        id: &F::Id,
    ) -> DispatchResult {
        let lp = Self::lp_token(asset_a, asset_b)
            .ok_or(pallet_asset_conversion::Error::<R>::PoolNotFound)?;
        F::thaw(lp, id, who)
    }

    fn create_pool(
        creator: &R::AccountId,
        asset_a: R::AssetKind,
        asset_b: R::AssetKind,
        fee: Permill,
    ) -> DispatchResult {
        let dex: R::AccountId = crate::PALLET_ID.into_account_truncating();
        pallet_asset_conversion::Pallet::<R>::create_pool_with_fee(
            frame_system::RawOrigin::Signed(dex).into(),
            creator.clone(),
            Box::new(asset_a),
            Box::new(asset_b),
            fee,
        )
    }

    fn pool_exists(asset_a: &R::AssetKind, asset_b: &R::AssetKind) -> bool {
        Self::lp_token(asset_a, asset_b).is_some()
    }

    fn pool_fee(asset_a: &R::AssetKind, asset_b: &R::AssetKind) -> Option<Permill> {
        let pool_id = R::PoolLocator::pool_id(asset_a, asset_b).ok()?;
        Pools::<R>::contains_key(&pool_id)
            .then(|| pallet_asset_conversion::Pallet::<R>::pool_fee(&pool_id))
    }

    fn pool_account(asset_a: &R::AssetKind, asset_b: &R::AssetKind) -> Option<R::AccountId> {
        R::PoolLocator::pool_address(asset_a, asset_b).ok()
    }

    fn reserves(
        asset_a: &R::AssetKind,
        asset_b: &R::AssetKind,
    ) -> Option<(R::Balance, R::Balance)> {
        Self::lp_token(asset_a, asset_b)?;
        pallet_asset_conversion::Pallet::<R>::get_reserves(asset_a.clone(), asset_b.clone()).ok()
    }

    fn lp_balance(
        asset_a: &R::AssetKind,
        asset_b: &R::AssetKind,
        who: &R::AccountId,
    ) -> R::Balance {
        Self::lp_token(asset_a, asset_b)
            .map(|lp| R::PoolAssets::balance(lp, who))
            .unwrap_or_default()
    }

    fn lp_issuance(asset_a: &R::AssetKind, asset_b: &R::AssetKind) -> R::Balance {
        Self::lp_token(asset_a, asset_b)
            .map(R::PoolAssets::total_issuance)
            .unwrap_or_default()
    }

    fn min_liquidity() -> R::Balance {
        R::MintMinLiquidity::get()
    }

    fn open_lp_account(
        asset_a: &R::AssetKind,
        asset_b: &R::AssetKind,
        who: &R::AccountId,
    ) -> DispatchResult {
        let lp = Self::lp_token(asset_a, asset_b)
            .ok_or(pallet_asset_conversion::Error::<R>::PoolNotFound)?;
        // Not `should_touch`: that says no while `who` has room for a
        // consumer reference, and the mint would then take one. An account
        // this opened holds a (zero) deposit; one holding tokens without one
        // was opened by a mint before, and is open.
        let open = R::PoolAssets::deposit_held(lp.clone(), who.clone()).is_some()
            || R::PoolAssets::balance(lp.clone(), who) != Default::default();
        if !open {
            R::PoolAssets::touch(lp, who, who)?;
        }
        Ok(())
    }

    fn add_liquidity(
        who: &R::AccountId,
        asset_a: R::AssetKind,
        asset_b: R::AssetKind,
        amount_a_desired: R::Balance,
        amount_b_desired: R::Balance,
        amount_a_min: R::Balance,
        amount_b_min: R::Balance,
    ) -> Result<(R::Balance, R::Balance, R::Balance), DispatchError> {
        let pool = Self::pool_account(&asset_a, &asset_b)
            .ok_or(pallet_asset_conversion::Error::<R>::InvalidAssetPair)?;
        let before_a = R::Assets::balance(asset_a.clone(), &pool);
        let before_b = R::Assets::balance(asset_b.clone(), &pool);
        let lp =
            <pallet_asset_conversion::Pallet<R> as MutateLiquidity<R::AccountId>>::add_liquidity(
                who,
                pallet_asset_conversion::AddLiquidityAsset {
                    asset: asset_a.clone(),
                    amount_desired: amount_a_desired,
                    amount_min: amount_a_min,
                },
                pallet_asset_conversion::AddLiquidityAsset {
                    asset: asset_b.clone(),
                    amount_desired: amount_b_desired,
                    amount_min: amount_b_min,
                },
                who,
            )?;
        let added_a = R::Assets::balance(asset_a, &pool) - before_a;
        let added_b = R::Assets::balance(asset_b, &pool) - before_b;
        Ok((added_a, added_b, lp))
    }

    fn remove_liquidity(
        who: &R::AccountId,
        asset_a: R::AssetKind,
        asset_b: R::AssetKind,
        lp: R::Balance,
        amount_a_min: R::Balance,
        amount_b_min: R::Balance,
    ) -> Result<(R::Balance, R::Balance), DispatchError> {
        <pallet_asset_conversion::Pallet<R> as MutateLiquidity<R::AccountId>>::remove_liquidity(
            who,
            asset_a,
            asset_b,
            lp,
            amount_a_min,
            amount_b_min,
            who,
        )
    }

    fn swap_credit(
        asset_out: R::AssetKind,
        credit_in: Credit<R::AccountId, R::Assets>,
    ) -> CreditSwap<R::AccountId, R::Assets> {
        let path = vec![credit_in.asset(), asset_out];
        <pallet_asset_conversion::Pallet<R> as SwapCredit<R::AccountId>>::swap_exact_tokens_for_tokens(
            path, credit_in, None,
        )
    }

    fn amount_out(
        asset_in: &R::AssetKind,
        asset_out: &R::AssetKind,
        amount_in: R::Balance,
    ) -> Option<R::Balance> {
        let fee = Self::pool_fee(asset_in, asset_out)?;
        let (reserve_in, reserve_out) = Self::reserves(asset_in, asset_out)?;
        pallet_asset_conversion::Pallet::<R>::get_amount_out(
            fee,
            &amount_in,
            &reserve_in,
            &reserve_out,
        )
        .ok()
    }
}
