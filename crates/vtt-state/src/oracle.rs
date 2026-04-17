use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, Timestamp, H256};

/// Type of data an oracle feed provides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum OracleFeedType {
    /// Price of a specific on-chain asset.
    AssetValuation(H256),
    /// Market price (e.g., "BTC/USD").
    MarketPrice(String),
    /// Interest rate (e.g., "SOFR").
    InterestRate(String),
    /// Custom data feed.
    Custom(String),
}

/// A single oracle value submission from a source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct OracleSubmission {
    pub source: Address,
    pub value: Amount,
    pub timestamp: Timestamp,
}

/// An oracle feed registered in the system.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct OracleFeed {
    /// Unique feed ID.
    pub feed_id: H256,
    /// Human-readable name.
    pub name: String,
    /// What this feed provides.
    pub feed_type: OracleFeedType,
    /// Authorized updaters (oracle node addresses).
    pub authorized_sources: Vec<Address>,
    /// Minimum sources required for a valid value (M-of-N quorum).
    pub quorum: u8,
    /// Maximum staleness in milliseconds before feed is invalid.
    pub max_staleness_ms: u64,
    /// Latest aggregated value (raw u128 carried by `Amount`).
    pub latest_value: Option<Amount>,
    /// Number of decimal places the raw `latest_value` should be scaled by
    /// for human display. Oracle sources submit values already scaled by
    /// `10^decimals`; consumers divide by `10^decimals` to recover the real
    /// number. Defaults to `18` for feeds created before this field existed.
    pub decimals: u8,
    /// Timestamp of the latest aggregated value.
    pub updated_at: Timestamp,
    /// Recent submissions for quorum aggregation.
    pub pending_submissions: Vec<OracleSubmission>,
    /// Block number when this feed was created.
    pub created_at: u64,
}

impl OracleFeed {
    /// Create a new oracle feed with the default 18-decimal scaling.
    pub fn new(
        feed_id: H256,
        name: String,
        feed_type: OracleFeedType,
        authorized_sources: Vec<Address>,
        quorum: u8,
        max_staleness_ms: u64,
    ) -> Self {
        Self::new_with_decimals(
            feed_id,
            name,
            feed_type,
            authorized_sources,
            quorum,
            max_staleness_ms,
            18,
        )
    }

