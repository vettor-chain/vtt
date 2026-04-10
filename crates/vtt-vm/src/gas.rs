use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Gas meter for tracking and limiting gas consumption during contract execution.
#[derive(Clone)]
pub struct GasMeter {
    used: Arc<AtomicU64>,
    limit: u64,
}

impl GasMeter {
    pub fn new(limit: u64) -> Self {
        Self {
            used: Arc::new(AtomicU64::new(0)),
            limit,
        }
    }

    /// Consume gas. Returns false if limit exceeded.
    pub fn consume(&self, amount: u64) -> bool {
        let prev = self.used.fetch_add(amount, Ordering::SeqCst);
        prev + amount <= self.limit
    }

    /// Get the amount of gas used so far.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::SeqCst)
    }

    /// Get the gas limit.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Get remaining gas.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used())
    }
}

/// Gas costs for host function calls.
pub struct GasCosts;

impl GasCosts {
    pub const STORAGE_READ: u64 = 200;
    pub const STORAGE_WRITE: u64 = 5000;
    pub const STORAGE_WRITE_NEW: u64 = 20000;
    pub const HOST_CALL_BASE: u64 = 100;
    pub const TRANSFER: u64 = 2100;
    pub const LOG_BASE: u64 = 375;
    pub const LOG_PER_BYTE: u64 = 8;
    pub const COMPLIANCE_CHECK: u64 = 1000;
    pub const ASSET_MINT: u64 = 10000;
    pub const ASSET_TRANSFER: u64 = 5000;
    pub const ORACLE_READ: u64 = 500;

    // --- VM resource limits ---

    /// Maximum contract bytecode size: 512 KB.
    pub const MAX_CONTRACT_SIZE: usize = 512 * 1024;
    /// Maximum WASM linear memory pages (256 pages = 16 MB).
    pub const MAX_WASM_MEMORY_PAGES: u32 = 256;
    /// Maximum nested call stack depth.
    pub const MAX_CALL_STACK_DEPTH: u32 = 64;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gas_meter_basic() {
        let meter = GasMeter::new(1000);
        assert_eq!(meter.used(), 0);
        assert_eq!(meter.remaining(), 1000);

        assert!(meter.consume(300));
        assert_eq!(meter.used(), 300);
        assert_eq!(meter.remaining(), 700);
    }

    #[test]
    fn gas_meter_exceeds_limit() {
        let meter = GasMeter::new(100);
        assert!(meter.consume(50));
        assert!(!meter.consume(60));
    }

    #[test]
    fn gas_meter_clone_shares_state() {
        let meter = GasMeter::new(1000);
        let meter2 = meter.clone();
        assert!(meter.consume(100));
        assert_eq!(meter2.used(), 100);
    }
}
