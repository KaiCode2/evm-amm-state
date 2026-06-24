// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Combined Balancer-vault stub for offline swap-sim + reactive tests.
///
/// Serves BOTH cold-start discovery and a swap quote off the SAME fixed slots,
/// so the reactive `Swap` refresh of the discovered balance slots is observable
/// through a subsequent `queryBatchSwap`:
///
/// - `getPoolTokens(bytes32)` SLOADs fixed slots 0..=4 (token0, token1,
///   balance0, balance1, lastChangeBlock) and returns the dynamic
///   `(address[] tokens, uint256[] balances, uint256 lastChangeBlock)` tuple —
///   identical shape to `MockBalancerVault`, so cold-start discovers the same
///   `(vault, slot)` set {0,1,2,3,4}.
/// - `queryBatchSwap(...)` SLOADs balance slot 2 and returns it as the NEGATIVE
///   `assetDeltas[1]` (the vault-paid-out tokenOut delta), with
///   `assetDeltas[0] = +amount` (the tokenIn the vault is owed). So
///   `simulate_swap` decodes `amount_out = balance0 (slot 2)`; refreshing slot 2
///   changes the quote. Arguments other than the GIVEN_IN amount are ignored.
contract MockBalancerVaultQuote {
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

    function queryBatchSwap(
        uint8,
        BatchSwapStep[] calldata swaps,
        address[] calldata,
        FundManagement calldata
    ) external view returns (int256[] memory assetDeltas) {
        uint256 b0;
        assembly {
            b0 := sload(2)
        }
        assetDeltas = new int256[](2);
        // tokenIn delta: positive = owed to the vault (the GIVEN_IN amount).
        assetDeltas[0] = int256(swaps[0].amount);
        // tokenOut delta: negative = paid out by the vault (balance0 at slot 2).
        assetDeltas[1] = -int256(b0);
    }
}
