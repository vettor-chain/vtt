// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "../src/WVTT.sol";
import "forge-std/interfaces/IERC20.sol";

/**
 * @title Deploy wVTT/USDT Liquidity on Uniswap V2
 *
 * Reads from environment:
 *   WVTT_CONTRACT          — deployed wVTT address
 *   USDT_ADDRESS           — USDT address (e.g. 0xdAC17F958D2ee523a2206206994597C13D831ec7 on mainnet)
 *   UNISWAP_ROUTER         — Uniswap V2 Router (0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D on mainnet)
 *   INITIAL_WVTT_AMOUNT    — wVTT amount in wei (18 decimals)
 *   INITIAL_USDT_AMOUNT    — USDT amount in base units (6 decimals, e.g. 10000000000 = 10,000 USDT)
 *   DEPLOYER_KEY           — private key for the deployer
 *
 * Run:
 *   source .env
 *   forge script script/DeployLiquidityWVTT_USDT.s.sol:DeployLiquidityWVTT_USDT \
 *     --rpc-url $ETH_RPC_URL --broadcast -vvvv
 */

interface IUniswapV2Router02 {
    function addLiquidity(
        address tokenA,
        address tokenB,
        uint256 amountADesired,
        uint256 amountBDesired,
        uint256 amountAMin,
        uint256 amountBMin,
        address to,
        uint256 deadline
    ) external returns (uint256 amountA, uint256 amountB, uint256 liquidity);

    function factory() external pure returns (address);
}

interface IUniswapV2Factory {
    function getPair(address tokenA, address tokenB) external view returns (address pair);
}

contract DeployLiquidityWVTT_USDT is Script {
    function run() external {
        address wvttAddr = vm.envAddress("WVTT_CONTRACT");
        address usdtAddr = vm.envAddress("USDT_ADDRESS");
        address routerAddr = vm.envAddress("UNISWAP_ROUTER");
        uint256 wvttAmount = vm.envUint("INITIAL_WVTT_AMOUNT");
        uint256 usdtAmount = vm.envUint("INITIAL_USDT_AMOUNT");

        require(wvttAddr != address(0), "WVTT_CONTRACT not set");
        require(usdtAddr != address(0), "USDT_ADDRESS not set");
        require(routerAddr != address(0), "UNISWAP_ROUTER not set");
        require(wvttAmount > 0, "INITIAL_WVTT_AMOUNT must be > 0");
        require(usdtAmount > 0, "INITIAL_USDT_AMOUNT must be > 0");

        WVTT wvtt = WVTT(wvttAddr);
        IERC20 usdt = IERC20(usdtAddr);
        IUniswapV2Router02 router = IUniswapV2Router02(routerAddr);

        vm.startBroadcast();

        // 1. Approve the router to spend wVTT and USDT
        wvtt.approve(routerAddr, wvttAmount);
        console.log("Approved wVTT spend:", wvttAmount);

        // USDT uses non-standard approve (no return value on some implementations)
        // SafeApprove pattern: set to 0 first, then to desired amount
        (bool resetOk,) = usdtAddr.call(
            abi.encodeWithSelector(IERC20.approve.selector, routerAddr, uint256(0))
        );
        require(resetOk, "USDT approve reset failed");

        (bool approveOk,) = usdtAddr.call(
            abi.encodeWithSelector(IERC20.approve.selector, routerAddr, usdtAmount)
        );
        require(approveOk, "USDT approve failed");
        console.log("Approved USDT spend:", usdtAmount);

        // 2. Add liquidity (wVTT + USDT)
        //    Accept up to 1% slippage on initial pool creation
        uint256 amountWvttMin = (wvttAmount * 99) / 100;
        uint256 amountUsdtMin = (usdtAmount * 99) / 100;
        uint256 deadline = block.timestamp + 20 minutes;

        (uint256 amountA, uint256 amountB, uint256 liquidity) = router.addLiquidity(
            wvttAddr,
            usdtAddr,
            wvttAmount,
            usdtAmount,
            amountWvttMin,
            amountUsdtMin,
            msg.sender, // LP tokens go to deployer
            deadline
        );

        console.log("Liquidity added:");
        console.log("  wVTT deposited:", amountA);
        console.log("  USDT deposited:", amountB);
        console.log("  LP tokens:     ", liquidity);

        // 3. Log the pair address
        IUniswapV2Factory factory = IUniswapV2Factory(router.factory());
        address pair = factory.getPair(wvttAddr, usdtAddr);
        console.log("Pair address:", pair);

        vm.stopBroadcast();
    }
}
