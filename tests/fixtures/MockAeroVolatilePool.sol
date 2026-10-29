// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Minimal Aero (Dromos) basic volatile pool stub for offline swap-simulation
/// and cold-start tests. `POOL_TYPE()` reports `V2_VOLATILE`; the total-fee
/// getters return a seeded storage word plus `tx.gasprice` so a test can prove
/// the configured transaction context reached the pool; the legacy
/// `getAmountOut` returns a different seeded word so a test can prove which
/// selector the adapter variant chose. `ORIGIN()` echoes `tx.origin`.
contract MockAeroVolatilePool {
    bytes32 public constant POOL_TYPE = "V2_VOLATILE";

    function getAmountOut(uint256, address) external view returns (uint256 out) {
        assembly {
            out := sload(0)
        }
    }

    function getAmountOutWithTotalFee(uint256, address) external view returns (uint256 out) {
        assembly {
            out := add(sload(1), gasprice())
        }
    }

    function getAmountInWithTotalFee(uint256, address) external view returns (uint256 amountIn) {
        assembly {
            amountIn := add(sload(2), gasprice())
        }
    }

    function ORIGIN() external view returns (address) {
        return tx.origin;
    }
}

/// Compiled with `forge build` (solc 0.8.23, optimizer off, evm cancun) into
/// the sibling `*_runtime.hex` (deployed bytecode).
