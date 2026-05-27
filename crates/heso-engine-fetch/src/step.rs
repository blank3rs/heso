//! Per-step status and logical timestamps for stamped / replayed plats.
//!
//! A stamped plat carries a `steps` array — one entry per executed plan
//! action. Each entry records what the verb did via three load-bearing
//! fields:
//!
//! - [`StepStatus`] — `"ok"`, `"partial"`, or `"error"`. Whether the
//!   step achieved the verb's contract end-to-end. A `click` whose
//!   selector did not match in the live DOM is `"partial"`; a `click`
//!   whose target ref could not be resolved at all is `"error"`. An
//!   `open` against a 404 page is `"partial"` (the network call
//!   succeeded; the page is degraded). A network failure during an
//!   `open` is `"error"`.
//! - `observed` — the verb-specific result JSON. Mirrors what the live
//!   verb would have emitted on stdout. Carried only on
//!   `"ok"` / `"partial"` outcomes; absent on `"error"` so the
//!   empty-vs-null-vs-absent distinction in HESO/1.0 §1.7 Property 5
//!   keeps the three statuses byte-distinct in the canonical bytes.
//! - `started_at` / `finished_at` — **logical** timestamps, NOT wall
//!   clock. Two stamps of the same plan against the same site that
//!   produce the same cassette MUST produce the same `plat_hash`
//!   ([HESO/1.0 §1.7 Property 1] determinism), so timing fields cannot
//!   carry the host's clock. The construction below derives both fields
//!   from the step index — see [`logical_step_timestamp`].
//!
//! The replay verb (`heso run`) re-executes each step against the
//! plat's cassette and compares the recorded `status` + `observed`
//! fields against the re-execution result. Any divergence is a per-
//! step verification failure that surfaces on stderr with the diverging
//! field name and the recorded vs re-executed values.
//!
//! [HESO/1.0 §1.7 Property 1]: https://heso.ca/spec#1.7

use serde::{Deserialize, Serialize};

/// Three-way step outcome. Distinguishes "the verb's contract was met"
/// (`Ok`), "the verb ran but the outcome is degraded" (`Partial`), and
/// "the verb could not run" (`Error`).
///
/// Wire form is snake-case (`"ok"`, `"partial"`, `"error"`) to match the
/// rest of the plat envelope's status tokens (`partial_reason: "ok"`,
/// `mode: "deterministic"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// The verb's contract was met end-to-end.
    Ok,
    /// The verb ran but produced a degraded outcome (4xx response, bot
    /// challenge, selector did not match in the live DOM, …). A
    /// `partial_reason` token MUST accompany the step.
    Partial,
    /// The verb could not run at all (network failure, ref resolution
    /// failure, malformed input). An `error` message MUST accompany the
    /// step.
    Error,
}

impl StepStatus {
    /// Wire token: `"ok"` / `"partial"` / `"error"`. Useful when
    /// composing JSON without paying for serde round-trips.
    pub fn as_token(self) -> &'static str {
        match self {
            StepStatus::Ok => "ok",
            StepStatus::Partial => "partial",
            StepStatus::Error => "error",
        }
    }
}

/// Which boundary of a step a timestamp marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepBoundary {
    /// The instant just before the step begins executing.
    Started,
    /// The instant just after the step finishes executing.
    Finished,
}

/// Render a deterministic per-step RFC 3339 timestamp.
///
/// The construction maps `(step_index, boundary)` to a millisecond
/// offset from the synthetic epoch `1970-01-01T00:00:00Z`:
///
/// - `Started` for step `i` ⇒ `(i * 2) ms`
/// - `Finished` for step `i` ⇒ `(i * 2 + 1) ms`
///
/// This produces a strictly monotonic, fully deterministic sequence
/// where each step occupies a 1 ms slice and consecutive steps do not
/// share boundaries. Both stamping and replay produce the same string
/// for the same step index, so the `started_at` / `finished_at` fields
/// can participate in canonical bytes without forcing a `plat_hash`
/// drift between the two.
///
/// The choice of synthetic epoch (1970-01-01T00:00:00.000Z) is
/// deliberate: it is the same anchor HESO/1.0 §5.4 fixes for the
/// virtual clock (`epoch_offset_ms` defaults to zero in §5.4.1), so an
/// observer scanning a plat's `steps[].started_at` and the embedded
/// JS-side `Date.now()` traces sees one consistent zero point.
///
/// Wall-clock annotations are deliberately not included — recording
/// them would break the HESO/1.0 §1.7 Property 1 determinism contract,
/// which the stamp→run round-trip test in
/// `crates/heso-cli/tests/cassette_replay.rs::stamp_then_run_is_byte_identical`
/// pins.
pub fn logical_step_timestamp(step_index: usize, boundary: StepBoundary) -> String {
    let ms = (step_index as u64).saturating_mul(2)
        + match boundary {
            StepBoundary::Started => 0,
            StepBoundary::Finished => 1,
        };
    format_rfc3339_ms_since_epoch(ms)
}

