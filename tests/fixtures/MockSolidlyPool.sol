// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Minimal Solidly V2 (Aerodrome / Velodrome V2) pool stub for offline
/// swap-sim tests. `getAmountOut(amountIn, tokenIn)` returns a deterministic
/// value SLOADed from a fixed slot (slot 0), so a test can seed the expected
/// output and assert `simulate_swap` decodes the pool's own quote. The args are
/// ignored (the adapter only needs the selector + decode path exercised).
contract MockSolidlyPool {
    function getAmountOut(uint256, address) external view returns (uint256 out) {
        assembly {
            out := sload(0)
        }
    }
}
