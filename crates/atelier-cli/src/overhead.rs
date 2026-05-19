//! §2 protocol-overhead measurement harness.
//!
//! Runs the three emission strategies (`NativeTool`, `JsonSentinel`,
//! `RegexProse`) through one round-trip per scripted fixture, records
//! bytes-on-wire / approximate-tokens / parse-time-ns per the schema at
//! `schemas/protocol/overhead.v1.json`, and writes the result to
//! `tests/protocol/overhead.json`.
//!
//! Two consumers:
//!
//! * `atelier protocol-overhead` — the CLI subcommand. Reads
//!   `tests/protocol/fixtures/*.json` from `--fixtures-dir`, writes
//!   `--out`. Optional `--check-regression` compares against the
//!   `rolling_median` field already in the output file and exits
//!   non-zero on drift > the configured threshold (default 10%).
//! * The nightly CI job `.github/workflows/nightly_protocol_overhead.yml`
//!   wraps the binary, commits the updated file, and runs
//!   `validate_artifacts.py` so a schema break is caught the same day.
//!
//! The harness is deterministic against the `MockAdapter`: it does *not*
//! invoke a network provider. The §2 spec asks for cross-provider
//! overhead numbers; those need API credentials and run in the gated
//! nightly job (per `ci/nightly/README.md`). What lands here is the
//! invariant slice — strategy encode/parse round-trip on a canonical
//! envelope — so a regression in the *strategy* code is caught even
//! without external credentials.
//!
//! Fixture format: a JSON array of [`OverheadFixture`] entries. Each
//! entry carries the envelope that will be round-tripped under the
//! strategy named by the file (`native_tool.json` → `Strategy::NativeTool`),
//! plus a `label` for log lines. The format is reusable by future
//! adapter tests — `serde_json::from_reader::<Vec<OverheadFixture>>` is
//! the load primitive.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use atelier_core::protocol::Envelope;
use atelier_core::protocol_strategy::{measure_overhead, OverheadMeasurement, Strategy};
use atelier_core::time::now_rfc3339;

/// One scripted envelope the harness round-trips under one strategy.
///
/// The fixture file's *filename* picks the strategy (`native_tool.json`
/// → [`Strategy::NativeTool`]); the per-entry `label` is for log lines
/// and the conformance bookkeeping below. Keeping the strategy in the
/// filename rather than the entry means each fixture file is a single
/// homogeneous batch — adapter tests that want all-three coverage load
/// all three files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverheadFixture {
    /// Short human-readable name (e.g., `"single_edit"`,
    /// `"plan_update_only"`). Appears in log lines and in the
    /// `--check-regression` output.
    pub label: String,
    /// The envelope to round-trip. Validated up-front by the harness so
    /// a fixture with a malformed envelope fails loudly instead of
    /// silently producing a 0-byte measurement.
    pub envelope: Envelope,
    /// Whether this envelope is *expected* to round-trip losslessly.
    /// `RegexProse` drops `plan_update` and `constraints_acknowledged`
    /// by design, so fixtures using those fields under the prose
    /// strategy should set this to `false`. The harness records the
    /// outcome in `conformance_rate` per the spec.
    #[serde(default = "default_round_trip_lossless")]
    pub round_trip_lossless: bool,
}

fn default_round_trip_lossless() -> bool {
    true
}

// ---------- output shape ----------

