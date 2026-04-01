// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/interfaces/IERC20.sol";

/**
 * @title Wrapped VTT (wVTT)
 * @notice ERC-20 representation of VTT on Ethereum/Base.
 *         Only the bridge contract can mint and burn.
 */
contract WVTT is IERC20 {
    string public constant name = "Wrapped VTT";
    string public constant symbol = "wVTT";
    uint8 public constant decimals = 18;

    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    address public bridge;
    address public owner;

    event BridgeUpdated(address indexed oldBridge, address indexed newBridge);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    modifier onlyBridge() {
        require(msg.sender == bridge, "WVTT: caller is not the bridge");
        _;
    }

    modifier onlyOwner() {
        require(msg.sender == owner, "WVTT: caller is not the owner");
        _;
    }

    constructor(address _bridge) {
        owner = msg.sender;
        bridge = _bridge;
    }

    function setBridge(address _bridge) external onlyOwner {
        emit BridgeUpdated(bridge, _bridge);
        bridge = _bridge;
    }

    function transferOwnership(address newOwner) external onlyOwner {
        require(newOwner != address(0), "WVTT: zero address");
        emit OwnershipTransferred(owner, newOwner);
        owner = newOwner;
    }

    // --- Bridge-only mint/burn ---

    function mint(address to, uint256 amount) external onlyBridge {
        totalSupply += amount;
        balanceOf[to] += amount;
        emit Transfer(address(0), to, amount);
    }

    function burn(address from, uint256 amount) external onlyBridge {
        require(balanceOf[from] >= amount, "WVTT: insufficient balance");
        balanceOf[from] -= amount;
        totalSupply -= amount;
        emit Transfer(from, address(0), amount);
    }

    // --- Standard ERC-20 ---

    function transfer(address to, uint256 amount) external returns (bool) {
        require(balanceOf[msg.sender] >= amount, "WVTT: insufficient balance");
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
        require(balanceOf[from] >= amount, "WVTT: insufficient balance");
        require(allowance[from][msg.sender] >= amount, "WVTT: insufficient allowance");
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
        return true;
    }
}