/// Format a millisecond offset from `1970-01-01T00:00:00.000Z` as an
/// RFC 3339 UTC string with millisecond precision (`YYYY-MM-DDTHH:MM:SS.sssZ`).
///
/// Pure arithmetic; no `chrono` / `time` dependency. The output is
/// stable across hosts and architectures because it depends only on
/// the input integer.
fn format_rfc3339_ms_since_epoch(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let millis = (ms % 1000) as u32;
    let (year, month, day, hour, minute, second) = unix_seconds_to_ymdhms(total_seconds);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    )
}

/// Convert an unsigned count of seconds since the Unix epoch into
/// `(year, month, day, hour, minute, second)`. Uses the proleptic
/// Gregorian calendar; valid for any input in `u64` range
/// (year ≈ 5.84 × 10^11 at saturation, far beyond protocol-relevant
/// ranges).
///
/// Algorithm is the well-known "civil from days" decomposition by
/// Howard Hinnant, adapted to take seconds directly. Branchless and
/// integer-only.
fn unix_seconds_to_ymdhms(total_seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    let seconds_per_day: u64 = 86_400;
    let days_since_epoch = (total_seconds / seconds_per_day) as i64;
    let secs_of_day = (total_seconds % seconds_per_day) as u32;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day / 60) % 60;
    let second = secs_of_day % 60;

    // Hinnant civil_from_days: shifts the epoch to 0000-03-01 so leap
    // logic becomes uniform. See http://howardhinnant.github.io/date_algorithms.html.
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146097)
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_token_round_trips_through_serde() {
        for status in [StepStatus::Ok, StepStatus::Partial, StepStatus::Error] {
            let s = serde_json::to_string(&status).unwrap();
            let back: StepStatus = serde_json::from_str(&s).unwrap();
            assert_eq!(status, back);
            assert!(
                s.contains(status.as_token()),
                "as_token must match the serde wire form ({s} vs {})",
                status.as_token()
            );
        }
    }

    #[test]
    fn status_wire_form_is_snake_case() {
        assert_eq!(serde_json::to_string(&StepStatus::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&StepStatus::Partial).unwrap(),
            "\"partial\""
        );
        assert_eq!(
            serde_json::to_string(&StepStatus::Error).unwrap(),
            "\"error\""
        );
    }

    #[test]
    fn logical_timestamp_is_strictly_monotonic_across_steps() {
        let mut prev = logical_step_timestamp(0, StepBoundary::Started);
        for i in 0..10 {
            let started = logical_step_timestamp(i, StepBoundary::Started);
            let finished = logical_step_timestamp(i, StepBoundary::Finished);
            if i > 0 {
                assert!(
                    started > prev,
                    "step {i} started_at must be > previous finished_at: {started} vs {prev}"
                );
            }
            assert!(
                finished > started,
                "step {i} finished_at must be > started_at: {finished} vs {started}"
            );
            prev = finished;
        }
    }

    #[test]
    fn logical_timestamp_is_deterministic() {
        for i in [0usize, 1, 42, 1_000, 1_000_000] {
            for boundary in [StepBoundary::Started, StepBoundary::Finished] {
                let a = logical_step_timestamp(i, boundary);
                let b = logical_step_timestamp(i, boundary);
                assert_eq!(a, b, "logical_step_timestamp must be a pure function");
            }
        }
    }

    #[test]
    fn logical_timestamp_format_is_rfc3339_millis() {
        // Pinned by exact string equality. If this trips, the wire format
        // changed and every consumer that pattern-matches on the shape
        // needs to be updated together.
        assert_eq!(
            logical_step_timestamp(0, StepBoundary::Started),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            logical_step_timestamp(0, StepBoundary::Finished),
            "1970-01-01T00:00:00.001Z"
        );
        assert_eq!(
            logical_step_timestamp(1, StepBoundary::Started),
            "1970-01-01T00:00:00.002Z"
        );
        assert_eq!(
            logical_step_timestamp(1, StepBoundary::Finished),
            "1970-01-01T00:00:00.003Z"
        );
        // Boundary at one full second.
        assert_eq!(
            logical_step_timestamp(500, StepBoundary::Started),
            "1970-01-01T00:00:01.000Z"
        );
    }

    #[test]
    fn ymdhms_decoder_matches_known_dates() {
        // Epoch.
        assert_eq!(unix_seconds_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
        // 1970-01-02T00:00:00Z = 86400.
        assert_eq!(unix_seconds_to_ymdhms(86_400), (1970, 1, 2, 0, 0, 0));
        // Leap-year smoke: 2000-02-29T12:34:56Z = 951_827_696.
        assert_eq!(
            unix_seconds_to_ymdhms(951_827_696),
            (2000, 2, 29, 12, 34, 56)
        );
        // 2024-01-01T00:00:00Z = 1_704_067_200 (after every leap fixup
        // since the epoch).
        assert_eq!(
            unix_seconds_to_ymdhms(1_704_067_200),
            (2024, 1, 1, 0, 0, 0)
        );
    }

    #[test]
    fn rfc3339_lex_sort_matches_chronological_order() {
        // The whole point of using RFC 3339 with zero-padded fields is
        // that string comparison agrees with chronological comparison.
        // A consumer sorting `steps[].started_at` lexically must get
        // the steps back in execution order.
        let mut ts: Vec<String> = (0..32)
            .map(|i| logical_step_timestamp(i, StepBoundary::Started))
            .collect();
        let sorted = ts.clone();
        ts.sort();
        assert_eq!(ts, sorted);
    }
}
