use rand::Rng;
use std::time::Duration;
use time::{OffsetDateTime, PrimitiveDateTime, parsing::Parsed};

pub const RETRY_INITIAL_DELAY_MS: u64 = 2000;
pub const RETRY_MAX_DELAY_MS: u64 = 30_000;
pub const RETRY_BACKOFF_FACTOR: u64 = 2;
pub const MAX_RATE_LIMIT_RETRIES: u32 = 3;

#[derive(Debug, Clone, Copy)]
pub struct BackoffOutcome {
    pub wait_ms: u64,
    pub exceeds_budget: bool,
}

pub fn should_retry_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

pub fn compute_backoff_delay(attempt: u32, retry_after: Option<&str>) -> BackoffOutcome {
    compute_backoff_delay_with(
        attempt,
        retry_after,
        OffsetDateTime::now_utc(),
        &mut rand::thread_rng(),
    )
}

fn compute_backoff_delay_with(
    attempt: u32,
    retry_after: Option<&str>,
    now: OffsetDateTime,
    rng: &mut impl Rng,
) -> BackoffOutcome {
    let target_ms = retry_after.and_then(|raw| {
        let raw = raw.trim();
        if let Ok(seconds) = raw.parse::<f64>() {
            return (seconds.is_finite() && seconds >= 0.0)
                .then(|| (seconds * 1000.0).ceil() as u64);
        }
        let date = parse_http_date(raw, now)?;
        let nanos = (date - now).whole_nanoseconds().max(0);
        Some(((nanos + 999_999) / 1_000_000).min(u64::MAX as i128) as u64)
    });
    if let Some(target_ms) = target_ms {
        return BackoffOutcome {
            wait_ms: target_ms.min(RETRY_MAX_DELAY_MS),
            exceeds_budget: target_ms > RETRY_MAX_DELAY_MS,
        };
    }

    let exp = RETRY_INITIAL_DELAY_MS
        .saturating_mul(RETRY_BACKOFF_FACTOR.saturating_pow(attempt))
        .min(RETRY_MAX_DELAY_MS);
    BackoffOutcome {
        wait_ms: rng.gen_range(exp / 2..=exp),
        exceeds_budget: false,
    }
}

fn parse_http_date(raw: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    // HTTP recipients accept IMF-fixdate and the two obsolete HTTP-date forms.
    for description in [
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT",
        "[weekday repr:long], [day]-[month repr:short]-[year repr:last_two] [hour]:[minute]:[second] GMT",
        "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]",
    ] {
        let format = time::format_description::parse_borrowed::<2>(description)
            .expect("valid HTTP date format");
        let mut parsed = Parsed::new();
        if !parsed
            .parse_items(raw.as_bytes(), &format)
            .is_ok_and(|rest| rest.is_empty())
        {
            continue;
        }
        if let Some(year) = parsed.year_last_two() {
            let mut year = (now.year() + 50) / 100 * 100 + i32::from(year);
            // RFC 9110: an RFC 850 date over 50 years ahead means the prior century.
            if (
                year,
                parsed.month()?,
                parsed.day()?.get(),
                parsed.hour_24()?,
                parsed.minute()?,
                parsed.second()?,
            ) > (
                now.year() + 50,
                now.month(),
                now.day(),
                now.hour(),
                now.minute(),
                now.second(),
            ) {
                year -= 100;
            }
            parsed.set_year(year)?;
        }
        return PrimitiveDateTime::try_from(parsed)
            .ok()
            .map(PrimitiveDateTime::assume_utc);
    }
    None
}

pub async fn sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}
