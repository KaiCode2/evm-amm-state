// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Minimal Uniswap V3 QuoterV2 stub for offline swap-sim tests.
///
/// `quoteExactInputSingle` SLOADs a FIXED slot (0) at the quoter address — the
/// warmed "quote" slot the test seeds — and returns it as `amountOut`, with the
/// auxiliary fields zeroed, so `simulate_swap` returns a deterministic,
/// slot-derived amount fully offline. The struct argument is ignored. The
/// return tuple matches the real `QuoterV2.quoteExactInputSingle` shape
/// `(uint256 amountOut, uint160, uint32, uint256)`.
contract MockV3Quoter {
    struct QuoteExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }

    function quoteExactInputSingle(QuoteExactInputSingleParams calldata)
        external
        view
        returns (
            uint256 amountOut,
            uint160 sqrtPriceX96After,
            uint32 initializedTicksCrossed,
            uint256 gasEstimate
        )
    {
        uint256 out;
        assembly {
            out := sload(0)
        }
        amountOut = out;
        sqrtPriceX96After = 0;
        initializedTicksCrossed = 0;
        gasEstimate = 0;
    }
}