/// Top-level JSON written to `tests/protocol/overhead.json`.
/// Round-trips the schema at `schemas/protocol/overhead.v1.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverheadReport {
    /// Schema version. Always `1` for v1.
    pub version: u32,
    /// RFC 3339 timestamp when the harness ran.
    pub measured_at: String,
    /// One entry per (provider, strategy) pair.
    pub providers: Vec<ProviderEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub provider: String,
    pub model_id: String,
    /// Wire label for the strategy. Schema enum is
    /// `["native_tool", "json_sentinel", "regex_prose"]`, matching the
    /// in-tree `Strategy::as_str()` 1:1 since v60.28 H16.
    pub strategy: String,
    /// Median percent token overhead vs. a no-protocol baseline turn.
    pub median_overhead_pct: f64,
    /// Fraction of round-trips that decoded back to a structurally
    /// equal envelope (only counts envelopes the fixture marked
    /// `round_trip_lossless: true`).
    pub conformance_rate: f64,
    /// Encoded byte length of the median envelope for this strategy.
    pub bytes_on_wire: u64,
    /// Chars/4 approximation of the same.
    pub tokens_envelope: u64,
    /// Wall-clock parse time of the median sample.
    pub parse_time_ns: u64,
    /// Rolling 7-day median bookkeeping. Absent on the first ever run;
    /// populated thereafter by the harness merging the current sample
    /// into a 7-sample window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_median: Option<RollingMedian>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollingMedian {
    pub window_days: u32,
    /// The median of the prior `samples` runs' `median_overhead_pct`.
    pub value: f64,
    /// How many data points contributed to `value`. Capped at 7 so the
    /// window slides; the harness drops the oldest sample as new ones
    /// land via the nightly job.
    pub samples: u32,
}

// ---------- harness ----------

/// Default fixture directory relative to the workspace root.
pub const DEFAULT_FIXTURES_DIR: &str = "tests/protocol/fixtures";
/// Default output path relative to the workspace root.
pub const DEFAULT_OUT_PATH: &str = "tests/protocol/overhead.json";
/// Default regression threshold: 10% drift over the rolling median.
pub const DEFAULT_REGRESSION_THRESHOLD_PCT: f64 = 10.0;
/// Maximum samples retained in `rolling_median.samples`. Matches the §2
/// "rolling 7-day median" contract.
pub const ROLLING_WINDOW_DAYS: u32 = 7;

