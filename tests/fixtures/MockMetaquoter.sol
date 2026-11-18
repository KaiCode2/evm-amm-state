// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Minimal Aero `Metaquoter` stub for offline swap-simulation tests. The
/// exact-input quote returns `sload(0) + tx.gasprice` and the exact-output
/// quote `sload(1) + tx.gasprice`, so a test can assert both the decode path
/// and that the configured transaction context reached the quoter. The pool
/// argument is echoed in `sqrtPriceX96After` (truncated) so the test can
/// prove the pool address was addressed directly.
contract MockMetaquoter {
    function quoteExactInputSingleV3(address pool, address, uint256, uint160)
        external
        view
        returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed)
    {
        assembly {
            amountOut := add(sload(0), gasprice())
        }
        sqrtPriceX96After = uint160(pool);
        initializedTicksCrossed = 0;
    }

    function quoteExactOutputSingleV3(address pool, address, uint256, uint160)
        external
        view
        returns (uint256 amountIn, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed)
    {
        assembly {
            amountIn := add(sload(1), gasprice())
        }
        sqrtPriceX96After = uint160(pool);
        initializedTicksCrossed = 0;
    }
}

/// Compiled with `forge build` (solc 0.8.23, optimizer off, evm cancun) into
/// the sibling `*_runtime.hex` (deployed bytecode).
