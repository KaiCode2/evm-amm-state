//! Balancer weighted pool wrapper implementing the AutomatedMarketMaker trait.

use std::collections::HashMap;

use super::balancer_math::{WeightedPool, WeightedPoolError, u256_to_f64_lossy};
use super::data::PoolParams;

use alloy_eips::BlockId;
use alloy_network::Network;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{MulticallError, Provider};

use alloy_rpc_types_eth::Log;
use alloy_sol_types::sol;

use amms::amms::{amm::AutomatedMarketMaker, balancer::BalancerError, error::AMMError};
use serde::{Deserialize, Serialize};

sol! {
    #[sol(rpc)]
    contract IERC20 {
        function decimals() external view returns (uint8);
    }
}

sol! {
    #[sol(rpc)]
    contract IBalancerPool {
        function getNormalizedWeights() external view returns (uint256[] memory);
        function getSwapFeePercentage() external view returns (uint256);
    }
}

sol! {
    #[sol(rpc)]
    contract IBalancerVault {
        function getPoolTokens(bytes32 poolId)
            external
            view
            returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock);
    }
}

/// Lossy conversion from f64 "real units" back to on-chain U256.
pub fn f64_to_u256_lossy(value: f64, decimals: u8) -> U256 {
    if value <= 0.0 {
        return U256::ZERO;
    }
    let scale = 10f64.powi(decimals as i32);
    let scaled = (value * scale).floor();
    U256::from(scaled as u128)
}

/// A Balancer-weighted pool wrapper that uses `WeightedPool`
/// math but implements the `AutomatedMarketMaker` trait.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalancerPool {
    /// Address of the Balancer vault
    pub vault: Address,
    /// Optional Balancer Vault poolId if you want to keep it around
    pub pool_id: B256,
    /// Core math / balances / weights in real units
    inner: WeightedPool,
    /// Token -> decimals map used for U256 <-> f64 conversion
    decimals: HashMap<Address, u8>,
}

impl BalancerPool {
    /// Constructor from pre-computed weights.
    pub fn from_weights(
        pool_id: B256,
        vault: Address,
        inner: WeightedPool,
        decimals: HashMap<Address, u8>,
    ) -> Self {
        Self {
            vault,
            pool_id,
            inner,
            decimals,
        }
    }

    /// Minimal constructor: just the pool ID and vault address.
    /// `init` will later populate `inner` and `decimals`.
    pub fn new(pool_id: B256, vault: Address) -> Self {
        Self {
            vault,
            pool_id,
            inner: WeightedPool {
                tokens: Vec::new(),
                balances: Vec::new(),
                weights: Vec::new(),
                swap_fee: 0.0,
            },
            decimals: HashMap::new(),
        }
    }

    /// Rebuild the inner WeightedPool with fresh balances from PoolParams.
    pub fn refresh_from_params(&mut self, params: &PoolParams, decimals: &HashMap<Address, u8>) {
        self.inner = WeightedPool::from_params(params, decimals);
    }

    /// Apply a Balancer V2 vault `Swap` event in place.
    ///
    /// The vault emits `Swap(poolId, tokenIn, tokenOut, amountIn, amountOut)`,
    /// which is enough to update the affected pool's balances exactly without an
    /// RPC round-trip: `balance[tokenIn] += amountIn` and
    /// `balance[tokenOut] -= amountOut`. Amounts are raw on-chain values and are
    /// converted to the pool's internal real-unit representation using the
    /// per-token decimals captured during initialization. Tokens not held by
    /// this pool are ignored. Returns `true` if any balance changed.
    pub fn apply_vault_swap(
        &mut self,
        token_in: Address,
        amount_in: U256,
        token_out: Address,
        amount_out: U256,
    ) -> bool {
        let mut changed = false;
        if let Some(idx) = self.inner.tokens.iter().position(|t| *t == token_in) {
            let delta = u256_to_f64_lossy(amount_in, self.token_decimals(token_in));
            self.inner.balances[idx] += delta;
            changed = true;
        }
        if let Some(idx) = self.inner.tokens.iter().position(|t| *t == token_out) {
            let delta = u256_to_f64_lossy(amount_out, self.token_decimals(token_out));
            self.inner.balances[idx] = (self.inner.balances[idx] - delta).max(0.0);
            changed = true;
        }
        changed
    }

    /// Return current balances as (token, U256) pairs for on-chain comparison.
    pub fn balances_u256(&self) -> Vec<(Address, U256)> {
        self.inner
            .tokens
            .iter()
            .zip(self.inner.balances.iter())
            .map(|(&token, &balance)| {
                let decimals = self.decimals.get(&token).copied().unwrap_or(18);
                (token, f64_to_u256_lossy(balance, decimals))
            })
            .collect()
    }

