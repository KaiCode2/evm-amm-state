// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Minimal UniswapV2Router02 stub for offline swap-sim tests.
///
/// `getAmountsOut` SLOADs a FIXED slot (0) at the router address — the warmed
/// "quote" slot the test seeds — and returns a 2-element `uint256[]` whose last
/// element is that value, so `simulate_swap` (which reads `amounts.last()`)
/// returns a deterministic, slot-derived amount fully offline. The arguments are
/// ignored: the planner is layout-agnostic for the offline harness. The DYNAMIC
/// `uint256[]` return matches the real `getAmountsOut(uint256,address[])` ABI.
contract MockV2Router {
    function getAmountsOut(uint256, address[] calldata)
        external
        view
        returns (uint256[] memory amounts)
    {
        uint256 out;
        assembly {
            out := sload(0)
        }
        amounts = new uint256[](2);
        amounts[0] = 0;
        amounts[1] = out;
    }
}
