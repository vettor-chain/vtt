// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./WVTT.sol";
import "forge-std/interfaces/IERC20.sol";

/**
 * @title VTT Bridge
 * @notice Custodial bridge between VTT chain and Ethereum/Base.
 *
 * Withdraw (VTT chain → Ethereum):
 *   1. User burns tokens on VTT chain (BridgeWithdraw tx)
 *   2. Relayer calls release() here to mint wVTT or send USDT
 *
 * Deposit (Ethereum → VTT chain):
 *   1. User calls deposit() here, locking USDT or burning wVTT
 *   2. Relayer mints vUSDT or credits VTT on VTT chain
 */
contract VTTBridge {
    WVTT public immutable wvtt;
    IERC20 public immutable usdt;

    address public relayer;
    address public owner;

    uint256 public protocolFeeBps; // e.g. 10 = 0.1%
    uint256 public collectedFeesWVTT;
    uint256 public collectedFeesUSDT;

    bool public paused;

    // Nonce to prevent replay attacks
    mapping(bytes32 => bool) public processedWithdrawals;

    // Deposit tracking
    uint256 public depositNonce;

    event Deposit(
        uint256 indexed nonce,
        address indexed sender,
        address token,        // address(0) = wVTT burned, USDT address = USDT locked
        uint256 amount,
        uint256 fee,
        bytes32 vttDestination // 20-byte VTT chain address, left-padded
    );

    event Release(
        bytes32 indexed vttTxHash,
        address indexed recipient,
        address token,
        uint256 amount
    );

    event RelayerUpdated(address indexed oldRelayer, address indexed newRelayer);
    event FeeUpdated(uint256 oldFee, uint256 newFee);
    event FeesWithdrawn(address indexed to, uint256 wvttAmount, uint256 usdtAmount);
    event Paused(address indexed by);
    event Unpaused(address indexed by);

    modifier onlyRelayer() {
        require(msg.sender == relayer, "Bridge: not relayer");
        _;
    }

    modifier onlyOwner() {
        require(msg.sender == owner, "Bridge: not owner");
        _;
    }

    modifier whenNotPaused() {
        require(!paused, "Bridge: paused");
        _;
    }

    function pause() external onlyOwner {
        paused = true;
        emit Paused(msg.sender);
    }

    function unpause() external onlyOwner {
        paused = false;
        emit Unpaused(msg.sender);
    }

    constructor(address _wvtt, address _usdt, address _relayer, uint256 _feeBps) {
        wvtt = WVTT(_wvtt);
        usdt = IERC20(_usdt);
        relayer = _relayer;
        owner = msg.sender;
        protocolFeeBps = _feeBps;
    }

    // ─── DEPOSIT (Ethereum → VTT chain) ─────────────────────────────────

    /**
     * @notice Deposit wVTT to receive VTT on VTT chain.
     *         Burns wVTT and emits event for relayer.
     */
    function depositWVTT(uint256 amount, bytes32 vttDestination) external whenNotPaused {
        require(amount > 0, "Bridge: zero amount");

        uint256 fee = (amount * protocolFeeBps) / 10000;
        uint256 net = amount - fee;

        // Burn the wVTT (bridge must be authorized on WVTT contract)
        wvtt.burn(msg.sender, amount);

        if (fee > 0) {
            collectedFeesWVTT += fee;
        }

        depositNonce++;
        emit Deposit(depositNonce, msg.sender, address(0), net, fee, vttDestination);
    }

    /**
     * @notice Deposit USDT to receive vUSDT on VTT chain.
     *         Locks USDT in this contract and emits event for relayer.
     */
    function depositUSDT(uint256 amount, bytes32 vttDestination) external whenNotPaused {
        require(amount > 0, "Bridge: zero amount");

        uint256 fee = (amount * protocolFeeBps) / 10000;
        uint256 net = amount - fee;

        // Transfer USDT from sender to this contract
        require(usdt.transferFrom(msg.sender, address(this), amount), "Bridge: USDT transfer failed");

        if (fee > 0) {
            collectedFeesUSDT += fee;
        }

        depositNonce++;
        emit Deposit(depositNonce, msg.sender, address(usdt), net, fee, vttDestination);
    }

    // ─── RELEASE (VTT chain → Ethereum) — called by relayer ─────────────

    /**
     * @notice Release wVTT to recipient after verified burn on VTT chain.
     * @param vttTxHash The BridgeWithdraw transaction hash on VTT chain (prevents replay)
     */
    function releaseWVTT(
        bytes32 vttTxHash,
        address recipient,
        uint256 amount
    ) external onlyRelayer whenNotPaused {
        require(!processedWithdrawals[vttTxHash], "Bridge: already processed");
        processedWithdrawals[vttTxHash] = true;

        wvtt.mint(recipient, amount);
        emit Release(vttTxHash, recipient, address(wvtt), amount);
    }

    /**
     * @notice Release USDT to recipient after verified burn on VTT chain.
     */
    function releaseUSDT(
        bytes32 vttTxHash,
        address recipient,
        uint256 amount
    ) external onlyRelayer whenNotPaused {
        require(!processedWithdrawals[vttTxHash], "Bridge: already processed");
        require(usdt.balanceOf(address(this)) >= amount, "Bridge: insufficient USDT reserve");
        processedWithdrawals[vttTxHash] = true;

        require(usdt.transfer(recipient, amount), "Bridge: USDT transfer failed");
        emit Release(vttTxHash, recipient, address(usdt), amount);
    }

    // ─── ADMIN ──────────────────────────────────────────────────────────

    function setRelayer(address _relayer) external onlyOwner {
        emit RelayerUpdated(relayer, _relayer);
        relayer = _relayer;
    }

    function setFee(uint256 _feeBps) external onlyOwner {
        require(_feeBps <= 500, "Bridge: fee too high"); // max 5%
        emit FeeUpdated(protocolFeeBps, _feeBps);
        protocolFeeBps = _feeBps;
    }

    function withdrawFees(address to) external onlyOwner {
        require(to != address(0), "Bridge: zero address");

        uint256 wvttFees = collectedFeesWVTT;
        uint256 usdtFees = collectedFeesUSDT;

        if (wvttFees > 0) {
            collectedFeesWVTT = 0;
            wvtt.mint(to, wvttFees);
        }

        if (usdtFees > 0) {
            collectedFeesUSDT = 0;
            require(usdt.transfer(to, usdtFees), "Bridge: USDT transfer failed");
        }

        emit FeesWithdrawn(to, wvttFees, usdtFees);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        require(newOwner != address(0), "Bridge: zero address");
        owner = newOwner;
    }
}
