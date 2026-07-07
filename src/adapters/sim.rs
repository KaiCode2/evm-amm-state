//! Swap simulation surface and quote-target configuration.
//!
//! [`AmmAdapter::simulate_swap`](super::AmmAdapter::simulate_swap) executes the
//! protocol's *canonical* on-chain quote entrypoint against the cold-start
//! snapshot via `AdapterCache::call_raw` and decodes the resulting `amount_out`
//! into a [`SwapQuote`] (or a [`SimError`] on revert). The deployed contract
//! bytecode performs the AMM math — this crate only builds calldata, runs it,
//! and decodes the output (no `amm-math` / `LocalAMM` / hand-rolled AMM math).
//!
//! - **Uniswap V3 (+ family):** `QuoterV2.quoteExactInputSingle(..)`. Target =
//!   the QuoterV2 contract (mainnet default, per-pool/chain override).
//! - **Uniswap V2:** `UniswapV2Router02.getAmountsOut(amountIn, path)`. Target =
//!   the router (mainnet default, override).
//! - **Balancer V2:** `Vault.queryBatchSwap(GIVEN_IN, swaps, assets, funds)`.
//!   Target = the pool's vault (from `BalancerV2Metadata.vault`).
//!
//! The quote contract's bytecode must be reachable: lazily fetched against a
//! live backend, or installed as a fixture for offline tests.

use alloy_primitives::{Address, Bytes, U256, address};

use super::{AdapterCache, CallOutcome};

/// A swap-simulation quote: the output amount the protocol's canonical quote
/// entrypoint returns for the requested input.
///
/// Intentionally a struct (not a bare `U256`) so future quote outputs (gas,
/// effective price, sqrt-price-after) can extend it without breaking callers.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapQuote {
    /// The token-out amount the quote returned for `amount_in`.
    pub amount_out: U256,
}

impl SwapQuote {
    /// Construct a quote from a decoded output amount.
    pub fn new(amount_out: U256) -> Self {
        Self { amount_out }
    }
}

/// Why a [`simulate_swap`](super::AmmAdapter::simulate_swap) could not produce a
/// quote.
///
/// Not `Clone`/`PartialEq` (the [`Execution`](Self::Execution) variant carries a
/// boxed source error that is neither), matching the crate's other
/// boxed-source facades ([`CacheError`](super::CacheError),
/// [`DriverError`](super::DriverError)). Match on the variant, or walk
/// [`source`](std::error::Error::source) for the underlying cause.
#[non_exhaustive]
#[derive(Debug)]
pub enum SimError {
    /// The adapter does not implement swap simulation for its protocol.
    Unsupported(super::ProtocolId),
    /// Required metadata (e.g. the Balancer vault, or a V3 fee) is missing.
    MissingMetadata(&'static str),
    /// The quote call reverted or halted in the EVM.
    Reverted,
    /// The quote call executed but its return data could not be decoded.
    MalformedOutput(&'static str),
    /// The underlying `call_raw` failed (host/transact error), carrying the
    /// un-flattened cause. Downcast the payload (or walk
    /// [`source`](std::error::Error::source)) — e.g. to
    /// [`CacheError`](super::CacheError) — for typed handling.
    Execution(Box<dyn std::error::Error + Send + Sync + 'static>),
    /// A catch-all for protocol-specific failures.
    Custom(String),
}

impl core::fmt::Display for SimError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unsupported(protocol) => {
                write!(f, "swap simulation unsupported for {protocol:?}")
            }
            Self::MissingMetadata(what) => write!(f, "missing metadata for swap sim: {what}"),
            Self::Reverted => write!(f, "quote call reverted or halted"),
            Self::MalformedOutput(what) => write!(f, "malformed quote output: {what}"),
            Self::Execution(err) => write!(f, "quote execution failed: {err}"),
            Self::Custom(err) => write!(f, "swap sim error: {err}"),
        }
    }
}

impl std::error::Error for SimError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Execution(err) => Some(&**err as &(dyn std::error::Error + 'static)),
            _ => None,
        }
    }
}

