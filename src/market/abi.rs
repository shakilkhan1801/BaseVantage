//! Contract interfaces used by the market layer. Encoding goes through
//! `sol!` so call data is generated, not hand-rolled; the hand-rolled
//! encoders live in `venues` and are cross-checked against these in tests.

use alloy::sol;

sol! {
    interface IERC20 {
        function symbol() external view returns (string);
        function name() external view returns (string);
        function decimals() external view returns (uint8);
        function balanceOf(address owner) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function transfer(address to, uint256 amount) external returns (bool);
    }

    interface IUniswapV2Factory {
        function getPair(address tokenA, address tokenB) external view returns (address);
    }

    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }

    interface IUniswapV2Router02 {
        function getAmountsOut(uint256 amountIn, address[] calldata path) external view returns (uint256[] memory amounts);
        function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] calldata path, address to, uint256 deadline) external returns (uint256[] memory amounts);
    }

    interface IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
    }

    interface IUniswapV3Pool {
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function tickBitmap(int16 wordPosition) external view returns (uint256);
        function ticks(int24 tick) external view returns (uint128 liquidityGross, int128 liquidityNet, uint256 feeGrowthOutside0X128, uint256 feeGrowthOutside1X128, int56 tickCumulativeOutside, uint160 secondsPerLiquidityOutsideX128, uint32 secondsOutside, bool initialized);
    }

    interface IQuoterV2 {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams calldata params) external returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
    }

    interface IExtsloadOne {
        function extsload(bytes32 slot) external view returns (bytes32);
    }

    interface IExtsloadMany {
        function extsload(bytes32[] calldata slots) external view returns (bytes32[] memory);
    }

    interface IAerodromeFactory {
        function getPool(address tokenA, address tokenB, bool stable) external view returns (address);
        function getFee(address pool, bool stable) external view returns (uint256);
    }

    interface IAerodromePool {
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
        function decimals0() external view returns (uint8);
        function decimals1() external view returns (uint8);
        function stable() external view returns (bool);
        function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    }
}

sol! {
    interface IMulticall3 {
        struct Call3 {
            bool allowFailure;
            address target;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }

    interface IAerodromeRouter {
        struct Route {
            address from;
            address to;
            bool stable;
            address factory;
        }
        function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, Route[] calldata routes, address to, uint256 deadline) external returns (uint256[] memory amounts);
    }
}

sol! {
    interface IV4Quoter {
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }
        struct QuoteExactSingleParams {
            PoolKey poolKey;
            bool zeroForOne;
            uint128 exactAmount;
            bytes hookData;
        }
        function quoteExactInputSingle(QuoteExactSingleParams calldata params) external returns (uint256 amountOut, uint256 gasEstimate);
    }

    interface IUniswapV3Router {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMin;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactInputParams {
            bytes path;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMin;
        }
        function exactInputSingle(ExactInputSingleParams calldata params) external returns (uint256 amountOut);
        function exactInput(ExactInputParams calldata params) external returns (uint256 amountOut);
    }

    interface IUniversalRouter {
        function execute(bytes commands, bytes[] calldata inputs, uint256 deadline) external payable;
    }
}
