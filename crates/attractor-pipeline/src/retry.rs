//! Backoff timing used by the canonical node-invocation policy.

use std::time::Duration;

pub(crate) fn retry_delay(attempt: usize) -> Duration {
    let shift = u32::try_from(attempt).unwrap_or(u32::MAX);
    let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
    let millis = 500_u64.saturating_mul(multiplier);
    Duration::from_millis(millis).min(Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_is_exponential_and_capped() {
        assert_eq!(retry_delay(0), Duration::from_millis(500));
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(20), Duration::from_secs(30));
        assert_eq!(retry_delay(usize::MAX), Duration::from_secs(30));
    }
}
