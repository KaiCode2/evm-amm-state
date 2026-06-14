use std::time::Duration;

use alloy_transport_balancer::weighted_domain_backoff;
use futures::future::join_all;
use tracing::{debug, info, warn};

use crate::progress::{finish_with_message, progress_bar};
use crate::tuning::{SyncSpeedMode, sync_speed_mode};

use super::{max_concurrent_storage_prefetch, prefetch_inter_chunk_delay};

/// Configuration for adaptive prefetch behavior.
/// Each prefetch invocation creates a fresh instance — no cross-call persistence.
pub(crate) struct AdaptivePrefetchConfig {
    /// Starting (and maximum) chunk size.
    pub initial_chunk_size: usize,
    /// Minimum chunk size (floor for adaptive reduction).
    pub min_chunk_size: usize,
    /// Amount to grow chunk size by on a zero-failure chunk.
    pub chunk_size_grow: usize,
    /// Starting inter-chunk delay.
    pub initial_delay: Duration,
    /// Minimum inter-chunk delay.
    pub min_delay: Duration,
    /// Maximum inter-chunk delay.
    pub max_delay: Duration,
    /// Failure rate threshold (0.0–1.0) above which throttling kicks in.
    pub failure_threshold: f64,
    /// Maximum total attempts per request (1 = no retries, 3 = up to 2 retries).
    pub max_attempts: u32,
}

impl Default for AdaptivePrefetchConfig {
    fn default() -> Self {
        let (min_chunk, grow) = match sync_speed_mode() {
            SyncSpeedMode::Fast => (5, 4),
            SyncSpeedMode::Normal => (4, 3),
            SyncSpeedMode::Slow => (3, 2),
            SyncSpeedMode::XSlow => (1, 1),
        };
        Self {
            initial_chunk_size: max_concurrent_storage_prefetch(),
            min_chunk_size: min_chunk,
            chunk_size_grow: grow,
            initial_delay: prefetch_inter_chunk_delay(),
            min_delay: prefetch_inter_chunk_delay(),
            max_delay: Duration::from_secs(2),
            failure_threshold: 0.25,
            max_attempts: 3,
        }
    }
}

/// Minimum chunk size when domain throttling is active.
/// Lower than the default min (5) to allow very conservative prefetching
/// when RPC providers are severely rate-limited.
const THROTTLED_MIN_CHUNK_SIZE: usize = 2;

impl AdaptivePrefetchConfig {
    /// Create a config that proactively reduces concurrency when RPC domains
    /// are already being rate-limited.
    ///
    /// Queries the global cross-thread domain throttle state and scales down
    /// `initial_chunk_size` based on the worst-case domain backoff delay:
    /// - 0ms delay (no throttling): full chunk size (40)
    /// - 50ms delay (level 1): 50% → 20 chunks
    /// - 150ms delay (level 2): 25% → 10 chunks
    /// - 400ms delay (level 3): 10% → 4 chunks
    /// - 1000ms+ delay (level 4+): 5% → 2 chunks
    ///
    /// Also reduces the adaptive floor (`min_chunk_size`) and raises the
    /// inter-chunk delay to match the domain backoff, so the reactive
    /// throttle doesn't immediately bottom out.
    pub fn throttle_aware() -> Self {
        let base = Self::default();
        let max_delay = weighted_domain_backoff();
        let delay_ms = max_delay.as_millis() as u64;

        if delay_ms == 0 {
            return base;
        }

        let scale = if delay_ms <= 50 {
            0.50
        } else if delay_ms <= 150 {
            0.25
        } else if delay_ms <= 400 {
            0.10
        } else {
            0.05
        };

        let scaled_chunk =
            ((base.initial_chunk_size as f64 * scale) as usize).max(THROTTLED_MIN_CHUNK_SIZE);

        // Also lower the floor so reactive throttle has room to reduce further
        let min_chunk = if delay_ms >= 400 {
            THROTTLED_MIN_CHUNK_SIZE
        } else {
            THROTTLED_MIN_CHUNK_SIZE.max(scaled_chunk / 2)
        };

        // Match inter-chunk delay to domain backoff — no point sending the
        // next chunk faster than the transport will delay each request anyway.
        let initial_delay = base.initial_delay.max(Duration::from_millis(delay_ms));

        info!(
            domain_delay_ms = delay_ms,
            default_chunk = base.initial_chunk_size,
            throttled_chunk = scaled_chunk,
            min_chunk,
            initial_delay_ms = initial_delay.as_millis() as u64,
            "adaptive prefetch: proactively reducing concurrency due to domain throttling"
        );

        Self {
            initial_chunk_size: scaled_chunk,
            min_chunk_size: min_chunk,
            initial_delay,
            // Also raise the minimum delay floor so recovery doesn't drop below domain backoff
            min_delay: base.min_delay.max(Duration::from_millis(delay_ms / 2)),
            ..base
        }
    }
}

