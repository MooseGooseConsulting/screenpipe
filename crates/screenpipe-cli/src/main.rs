use std::ffi::OsString;

use anyhow::{Context, Result, ensure};
use chrono::Duration;
use clap::{Parser, Subcommand};
use screenpipe_memory::{
    EventSink, MAX_CADENCE_INTERVAL_SECONDS, MergeConfig, PgEventReader, PgEventWriter, RunOutcome,
    Runner, SampleSource,
};
use screenpipe_screen::{WindowsCapture, WindowsOcr};

use crate::service::{ServiceManager, ServiceRoot, ServiceStatus, WindowsTaskScheduler};

mod service;
mod windows_source;

use crate::windows_source::WindowsSampleSource;

const DATABASE_URL_ENV: &str = "SCREEN_MEMORY_DATABASE_URL";
const DEFAULT_MACHINE_SLUG: &str = "icarus";
const DEFAULT_DISPLAY_NAME: &str = "Icarus-Laptop";

#[derive(Debug, Parser)]
#[command(
    name = "screenpipe",
    about = "Headless OCR-first Windows screen-memory service"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run foreground capture in the current interactive session.
    Run {
        #[arg(long, default_value = DEFAULT_MACHINE_SLUG)]
        machine_slug: String,
        #[arg(long, default_value = DEFAULT_DISPLAY_NAME)]
        display_name: String,
    },
    /// Check capture, OCR, and PostgreSQL prerequisites.
    Doctor {
        #[arg(long, default_value = DEFAULT_MACHINE_SLUG)]
        machine_slug: String,
        #[arg(long, default_value = DEFAULT_DISPLAY_NAME)]
        display_name: String,
    },
    /// Search recorded screen memory.
    Search {
        /// Words to look for. Ranked, not literal. May be omitted if --since
        /// or --until is given.
        query: Vec<String>,
        /// Existing machine to read without creating or changing its identity.
        #[arg(long, default_value = DEFAULT_MACHINE_SLUG)]
        machine_slug: String,
        #[arg(long, default_value_t = 10)]
        limit: i64,
        /// Only events that were still going after this. Accepts `90m`, `4h`,
        /// `3d`, `today`, `yesterday`, or a date like `2026-08-06`.
        #[arg(long)]
        since: Option<String>,
        /// Only events that had started by this. Same formats as --since.
        #[arg(long)]
        until: Option<String>,
    },
    /// Manage the per-user headless service.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Run {
            machine_slug,
            display_name,
        } => {
            let database_url = required_database_url(std::env::var_os(DATABASE_URL_ENV))?;
            run_capture(&database_url, &machine_slug, &display_name).await
        }
        Command::Doctor {
            machine_slug,
            display_name,
        } => {
            let database_url = required_database_url(std::env::var_os(DATABASE_URL_ENV))?;
            run_doctor(&database_url, &machine_slug, &display_name).await
        }
        Command::Search {
            query,
            machine_slug,
            limit,
            since,
            until,
        } => {
            let database_url = required_database_url(std::env::var_os(DATABASE_URL_ENV))?;
            let request = screenpipe_memory::SearchRequest {
                query: query.join(" "),
                limit,
                since: since.as_deref().map(parse_when).transpose()?,
                until: until.as_deref().map(parse_when).transpose()?,
            };
            run_search(&database_url, &machine_slug, &request).await
        }
        Command::Service { action } => run_service_action(action),
    }
}

