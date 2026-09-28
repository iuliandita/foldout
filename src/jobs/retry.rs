pub const MAX_CLAIMS: i64 = 5;
pub const MAX_LEASE_SECONDS: i64 = 300;

pub fn retry_at(now: i64, attempt: i64, jitter_seconds: i64) -> i64 {
    let exponent = attempt.clamp(1, MAX_CLAIMS) as u32 - 1;
    let delay = 2_i64.saturating_pow(exponent).min(60);
    now.saturating_add(
        delay
            .saturating_add(jitter_seconds.clamp(-delay / 2, delay / 2))
            .max(1),
    )
}
