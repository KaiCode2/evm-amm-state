// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Balancer-vault stub whose `getPoolTokens` returns a VALID, decodable
/// `(address[], uint256[], uint256)` tuple built entirely from literals — it is
/// `pure` and performs NO SLOAD. So `call_raw_with_access_list` (restricted to
/// the vault) captures an EMPTY `(vault, slot)` set, driving the planner's
/// `NoSlotsDiscovered` branch (decode succeeds, but nothing was warmed). Used to
/// pin that empty-capture repairs via re-discovery rather than a vault-wide
/// storage purge (the vault is a shared singleton).
contract MockBalancerVaultNoSlot {
    function getPoolTokens(bytes32)
        external
        pure
        returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock)
    {
        tokens = new address[](2);
        tokens[0] = address(uint160(0xC0));
        tokens[1] = address(uint160(0xC1));
        balances = new uint256[](2);
        balances[0] = 100;
        balances[1] = 200;
        lastChangeBlock = 7;
    }
}
