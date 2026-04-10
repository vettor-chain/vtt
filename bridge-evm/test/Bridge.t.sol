// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/WVTT.sol";
import "../src/VTTBridge.sol";
import "forge-std/interfaces/IERC20.sol";

// Mock USDT for testing
contract MockUSDT is IERC20 {
    string public constant name = "Mock USDT";
    string public constant symbol = "USDT";
    uint8 public constant decimals = 6;
    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        totalSupply += amount;
        balanceOf[to] += amount;
        emit Transfer(address(0), to, amount);
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        emit Transfer(msg.sender, to, amount);
        return true;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
        return true;
    }
}

contract BridgeTest is Test {
    WVTT wvtt;
    MockUSDT usdt;
    VTTBridge bridge;

    address relayer = address(0xBEEF);
    address user = address(0xCAFE);
    bytes32 vttDest = bytes32(uint256(uint160(0xDEAD)));

    function setUp() public {
        usdt = new MockUSDT();
        wvtt = new WVTT(address(0)); // temp bridge
        bridge = new VTTBridge(address(wvtt), address(usdt), relayer, 10); // 0.1% fee
        wvtt.setBridge(address(bridge));
    }

    // ─── wVTT tests ───

    function test_wvtt_mint_by_bridge() public {
        vm.prank(address(bridge));
        wvtt.mint(user, 1000 ether);
        assertEq(wvtt.balanceOf(user), 1000 ether);
        assertEq(wvtt.totalSupply(), 1000 ether);
    }

    function test_wvtt_mint_not_bridge_reverts() public {
        vm.expectRevert("WVTT: caller is not the bridge");
        wvtt.mint(user, 1000 ether);
    }

    function test_wvtt_transfer() public {
        vm.prank(address(bridge));
        wvtt.mint(user, 100 ether);

        vm.prank(user);
        wvtt.transfer(address(0xBBBB), 40 ether);

        assertEq(wvtt.balanceOf(user), 60 ether);
        assertEq(wvtt.balanceOf(address(0xBBBB)), 40 ether);
    }

    // ─── Release (VTT chain → Ethereum) ───

    function test_release_wvtt() public {
        bytes32 txHash = keccak256("vtt-tx-1");

        vm.prank(relayer);
        bridge.releaseWVTT(txHash, user, 500 ether);

        assertEq(wvtt.balanceOf(user), 500 ether);
        assertTrue(bridge.processedWithdrawals(txHash));
    }

    function test_release_wvtt_replay_reverts() public {
        bytes32 txHash = keccak256("vtt-tx-2");

        vm.prank(relayer);
        bridge.releaseWVTT(txHash, user, 100 ether);

        vm.prank(relayer);
        vm.expectRevert("Bridge: already processed");
        bridge.releaseWVTT(txHash, user, 100 ether);
    }

    function test_release_wvtt_not_relayer_reverts() public {
        vm.expectRevert("Bridge: not relayer");
        bridge.releaseWVTT(keccak256("tx"), user, 100 ether);
    }

    function test_release_usdt() public {
        // Fund bridge with USDT
        usdt.mint(address(bridge), 10000e6);

        bytes32 txHash = keccak256("vtt-tx-3");
        vm.prank(relayer);
        bridge.releaseUSDT(txHash, user, 1000e6);

        assertEq(usdt.balanceOf(user), 1000e6);
    }

    // ─── Deposit (Ethereum → VTT chain) ───

    function test_deposit_wvtt() public {
        // Mint wVTT to user first
        vm.prank(address(bridge));
        wvtt.mint(user, 1000 ether);

        vm.prank(user);
        bridge.depositWVTT(1000 ether, vttDest);

        // wVTT should be burned
        assertEq(wvtt.balanceOf(user), 0);
        assertEq(bridge.depositNonce(), 1);
    }

    function test_deposit_usdt() public {
        usdt.mint(user, 5000e6);

        vm.prank(user);
        usdt.approve(address(bridge), 5000e6);

        vm.prank(user);
        bridge.depositUSDT(5000e6, vttDest);

        assertEq(usdt.balanceOf(user), 0);
        assertEq(usdt.balanceOf(address(bridge)), 5000e6);
        assertEq(bridge.depositNonce(), 1);
    }

    // ─── Fees ───

    function test_deposit_fee_collected() public {
        usdt.mint(user, 10000e6);

        vm.prank(user);
        usdt.approve(address(bridge), 10000e6);

        vm.prank(user);
        bridge.depositUSDT(10000e6, vttDest);

        // 0.1% of 10000 = 10 USDT
        assertEq(bridge.collectedFees(), 10e6);
    }

    // ─── Admin ───

    function test_set_fee() public {
        bridge.setFee(50); // 0.5%
        assertEq(bridge.protocolFeeBps(), 50);
    }

    function test_set_fee_too_high_reverts() public {
        vm.expectRevert("Bridge: fee too high");
        bridge.setFee(600); // 6% > 5% max
    }

    // ─── Timelock ───

    function test_timelock_setRelayer() public {
        address newRelayer = address(0x1234);

        // Queue the relayer change
        bridge.queueSetRelayer(newRelayer);

        // Try execute immediately -- should revert (timelock active)
        vm.expectRevert("Bridge: timelock active");
        bridge.executeSetRelayer(newRelayer);

        // Warp 2 days
        vm.warp(block.timestamp + 2 days);

        // Execute succeeds
        bridge.executeSetRelayer(newRelayer);
        assertEq(bridge.relayer(), newRelayer);
    }

    function test_timelock_not_queued_reverts() public {
        address newRelayer = address(0x1234);

        // Try execute without queueing
        vm.expectRevert("Bridge: not queued");
        bridge.executeSetRelayer(newRelayer);
    }

    function test_timelock_already_executed_reverts() public {
        address newRelayer = address(0x1234);

        // Queue and execute
        bridge.queueSetRelayer(newRelayer);
        vm.warp(block.timestamp + 2 days);
        bridge.executeSetRelayer(newRelayer);

        // Try execute again
        vm.expectRevert("Bridge: already executed");
        bridge.executeSetRelayer(newRelayer);
    }
}
