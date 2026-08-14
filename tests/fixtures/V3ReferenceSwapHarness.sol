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
/// protocol-specific swap math of its own. The Slipstream quote entrypoint uses
/// callback-revert semantics so adapter tests execute real pool math while
/// retaining the production quoter ABI.
contract V3ReferenceSwapHarness {
    address public token0;
    address public token1;
    address public pool;

    struct SlipstreamQuoteExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        int24 tickSpacing;
        uint160 sqrtPriceLimitX96;
    }

    function quoteExactInputSingle(
        SlipstreamQuoteExactInputSingleParams calldata params
    ) external returns (
        uint256 amountOut,
        uint160 sqrtPriceX96After,
        uint32 initializedTicksCrossed,
        uint256 gasEstimate
    ) {
        require(params.tickSpacing == 200, "spacing");
        require(params.amountIn <= uint256(type(int256).max), "amount");
        bool zeroForOne;
        if (params.tokenIn == token0 && params.tokenOut == token1) {
            zeroForOne = true;
        } else {
            require(params.tokenIn == token1 && params.tokenOut == token0, "pair");
            zeroForOne = false;
        }
        uint160 limit = params.sqrtPriceLimitX96;
        if (limit == 0) {
            limit = zeroForOne
                ? uint160(4_295_128_740)
                : uint160(1_461_446_703_485_210_103_287_273_052_203_988_822_378_723_970_341);
        }
        try IReferenceV3Pool(pool).swap(
            address(this),
            zeroForOne,
            int256(params.amountIn),
            limit,
            abi.encode(zeroForOne)
        ) returns (int256, int256) {
            revert("callback did not quote");
        } catch (bytes memory quote) {
            if (quote.length != 32) {
                assembly {
                    revert(add(quote, 32), mload(quote))
                }
            }
            amountOut = abi.decode(quote, (uint256));
        }
        sqrtPriceX96After = 0;
        initializedTicksCrossed = 0;
        gasEstimate = 0;
    }

    function execute(
        address poolAddress,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96
    ) external returns (int256 amount0, int256 amount1) {
        return IReferenceV3Pool(poolAddress).swap(
            address(this),
            zeroForOne,
            amountSpecified,
            sqrtPriceLimitX96,
            bytes("")
        );
    }

    function executeMint(
        address poolAddress,
        int24 tickLower,
        int24 tickUpper,
        uint128 amount
    ) external returns (uint256 amount0, uint256 amount1) {
        return IReferenceV3Pool(poolAddress).mint(
            address(this),
            tickLower,
            tickUpper,
            amount,
            bytes("")
        );
    }

    function executeBurn(
        address poolAddress,
        int24 tickLower,
        int24 tickUpper,
        uint128 amount
    ) external returns (uint256 amount0, uint256 amount1) {
        return IReferenceV3Pool(poolAddress).burn(tickLower, tickUpper, amount);
    }

    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external {
        if (data.length != 0) {
            require(msg.sender == pool, "pool");
            bool zeroForOne = abi.decode(data, (bool));
            uint256 amountOut = uint256(-(zeroForOne ? amount1Delta : amount0Delta));
            assembly {
                mstore(0, amountOut)
                revert(0, 32)
            }
        }
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