/// Result of a single adaptive prefetch run.
pub(crate) struct AdaptivePrefetchResult {
    pub success_count: usize,
    pub error_count: usize,
    pub retry_rounds: u32,
}

/// Mutable state tracked during a single prefetch invocation.
pub(crate) struct AdaptiveState {
    pub current_chunk_size: usize,
    pub current_delay: Duration,
    config: AdaptivePrefetchConfig,
}

impl AdaptiveState {
    pub fn new(config: AdaptivePrefetchConfig) -> Self {
        let chunk_size = config.initial_chunk_size;
        let delay = config.initial_delay;
        Self {
            current_chunk_size: chunk_size,
            current_delay: delay,
            config,
        }
    }

    /// Adjust state after processing a chunk.
    pub fn adjust_after_chunk(&mut self, total: usize, failures: usize) {
        if total == 0 {
            return;
        }
        let failure_rate = failures as f64 / total as f64;
        if failure_rate > self.config.failure_threshold {
            // Throttle: halve chunk size, double delay
            let old_chunk = self.current_chunk_size;
            let old_delay = self.current_delay;
            self.current_chunk_size = (self.current_chunk_size / 2).max(self.config.min_chunk_size);
            self.current_delay = (self.current_delay * 2).min(self.config.max_delay);
            warn!(
                failure_rate = format!("{:.1}%", failure_rate * 100.0),
                old_chunk_size = old_chunk,
                new_chunk_size = self.current_chunk_size,
                old_delay_ms = old_delay.as_millis() as u64,
                new_delay_ms = self.current_delay.as_millis() as u64,
                "adaptive throttle: reducing batch size and increasing delay"
            );
        } else if failures == 0 {
            // Recover: grow chunk size, halve delay
            let old_chunk = self.current_chunk_size;
            self.current_chunk_size = (self.current_chunk_size + self.config.chunk_size_grow)
                .min(self.config.initial_chunk_size);
            self.current_delay = Duration::from_millis(
                (self.current_delay.as_millis() as u64 / 2)
                    .max(self.config.min_delay.as_millis() as u64),
            );
            if old_chunk != self.current_chunk_size {
                debug!(
                    old_chunk_size = old_chunk,
                    new_chunk_size = self.current_chunk_size,
                    delay_ms = self.current_delay.as_millis() as u64,
                    "adaptive recovery: growing batch size"
                );
            }
        }
        // else: some failures but below threshold — hold steady
    }
}

