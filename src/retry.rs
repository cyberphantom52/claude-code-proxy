use rand::Rng;
use std::sync::atomic::{AtomicBool, Ordering};
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

static ZERO_RETRY_DELAY_FOR_TESTS: AtomicBool = AtomicBool::new(false);

/// Make retry sleeps return immediately so exhaustion paths can be exercised
/// in tests without waiting out the real backoff schedule.
pub fn set_zero_retry_delay_for_tests(enabled: bool) {
    ZERO_RETRY_DELAY_FOR_TESTS.store(enabled, Ordering::SeqCst);
}

pub async fn sleep(ms: u64) {
    if ZERO_RETRY_DELAY_FOR_TESTS.load(Ordering::SeqCst) {
        return;
    }
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};
    use time::format_description::well_known::Rfc3339;

    fn now() -> OffsetDateTime {
        OffsetDateTime::parse("1994-11-06T08:49:27.000000001Z", &Rfc3339).unwrap()
    }

    #[test]
    fn retry_after_numeric_and_all_http_date_forms_obey_the_budget() {
        for (header, wait_ms, exceeds_budget) in [
            ("0", 0, false),
            (" 5 ", 5000, false),
            ("0.0001", 1, false),
            ("30", 30_000, false),
            ("30.0001", 30_000, true),
            ("120", 30_000, true),
            ("1e300", 30_000, true),
            ("Sun, 06 Nov 1994 08:49:37 GMT", 10_000, false),
            ("Sunday, 06-Nov-94 08:49:37 GMT", 10_000, false),
            ("Sun Nov  6 08:49:37 1994", 10_000, false),
            ("Sun, 06 Nov 1994 08:49:57 GMT", 30_000, false),
            ("Sun, 06 Nov 1994 08:49:58 GMT", 30_000, true),
            ("Sun, 06 Nov 1994 08:49:00 GMT", 0, false),
        ] {
            let outcome =
                compute_backoff_delay_with(0, Some(header), now(), &mut StdRng::seed_from_u64(1));
            assert_eq!(outcome.wait_ms, wait_ms, "{header}");
            assert_eq!(outcome.exceeds_budget, exceeds_budget, "{header}");
        }
    }

    #[test]
    fn obsolete_http_date_uses_a_rolling_fifty_year_cutoff() {
        let now = OffsetDateTime::parse("2026-10-22T12:00:00Z", &Rfc3339).unwrap();
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT", now)
                .unwrap()
                .year(),
            1994
        );
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-06 08:49:37 GMT", now)
                .unwrap()
                .year(),
            2006
        );
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-76 08:49:37 GMT", now)
                .unwrap()
                .year(),
            1976
        );
        assert_eq!(
            parse_http_date("Sunday, 06-Sep-76 08:49:37 GMT", now)
                .unwrap()
                .year(),
            2076
        );
    }

    #[test]
    fn invalid_retry_after_falls_back_to_bounded_random_jitter() {
        for header in [
            None,
            Some(""),
            Some("garbage"),
            Some("-1"),
            Some("NaN"),
            Some("inf"),
            Some("Sun, 99 Nov 1994 08:49:37 GMT"),
        ] {
            for attempt in [0, 1, 4, u32::MAX] {
                let ceiling = RETRY_INITIAL_DELAY_MS
                    .saturating_mul(RETRY_BACKOFF_FACTOR.saturating_pow(attempt))
                    .min(RETRY_MAX_DELAY_MS);
                let mut rng = StdRng::seed_from_u64(1);
                let waits: Vec<u64> = (0..8)
                    .map(|_| {
                        let outcome = compute_backoff_delay_with(attempt, header, now(), &mut rng);
                        assert!(!outcome.exceeds_budget);
                        assert!((ceiling / 2..=ceiling).contains(&outcome.wait_ms));
                        outcome.wait_ms
                    })
                    .collect();
                assert!(waits.windows(2).any(|pair| pair[0] != pair[1]));
            }
        }
    }
}
