use vtt_chain::Chain;
use vtt_consensus::ConsensusEngine;
use vtt_crypto::{blake3_hash, merkle_root, Keypair};
use vtt_executor::execute_block_transactions;

use vtt_primitives::amount::Amount;
use vtt_primitives::block::{Block, BlockHeader};
use vtt_primitives::chain::{ConsensusParams, GasConfig};
use vtt_primitives::transaction::{SignedTransaction, TransactionAction, TransactionPayload};
use vtt_primitives::{Address, ChainId, Signature, H256};
use vtt_state::account::{AccountState, StakingState};
use vtt_state::StateDB;

/// Full chain lifecycle: genesis → block production → transactions → staking
#[test]
fn full_chain_lifecycle() {
    // 1. Setup: genesis with one validator and funded accounts
    let val_kp = Keypair::from_seed(&[0x10; 32]);
    let val_addr = val_kp.address();
    let alice_kp = Keypair::from_seed(&[0x01; 32]);
    let alice_addr = alice_kp.address();
    let bob_addr = Address::from([0x02; 20]);

    let gas_config = GasConfig::default();
    let consensus = ConsensusEngine::new(ConsensusParams {
        epoch_length: 100,
        active_validators: 1,
        min_self_stake: Amount::from_vtt(100),
        ..Default::default()
    });

    let mut state = StateDB::new();

    // Fund validator
    let mut val_account = AccountState::with_balance(Amount::from_vtt(500_000));
    val_account.staking = Some(StakingState {
        total_stake: Amount::from_vtt(100_000),
        self_stake: Amount::from_vtt(100_000),
        commission_bps: 500,
        active: true,
        delegations: Vec::new(),
        unbonding: Vec::new(),
    });
    state.put_account(val_addr, val_account);

    // Fund alice
    state.put_account(
        alice_addr,
        AccountState::with_balance(Amount::from_vtt(10_000)),
    );

    let state_root = state.compute_state_root();

    // 2. Create genesis block
    let genesis = Block::new(
        BlockHeader {
            version: 1,
            chain_id: ChainId::RELAY,
            number: 0,
            parent_hash: H256::ZERO,
            transactions_root: merkle_root(&[]),
            state_root,
            receipts_root: merkle_root(&[]),
            validator: val_addr,
            epoch: 0,
            slot: 0,
            timestamp: 1_700_000_000_000,
            gas_limit: 10_000_000,
            gas_used: 0,
            cross_chain_root: None,
            signature: Signature::ZERO,
        },
        vec![],
    );

    let mut chain = Chain::new(consensus, gas_config.clone());
    let gen_hash = chain.init_genesis(genesis, state).unwrap();
    assert_eq!(chain.height(), Some(0));

    // 3. Produce block 1 with a transfer: alice → bob
    let tx = make_tx(
        &alice_kp,
        0,
        TransactionAction::Transfer {
            to: bob_addr,
            amount: Amount::from_vtt(100),
        },
    );

    let (receipts, gas_used) =
        execute_block_transactions(chain.state_mut(), &[tx.clone()], &gas_config, 10_000_000);
    assert!(receipts[0].success);

    let state_root = chain.state_mut().compute_state_root();
    let tx_root = merkle_root(&[blake3_hash(&tx.payload_bytes())]);
    let parent_hash = blake3_hash(&chain.get_block(&gen_hash).unwrap().header.signable_bytes());

    let block1 = Block::new(
        BlockHeader {
            version: 1,
            chain_id: ChainId::RELAY,
            number: 1,
            parent_hash,
            transactions_root: tx_root,
            state_root,
            receipts_root: merkle_root(
                &receipts
                    .iter()
                    .map(|r| blake3_hash(&borsh::to_vec(r).unwrap()))
                    .collect::<Vec<_>>(),
            ),
            validator: val_addr,
            epoch: 0,
            slot: 1,
            timestamp: 1_700_000_003_000,
            gas_limit: 10_000_000,
            gas_used,
            cross_chain_root: None,
            signature: Signature::ZERO,
        },
        vec![tx],
    );

    let result = chain.import_block(block1).unwrap();
    assert!(result.is_new_head);
    assert_eq!(chain.height(), Some(1));

    // 4. Verify balances
    assert_eq!(chain.get_balance_of(&bob_addr), Amount::from_vtt(100));
    assert!(chain.get_balance_of(&alice_addr) < Amount::from_vtt(10_000)); // deducted transfer + gas

    // 5. Verify block retrieval
    let block = chain.get_block_by_number(1).unwrap();
    assert_eq!(block.tx_count(), 1);
}

