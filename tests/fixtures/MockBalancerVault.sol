// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Minimal Balancer-vault stub for offline cold-start discover tests.
///
/// `getPoolTokens` SLOADs five FIXED slots (0..=4) — token0, token1, balance0,
/// balance1, lastChangeBlock — so a `call_raw_with_access_list` captures a
/// deterministic `(vault, slot)` set, and returns the decoded
/// `(address[2], uint256[2], uint256)` tuple built from them. The poolId
/// argument is ignored (the planner is storage-layout-agnostic: it verifies
/// whatever slots the call touches).
contract MockBalancerVault {
    function getPoolTokens(bytes32)
        external
        view
        returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock)
    {
        uint256 t0;
        uint256 t1;
        uint256 b0;
        uint256 b1;
        uint256 lcb;
        assembly {
            t0 := sload(0)
            t1 := sload(1)
            b0 := sload(2)
            b1 := sload(3)
            lcb := sload(4)
        }
        tokens = new address[](2);
        tokens[0] = address(uint160(t0));
        tokens[1] = address(uint160(t1));
        balances = new uint256[](2);
        balances[0] = b0;
        balances[1] = b1;
        lastChangeBlock = lcb;
    }
}
