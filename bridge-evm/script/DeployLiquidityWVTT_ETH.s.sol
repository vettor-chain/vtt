// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import { Script, console } from "forge-std/Script.sol";
import { WVTT } from "../src/WVTT.sol";

/**
 * @title Deploy wVTT/ETH Liquidity on Uniswap V2
 *
 * Reads from environment:
 *   WVTT_CONTRACT          — deployed wVTT address
 *   UNISWAP_ROUTER         — Uniswap V2 Router (0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D on mainnet)
 *   INITIAL_WVTT_AMOUNT    — wVTT amount in wei (e.g. 1000000000000000000000 = 1000 wVTT)
 *   INITIAL_ETH_AMOUNT     — ETH amount in wei  (e.g. 1000000000000000000   = 1 ETH)
 *   DEPLOYER_KEY           — private key for the deployer
 *
 * Run:
 *   source .env
 *   forge script script/DeployLiquidityWVTT_ETH.s.sol:DeployLiquidityWVTT_ETH \
 *     --rpc-url $ETH_RPC_URL --broadcast -vvvv
 */

interface IUniswapV2Router02 {
    function addLiquidityETH(
        address token,
        uint256 amountTokenDesired,
        uint256 amountTokenMin,
        uint256 amountETHMin,
        address to,
        uint256 deadline
    ) external payable returns (uint256 amountToken, uint256 amountETH, uint256 liquidity);

    function factory() external pure returns (address);
}

interface IUniswapV2Factory {
    function getPair(address tokenA, address tokenB) external view returns (address pair);
}

contract DeployLiquidityWVTT_ETH is Script {
    function run() external {
        address wvttAddr = vm.envAddress("WVTT_CONTRACT");
        address routerAddr = vm.envAddress("UNISWAP_ROUTER");
        uint256 wvttAmount = vm.envUint("INITIAL_WVTT_AMOUNT");
        uint256 ethAmount = vm.envUint("INITIAL_ETH_AMOUNT");

        require(wvttAddr != address(0), "WVTT_CONTRACT not set");
        require(routerAddr != address(0), "UNISWAP_ROUTER not set");
        require(wvttAmount > 0, "INITIAL_WVTT_AMOUNT must be > 0");
        require(ethAmount > 0, "INITIAL_ETH_AMOUNT must be > 0");

        WVTT wvtt = WVTT(wvttAddr);
        IUniswapV2Router02 router = IUniswapV2Router02(routerAddr);

        vm.startBroadcast();

        // 1. Approve the router to spend our wVTT
        wvtt.approve(routerAddr, wvttAmount);
        console.log("Approved wVTT spend:", wvttAmount);

        // 2. Add liquidity (wVTT + ETH)
        //    Accept up to 1% slippage on initial pool creation
        uint256 amountTokenMin = (wvttAmount * 99) / 100;
        uint256 amountETHMin = (ethAmount * 99) / 100;
        uint256 deadline = block.timestamp + 20 minutes;

        (uint256 amountToken, uint256 amountETH, uint256 liquidity) = router.addLiquidityETH{value: ethAmount}(
            wvttAddr,
            wvttAmount,
            amountTokenMin,
            amountETHMin,
            msg.sender, // LP tokens go to deployer
            deadline
        );

        console.log("Liquidity added:");
        console.log("  wVTT deposited:", amountToken);
        console.log("  ETH  deposited:", amountETH);
        console.log("  LP tokens:     ", liquidity);

        // 3. Log the pair address
        IUniswapV2Factory factory = IUniswapV2Factory(router.factory());
        address weth = address(0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2); // mainnet WETH
        address pair = factory.getPair(wvttAddr, weth);
        console.log("Pair address:", pair);

        vm.stopBroadcast();
    }
}
