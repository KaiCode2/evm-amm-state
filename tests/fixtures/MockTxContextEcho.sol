// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/// Echoes the transaction context a simulated call runs under, for tests of
/// the cache facades' `call_raw_with_context`: any call returns
/// `(tx.gasprice, tx.origin, block.basefee)`.
contract MockTxContextEcho {
    fallback() external {
        assembly {
            mstore(0x00, gasprice())
            mstore(0x20, origin())
            mstore(0x40, basefee())
            return(0x00, 0x60)
        }
    }
}

/// Compiled with `forge build` (solc 0.8.23, optimizer off, evm cancun) into
/// the sibling `*_runtime.hex` (deployed bytecode).