/// Test asset creation and transfer through executor
#[test]
fn asset_tokenization_lifecycle() {
    let issuer_kp = Keypair::from_seed(&[0x20; 32]);
    let issuer_addr = issuer_kp.address();
    let investor_addr = Address::from([0x30; 20]);

    let mut state = StateDB::new();
    state.put_account(
        issuer_addr,
        AccountState::with_balance(Amount::from_vtt(100_000)),
    );

    let gas_config = GasConfig::default();

    // Create asset
    let create_tx = make_tx(
        &issuer_kp,
        0,
        TransactionAction::CreateAssetClass {
            name: "Test Real Estate".to_string(),
            symbol: "TRE".to_string(),
            metadata_uri: "ipfs://test".to_string(),
            total_supply: Amount::from_vtt(1_000_000),
        },
    );

    let (receipts, _) =
        execute_block_transactions(&mut state, &[create_tx], &gas_config, 10_000_000);
    assert!(receipts[0].success);
    assert_eq!(state.asset_count(), 1);

    // Find the asset
    let (asset_id, asset_name, asset_symbol, asset_tradeable) = {
        let (id, asset) = state.iter_assets().next().unwrap();
        (
            *id,
            asset.name.clone(),
            asset.symbol.clone(),
            asset.is_tradeable(),
        )
    };
    assert_eq!(asset_name, "Test Real Estate");
    assert_eq!(asset_symbol, "TRE");
    assert!(asset_tradeable);

    // Check issuer received the total supply
    let issuer_ownership = state.get_ownership(&asset_id, &issuer_addr);
    assert_eq!(issuer_ownership.available, Amount::from_vtt(1_000_000));

    // Transfer asset to investor
    let transfer_tx = make_tx(
        &issuer_kp,
        1,
        TransactionAction::AssetTransfer {
            asset_id,
            to: investor_addr,
            amount: Amount::from_vtt(10_000),
        },
    );

    let (receipts2, _) =
        execute_block_transactions(&mut state, &[transfer_tx], &gas_config, 10_000_000);
    assert!(receipts2[0].success);

    let investor_ownership = state.get_ownership(&asset_id, &investor_addr);
    assert_eq!(investor_ownership.available, Amount::from_vtt(10_000));

    let issuer_ownership = state.get_ownership(&asset_id, &issuer_addr);
    assert_eq!(issuer_ownership.available, Amount::from_vtt(990_000));
}