/// Parse the time expressions a person actually types.
///
/// Deliberately small and local. `90m`, `4h`, `3d` are relative to now;
/// `today` and `yesterday` are local midnights, because that is what those
/// words mean to someone looking back at their own day; a bare `YYYY-MM-DD` is
/// local midnight on that date. Everything is converted to UTC at the boundary
/// so the query never depends on the server's timezone.
fn parse_when(value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    use chrono::{Duration as ChronoDuration, Local, NaiveDate, TimeZone};

    let raw = value.trim().to_ascii_lowercase();
    let local_midnight = |date: chrono::NaiveDate| -> Result<chrono::DateTime<chrono::Utc>> {
        let naive = date.and_hms_opt(0, 0, 0).context("build local midnight")?;
        Ok(Local
            .from_local_datetime(&naive)
            .single()
            .context("ambiguous local midnight (daylight-saving boundary)")?
            .with_timezone(&chrono::Utc))
    };

    if raw == "today" {
        return local_midnight(Local::now().date_naive());
    }
    if raw == "yesterday" {
        return local_midnight(
            Local::now()
                .date_naive()
                .pred_opt()
                .context("yesterday is out of range")?,
        );
    }
    if let Some(rest) = raw.strip_suffix('m') {
        ensure!(
            !rest.starts_with('-'),
            "relative times must not be negative"
        );
        let minutes: i64 = rest.parse().context("minutes must be a whole number")?;
        let duration = ChronoDuration::try_minutes(minutes).context("minutes is too large")?;
        return chrono::Utc::now()
            .checked_sub_signed(duration)
            .context("relative time is too large");
    }
    if let Some(rest) = raw.strip_suffix('h') {
        ensure!(
            !rest.starts_with('-'),
            "relative times must not be negative"
        );
        let hours: i64 = rest.parse().context("hours must be a whole number")?;
        let duration = ChronoDuration::try_hours(hours).context("hours is too large")?;
        return chrono::Utc::now()
            .checked_sub_signed(duration)
            .context("relative time is too large");
    }
    if let Some(rest) = raw.strip_suffix('d') {
        ensure!(
            !rest.starts_with('-'),
            "relative times must not be negative"
        );
        let days: i64 = rest.parse().context("days must be a whole number")?;
        let duration = ChronoDuration::try_days(days).context("days is too large")?;
        return chrono::Utc::now()
            .checked_sub_signed(duration)
            .context("relative time is too large");
    }
    let date = NaiveDate::parse_from_str(&raw, "%Y-%m-%d").with_context(|| {
        format!(
            "could not read {value:?} as a time: try 90m, 4h, 3d, today, yesterday, or 2026-08-06"
        )
    })?;
    local_midnight(date)
}

async fn run_search(
    database_url: &str,
    machine_slug: &str,
    request: &screenpipe_memory::SearchRequest,
) -> Result<()> {
    let reader = PgEventReader::connect(database_url, machine_slug).await?;
    let hits = reader.search(request).await?;

    if hits.is_empty() {
        if request.query.trim().is_empty() {
            println!("nothing recorded in that window");
        } else {
            println!("no matches for {:?}", request.query);
        }
        return Ok(());
    }

    for hit in &hits {
        let minutes = (hit.ended_at - hit.started_at).num_minutes();
        let label = hit
            .title
            .as_deref()
            .or(hit.app_title.as_deref())
            .unwrap_or("(untitled)");
        println!(
            "{}  {}  ({} samples, {} min)",
            hit.started_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M"),
            label,
            hit.sample_count,
            minutes.max(0)
        );
        if let Some(url) = hit.browser_url.as_deref() {
            println!("    {url}");
        }
        let snippet = hit.snippet.split_whitespace().collect::<Vec<_>>().join(" ");
        if !snippet.is_empty() {
            println!("    {snippet}");
        }
        println!("    {}", hit.event_id);
        println!();
    }
    println!("{} match(es)", hits.len());
    Ok(())
}

fn required_database_url(value: Option<OsString>) -> Result<String> {
    let value = value.with_context(|| format!("{DATABASE_URL_ENV} must be injected"))?;
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{DATABASE_URL_ENV} must contain valid Unicode"))?;
    ensure!(!value.trim().is_empty(), "{DATABASE_URL_ENV} is blank");
    Ok(value)
}

/// Strictly above the slowest cadence. At max backoff the sleep alone is
/// `MAX_CADENCE_INTERVAL_SECONDS`, so an equal threshold splits on every idle
/// sample once capture and OCR overhead is added, fragmenting a quiet window
/// into one-sample events.
const IDLE_GAP_SECONDS: i64 = MAX_CADENCE_INTERVAL_SECONDS * 2;

fn default_runner() -> Runner {
    Runner::new(MergeConfig {
        idle_gap: Duration::seconds(IDLE_GAP_SECONDS),
        scroll_overlap: 0.35,
    })
}

async fn run_iteration(
    runner: &mut Runner,
    source: &mut dyn SampleSource,
    sink: &dyn EventSink,
) -> Result<RunOutcome> {
    runner.run_once(source, sink).await
}

