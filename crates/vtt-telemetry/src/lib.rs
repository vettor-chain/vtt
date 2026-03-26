use prometheus::{Histogram, HistogramOpts, IntCounter, IntGauge, Opts, Registry};

/// Metrics for a VTT node.
pub struct NodeMetrics {
    pub registry: Registry,
    /// Current block height.
    pub block_height: IntGauge,
    /// Number of connected peers.
    pub connected_peers: IntGauge,
    /// Transaction pool size.
    pub txpool_size: IntGauge,
    /// Total blocks imported.
    pub blocks_imported: IntCounter,
    /// Total transactions executed.
    pub transactions_executed: IntCounter,
    /// Block import duration in seconds.
    pub block_import_duration: Histogram,
    /// Current epoch.
    pub current_epoch: IntGauge,
    /// Number of active validators.
    pub active_validators: IntGauge,
}

impl NodeMetrics {
    /// Create a new set of node metrics and register them.
    pub fn new() -> Self {
        let registry = Registry::new();

        let block_height =
            IntGauge::with_opts(Opts::new("vtt_block_height", "Current block height")).unwrap();
        let connected_peers = IntGauge::with_opts(Opts::new(
            "vtt_connected_peers",
            "Number of connected peers",
        ))
        .unwrap();
        let txpool_size =
            IntGauge::with_opts(Opts::new("vtt_txpool_size", "Transaction pool size")).unwrap();
        let blocks_imported = IntCounter::with_opts(Opts::new(
            "vtt_blocks_imported_total",
            "Total blocks imported",
        ))
        .unwrap();
        let transactions_executed = IntCounter::with_opts(Opts::new(
            "vtt_transactions_executed_total",
            "Total transactions executed",
        ))
        .unwrap();
        let block_import_duration = Histogram::with_opts(HistogramOpts::new(
            "vtt_block_import_duration_seconds",
            "Block import duration",
        ))
        .unwrap();
        let current_epoch =
            IntGauge::with_opts(Opts::new("vtt_current_epoch", "Current DPoS epoch")).unwrap();
        let active_validators = IntGauge::with_opts(Opts::new(
            "vtt_active_validators",
            "Number of active validators",
        ))
        .unwrap();

        registry.register(Box::new(block_height.clone())).unwrap();
        registry
            .register(Box::new(connected_peers.clone()))
            .unwrap();
        registry.register(Box::new(txpool_size.clone())).unwrap();
        registry
            .register(Box::new(blocks_imported.clone()))
            .unwrap();
        registry
            .register(Box::new(transactions_executed.clone()))
            .unwrap();
        registry
            .register(Box::new(block_import_duration.clone()))
            .unwrap();
        registry.register(Box::new(current_epoch.clone())).unwrap();
        registry
            .register(Box::new(active_validators.clone()))
            .unwrap();

        Self {
            registry,
            block_height,
            connected_peers,
            txpool_size,
            blocks_imported,
            transactions_executed,
            block_import_duration,
            current_epoch,
            active_validators,
        }
    }

    /// Export all metrics in Prometheus text format.
    pub fn export(&self) -> String {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }
}

impl Default for NodeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_metrics() {
        let metrics = NodeMetrics::new();
        assert_eq!(metrics.block_height.get(), 0);
        assert_eq!(metrics.connected_peers.get(), 0);
    }

    #[test]
    fn update_and_export_metrics() {
        let metrics = NodeMetrics::new();
        metrics.block_height.set(42);
        metrics.connected_peers.set(5);
        metrics.blocks_imported.inc();
        metrics.blocks_imported.inc();
        metrics.transactions_executed.inc_by(10);

        let output = metrics.export();
        assert!(output.contains("vtt_block_height 42"));
        assert!(output.contains("vtt_connected_peers 5"));
        assert!(output.contains("vtt_blocks_imported_total 2"));
        assert!(output.contains("vtt_transactions_executed_total 10"));
    }

    #[test]
    fn histogram_records() {
        let metrics = NodeMetrics::new();
        metrics.block_import_duration.observe(0.05);
        metrics.block_import_duration.observe(0.10);

        let output = metrics.export();
        assert!(output.contains("vtt_block_import_duration_seconds"));
    }
}
