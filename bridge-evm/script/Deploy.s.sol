// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import { Script, console } from "forge-std/Script.sol";
import { WVTT } from "../src/WVTT.sol";
import { VTTBridge } from "../src/VTTBridge.sol";

contract DeployBridge is Script {
    function run() external {
        // USDT addresses per chain
        // Sepolia: 0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238
        // Mainnet: 0xdAC17F958D2ee523a2206206994597C13D831ec7
        // Base:    0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913
        address usdt = vm.envAddress("USDT_ADDRESS");
        address relayer = vm.envAddress("RELAYER_ADDRESS");
        uint256 feeBps = vm.envOr("BRIDGE_FEE_BPS", uint256(10));

        vm.startBroadcast();

        // 1. Deploy wVTT with temporary bridge (address(0))
        WVTT wvtt = new WVTT(address(0));
        console.log("wVTT deployed at:", address(wvtt));

        // 2. Deploy Bridge
        VTTBridge bridge = new VTTBridge(
            address(wvtt),
            usdt,
            relayer,
            feeBps
        );
        console.log("VTTBridge deployed at:", address(bridge));

        // 3. Set bridge on wVTT
        wvtt.setBridge(address(bridge));
        console.log("wVTT bridge set to:", address(bridge));

        vm.stopBroadcast();
    }
}