    /// Create a new oracle feed with an explicit decimals scale.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_decimals(
        feed_id: H256,
        name: String,
        feed_type: OracleFeedType,
        authorized_sources: Vec<Address>,
        quorum: u8,
        max_staleness_ms: u64,
        decimals: u8,
    ) -> Self {
        Self {
            feed_id,
            name,
            feed_type,
            authorized_sources,
            quorum: quorum.max(1),
            max_staleness_ms,
            latest_value: None,
            decimals,
            updated_at: 0,
            pending_submissions: Vec::new(),
            created_at: 0,
        }
    }

    /// Submit a new value from an authorized source.
    /// Returns true if the quorum was reached and the value was updated.
    pub fn submit(&mut self, source: Address, value: Amount, timestamp: Timestamp) -> bool {
        if !self.authorized_sources.contains(&source) {
            return false;
        }

        // Remove any previous submission from this source
        self.pending_submissions.retain(|s| s.source != source);

        self.pending_submissions.push(OracleSubmission {
            source,
            value,
            timestamp,
        });

        // Check quorum
        if self.pending_submissions.len() >= self.quorum as usize {
            self.aggregate();
            return true;
        }

        false
    }

    /// Aggregate pending submissions into a final value (median).
    fn aggregate(&mut self) {
        if self.pending_submissions.is_empty() {
            return;
        }

        let mut values: Vec<Amount> = self.pending_submissions.iter().map(|s| s.value).collect();
        values.sort();

        // Use median
        let median = values[values.len() / 2];
        let latest_ts = self
            .pending_submissions
            .iter()
            .map(|s| s.timestamp)
            .max()
            .unwrap_or(0);

        self.latest_value = Some(median);
        self.updated_at = latest_ts;
        self.pending_submissions.clear();
    }

    /// Read the latest value, checking staleness.
    pub fn read(&self, current_time: Timestamp) -> Option<(Amount, Timestamp)> {
        let value = self.latest_value?;
        if self.max_staleness_ms > 0 && current_time > self.updated_at + self.max_staleness_ms {
            return None; // stale
        }
        Some((value, self.updated_at))
    }

    /// Check if the feed has a valid (non-stale) value.
    pub fn is_valid(&self, current_time: Timestamp) -> bool {
        self.read(current_time).is_some()
    }

    /// Check if an address is an authorized source.
    pub fn is_authorized(&self, source: &Address) -> bool {
        self.authorized_sources.contains(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_crypto::blake3_hash;

    fn test_feed() -> OracleFeed {
        let oracle_a = Address::from([0xA0; 20]);
        let oracle_b = Address::from([0xB0; 20]);
        let oracle_c = Address::from([0xC0; 20]);

        OracleFeed::new(
            blake3_hash(b"BTC/USD"),
            "BTC/USD Price".to_string(),
            OracleFeedType::MarketPrice("BTC/USD".to_string()),
            vec![oracle_a, oracle_b, oracle_c],
            2,      // quorum of 2
            60_000, // 60 second staleness
        )
    }

    #[test]
    fn create_feed() {
        let feed = test_feed();
        assert_eq!(feed.quorum, 2);
        assert_eq!(feed.authorized_sources.len(), 3);
        assert!(feed.latest_value.is_none());
    }

    #[test]
    fn single_submission_no_quorum() {
        let mut feed = test_feed();
        let oracle_a = Address::from([0xA0; 20]);

        let reached = feed.submit(oracle_a, Amount::from_vtt(50_000), 1000);
        assert!(!reached);
        assert!(feed.latest_value.is_none());
    }

    #[test]
    fn quorum_reached_updates_value() {
        let mut feed = test_feed();
        let oracle_a = Address::from([0xA0; 20]);
        let oracle_b = Address::from([0xB0; 20]);

        feed.submit(oracle_a, Amount::from_vtt(50_000), 1000);
        let reached = feed.submit(oracle_b, Amount::from_vtt(51_000), 1001);

        assert!(reached);
        assert!(feed.latest_value.is_some());
        // Median of [50000, 51000] = 51000 (index 1 of sorted array len 2)
        assert_eq!(feed.latest_value.unwrap(), Amount::from_vtt(51_000));
    }

    #[test]
    fn three_submissions_median() {
        let mut feed = OracleFeed::new(
            H256::from([0x01; 32]),
            "test".to_string(),
            OracleFeedType::Custom("test".to_string()),
            vec![
                Address::from([0xA0; 20]),
                Address::from([0xB0; 20]),
                Address::from([0xC0; 20]),
            ],
            3, // quorum of 3
            60_000,
        );

        feed.submit(Address::from([0xA0; 20]), Amount::from_vtt(100), 1000);
        feed.submit(Address::from([0xB0; 20]), Amount::from_vtt(200), 1001);
        feed.submit(Address::from([0xC0; 20]), Amount::from_vtt(150), 1002);

        // Median of [100, 150, 200] = 150
        assert_eq!(feed.latest_value.unwrap(), Amount::from_vtt(150));
    }

    #[test]
    fn unauthorized_source_rejected() {
        let mut feed = test_feed();
        let unauthorized = Address::from([0xFF; 20]);

        let reached = feed.submit(unauthorized, Amount::from_vtt(50_000), 1000);
        assert!(!reached);
        assert!(feed.pending_submissions.is_empty());
    }

    #[test]
    fn read_valid_value() {
        let mut feed = test_feed();
        feed.submit(Address::from([0xA0; 20]), Amount::from_vtt(50_000), 1000);
        feed.submit(Address::from([0xB0; 20]), Amount::from_vtt(51_000), 1001);

        let result = feed.read(1500); // within staleness window
        assert!(result.is_some());
        let (value, _ts) = result.unwrap();
        assert_eq!(value, Amount::from_vtt(51_000));
    }

    #[test]
    fn stale_value_returns_none() {
        let mut feed = test_feed();
        feed.submit(Address::from([0xA0; 20]), Amount::from_vtt(50_000), 1000);
        feed.submit(Address::from([0xB0; 20]), Amount::from_vtt(51_000), 1001);

        // Read far in the future (beyond 60s staleness)
        let result = feed.read(100_000);
        assert!(result.is_none());
    }

    #[test]
    fn duplicate_source_replaces() {
        let mut feed = test_feed();
        let oracle_a = Address::from([0xA0; 20]);

        feed.submit(oracle_a, Amount::from_vtt(50_000), 1000);
        feed.submit(oracle_a, Amount::from_vtt(52_000), 1001); // replaces

        assert_eq!(feed.pending_submissions.len(), 1);
        assert_eq!(feed.pending_submissions[0].value, Amount::from_vtt(52_000));
    }

    #[test]
    fn feed_borsh_roundtrip() {
        let mut feed = test_feed();
        feed.submit(Address::from([0xA0; 20]), Amount::from_vtt(50_000), 1000);
        feed.submit(Address::from([0xB0; 20]), Amount::from_vtt(51_000), 1001);

        let bytes = borsh::to_vec(&feed).unwrap();
        let feed2 = OracleFeed::try_from_slice(&bytes).unwrap();
        assert_eq!(feed, feed2);
    }
}
