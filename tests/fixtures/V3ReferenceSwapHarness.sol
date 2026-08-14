// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.20;

interface IReferenceV3Pool {
    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);

    function mint(
        address recipient,
        int24 tickLower,
        int24 tickUpper,
        uint128 amount,
        bytes calldata data
    ) external returns (uint256 amount0, uint256 amount1);

    function burn(
        int24 tickLower,
        int24 tickUpper,
        uint128 amount
    ) external returns (uint256 amount0, uint256 amount1);
}

interface IMintableToken {
    function mint(address recipient, uint256 amount) external;
}

/// Minimal stateful token used by the offline reference-bytecode test. The
/// pool's output transfer debits its seeded balance and the callback mints the
/// required input directly to the pool, satisfying canonical balance checks.
contract ReferenceSwapToken {
    mapping(address => uint256) public balanceOf;

    function transfer(address recipient, uint256 amount) external returns (bool) {
        require(balanceOf[msg.sender] >= amount, "balance");
        unchecked {
            balanceOf[msg.sender] -= amount;
            balanceOf[recipient] += amount;
        }
        return true;
    }

    function mint(address recipient, uint256 amount) external {
        balanceOf[recipient] += amount;
    }
}

/// Calls the real pool runtime and pays its callback without introducing any
/// protocol-specific swap math of its own.
contract V3ReferenceSwapHarness {
    address public token0;
    address public token1;

    function execute(
        address pool,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96
    ) external returns (int256 amount0, int256 amount1) {
        return IReferenceV3Pool(pool).swap(
            address(this),
            zeroForOne,
            amountSpecified,
            sqrtPriceLimitX96,
            bytes("")
        );
    }

    function executeMint(
        address pool,
        int24 tickLower,
        int24 tickUpper,
        uint128 amount
    ) external returns (uint256 amount0, uint256 amount1) {
        return IReferenceV3Pool(pool).mint(
            address(this),
            tickLower,
            tickUpper,
            amount,
            bytes("")
        );
    }

    function executeBurn(
        address pool,
        int24 tickLower,
        int24 tickUpper,
        uint128 amount
    ) external returns (uint256 amount0, uint256 amount1) {
        return IReferenceV3Pool(pool).burn(tickLower, tickUpper, amount);
    }

    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata
    ) external {
        if (amount0Delta > 0) {
            IMintableToken(token0).mint(msg.sender, uint256(amount0Delta));
        }
        if (amount1Delta > 0) {
            IMintableToken(token1).mint(msg.sender, uint256(amount1Delta));
        }
    }

    function uniswapV3MintCallback(
        uint256 amount0Owed,
        uint256 amount1Owed,
        bytes calldata
    ) external {
        if (amount0Owed > 0) {
            IMintableToken(token0).mint(msg.sender, amount0Owed);
        }
        if (amount1Owed > 0) {
            IMintableToken(token1).mint(msg.sender, amount1Owed);
        }
    }
}