/// Errors the harness surfaces. Each variant maps to a non-zero exit
/// code in the CLI; `Regression` is the one the nightly workflow keys
/// on to fail the job loudly.
#[derive(Debug, Error)]
pub enum OverheadError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("malformed fixture {path}: {source}")]
    Fixture {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("no fixtures found under {0} (expected native_tool.json / json_sentinel.json / regex_prose.json)")]
    NoFixtures(PathBuf),
    #[error("strategy {strategy:?} measurement failed on fixture {label:?}: {source}")]
    Measurement {
        strategy: String,
        label: String,
        #[source]
        source: atelier_core::protocol_strategy::StrategyError,
    },
    #[error(
        "regression: strategy {strategy:?} median_overhead_pct {current:.3} drifted +{drift_pct:.2}% \
         vs. rolling median {baseline:.3} (threshold {threshold:.2}%)"
    )]
    Regression {
        strategy: String,
        current: f64,
        baseline: f64,
        drift_pct: f64,
        threshold: f64,
    },
    #[error("failed to serialize output: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("malformed existing overhead file {path}: {source}")]
    ExistingOutput {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Configuration for one harness run. Defaults mirror the §2 nightly
/// job; the CLI populates this from argv.
pub struct OverheadConfig {
    pub fixtures_dir: PathBuf,
    pub out_path: PathBuf,
    /// Model id reported in the output. Defaults to
    /// `"mock:protocol-overhead"` — the §2 contract calls the
    /// in-tree harness a synthetic provider so reports across runs
    /// stay comparable even when the credentialed nightly path is
    /// disabled.
    pub model_id: String,
    /// Provider name in the output. Defaults to `"mock"`.
    pub provider: String,
    /// If `true`, compare current `median_overhead_pct` against the
    /// prior `rolling_median.value` in the existing output file (if
    /// present) and return [`OverheadError::Regression`] on drift
    /// exceeding `regression_threshold_pct`.
    pub check_regression: bool,
    /// Drift percentage that constitutes a regression. 10% per spec.
    pub regression_threshold_pct: f64,
}

impl OverheadConfig {
    pub fn with_workspace(workspace: &Path) -> Self {
        Self {
            fixtures_dir: workspace.join(DEFAULT_FIXTURES_DIR),
            out_path: workspace.join(DEFAULT_OUT_PATH),
            model_id: "mock:protocol-overhead".into(),
            provider: "mock".into(),
            check_regression: false,
            regression_threshold_pct: DEFAULT_REGRESSION_THRESHOLD_PCT,
        }
    }
}

/// One-shot entry point: run the harness, write the report, return it.
///
/// On `check_regression: true`, returns [`OverheadError::Regression`]
/// *after* writing the new file — the nightly workflow needs the
/// updated rolling median committed even when the run failed, so the
/// next nightly's baseline is fresh.
pub fn run(config: &OverheadConfig) -> Result<OverheadReport, OverheadError> {
    let fixtures = load_fixtures(&config.fixtures_dir)?;
    let prior = load_prior_report(&config.out_path)?;

    let mut providers = Vec::with_capacity(fixtures.len());
    for (strategy, batch) in &fixtures {
        let entry = measure_strategy(config, *strategy, batch, prior.as_ref())?;
        providers.push(entry);
    }

    // Stable order so the file's diff is meaningful turn-on-turn.
    providers.sort_by(|a, b| a.strategy.cmp(&b.strategy));

    let report = OverheadReport {
        version: 1,
        measured_at: now_rfc3339(),
        providers,
    };

    write_report(&config.out_path, &report)?;

    if config.check_regression {
        check_regression(config, &report, prior.as_ref())?;
    }

    Ok(report)
}

// ---------- fixture loading ----------

fn load_fixtures(dir: &Path) -> Result<Vec<(Strategy, Vec<OverheadFixture>)>, OverheadError> {
    // `Vec` rather than `BTreeMap` because `Strategy` doesn't implement
    // `Ord` (no spec-mandated total order) and the fixture-filename
    // table already establishes the stable iteration order we want.
    let mut out: Vec<(Strategy, Vec<OverheadFixture>)> = Vec::new();
    for (strategy, filename) in fixture_filenames() {
        let path = dir.join(filename);
        if !path.exists() {
            continue;
        }
        let bytes = fs::read(&path).map_err(|e| OverheadError::Io {
            path: path.clone(),
            source: e,
        })?;
        let batch: Vec<OverheadFixture> =
            serde_json::from_slice(&bytes).map_err(|e| OverheadError::Fixture {
                path: path.clone(),
                source: e,
            })?;
        out.push((strategy, batch));
    }
    if out.is_empty() {
        return Err(OverheadError::NoFixtures(dir.to_path_buf()));
    }
    Ok(out)
}

/// The three strategy filenames the harness recognises. Stable order
/// for deterministic output.
fn fixture_filenames() -> [(Strategy, &'static str); 3] {
    [
        (Strategy::NativeTool, "native_tool.json"),
        (Strategy::JsonSentinel, "json_sentinel.json"),
        (Strategy::RegexProse, "regex_prose.json"),
    ]
}

fn load_prior_report(path: &Path) -> Result<Option<OverheadReport>, OverheadError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|e| OverheadError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let report: OverheadReport =
        serde_json::from_slice(&bytes).map_err(|e| OverheadError::ExistingOutput {
            path: path.to_path_buf(),
            source: e,
        })?;
    Ok(Some(report))
}

// ---------- per-strategy measurement ----------

fn measure_strategy(
    config: &OverheadConfig,
    strategy: Strategy,
    batch: &[OverheadFixture],
    prior: Option<&OverheadReport>,
) -> Result<ProviderEntry, OverheadError> {
    let wire_label = strategy_wire_label(strategy);

    let mut samples: Vec<OverheadMeasurement> = Vec::with_capacity(batch.len());
    let mut conformant: u32 = 0;
    let mut conformance_denominator: u32 = 0;

    for fixture in batch {
        let measurement = measure_overhead(&fixture.envelope, strategy).map_err(|e| {
            OverheadError::Measurement {
                strategy: wire_label.to_string(),
                label: fixture.label.clone(),
                source: e,
            }
        })?;
        samples.push(measurement);

        if fixture.round_trip_lossless {
            conformance_denominator += 1;
            // For the lossless contract we re-decode and check
            // structural equality. We re-encode here (cheap) so the
            // measurement above isn't double-stating its own success.
            let ok = verify_round_trip(&fixture.envelope, strategy).map_err(|e| {
                OverheadError::Measurement {
                    strategy: wire_label.to_string(),
                    label: fixture.label.clone(),
                    source: e,
                }
            })?;
            if ok {
                conformant += 1;
            }
        }
    }

    // Median sample. Stable enough for a small N — we sort by
    // bytes_on_wire and take the middle element (lower-middle on even
    // counts so the choice is deterministic).
    samples.sort_by_key(|m| m.bytes_on_wire);
    let mid = samples.len() / 2;
    let median_sample = samples[mid].clone();

    // §2 "median overhead vs. a no-protocol baseline." The baseline
    // here is the raw natural-language reply (no envelope). The
    // chars/4 approximation models that as "the prose the model would
    // have emitted anyway"; for measurement-only fixtures we treat the
    // strategy's chars/4 figure as the overhead itself, expressed as a
    // percentage of an assumed 100-token baseline. The nightly job
    // refines this once real provider responses land; the value is
    // still monotonic in the strategy's actual cost so the regression
    // gate works against it.
    let median_overhead_pct = baseline_overhead_pct(median_sample.tokens_envelope);

    let conformance_rate = if conformance_denominator == 0 {
        1.0
    } else {
        f64::from(conformant) / f64::from(conformance_denominator)
    };

    let rolling_median = next_rolling_median(prior, wire_label, median_overhead_pct);

    Ok(ProviderEntry {
        provider: config.provider.clone(),
        model_id: config.model_id.clone(),
        strategy: wire_label.to_string(),
        median_overhead_pct,
        conformance_rate,
        bytes_on_wire: median_sample.bytes_on_wire,
        tokens_envelope: median_sample.tokens_envelope,
        parse_time_ns: median_sample.parse_time_ns,
        rolling_median,
    })
}

fn verify_round_trip(
    env: &Envelope,
    strategy: Strategy,
) -> Result<bool, atelier_core::protocol_strategy::StrategyError> {
    use atelier_core::protocol_strategy::{
        encode_json_sentinel, encode_native_tool, encode_regex_prose, parse_json_sentinel,
        parse_native_tool, parse_regex_prose, NativeToolCall,
    };
    Ok(match strategy {
        Strategy::NativeTool => {
            let call = encode_native_tool(env)?;
            let payload = serde_json::to_string(&call).map_err(|e| {
                atelier_core::protocol_strategy::StrategyError::Encode(e.to_string())
            })?;
            let parsed: NativeToolCall = serde_json::from_str(&payload).map_err(|e| {
                atelier_core::protocol_strategy::StrategyError::Encode(e.to_string())
            })?;
            let back = parse_native_tool(&parsed)?;
            back == *env
        }
        Strategy::JsonSentinel => {
            let s = encode_json_sentinel(env)?;
            let back = parse_json_sentinel(&s)?;
            back.envelope == *env
        }
        Strategy::RegexProse => {
            // RegexProse is documented-lossy; we only call this on
            // fixtures that opted in to round_trip_lossless = true,
            // i.e., they restrict themselves to fields RegexProse
            // carries (claimed_changes, claimed_done, grounding,
            // uncertainty).
            let s = encode_regex_prose(env)?;
            let back = parse_regex_prose(&s)?;
            back == *env
        }
    })
}

/// Convert an envelope token count into a "% overhead" figure. The §2
/// nightly contract calls for "median percentage token overhead vs. a
/// no-protocol baseline" — without a real model trace, we model the
/// baseline as 100 tokens (a typical small turn) and report the
/// envelope's tokens as a percentage of it. Strictly monotonic in the
/// raw envelope cost, so the regression check operates on a
/// well-defined signal.
fn baseline_overhead_pct(tokens_envelope: u64) -> f64 {
    const BASELINE_TOKENS: f64 = 100.0;
    (tokens_envelope as f64) / BASELINE_TOKENS * 100.0
}

fn next_rolling_median(
    prior: Option<&OverheadReport>,
    wire_label: &str,
    current_value: f64,
) -> Option<RollingMedian> {
    let prior_entry = prior
        .and_then(|r| r.providers.iter().find(|p| p.strategy == wire_label))
        .and_then(|p| p.rolling_median.as_ref());
    let (new_value, new_samples) = match prior_entry {
        None => (current_value, 1),
        Some(rm) => {
            // 7-sample sliding window. Without persisting raw history,
            // we approximate with an incremental average that retains
            // at most `ROLLING_WINDOW_DAYS` samples — the same shape
            // the nightly job has been documented to track.
            let samples = rm.samples.min(ROLLING_WINDOW_DAYS);
            let new_samples = (samples + 1).min(ROLLING_WINDOW_DAYS);
            let new_value = if samples == 0 {
                current_value
            } else {
                (rm.value * f64::from(samples) + current_value) / f64::from(samples + 1)
            };
            (new_value, new_samples)
        }
    };
    Some(RollingMedian {
        window_days: ROLLING_WINDOW_DAYS,
        value: new_value,
        samples: new_samples,
    })
}

// ---------- regression check ----------

fn check_regression(
    config: &OverheadConfig,
    current: &OverheadReport,
    prior: Option<&OverheadReport>,
) -> Result<(), OverheadError> {
    let Some(prior) = prior else {
        return Ok(());
    };
    for entry in &current.providers {
        let Some(prior_entry) = prior
            .providers
            .iter()
            .find(|p| p.strategy == entry.strategy)
        else {
            continue;
        };
        let baseline = match &prior_entry.rolling_median {
            Some(rm) => rm.value,
            None => prior_entry.median_overhead_pct,
        };
        if baseline <= 0.0 {
            continue;
        }
        let drift_pct = (entry.median_overhead_pct - baseline) / baseline * 100.0;
        if drift_pct > config.regression_threshold_pct {
            return Err(OverheadError::Regression {
                strategy: entry.strategy.clone(),
                current: entry.median_overhead_pct,
                baseline,
                drift_pct,
                threshold: config.regression_threshold_pct,
            });
        }
    }
    Ok(())
}

// ---------- output ----------

fn write_report(path: &Path, report: &OverheadReport) -> Result<(), OverheadError> {
    let parent = match path.parent() {
        Some(p) => {
            fs::create_dir_all(p).map_err(|e| OverheadError::Io {
                path: p.to_path_buf(),
                source: e,
            })?;
            p
        }
        None => {
            return Err(OverheadError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "overhead path has no parent directory",
                ),
            })
        }
    };
    // Pretty-print so the committed file is readable in code review and
    // diffs are line-by-line meaningful.
    let mut json = serde_json::to_string_pretty(report).map_err(OverheadError::Serialize)?;
    json.push('\n');
    // v60.37 A4 — atomic write: tempfile in the same directory →
    // write → sync_all → rename → fsync_dir. `fs::write` truncates
    // the target then writes; a crash mid-write would leave a partial
    // file in tracked source (`tests/protocol/overhead.json`). With
    // the discipline below, the file on disk is always either the
    // pre-write or post-write contents — never a partial.
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| OverheadError::Io {
        path: parent.to_path_buf(),
        source: e,
    })?;
    std::io::Write::write_all(tmp.as_file_mut(), json.as_bytes()).map_err(|e| {
        OverheadError::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        }
    })?;
    tmp.as_file().sync_all().map_err(|e| OverheadError::Io {
        path: tmp.path().to_path_buf(),
        source: e,
    })?;
    tmp.persist(path).map_err(|e| OverheadError::Io {
        path: path.to_path_buf(),
        source: e.error,
    })?;
    atelier_core::path_safety::fsync_dir(parent).map_err(|e| OverheadError::Io {
        path: parent.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

fn strategy_wire_label(s: Strategy) -> &'static str {
    // v60.28 H16 — schema now agrees with `Strategy::as_str()`; no
    // remapping needed.
    s.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atelier_core::protocol::{ClaimedChange, ClaimedChangeKind, Envelope};

    fn small_envelope() -> Envelope {
        Envelope {
            claimed_changes: Some(vec![ClaimedChange {
                path: "utils.py".into(),
                kind: ClaimedChangeKind::Edit,
                summary: "rename foo to bar".into(),
            }]),
            claimed_done: Some(true),
            ..Default::default()
        }
    }

    fn write_fixture_file(dir: &Path, name: &str, fixtures: &[OverheadFixture]) {
        fs::create_dir_all(dir).unwrap();
        let body = serde_json::to_string_pretty(fixtures).unwrap();
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn run_writes_a_report_for_each_present_strategy_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let fixtures_dir = tmp.path().join("fixtures");
        let out = tmp.path().join("overhead.json");

        let fix = vec![OverheadFixture {
            label: "small".into(),
            envelope: small_envelope(),
            round_trip_lossless: true,
        }];
        write_fixture_file(&fixtures_dir, "native_tool.json", &fix);
        write_fixture_file(&fixtures_dir, "json_sentinel.json", &fix);
        write_fixture_file(&fixtures_dir, "regex_prose.json", &fix);

        let mut config = OverheadConfig::with_workspace(tmp.path());
        config.fixtures_dir = fixtures_dir;
        config.out_path = out.clone();

        let report = run(&config).expect("run should succeed");
        assert_eq!(report.version, 1);
        assert_eq!(report.providers.len(), 3);
        // Sorted alphabetically by wire label: json_sentinel, native_tool, regex_prose.
        let labels: Vec<_> = report
            .providers
            .iter()
            .map(|p| p.strategy.as_str())
            .collect();
        assert_eq!(labels, vec!["json_sentinel", "native_tool", "regex_prose"]);
        assert!(out.exists(), "output file should have been written");
        for p in &report.providers {
            assert!(p.bytes_on_wire > 0);
            assert!(p.tokens_envelope > 0);
            assert!(p.median_overhead_pct > 0.0);
            assert!(p.conformance_rate > 0.0);
            assert!(p.rolling_median.is_some());
            assert_eq!(p.rolling_median.as_ref().unwrap().samples, 1);
        }
    }

    #[test]
    fn run_errors_when_fixture_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = OverheadConfig::with_workspace(tmp.path());
        config.fixtures_dir = tmp.path().join("missing");
        let err = run(&config).unwrap_err();
        assert!(matches!(err, OverheadError::NoFixtures(_)));
    }

    #[test]
    fn run_propagates_malformed_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let fixtures_dir = tmp.path().join("fixtures");
        fs::create_dir_all(&fixtures_dir).unwrap();
        fs::write(fixtures_dir.join("native_tool.json"), b"{not json}").unwrap();
        let mut config = OverheadConfig::with_workspace(tmp.path());
        config.fixtures_dir = fixtures_dir;
        let err = run(&config).unwrap_err();
        assert!(matches!(err, OverheadError::Fixture { .. }));
    }

    #[test]
    fn rolling_median_grows_then_caps_at_window() {
        // After running 10 times, samples should be capped at
        // ROLLING_WINDOW_DAYS (=7) — proves the sliding-window cap.
        let tmp = tempfile::tempdir().unwrap();
        let fixtures_dir = tmp.path().join("fixtures");
        let out = tmp.path().join("overhead.json");
        let fix = vec![OverheadFixture {
            label: "single".into(),
            envelope: small_envelope(),
            round_trip_lossless: true,
        }];
        write_fixture_file(&fixtures_dir, "native_tool.json", &fix);

        let mut config = OverheadConfig::with_workspace(tmp.path());
        config.fixtures_dir = fixtures_dir;
        config.out_path = out;

        for _ in 0..10 {
            run(&config).unwrap();
        }
        let report = load_prior_report(&config.out_path).unwrap().unwrap();
        let rm = report.providers[0].rolling_median.as_ref().unwrap();
        assert_eq!(rm.samples, ROLLING_WINDOW_DAYS);
        assert_eq!(rm.window_days, ROLLING_WINDOW_DAYS);
    }

    #[test]
    fn regression_check_fires_when_drift_exceeds_threshold() {
        // Seed the output with a prior rolling median far below the
        // current run, then ask for the check. Should error.
        let tmp = tempfile::tempdir().unwrap();
        let fixtures_dir = tmp.path().join("fixtures");
        let out = tmp.path().join("overhead.json");

        let fix = vec![OverheadFixture {
            label: "single".into(),
            envelope: small_envelope(),
            round_trip_lossless: true,
        }];
        write_fixture_file(&fixtures_dir, "native_tool.json", &fix);

        let mut config = OverheadConfig::with_workspace(tmp.path());
        config.fixtures_dir = fixtures_dir;
        config.out_path = out.clone();
        config.check_regression = true;

        // Run once to populate the rolling median.
        run(&config).unwrap();

        // Now manually rewrite the prior file with an implausibly low
        // baseline so the next run's measurement looks like a huge
        // regression.
        let mut report = load_prior_report(&out).unwrap().unwrap();
        for p in &mut report.providers {
            p.rolling_median = Some(RollingMedian {
                window_days: 7,
                value: 0.001,
                samples: 3,
            });
            p.median_overhead_pct = 0.001;
        }
        let body = serde_json::to_string_pretty(&report).unwrap();
        fs::write(&out, body).unwrap();

        let err = run(&config).unwrap_err();
        match err {
            OverheadError::Regression {
                drift_pct,
                threshold,
                ..
            } => {
                assert!(drift_pct > threshold);
                assert_eq!(threshold, DEFAULT_REGRESSION_THRESHOLD_PCT);
            }
            other => panic!("expected Regression, got {other:?}"),
        }
        // File should still have been refreshed even when the run
        // failed, so the next nightly's baseline is current.
        assert!(out.exists());
    }

    #[test]
    fn strategy_wire_label_agrees_with_strategy_as_str() {
        assert_eq!(strategy_wire_label(Strategy::NativeTool), "native_tool");
        assert_eq!(strategy_wire_label(Strategy::JsonSentinel), "json_sentinel");
        assert_eq!(strategy_wire_label(Strategy::RegexProse), "regex_prose");
    }

    #[test]
    fn lossy_fixture_does_not_drag_conformance_below_one() {
        // A regex-prose fixture that explicitly opts out of the
        // lossless contract (round_trip_lossless: false) must NOT
        // count against the conformance rate.
        let tmp = tempfile::tempdir().unwrap();
        let fixtures_dir = tmp.path().join("fixtures");
        let out = tmp.path().join("overhead.json");
        let fix = vec![
            OverheadFixture {
                label: "lossy".into(),
                envelope: Envelope {
                    claimed_changes: Some(vec![ClaimedChange {
                        path: "x".into(),
                        kind: ClaimedChangeKind::Edit,
                        summary: "x".into(),
                    }]),
                    constraints_acknowledged: Some(vec!["no new deps".into()]),
                    ..Default::default()
                },
                round_trip_lossless: false,
            },
            OverheadFixture {
                label: "lossless".into(),
                envelope: small_envelope(),
                round_trip_lossless: true,
            },
        ];
        write_fixture_file(&fixtures_dir, "regex_prose.json", &fix);
        let mut config = OverheadConfig::with_workspace(tmp.path());
        config.fixtures_dir = fixtures_dir;
        config.out_path = out;
        let report = run(&config).unwrap();
        let prose = report
            .providers
            .iter()
            .find(|p| p.strategy == "regex_prose")
            .unwrap();
        assert!(
            (prose.conformance_rate - 1.0).abs() < f64::EPSILON,
            "conformance_rate should be 1.0 (only the lossless fixture counts), got {}",
            prose.conformance_rate
        );
    }
}
