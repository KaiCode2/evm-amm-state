// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Minimal Slipstream quoter stub for offline swap-simulation tests.
contract MockSlipstreamQuoter {
    struct QuoteExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        int24 tickSpacing;
        uint160 sqrtPriceLimitX96;
    }

    function quoteExactInputSingle(QuoteExactInputSingleParams calldata params)
        external
        view
        returns (
            uint256 amountOut,
            uint160 sqrtPriceX96After,
            uint32 initializedTicksCrossed,
            uint256 gasEstimate
        )
    {
        require(params.tickSpacing == 200, "wrong tick spacing");
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
