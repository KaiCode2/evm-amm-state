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

#[cfg(any(
    feature = "uniswap-v2",
    feature = "uniswap-v3",
    feature = "balancer-v2",
    feature = "solidly-v2",
    feature = "curve"
))]
use alloy_primitives::Bytes;
use alloy_primitives::{Address, U256, address};
use alloy_sol_types::sol;
#[cfg(any(
    feature = "uniswap-v2",
    feature = "uniswap-v3",
    feature = "balancer-v2",
    feature = "solidly-v2",
    feature = "curve"
))]
use revm::context::result::ExecutionResult;

#[cfg(any(
    feature = "uniswap-v2",
    feature = "uniswap-v3",
    feature = "balancer-v2",
    feature = "solidly-v2",
    feature = "curve"
))]
use super::AdapterCache;

/// A swap-simulation quote: the output amount the protocol's canonical quote
/// entrypoint returns for the requested input.
///
/// Intentionally a struct (not a bare `U256`) so future quote outputs (gas,
/// effective price, sqrt-price-after) can extend it without breaking callers.
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SimError {
    /// The adapter does not implement swap simulation for its protocol.
    Unsupported(super::ProtocolId),
    /// Required metadata (e.g. the Balancer vault, or a V3 fee) is missing.
    MissingMetadata(&'static str),
    /// The quote call reverted or halted in the EVM.
    Reverted,
    /// The quote call executed but its return data could not be decoded.
    MalformedOutput(&'static str),
    /// The underlying `call_raw` failed (host/transact error).
    Execution(String),
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

impl std::error::Error for SimError {}

/// Resolved quote-target addresses for swap simulation.
///
/// Defaults to the canonical Ethereum-mainnet QuoterV2 and UniswapV2Router02.
/// Per-pool/chain overrides are applied with the `with_*` builders. The Balancer
/// vault is *not* configured here — it is the pool's own vault
/// (`BalancerV2Metadata.vault`), resolved per-pool at quote time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimConfig {
    /// Uniswap V3 `QuoterV2` (and family) quote target.
    pub v3_quoter: Address,
    /// Uniswap V2 `UniswapV2Router02` quote target.
    pub v2_router: Address,
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
/// Gated on the protocols that call it: with no protocol adapter compiled there
/// is no `simulate_swap` impl, so this helper would be dead code.
#[cfg(any(
    feature = "uniswap-v2",
    feature = "uniswap-v3",
    feature = "balancer-v2",
    feature = "solidly-v2",
    feature = "curve"
))]
pub(crate) fn run_quote(
    cache: &mut dyn AdapterCache,
    target: Address,
    calldata: Bytes,
) -> Result<Bytes, SimError> {
    let result = cache
        .call_raw(Address::ZERO, target, calldata, false)
        .map_err(|err| SimError::Execution(err.to_string()))?;

    match result {
        ExecutionResult::Success { output, .. } => Ok(output.into_data()),
        ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. } => Err(SimError::Reverted),
    }
}

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
    /// fee and returns the `j`-coin output for `dx` of coin `i`. `int128` indices
    /// match classic StableSwap (CryptoSwap / StableSwap-NG use `uint256` — out
    /// of scope here).
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
