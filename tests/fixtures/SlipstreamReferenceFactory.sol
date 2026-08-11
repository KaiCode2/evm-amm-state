// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.7.6;

/// Deterministic local-EVM fee source for exercising deployed Slipstream pool
/// bytecode. It deliberately implements only the two selectors consumed by
/// `CLPool.swap`; adapter fee evidence is produced independently from the
/// reviewed deployed factory/voter/module runtimes.
contract SlipstreamReferenceFactory {
    uint24 public swapFee;
    uint24 public unstakedFee;

    function setFees(uint24 swapFee_, uint24 unstakedFee_) external {
        swapFee = swapFee_;
        unstakedFee = unstakedFee_;
    }

    function getSwapFee(address) external view returns (uint24) {
        return swapFee;
    }

    function getUnstakedFee(address) external view returns (uint24) {
        return unstakedFee;
    }
}