/// Resolved quote-target addresses for swap simulation.
///
/// Defaults to the canonical Ethereum-mainnet QuoterV2 and UniswapV2Router02.
/// Per-pool/chain overrides are applied with the `with_*` builders. The Balancer
/// vault is *not* configured here — it is the pool's own vault
/// (`BalancerV2Metadata.vault`), resolved per-pool at quote time.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimConfig {
    /// Uniswap V3 `QuoterV2` (and family) quote target.
    pub v3_quoter: Address,
    /// Uniswap V2 `UniswapV2Router02` quote target.
    pub v2_router: Address,
    /// The `msg.sender` (`from`) each quote call runs as. Defaults to
    /// [`Address::ZERO`]; override for the rare quoter/router that gates its
    /// output on the caller. Threaded into [`quote_via_call_from`] by every
    /// adapter's `simulate_swap`.
    pub from: Address,
}

/// Ethereum-mainnet Uniswap V3 `QuoterV2`.
pub const MAINNET_V3_QUOTER_V2: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");

/// Ethereum-mainnet `UniswapV2Router02`.
pub const MAINNET_V2_ROUTER_02: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            v3_quoter: MAINNET_V3_QUOTER_V2,
            v2_router: MAINNET_V2_ROUTER_02,
            from: Address::ZERO,
        }
    }
}

impl SimConfig {
    /// Override the Uniswap V3 QuoterV2 target (e.g. a non-mainnet chain or a
    /// per-deployment quoter).
    pub fn with_v3_quoter(mut self, quoter: Address) -> Self {
        self.v3_quoter = quoter;
        self
    }

    /// Override the Uniswap V2 Router02 target.
    pub fn with_v2_router(mut self, router: Address) -> Self {
        self.v2_router = router;
        self
    }

    /// Override the `msg.sender` (`from`) quote calls run as (default
    /// [`Address::ZERO`]). Use for a quote target that gates on the caller.
    pub fn with_from(mut self, from: Address) -> Self {
        self.from = from;
        self
    }
}

/// Run a quote `calldata` against `target` on the cache and return the raw
/// success output, mapping revert/halt to [`SimError::Reverted`].
///
/// The call is executed with `from = ZERO`, `commit = false` — it never mutates
/// the cache, only reads the warmed snapshot (lazily fetching cold slots from
/// the backend when one is configured). Used by every per-protocol
/// `simulate_swap` so the execution + revert classification lives in one place;
/// the protocol-specific code only builds calldata and decodes the output.
///
/// This is the public helper that custom-adapter authors use to run a quote
/// entrypoint: build the target's quote calldata, call this, then decode the
/// returned [`Bytes`] into the protocol's output.
///
/// Runs the call as `from = ZERO`. Use [`quote_via_call_from`] when the quote
/// target gates its output on `msg.sender`.
pub fn quote_via_call(
    cache: &mut dyn AdapterCache,
    target: Address,
    calldata: Bytes,
) -> Result<Bytes, SimError> {
    quote_via_call_from(cache, Address::ZERO, target, calldata)
}

/// Like [`quote_via_call`], but runs the quote as `from` (`msg.sender`) rather
/// than [`Address::ZERO`]. Adapters thread [`SimConfig::from`] here so a caller
/// can quote against a target that gates behavior on the sender; the default
/// [`SimConfig::from`] keeps the `ZERO`-sender behavior.
pub fn quote_via_call_from(
    cache: &mut dyn AdapterCache,
    from: Address,
    target: Address,
    calldata: Bytes,
) -> Result<Bytes, SimError> {
    match cache
        .call_raw(from, target, calldata, false)
        .map_err(|e| SimError::Execution(Box::new(e)))?
    {
        CallOutcome::Success { output, .. } => Ok(output),
        CallOutcome::Revert { .. } | CallOutcome::Halt { .. } => Err(SimError::Reverted),
    }
}

