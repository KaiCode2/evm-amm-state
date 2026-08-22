// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.23;

interface IReferenceV3PoolAccounting {
    function setFeeProtocol(uint8 feeProtocol0, uint8 feeProtocol1) external;

    function collectProtocol(
        address recipient,
        uint128 amount0Requested,
        uint128 amount1Requested
    ) external returns (uint128 amount0, uint128 amount1);

    function flash(
        address recipient,
        uint256 amount0,
        uint256 amount1,
        bytes calldata data
    ) external;

    function increaseObservationCardinalityNext(uint16 observationCardinalityNext) external;
}

interface IMintableToken {
    function mint(address recipient, uint256 amount) external;
}

/// Drives canonical Uniswap V3's accounting entrypoints against the deployed
/// pool runtime for the offline differential corpus.
///
/// Installed at the address the pool's `factory` immutable points to, and
/// reports itself as that factory's owner, so canonical `onlyFactoryOwner`
/// executes for real rather than being stubbed away. It also serves the
/// flash-loan callback, repaying the pool by minting to it.
///
/// Runtime fixture `v3_reference_accounting_harness_runtime.hex` is produced
/// with `solc 0.8.23 --bin-runtime` (no optimizer), matching the other
/// checked-in harness fixtures.
contract V3ReferenceAccountingHarness {
    address public token0;
    address public token1;

    /// Canonical `onlyFactoryOwner` compares `msg.sender` against
    /// `factory.owner()`. Reporting itself makes this contract the privileged
    /// caller without weakening the check.
    function owner() external view returns (address) {
        return address(this);
    }

    function executeSetFeeProtocol(
        address pool,
        uint8 feeProtocol0,
        uint8 feeProtocol1
    ) external {
        IReferenceV3PoolAccounting(pool).setFeeProtocol(feeProtocol0, feeProtocol1);
    }

    function executeCollectProtocol(
        address pool,
        uint128 amount0Requested,
        uint128 amount1Requested
    ) external returns (uint128 amount0, uint128 amount1) {
        return
            IReferenceV3PoolAccounting(pool).collectProtocol(
                address(this),
                amount0Requested,
                amount1Requested
            );
    }

    function executeIncreaseObservationCardinalityNext(
        address pool,
        uint16 observationCardinalityNext
    ) external {
        IReferenceV3PoolAccounting(pool).increaseObservationCardinalityNext(
            observationCardinalityNext
        );
    }

    /// `repay0`/`repay1` are the totals minted back to the pool inside the
    /// callback. The pool derives `paid = balanceAfter - balanceBefore`, so the
    /// fee it credits is `repay - amount`.
    function executeFlash(
        address pool,
        uint256 amount0,
        uint256 amount1,
        uint256 repay0,
        uint256 repay1
    ) external {
        IReferenceV3PoolAccounting(pool).flash(
            address(this),
            amount0,
            amount1,
            abi.encode(repay0, repay1)
        );
    }

    function uniswapV3FlashCallback(uint256, uint256, bytes calldata data) external {
        (uint256 repay0, uint256 repay1) = abi.decode(data, (uint256, uint256));
        if (repay0 > 0) {
            IMintableToken(token0).mint(msg.sender, repay0);
        }
        if (repay1 > 0) {
            IMintableToken(token1).mint(msg.sender, repay1);
        }
    }
}
