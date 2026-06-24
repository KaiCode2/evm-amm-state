// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Three-token Balancer-vault stub, to exercise dynamic-array decode for N != 2.
///
/// Like `MockBalancerVault` but SLOADs seven FIXED slots (0..=6) — token0..2,
/// balance0..2, lastChangeBlock — and returns dynamic `address[3]`/`uint256[3]`
/// arrays (real weighted/stable pools hold 3..8 tokens). Proves the planner's
/// `getPoolTokens` decode and the warmed `(vault, slot)` capture generalise
/// beyond the 2-token happy path.
contract MockBalancerVault3 {
    function getPoolTokens(bytes32)
        external
        view
        returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock)
    {
        uint256 t0;
        uint256 t1;
        uint256 t2;
        uint256 b0;
        uint256 b1;
        uint256 b2;
        uint256 lcb;
        assembly {
            t0 := sload(0)
            t1 := sload(1)
            t2 := sload(2)
            b0 := sload(3)
            b1 := sload(4)
            b2 := sload(5)
            lcb := sload(6)
        }
        tokens = new address[](3);
        tokens[0] = address(uint160(t0));
        tokens[1] = address(uint160(t1));
        tokens[2] = address(uint160(t2));
        balances = new uint256[](3);
        balances[0] = b0;
        balances[1] = b1;
        balances[2] = b2;
        lastChangeBlock = lcb;
    }
}