/// `sol!`-generated ABI bindings for the canonical quote entrypoints.
///
/// Crate-internal plumbing: the per-protocol `simulate_swap` implementations
/// build calldata and decode outputs with these. Deliberately not public API
/// — custom adapters declare their own bindings (see
/// `examples/custom_adapter.rs`) rather than reusing these.
pub(crate) mod abi {
    use alloy_sol_types::sol;

    sol! {
        /// Uniswap V3 `QuoterV2.quoteExactInputSingle` (the struct-arg variant).
        ///
        /// `sqrtPriceLimitX96 = 0` means "no limit" (quote the full input). Returns
        /// `amountOut` plus auxiliary fields we ignore.
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }

        function quoteExactInputSingle(QuoteExactInputSingleParams params)
            returns (
                uint256 amountOut,
                uint160 sqrtPriceX96After,
                uint32 initializedTicksCrossed,
                uint256 gasEstimate
            );

        /// Uniswap V2 `UniswapV2Router02.getAmountsOut(amountIn, path)`.
        ///
        /// Runs the on-chain `UniswapV2Library` math against the warmed pair
        /// reserves and returns the amount at each hop; the last element is the
        /// output for the final token in `path`.
        function getAmountsOut(uint256 amountIn, address[] path)
            returns (uint256[] amounts);

        /// Solidly V2 (Velodrome / Aerodrome) `Pool.getAmountOut(amountIn, tokenIn)`.
        ///
        /// Subtracts the fee via an external `IPoolFactory(factory).getFee()`
        /// STATICCALL, then applies the stable (x³y+y³x) or volatile (xy=k) invariant
        /// in-EVM and returns the `tokenOut` amount. Beyond the reserves it reads
        /// `factory`/`stable`/`token0`/`decimals0`/`decimals1` from pool storage, so
        /// the factory's bytecode + those slots must be reachable (not just reserves).
        function getAmountOut(uint256 amountIn, address tokenIn) returns (uint256 amountOut);

        /// Curve StableSwap (plain pool) `get_dy(i, j, dx)`.
        ///
        /// `i`/`j` are the pool's coin indices (the `coins[]` ordering). Applies the
        /// StableSwap invariant in-EVM against the warmed balances + amplification +
        /// fee and returns the `j`-coin output for `dx` of coin `i`. This `int128`
        /// binding serves the StableSwap / StableSwap-NG (int128-index) variants; the
        /// `uint256` `CurveCryptoSwap::get_dy` below serves CryptoSwap / CryptoSwapNG.
        /// The Curve adapter selects the correct binding per the pool's
        /// [`CurveVariant`](crate::adapters::CurveVariant).
        function get_dy(int128 i, int128 j, uint256 dx) returns (uint256 dy);

        /// Balancer V2 `Vault.queryBatchSwap(kind, swaps, assets, funds)`.
        ///
        /// `kind = 0` is `GIVEN_IN`. Returns the signed asset deltas (per `assets`
        /// index): positive = owed to the vault (input), negative = paid out by the
        /// vault (output).
        function queryBatchSwap(
            uint8 kind,
            BatchSwapStep[] swaps,
            address[] assets,
            FundManagement funds
        ) returns (int256[] assetDeltas);

        struct BatchSwapStep {
            bytes32 poolId;
            uint256 assetInIndex;
            uint256 assetOutIndex;
            uint256 amount;
            bytes userData;
        }

        struct FundManagement {
            address sender;
            bool fromInternalBalance;
            address recipient;
            bool toInternalBalance;
        }
    }

    sol! {
        /// Curve CryptoSwap (Curve v2, e.g. tricrypto) `get_dy(i, j, dx)` — the
        /// **uint256-index** variant (classic/NG StableSwap use the `int128`
        /// `get_dy` above). Namespaced under an interface so its generated
        /// `CurveCryptoSwap::get_dyCall` does not collide with the top-level
        /// `int128` `get_dyCall`. Same semantics: chain code applies the CryptoSwap
        /// invariant against the warmed state; this crate only builds calldata and
        /// decodes the `uint256` output.
        interface CurveCryptoSwap {
            function get_dy(uint256 i, uint256 j, uint256 dx) returns (uint256 dy);
        }
    }
}

pub(crate) use abi::*;