async fn run_capture(database_url: &str, machine_slug: &str, display_name: &str) -> Result<()> {
    let writer = PgEventWriter::connect(database_url, machine_slug, display_name).await?;
    let mut source = WindowsSampleSource::new();
    let mut runner = default_runner();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut consecutive_failures: u32 = 0;
    let mut consecutive_gaps: u32 = 0;
    println!("event=runtime_ready machine_slug={machine_slug}");

    loop {
        tokio::select! {
            signal = &mut shutdown => {
                signal.context("listen for Ctrl-C")?;
                println!("event=shutdown reason=ctrl_c");
                return Ok(());
            }
            outcome = run_iteration(&mut runner, &mut source, &writer) => {
                match outcome {
                    Ok(outcome) => {
                        print_run_outcome(&outcome);
                        let kind = match &outcome {
                            RunOutcome::GapRecorded {
                                gap: screenpipe_memory::CaptureGap::DesktopLocked,
                            } => IterationKind::DesktopLocked,
                            RunOutcome::GapRecorded { .. } => IterationKind::Gap,
                            _ => IterationKind::Persisted,
                        };
                        match next_step(kind, &mut consecutive_failures, &mut consecutive_gaps) {
                            LoopStep::Continue => {}
                            LoopStep::Retry(delay) => tokio::time::sleep(delay).await,
                            LoopStep::AbortGaps => {
                                return Err(anyhow::anyhow!(
                                    "aborting after {MAX_CONSECUTIVE_GAPS} consecutive capture gaps"
                                ));
                            }
                            LoopStep::AbortFailures => unreachable!("no failure was recorded"),
                        }
                    }
                    Err(error) => {
                        // `Runner::run_once` deliberately RETAINS the
                        // unpersisted observation and the gap counters when a
                        // sink write fails, so the caller can retry the exact
                        // sample. Propagating with `?` here threw that away and
                        // ended the whole run on the first transient PostgreSQL
                        // blip - the single largest risk to an unattended
                        // 24-hour capture.
                        let step = next_step(
                            IterationKind::Failure,
                            &mut consecutive_failures,
                            &mut consecutive_gaps,
                        );
                        let category = failure_category(&error);
                        println!(
                            "event=capture_error category={category} consecutive={consecutive_failures}"
                        );
                        match step {
                            LoopStep::Retry(delay) => tokio::time::sleep(delay).await,
                            LoopStep::AbortFailures => {
                                // Do not spin forever on a permanent fault.
                                // Exiting hands off to the service wrapper's
                                // restart loop, which re-runs preflight from a
                                // clean process.
                                //
                                // The raw error is deliberately NOT propagated.
                                // `main` returns `anyhow::Result`, so Rust's
                                // Termination impl prints the whole `{:?}`
                                // chain to stderr - and the service wrapper now
                                // persists stderr to a dated log file that
                                // `uninstall` preserves. The redaction that
                                // `failure_category` exists to guarantee would
                                // have been abandoned on the one path that
                                // exits.
                                return Err(anyhow::anyhow!(
                                    "aborting after {MAX_CONSECUTIVE_FAILURES} consecutive capture failures (category={category})"
                                ));
                            }
                            LoopStep::Continue | LoopStep::AbortGaps => {
                                unreachable!("a failure never yields a gap decision")
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What one loop iteration produced, reduced to the only distinction the
/// restart policy cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IterationKind {
    /// An observation reached PostgreSQL. This is the only genuine success.
    Persisted,
    /// A typed capture gap on a live desktop. Ordinary and expected, but NOT
    /// a success - a persistent one means capture is broken.
    Gap,
    /// The desktop is locked or absent.
    ///
    /// Held separate from `Gap` because it is not a fault at all. A machine
    /// left locked overnight produces nothing else for eight hours, and the
    /// gap ceiling exists to catch a dead capture device - it must not fire on
    /// a normal night. Backs off, never aborts, and never touches either
    /// streak.
    DesktopLocked,
    /// The iteration returned an error.
    Failure,
}

/// What the run loop should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopStep {
    Continue,
    Retry(std::time::Duration),
    AbortGaps,
    AbortFailures,
}

/// The restart policy, extracted from the loop so it can be driven directly.
///
/// Inline, this was untestable, and it was wrong in a way no test could have
/// caught: the loop reset `consecutive_failures` on ANY `Ok`, and
/// `WindowsSampleSource` converts every capture and OCR failure into
/// `Ok(SampleRead::Gap(..))`. The ceiling was therefore unreachable for the
/// entire capture path. A permanently dead WGC device - GPU driver reset, D3D
/// device lost - printed `capture_gap` every two seconds until logoff:
/// ~43,000 futile attempts a day, zero events persisted, and the wrapper's
/// clean restart never reached. The backoff the ceiling exists to pair with
/// never applied either.
fn next_step(
    kind: IterationKind,
    consecutive_failures: &mut u32,
    consecutive_gaps: &mut u32,
) -> LoopStep {
    match kind {
        IterationKind::Persisted => {
            *consecutive_failures = 0;
            *consecutive_gaps = 0;
            LoopStep::Continue
        }
        IterationKind::DesktopLocked => {
            // Deliberately does NOT advance the gap streak. Waiting for a user
            // to come back is not a fault, and treating it as one made the
            // agent abort and restart every few hours on an idle machine.
            LoopStep::Retry(failure_backoff(MAX_BACKOFF_STEP))
        }
        IterationKind::Gap => {
            *consecutive_gaps = consecutive_gaps.saturating_add(1);
            if *consecutive_gaps >= MAX_CONSECUTIVE_GAPS {
                LoopStep::AbortGaps
            } else {
                LoopStep::Retry(failure_backoff(*consecutive_gaps))
            }
        }
        IterationKind::Failure => {
            *consecutive_failures = consecutive_failures.saturating_add(1);
            if *consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                LoopStep::AbortFailures
            } else {
                LoopStep::Retry(failure_backoff(*consecutive_failures))
            }
        }
    }
}

/// Consecutive failures tolerated before the process exits and lets the service
/// wrapper restart it from a clean state.
const MAX_CONSECUTIVE_FAILURES: u32 = 20;

/// Exponent clamp for `failure_backoff`. `2^5 = 32` seconds.
const MAX_BACKOFF_STEP: u32 = 5;

/// Consecutive capture gaps tolerated before the process exits.
///
/// Far higher than `MAX_CONSECUTIVE_FAILURES` because the two describe
/// different things. A failure is an error the pipeline could not absorb; a gap
/// is an ordinary, expected outcome - a locked workstation produces nothing
/// else, and must survive the night. At the 30s backoff ceiling this is a
/// little over five hours of uninterrupted gaps before handing off to the
/// wrapper, which is long enough for any legitimate lock and short enough that
/// a dead capture device does not spin until logoff.
const MAX_CONSECUTIVE_GAPS: u32 = 640;

/// Exponential backoff, capped at 32s.
///
/// Without this, a deterministically failing capture or OCR path retried every
/// two seconds forever - roughly 43,000 attempts a day, each one a wasted
/// capture, OCR, and log line.
///
/// The exponent clamp is what sets the ceiling: `2^5 = 32`. An additional
/// `.min(60)` used to sit here and could never bind, so the documented 60s cap
/// was unreachable and anyone raising the clamp to 6 expecting it to hold would
/// have got 64s instead.
fn failure_backoff(consecutive_failures: u32) -> std::time::Duration {
    let seconds = 2_u64.saturating_pow(consecutive_failures.min(MAX_BACKOFF_STEP));
    std::time::Duration::from_secs(seconds)
}

/// Bounded, redacted failure category.
///
/// Never includes the error text: capture, OCR, and browser-URL errors can
/// carry window titles, file paths, and URLs. Previously these errors were
/// discarded with no category at all, so a run that silently lost every browser
/// URL was externally indistinguishable from a healthy one.
fn failure_category(error: &anyhow::Error) -> &'static str {
    let detail = format!("{error:#}").to_ascii_lowercase();
    if detail.contains("postgres") || detail.contains("pool") || detail.contains("connection") {
        "postgres"
    } else if detail.contains("schema") {
        "schema"
    } else if detail.contains("ocr") {
        "ocr"
    } else if detail.contains("capture") || detail.contains("foreground") {
        "capture"
    } else if detail.contains("session") || detail.contains("desktop") {
        "session"
    } else {
        "other"
    }
}

fn print_run_outcome(outcome: &RunOutcome) {
    match outcome {
        RunOutcome::GapRecorded { gap } => {
            println!("event=capture_gap gap={}", gap.as_code());
        }
        RunOutcome::Started { event_id, reason } => {
            println!(
                "event=screen_started event_id={event_id} reason={}",
                reason.as_code()
            );
        }
        RunOutcome::Merged { event_id } => {
            println!("event=screen_merged event_id={event_id}");
        }
    }
}

async fn run_doctor(database_url: &str, machine_slug: &str, display_name: &str) -> Result<()> {
    println!("doctor secret=present variable={DATABASE_URL_ENV}");
    WindowsCapture
        .preflight_interactive()
        .context("verify interactive Windows session")?;
    println!("doctor interactive_windows=available");
    WindowsOcr
        .preflight()
        .context("verify Windows OCR engine")?;
    println!("doctor windows_ocr=available");

    let writer = PgEventWriter::connect(database_url, machine_slug, display_name)
        .await
        .context("verify PostgreSQL connection and machine identity")?;
    let report = writer.preflight().await?;
    ensure!(
        report.machine_slug == machine_slug && report.display_name == display_name,
        "PostgreSQL machine identity does not match requested identity"
    );
    // `preflight` returns Err unless the authoritative schema validated, so
    // reaching this line is itself the evidence that the schema is present.
    println!(
        "doctor postgres=available version={} schema=present machine_slug={} display_name={}",
        report.server_version, report.machine_slug, report.display_name
    );
    Ok(())
}

fn run_service_action(action: ServiceAction) -> anyhow::Result<()> {
    let service_root = ServiceRoot::current_user()?;
    let mut manager = ServiceManager::new(WindowsTaskScheduler);
    let status = match action {
        ServiceAction::Install => {
            let current_exe = std::env::current_exe().context("resolve running executable")?;
            manager.install(&service_root, &current_exe)?
        }
        ServiceAction::Uninstall => manager.uninstall(&service_root)?,
        ServiceAction::Status => manager.status(&service_root)?,
    };
    print_service_status(&status);
    Ok(())
}

fn print_service_status(status: &ServiceStatus) {
    println!(
        "task={} process={}",
        status.task_state,
        if status.process_running {
            "running"
        } else {
            "stopped"
        }
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{Duration, TimeZone, Utc};
    use clap::Parser;
    use screenpipe_memory::{
        CadenceInput, CadenceRecord, EventId, EventSink, MergeConfig, ObservationSample, OpenEvent,
        RunOutcome, Runner, SampleRead, SampleSource, SplitReason,
    };

    use super::{Cli, Command, IDLE_GAP_SECONDS, run_iteration};

    #[test]
    fn idle_gap_keeps_a_full_cadence_of_margin_above_the_slowest_cadence() {
        // A strictly-greater threshold is not enough. At max backoff the sleep
        // alone is MAX_CADENCE_INTERVAL_SECONDS, and the capture, the OCR pass,
        // and the sink write all land on top of it, so consecutive idle samples
        // are routinely several seconds further apart than the sleep. A
        // one-second margin is inside that overhead: every sample of a long
        // idle window would clear the threshold and open its own one-sample
        // event, collapsing the merge ratio Goal 1 measures. A full extra
        // cadence interval is the smallest margin that absorbs it.
        // A `const` block, not a runtime assert: both operands are constants,
        // so this is an invariant of the build rather than an observation about
        // a run. Evaluating it at compile time means trimming the margin fails
        // `cargo build`, not merely `cargo test` - it cannot be missed by
        // anyone who skips the suite.
        const {
            assert!(
                IDLE_GAP_SECONDS >= screenpipe_memory::MAX_CADENCE_INTERVAL_SECONDS * 2,
                "idle gap leaves no room above the slowest cadence"
            );
        }
    }

    struct QueuedSamples(std::collections::VecDeque<SampleRead>);

    #[async_trait]
    impl SampleSource for QueuedSamples {
        async fn next_sample(&mut self) -> Result<SampleRead> {
            Ok(self.0.pop_front().expect("another queued sample"))
        }
    }

    #[derive(Default)]
    struct MergeCountingSink {
        starts: Mutex<Vec<SplitReason>>,
        merges: Mutex<usize>,
    }

    #[async_trait]
    impl EventSink for MergeCountingSink {
        async fn start(&self, _event: &OpenEvent, reason: SplitReason) -> Result<EventId> {
            self.starts.lock().unwrap().push(reason);
            EventId::try_from("icarus_1".to_owned())
        }

        async fn merge(&self, _event_id: &str, _event: &OpenEvent) -> Result<()> {
            *self.merges.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn an_idle_window_sampled_at_max_backoff_merges_under_the_default_runner() {
        // The observable consequence of the margin above: samples arriving one
        // max-backoff sleep plus two seconds of capture and OCR overhead apart
        // must stay in one event. This drives the real `default_runner`, so it
        // fails if IDLE_GAP_SECONDS is ever trimmed to something the overhead
        // can cross.
        let overhead = 2;
        let spacing = screenpipe_memory::MAX_CADENCE_INTERVAL_SECONDS + overhead;
        let base = Utc.with_ymd_and_hms(2026, 8, 4, 12, 0, 0).single().unwrap();
        let idle_cadence = CadenceRecord::from_input(CadenceInput {
            input_idle: Duration::seconds(600),
            frame_stable_for: Duration::seconds(600),
            foreground_changed: false,
            frame_changed: false,
        });
        let mut source = QueuedSamples(
            (0..4)
                .map(|index| SampleRead::Sample {
                    sample: ObservationSample {
                        captured_at: base + Duration::seconds(index * spacing),
                        app_key: "notepad.exe".to_owned(),
                        app_title: "Notepad".to_owned(),
                        window_title: "Goal 1".to_owned(),
                        ocr_text: "an unchanged idle window".to_owned(),
                        readable_text: "an unchanged idle window".to_owned(),
                        browser_url: None,
                    },
                    cadence: idle_cadence,
                })
                .collect(),
        );
        let sink = MergeCountingSink::default();
        let mut runner = super::default_runner();

        for _ in 0..4 {
            run_iteration(&mut runner, &mut source, &sink)
                .await
                .unwrap();
        }

        assert_eq!(
            *sink.starts.lock().unwrap(),
            [SplitReason::Initial],
            "an unchanged idle window must open exactly one event, not fragment"
        );
        assert_eq!(*sink.merges.lock().unwrap(), 3);
    }

    struct OneSample(Option<SampleRead>);

    #[async_trait]
    impl SampleSource for OneSample {
        async fn next_sample(&mut self) -> Result<SampleRead> {
            Ok(self.0.take().expect("one sample available"))
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        starts: Mutex<Vec<SplitReason>>,
    }

    #[async_trait]
    impl EventSink for RecordingSink {
        async fn start(&self, _event: &OpenEvent, reason: SplitReason) -> Result<EventId> {
            self.starts.lock().unwrap().push(reason);
            EventId::try_from("icarus_1".to_owned())
        }

        async fn merge(&self, _event_id: &str, _event: &OpenEvent) -> Result<()> {
            unreachable!("first sample cannot merge")
        }
    }

    #[test]
    fn run_command_defaults_to_the_icarus_machine_identity() {
        let cli = Cli::try_parse_from(["screenpipe", "run"]).unwrap();
        let Command::Run {
            machine_slug,
            display_name,
        } = cli.command
        else {
            panic!("run command expected");
        };
        assert_eq!(machine_slug, "icarus");
        assert_eq!(display_name, "Icarus-Laptop");
    }

    #[test]
    fn search_command_scopes_reads_to_the_explicit_machine() {
        let cli = Cli::try_parse_from([
            "screenpipe",
            "search",
            "--machine-slug",
            "laptop_b",
            "release",
        ])
        .unwrap();
        let Command::Search {
            machine_slug,
            query,
            ..
        } = cli.command
        else {
            panic!("search command expected");
        };

        assert_eq!(machine_slug, "laptop_b");
        assert_eq!(query, ["release"]);
    }

    #[tokio::test]
    async fn run_iteration_wires_a_sample_through_runner_and_sink() {
        let captured_at = Utc.with_ymd_and_hms(2026, 8, 4, 12, 0, 0).single().unwrap();
        let cadence = CadenceRecord::from_input(CadenceInput {
            input_idle: Duration::zero(),
            frame_stable_for: Duration::zero(),
            foreground_changed: false,
            frame_changed: false,
        });
        let mut source = OneSample(Some(SampleRead::Sample {
            sample: ObservationSample {
                captured_at,
                app_key: "notepad.exe".to_owned(),
                app_title: "Notepad".to_owned(),
                window_title: "Goal 1".to_owned(),
                ocr_text: "runner wiring sample".to_owned(),
                readable_text: "runner wiring sample".to_owned(),
                browser_url: None,
            },
            cadence,
        }));
        let sink = RecordingSink::default();
        let mut runner = Runner::new(MergeConfig {
            idle_gap: Duration::seconds(30),
            scroll_overlap: 0.35,
        });

        let outcome = run_iteration(&mut runner, &mut source, &sink)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            RunOutcome::Started {
                event_id: "icarus_1".to_owned(),
                reason: SplitReason::Initial,
            }
        );
        assert_eq!(*sink.starts.lock().unwrap(), [SplitReason::Initial]);
    }

    #[test]
    fn failure_backoff_grows_then_caps_and_is_never_zero() {
        // A zero or flat backoff turns a deterministic failure into ~43,000
        // wasted capture+OCR attempts a day.
        let delays: Vec<u64> = (1..=8)
            .map(|attempt| super::failure_backoff(attempt).as_secs())
            .collect();

        assert!(
            delays.iter().all(|seconds| *seconds > 0),
            "backoff must never be zero: {delays:?}"
        );
        assert!(
            delays.windows(2).all(|pair| pair[1] >= pair[0]),
            "backoff must be monotonically non-decreasing: {delays:?}"
        );
        assert!(
            delays[1] > delays[0],
            "backoff must actually grow, not stay flat: {delays:?}"
        );
        // Assert the REAL ceiling. The previous bound here was `<= 60`, which
        // is vacuous against a function that clamps the exponent to 5 and can
        // never exceed 32 - it agreed with a doc comment that claimed a 60s cap
        // the code could not produce.
        assert!(
            delays.iter().all(|seconds| *seconds <= 32),
            "backoff must stay capped so a restart is never delayed unboundedly: {delays:?}"
        );
        // The cap must be reached, or a long outage backs off toward hours.
        assert_eq!(*delays.last().expect("delays"), 32);
    }

    #[test]
    fn an_endless_run_of_gaps_terminates_instead_of_spinning_forever() {
        use super::{IterationKind, LoopStep, next_step};

        // The defect this pins: `WindowsSampleSource` turns every capture and
        // OCR failure into `Ok(SampleRead::Gap(..))`, and the loop reset the
        // failure counter on any `Ok`. A dead capture device therefore never
        // reached any ceiling - it printed `capture_gap` every two seconds
        // until logoff and never handed off to the wrapper's clean restart.
        let mut failures = 0;
        let mut gaps = 0;
        let mut steps = 0_u32;
        loop {
            let step = next_step(IterationKind::Gap, &mut failures, &mut gaps);
            steps += 1;
            if step == LoopStep::AbortGaps {
                break;
            }
            assert!(
                matches!(step, LoopStep::Retry(_)),
                "a gap must back off, not continue at full speed: {step:?}"
            );
            assert!(steps < 10_000, "gap handling never terminates");
        }
        assert_eq!(steps, super::MAX_CONSECUTIVE_GAPS);
        assert_eq!(
            failures, 0,
            "gaps must not be counted as failures - the two ceilings differ on purpose"
        );
    }

    #[test]
    fn time_expressions_a_person_would_actually_type() {
        use super::parse_when;
        use chrono::{Local, TimeZone, Utc};

        let now = Utc::now();

        // Relative windows land in the past, in the right ballpark.
        let ninety = parse_when("90m").unwrap();
        let delta = (now - ninety).num_minutes();
        assert!(
            (89..=91).contains(&delta),
            "90m resolved to {delta} minutes ago"
        );
        assert!(
            ((3 * 60 - 1)..=(4 * 60 + 1))
                .contains(&(now - parse_when("4h").unwrap()).num_minutes())
        );
        // Hours, not days: `now` is sampled before parse_when runs, so the
        // delta is a hair under 3 days and num_days() would floor it to 2.
        let three_days = (now - parse_when("3d").unwrap()).num_hours();
        assert!(
            (71..=72).contains(&three_days),
            "3d resolved to {three_days} hours ago"
        );

        // `today` is LOCAL midnight, not UTC midnight. Getting this wrong
        // silently shifts the window by the timezone offset, which is exactly
        // the kind of error nobody notices until a search misses.
        let today = parse_when("today").unwrap();
        let expected = Local
            .from_local_datetime(&Local::now().date_naive().and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(today, expected);
        let yesterday = parse_when("yesterday").unwrap();
        assert_eq!(
            yesterday.with_timezone(&Local).date_naive(),
            today.with_timezone(&Local).date_naive().pred_opt().unwrap(),
            "yesterday must select the preceding local calendar date, including across DST"
        );
        assert!(yesterday < today, "yesterday must precede today");

        // Absolute dates.
        assert_eq!(
            parse_when("2026-08-06").unwrap(),
            Local
                .from_local_datetime(
                    &chrono::NaiveDate::from_ymd_opt(2026, 8, 6)
                        .unwrap()
                        .and_hms_opt(0, 0, 0)
                        .unwrap()
                )
                .single()
                .unwrap()
                .with_timezone(&Utc)
        );

        // Nonsense must be refused with something a person can act on, not
        // silently treated as "now" - which would quietly return everything.
        let error = parse_when("last tuesday").unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("try 90m"),
            "the error must say what IS accepted: {rendered}"
        );
        assert!(parse_when("").is_err());
        assert!(parse_when("4 hours").is_err());
    }

    #[test]
    fn relative_time_rejects_a_leading_minus_instead_of_searching_the_future() {
        use super::parse_when;

        for value in ["-1m", "-4h", "-3d"] {
            let error = parse_when(value).expect_err("{value} must not become a future window");
            assert!(
                format!("{error:#}").contains("must not be negative"),
                "{value} returned the wrong error: {error:#}"
            );
        }
    }

    #[test]
    fn relative_time_rejects_an_interval_that_chrono_cannot_represent() {
        use super::parse_when;

        for value in [
            "9223372036854775807m",
            "9223372036854775807h",
            "9223372036854775807d",
        ] {
            let error = parse_when(value).expect_err("{value} must not panic or wrap");
            assert!(
                format!("{error:#}").contains("is too large"),
                "{value} returned the wrong error: {error:#}"
            );
        }
    }

    #[test]
    fn a_locked_desktop_never_reaches_the_gap_ceiling() {
        use super::{IterationKind, LoopStep, next_step};

        // A machine left locked overnight produces nothing but gaps for eight
        // hours. The gap ceiling exists to catch a DEAD CAPTURE DEVICE, and
        // before `DesktopLocked` was typed the two were the same code, so the
        // agent would abort and restart every few hours on an idle machine.
        //
        // Ten times the gap ceiling: if a lock advanced either streak at all,
        // this would abort long before the loop ends.
        let mut failures = 0;
        let mut gaps = 0;
        for _ in 0..(super::MAX_CONSECUTIVE_GAPS * 10) {
            let step = next_step(IterationKind::DesktopLocked, &mut failures, &mut gaps);
            assert!(
                matches!(step, LoopStep::Retry(_)),
                "a locked desktop must back off and keep waiting, got {step:?}"
            );
        }
        assert_eq!(gaps, 0, "a lock advanced the gap streak");
        assert_eq!(failures, 0, "a lock advanced the failure streak");
    }

    #[test]
    fn a_locked_desktop_waits_at_the_slowest_backoff() {
        use super::{IterationKind, next_step};

        // Polling a locked desktop every two seconds is 43,000 futile probes a
        // day. It must sit at the ceiling, not the floor.
        let mut failures = 0;
        let mut gaps = 0;
        let step = next_step(IterationKind::DesktopLocked, &mut failures, &mut gaps);
        let super::LoopStep::Retry(delay) = step else {
            panic!("expected a retry, got {step:?}");
        };
        assert_eq!(delay, super::failure_backoff(super::MAX_BACKOFF_STEP));
        assert_eq!(delay.as_secs(), 32);
    }

    #[test]
    fn a_gap_does_not_clear_an_outstanding_failure_streak() {
        use super::{IterationKind, LoopStep, next_step};

        // Only a persisted observation is a success. If a gap cleared the
        // failure streak, a sink that failed on every write while capture kept
        // producing gaps would never reach its ceiling either.
        let mut failures = 0;
        let mut gaps = 0;
        next_step(IterationKind::Failure, &mut failures, &mut gaps);
        next_step(IterationKind::Failure, &mut failures, &mut gaps);
        assert_eq!(failures, 2);

        next_step(IterationKind::Gap, &mut failures, &mut gaps);
        assert_eq!(failures, 2, "a gap cleared a real failure streak");

        assert_eq!(
            next_step(IterationKind::Persisted, &mut failures, &mut gaps),
            LoopStep::Continue
        );
        assert_eq!(failures, 0, "a persisted event must clear the streak");
        assert_eq!(gaps, 0, "a persisted event must clear the gap streak");
    }

    #[test]
    fn a_persistent_failure_reaches_the_ceiling_and_aborts() {
        use super::{IterationKind, LoopStep, next_step};

        let mut failures = 0;
        let mut gaps = 0;
        for _ in 1..super::MAX_CONSECUTIVE_FAILURES {
            assert!(matches!(
                next_step(IterationKind::Failure, &mut failures, &mut gaps),
                LoopStep::Retry(_)
            ));
        }
        assert_eq!(
            next_step(IterationKind::Failure, &mut failures, &mut gaps),
            LoopStep::AbortFailures
        );
    }

    #[test]
    fn failure_categories_are_fixed_codes_that_never_echo_error_text() {
        // The category is printed to the service log. Window titles, file
        // paths, and URLs must never reach it.
        let secret = "https://private.example.test/token?value=hunter2";
        let cases = [
            (
                anyhow::anyhow!("PostgreSQL pool timed out")
                    .context("insert allocated screen event"),
                "postgres",
            ),
            (anyhow::anyhow!("windows OCR engine unavailable"), "ocr"),
            (
                anyhow::anyhow!("capture foreground window through Windows Graphics Capture"),
                "capture",
            ),
            (
                anyhow::anyhow!("process is not running in the active interactive Windows session"),
                "session",
            ),
            (anyhow::anyhow!("something else entirely"), "other"),
        ];

        for (error, expected) in cases {
            assert_eq!(super::failure_category(&error), expected, "for {error:#}");
        }

        // Whatever the error carries, the category is one of a fixed set and
        // contains none of it.
        let leaky = anyhow::anyhow!("failed reading {secret}");
        let category = super::failure_category(&leaky);
        assert!(
            ["postgres", "schema", "ocr", "capture", "session", "other"].contains(&category),
            "unexpected category {category}"
        );
        assert!(!category.contains("hunter2") && !category.contains("example.test"));
    }

    #[test]
    fn consecutive_failure_ceiling_allows_recovery_but_still_terminates() {
        // High enough that ordinary transients (a PostgreSQL restart, a lock
        // screen) are ridden out, low enough that a permanent fault reaches the
        // wrapper restart in minutes rather than never.
        // Compile-time, for the same reason as the idle-gap invariant above:
        // this bounds a constant, so it should fail the build.
        const {
            assert!(super::MAX_CONSECUTIVE_FAILURES >= 10);
            assert!(super::MAX_CONSECUTIVE_FAILURES <= 100);
        }

        let total: u64 = (1..=super::MAX_CONSECUTIVE_FAILURES)
            .map(|attempt| super::failure_backoff(attempt).as_secs())
            .sum();
        assert!(
            (120..=3600).contains(&total),
            "total retry window before handing off to the wrapper was {total}s"
        );
    }
}