    /// Helper: get decimals for a token (default 18 if missing).
    fn token_decimals(&self, token: Address) -> u8 {
        self.decimals.get(&token).copied().unwrap_or(18)
    }

    pub fn address(pool_id: B256) -> Address {
        Address::from_slice(&pool_id[0..20])
    }

    pub async fn get_pool_params<P, N>(
        provider: &P,
        vault: Address,
        pool_id: B256,
    ) -> Result<(WeightedPool, HashMap<Address, u8>), AMMError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let pool_addr = Self::address(pool_id);
        let pool = IBalancerPool::IBalancerPoolInstance::new(pool_addr, provider);
        let vault = IBalancerVault::IBalancerVaultInstance::new(vault, provider);

        let multicall = &provider
            .multicall()
            .add(vault.getPoolTokens(pool_id))
            .add(pool.getNormalizedWeights())
            .add(pool.getSwapFeePercentage());
        let (tokens, normalized_weights, swap_fee) =
            multicall.aggregate().await.map_err(map_multicall_error)?;

        let params = PoolParams::new_from_parts(
            tokens.tokens,
            tokens.balances,
            normalized_weights,
            swap_fee,
        );

        // Fetch decimals for all tokens in this pool
        let mut dynamic_multicall = provider.multicall().dynamic();
        for token in params.tokens() {
            dynamic_multicall =
                dynamic_multicall.add_dynamic(IERC20::new(token, &provider).decimals());
        }
        let decimals = dynamic_multicall
            .aggregate()
            .await
            .map_err(map_multicall_error)?
            .into_iter()
            .zip(params.tokens())
            .map(|(decimals, token)| (token, decimals))
            .collect::<HashMap<Address, u8>>();

        Ok((WeightedPool::from_params(&params, &decimals), decimals))
    }
}

#[allow(async_fn_in_trait)]
impl AutomatedMarketMaker for BalancerPool {
    fn address(&self) -> Address {
        Self::address(self.pool_id)
    }

    fn sync_events(&self) -> Vec<B256> {
        Vec::new()
    }

    fn sync(&mut self, _log: &Log) -> Result<(), AMMError> {
        Ok(())
    }

    fn tokens(&self) -> Vec<Address> {
        self.inner.tokens.clone()
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        if base_token == quote_token {
            return Ok(1.0);
        }

        let price = self
            .inner
            .spot_price(base_token, quote_token)
            .map_err(map_weighted_pool_error)?;

        Ok(price)
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }

        let dec_in = self.token_decimals(base_token);
        let dec_out = self.token_decimals(quote_token);

        let amount_in_f = u256_to_f64_lossy(amount_in, dec_in);

        if amount_in_f <= 0.0 {
            return Ok(U256::ZERO);
        }

        // non-mutating: clone the inner pool
        let mut tmp = self.inner.clone();

        let out_f = tmp
            .swap_out_given_in(base_token, quote_token, amount_in_f)
            .map_err(map_weighted_pool_error)?;

        let out_u = f64_to_u256_lossy(out_f, dec_out);
        Ok(out_u)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }

        let dec_in = self.token_decimals(base_token);
        let dec_out = self.token_decimals(quote_token);

        let amount_in_f = u256_to_f64_lossy(amount_in, dec_in);

        if amount_in_f <= 0.0 {
            return Ok(U256::ZERO);
        }

        let out_f = self
            .inner
            .swap_out_given_in(base_token, quote_token, amount_in_f)
            .map_err(map_weighted_pool_error)?;

        let out_u = f64_to_u256_lossy(out_f, dec_out);
        Ok(out_u)
    }

    async fn init<N2, P>(mut self, _block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        Self: Sized,
        N2: Network,
        P: Provider<N2> + Clone,
    {
        let (inner, decimals) = Self::get_pool_params(&provider, self.vault, self.pool_id).await?;
        self.inner = inner;
        self.decimals = decimals;
        Ok(self)
    }
}

fn map_multicall_error(e: MulticallError) -> AMMError {
    match e {
        MulticallError::TransportError(e) => AMMError::TransportError(e),
        MulticallError::DecodeError(e) => AMMError::SolTypesError(e),
        _ => AMMError::BalancerError(BalancerError::InitializationError),
    }
}

fn map_weighted_pool_error(e: WeightedPoolError) -> AMMError {
    match e {
        WeightedPoolError::TokenInDoesNotExist => {
            AMMError::from(BalancerError::TokenInDoesNotExist)
        }
        WeightedPoolError::TokenOutDoesNotExist => {
            AMMError::from(BalancerError::TokenOutDoesNotExist)
        }
    }
}