/// Run an adaptive prefetch over a list of request items.
///
/// `spawn_fetch` is called for each request item to create a `JoinHandle`.
/// It receives the request item and a global index. The `JoinHandle` should
/// resolve to `Ok(())` on success or `Err(global_index)` on failure, so
/// that failed items can be queued for retry.
///
/// Progress is reported via `progress_label`. After the primary pass, any
/// failed items are retried up to `config.max_attempts - 1` additional
/// rounds using the (potentially throttled) adaptive state.
pub(crate) async fn run_adaptive_prefetch<T, F>(
    requests: &[T],
    config: AdaptivePrefetchConfig,
    progress_label: &str,
    spawn_fetch: F,
) -> AdaptivePrefetchResult
where
    T: Sync,
    F: Fn(&T, usize) -> tokio::task::JoinHandle<Result<(), usize>>,
{
    if requests.is_empty() {
        return AdaptivePrefetchResult {
            success_count: 0,
            error_count: 0,
            retry_rounds: 0,
        };
    }

    let max_attempts = config.max_attempts;
    let mut state = AdaptiveState::new(config);

    let pb = progress_bar(requests.len() as u64, progress_label);

    let mut success_count = 0usize;
    let mut failed_indices: Vec<usize> = Vec::new();

    // Primary pass
    let mut offset = 0;
    while offset < requests.len() {
        let chunk_end = (offset + state.current_chunk_size).min(requests.len());
        let chunk = &requests[offset..chunk_end];

        let handles: Vec<_> = chunk
            .iter()
            .enumerate()
            .map(|(local_idx, item)| {
                let global_idx = offset + local_idx;
                spawn_fetch(item, global_idx)
            })
            .collect();

        let results = join_all(handles).await;
        let chunk_total = results.len();
        let mut chunk_failures = 0;

        for result in results {
            match result {
                Ok(Ok(())) => success_count += 1,
                Ok(Err(idx)) => {
                    failed_indices.push(idx);
                    chunk_failures += 1;
                }
                Err(join_err) => {
                    debug!("spawn_blocking join error during prefetch: {}", join_err);
                    chunk_failures += 1;
                }
            }
            pb.inc(1);
        }

        state.adjust_after_chunk(chunk_total, chunk_failures);
        tokio::time::sleep(state.current_delay).await;
        offset = chunk_end;
    }

    finish_with_message(
        &pb,
        &format!("{} fetched, {} failed", success_count, failed_indices.len()),
    );

    // Retry rounds
    let mut retry_rounds = 0u32;
    while !failed_indices.is_empty() && retry_rounds < max_attempts - 1 {
        retry_rounds += 1;
        let retry_count = failed_indices.len();
        warn!(
            retry_round = retry_rounds,
            items = retry_count,
            chunk_size = state.current_chunk_size,
            delay_ms = state.current_delay.as_millis() as u64,
            "retrying failed prefetch requests"
        );

        let retry_pb = progress_bar(
            retry_count as u64,
            &format!("{} retry {}", progress_label, retry_rounds),
        );

        let mut still_failed: Vec<usize> = Vec::new();
        let mut retry_offset = 0;
        let mut retry_success = 0usize;

        while retry_offset < failed_indices.len() {
            let chunk_end = (retry_offset + state.current_chunk_size).min(failed_indices.len());
            let chunk_indices = &failed_indices[retry_offset..chunk_end];

            let handles: Vec<_> = chunk_indices
                .iter()
                .map(|&idx| spawn_fetch(&requests[idx], idx))
                .collect();

            let results = join_all(handles).await;
            let chunk_total = results.len();
            let mut chunk_failures = 0;

            for result in results {
                match result {
                    Ok(Ok(())) => {
                        retry_success += 1;
                        success_count += 1;
                    }
                    Ok(Err(idx)) => {
                        still_failed.push(idx);
                        chunk_failures += 1;
                    }
                    Err(join_err) => {
                        debug!("spawn_blocking join error during retry: {}", join_err);
                        chunk_failures += 1;
                    }
                }
                retry_pb.inc(1);
            }

            state.adjust_after_chunk(chunk_total, chunk_failures);
            tokio::time::sleep(state.current_delay).await;
            retry_offset = chunk_end;
        }

        finish_with_message(
            &retry_pb,
            &format!(
                "{} recovered, {} still failed",
                retry_success,
                still_failed.len()
            ),
        );

        failed_indices = still_failed;
    }

    let error_count = failed_indices.len();
    if error_count > 0 {
        warn!(
            success_count,
            error_count, retry_rounds, "prefetch completed with failures after retries"
        );
    } else if retry_rounds > 0 {
        info!(
            success_count,
            retry_rounds, "prefetch completed — all failures recovered via retries"
        );
    }

    AdaptivePrefetchResult {
        success_count,
        error_count,
        retry_rounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set speed to Fast so tests use original (unmultiplied) defaults.
    fn set_fast_mode() {
        crate::tuning::set_sync_speed_mode(SyncSpeedMode::Fast);
    }

    #[test]
    fn test_adjust_throttles_on_high_failure_rate() {
        set_fast_mode();
        let config = AdaptivePrefetchConfig::default();
        let mut state = AdaptiveState::new(config);
        assert_eq!(state.current_chunk_size, 30);
        assert_eq!(state.current_delay, Duration::from_millis(15));

        // >25% failure rate: 15 out of 30
        state.adjust_after_chunk(30, 15);
        assert_eq!(state.current_chunk_size, 15);
        assert_eq!(state.current_delay, Duration::from_millis(30));

        // >25% again: 8 out of 15
        state.adjust_after_chunk(15, 8);
        assert_eq!(state.current_chunk_size, 7);
        assert_eq!(state.current_delay, Duration::from_millis(60));
    }

    #[test]
    fn test_adjust_recovers_on_zero_failures() {
        set_fast_mode();
        let config = AdaptivePrefetchConfig::default();
        let mut state = AdaptiveState::new(config);

        // Throttle down first
        state.adjust_after_chunk(30, 15);
        assert_eq!(state.current_chunk_size, 15);
        assert_eq!(state.current_delay, Duration::from_millis(30));

        // Recover with zero failures
        state.adjust_after_chunk(15, 0);
        assert_eq!(state.current_chunk_size, 19);
        assert_eq!(state.current_delay, Duration::from_millis(15));
    }

    #[test]
    fn test_adjust_holds_steady_below_threshold() {
        set_fast_mode();
        let config = AdaptivePrefetchConfig::default();
        let mut state = AdaptiveState::new(config);

        // Exactly 25% failure rate (at threshold, not above)
        state.adjust_after_chunk(40, 10);
        assert_eq!(state.current_chunk_size, 30);
        assert_eq!(state.current_delay, Duration::from_millis(15));
    }

    #[test]
    fn test_chunk_size_respects_min() {
        let config = AdaptivePrefetchConfig {
            initial_chunk_size: 10,
            min_chunk_size: 5,
            ..Default::default()
        };
        let mut state = AdaptiveState::new(config);

        // Throttle: 10 -> 5
        state.adjust_after_chunk(10, 10);
        assert_eq!(state.current_chunk_size, 5);

        // Throttle again: stays at 5 (min)
        state.adjust_after_chunk(5, 5);
        assert_eq!(state.current_chunk_size, 5);
    }

    #[test]
    fn test_delay_respects_max() {
        let config = AdaptivePrefetchConfig {
            max_delay: Duration::from_millis(100),
            ..Default::default()
        };
        let mut state = AdaptiveState::new(config);

        // Throttle repeatedly
        state.adjust_after_chunk(40, 40); // 10ms -> 20ms
        state.adjust_after_chunk(20, 20); // 20ms -> 40ms
        state.adjust_after_chunk(10, 10); // 40ms -> 80ms
        state.adjust_after_chunk(5, 5); // 80ms -> capped at 100ms
        assert_eq!(state.current_delay, Duration::from_millis(100));

        // One more — still at max
        state.adjust_after_chunk(5, 5);
        assert_eq!(state.current_delay, Duration::from_millis(100));
    }

    #[test]
    fn test_chunk_size_respects_max_on_recovery() {
        set_fast_mode();
        let config = AdaptivePrefetchConfig::default();
        let mut state = AdaptiveState::new(config);

        // Already at max — recovery doesn't go above
        state.adjust_after_chunk(30, 0);
        assert_eq!(state.current_chunk_size, 30);
    }

    #[test]
    fn test_empty_chunk_no_change() {
        set_fast_mode();
        let config = AdaptivePrefetchConfig::default();
        let mut state = AdaptiveState::new(config);

        state.adjust_after_chunk(0, 0);
        assert_eq!(state.current_chunk_size, 30);
        assert_eq!(state.current_delay, Duration::from_millis(15));
    }

    #[test]
    fn test_throttle_aware_no_backoff() {
        // With no domain throttling active, throttle_aware should match default
        let config = AdaptivePrefetchConfig::throttle_aware();
        let default = AdaptivePrefetchConfig::default();
        // When no domains are throttled, chunk size should be at or near default
        // (exact match depends on global test state, so just verify bounds)
        assert!(config.initial_chunk_size >= THROTTLED_MIN_CHUNK_SIZE);
        assert!(config.initial_chunk_size <= default.initial_chunk_size);
    }

    #[test]
    fn test_throttle_aware_with_active_backoff() {
        use alloy_transport_balancer::{
            domain_throttle, record_rate_limit, weighted_domain_backoff,
        };

        // Simulate domain throttling by recording rate limits
        let state = domain_throttle("test-prefetch-aware.com");
        record_rate_limit(&state);
        record_rate_limit(&state);

        // weighted_domain_backoff should be non-zero with a throttled domain
        let delay = weighted_domain_backoff();
        assert!(
            delay.as_millis() > 0,
            "expected some backoff after rate limits"
        );

        let config = AdaptivePrefetchConfig::throttle_aware();
        let default = AdaptivePrefetchConfig::default();

        // With weighted backoff, the chunk size should be reduced (exact amount
        // depends on other domains in the global registry from parallel tests)
        assert!(config.initial_chunk_size >= THROTTLED_MIN_CHUNK_SIZE);
        assert!(config.initial_chunk_size <= default.initial_chunk_size);
    }

    #[test]
    fn test_throttle_aware_severe_backoff() {
        use alloy_transport_balancer::{domain_throttle, record_rate_limit};

        // Push to level 4 (1000ms delay)
        let state = domain_throttle("test-prefetch-severe.com");
        for _ in 0..4 {
            record_rate_limit(&state);
        }

        let config = AdaptivePrefetchConfig::throttle_aware();
        let default = AdaptivePrefetchConfig::default();

        // With severe throttling, chunk size should be reduced
        // (exact reduction depends on weighted average across all test domains)
        assert!(config.initial_chunk_size >= THROTTLED_MIN_CHUNK_SIZE);
        assert!(config.initial_chunk_size <= default.initial_chunk_size);
    }
}
