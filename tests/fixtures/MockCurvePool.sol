// SPDX-License-Identifier: MIT
//
// Documentation for `mock_curve_pool_runtime.hex` (hand-assembled, NOT solc
// output — so no real ABI dispatch is needed and the same stub serves both the
// offline `get_dy` quote test and the cold-start discover test).
//
// Runtime bytecode: 60005460005260206000f3
//   PUSH1 0x00 ; SLOAD            -> storage[0]
//   PUSH1 0x00 ; MSTORE           ; mem[0..32] = storage[0]
//   PUSH1 0x20 ; PUSH1 0x00 ; RETURN
//
// For ANY calldata (any selector, incl. get_dy(int128,int128,uint256)) it
// returns storage slot 0 as a single 32-byte word, and SLOADs slot 0 along the
// way (so a cold-start discover pass over a `get_dy` call captures (pool, 0)).
//
// Tests seed slot 0 with the expected get_dy output via
// `db_mut().insert_account_storage(pool, U256::ZERO, expected)`.
pragma solidity ^0.8.0;

contract MockCurvePool {
    uint256 slot0;

    // Conceptually: every entrypoint returns slot0. Real arity/selectors are
    // irrelevant because the deployed stub does not dispatch.
    fallback() external {
        uint256 v = slot0;
        assembly {
            mstore(0x00, v)
            return(0x00, 0x20)
        }
    }
}