/// Test governance proposal lifecycle
#[test]
fn governance_lifecycle() {
    use vtt_consensus::governance::{
        GovernanceSystem, ProposalAction, ProposalStatus, VOTING_PERIOD_BLOCKS,
    };
    use vtt_primitives::Vote;

    let mut gov = GovernanceSystem::new();

    // Create proposal
    let proposer = Address::from([0x01; 20]);
    let id = gov.create_proposal(
        proposer,
        ProposalAction::TreasurySpend {
            recipient: Address::from([0x50; 20]),
            amount: Amount::from_vtt(10_000),
        },
        "Fund ecosystem development".to_string(),
        1000,
    );

    // Multiple validators vote
    let total_staked = Amount::from_vtt(1_000_000);
    gov.vote(
        &id,
        Address::from([0x10; 20]),
        Vote::Yes,
        Amount::from_vtt(200_000),
        1500,
    )
    .unwrap();
    gov.vote(
        &id,
        Address::from([0x11; 20]),
        Vote::Yes,
        Amount::from_vtt(150_000),
        1500,
    )
    .unwrap();
    gov.vote(
        &id,
        Address::from([0x12; 20]),
        Vote::No,
        Amount::from_vtt(50_000),
        1500,
    )
    .unwrap();

    // Finalize after voting period
    let status = gov
        .finalize(&id, total_staked, 1000 + VOTING_PERIOD_BLOCKS)
        .unwrap();
    assert_eq!(status, ProposalStatus::Passed);

    // Execute
    gov.mark_executed(&id).unwrap();
    assert_eq!(gov.get(&id).unwrap().status, ProposalStatus::Executed);
}

/// Test cross-chain messaging flow
#[test]
fn cross_chain_messaging_flow() {
    use vtt_multichain::messaging::{CrossChainPayload, MessageInbox, MessageOutbox};

    let mut outbox_chain1 = MessageOutbox::new(ChainId::new(1));
    let mut inbox_chain2 = MessageInbox::new(ChainId::new(2));

    // Send 3 messages from chain 1 to chain 2
    for i in 0..3 {
        outbox_chain1
            .send(
                ChainId::new(2),
                Address::from([0x01; 20]),
                Address::from([0x02; 20]),
                CrossChainPayload::VttTransfer {
                    amount: Amount::from_vtt(100 * (i + 1)),
                },
                i as u64,
            )
            .unwrap();
    }

    assert_eq!(outbox_chain1.pending_count(), 3);
    assert_ne!(outbox_chain1.merkle_root(), H256::ZERO);

    // Relay picks up messages
    let messages = outbox_chain1.drain();
    assert_eq!(messages.len(), 3);
    assert_eq!(outbox_chain1.pending_count(), 0);

    // Deliver to chain 2
    for msg in &messages {
        inbox_chain2.receive(msg.clone()).unwrap();
    }
    assert_eq!(inbox_chain2.pending_count(), 3);

    // Process messages
    while let Some(msg) = inbox_chain2.next_pending() {
        match &msg.payload {
            CrossChainPayload::VttTransfer { amount } => {
                assert!(!amount.is_zero());
            }
            _ => panic!("unexpected payload"),
        }
    }
    assert_eq!(inbox_chain2.processed_count(), 3);
}

/// Test shared security validator assignment
#[test]
fn shared_security_rotation() {
    use vtt_multichain::shared_security::assign_validators;

    let validators: Vec<Address> = (1..=21).map(|i| Address::from([i; 20])).collect();

    // Assign 11 validators to chain 1 over multiple epochs
    let mut all_assigned = std::collections::HashSet::new();
    for epoch in 0..50 {
        let assignment = assign_validators(&validators, ChainId::new(1), epoch, 11);
        assert_eq!(assignment.validators.len(), 11);

        // No duplicates within assignment
        let unique: std::collections::HashSet<_> = assignment.validators.iter().collect();
        assert_eq!(unique.len(), 11);

        for v in &assignment.validators {
            all_assigned.insert(*v);
        }
    }

    // All 21 validators should have been assigned at least once
    assert_eq!(all_assigned.len(), 21);
}

fn make_tx(keypair: &Keypair, nonce: u64, action: TransactionAction) -> SignedTransaction {
    let payload = TransactionPayload {
        chain_id: ChainId::RELAY,
        nonce,
        gas_price: Amount::from_raw(1_000_000_000),
        gas_limit: 200_000,
        action,
    };
    let bytes = borsh::to_vec(&payload).unwrap();
    SignedTransaction {
        payload,
        signature: keypair.sign(&bytes),
        public_key: keypair.public_key(),
    }
}
