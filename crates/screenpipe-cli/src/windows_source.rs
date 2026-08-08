use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use screenpipe_memory::{
    CadenceInput, CadenceRecord, CaptureGap, ObservationSample, SampleRead, SampleSource,
};
use screenpipe_screen::{
    BrowserUrlReader, ForegroundMetadata, FrameFingerprint, TransientFrame, WindowsCapture,
    WindowsLastInput, WindowsOcr,
};

const RETRY_CADENCE: Duration = Duration::from_secs(2);

fn cadence_record(
    input_idle: Duration,
    frame_stable_for: Duration,
    foreground_changed: bool,
    frame_changed: bool,
) -> Result<CadenceRecord> {
    Ok(CadenceRecord::from_input(CadenceInput {
        input_idle: chrono::Duration::from_std(input_idle)
            .context("input-idle duration exceeds chrono range")?,
        frame_stable_for: chrono::Duration::from_std(frame_stable_for)
            .context("frame-stability duration exceeds chrono range")?,
        foreground_changed,
        frame_changed,
    }))
}

#[async_trait]
trait WindowsSampleOps: Send {
    async fn sleep(&mut self, duration: Duration);
    async fn now(&mut self) -> (DateTime<Utc>, Instant);
    async fn capture_foreground(&mut self) -> Result<(TransientFrame, ForegroundMetadata)>;
    async fn recognize(&mut self, frame: &TransientFrame) -> Result<String>;
    async fn input_idle(&mut self) -> Result<Duration>;
    async fn browser_url(&mut self, metadata: &ForegroundMetadata) -> Result<Option<String>>;
    /// False when there is no unlocked interactive desktop to capture.
    fn interactive_desktop_available(&mut self) -> bool;
}

struct LiveWindowsOps;

#[async_trait]
impl WindowsSampleOps for LiveWindowsOps {
    fn interactive_desktop_available(&mut self) -> bool {
        matches!(
            screenpipe_screen::probe_interactive_capability(),
            screenpipe_screen::InteractiveCapability::Available
        )
    }

    async fn sleep(&mut self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn now(&mut self) -> (DateTime<Utc>, Instant) {
        (Utc::now(), Instant::now())
    }

    async fn capture_foreground(&mut self) -> Result<(TransientFrame, ForegroundMetadata)> {
        WindowsCapture.capture_foreground().await
    }

    async fn recognize(&mut self, frame: &TransientFrame) -> Result<String> {
        WindowsOcr.recognize(frame).await
    }

    async fn input_idle(&mut self) -> Result<Duration> {
        WindowsLastInput::idle_for()
    }

    async fn browser_url(&mut self, metadata: &ForegroundMetadata) -> Result<Option<String>> {
        BrowserUrlReader
            .read_for_foreground(metadata)
            .map(|url| url.map(|url| url.to_string()))
    }
}

#[derive(PartialEq, Eq)]
struct ForegroundKey {
    window_handle: isize,
    app_key: String,
    window_title: String,
}

impl From<&ForegroundMetadata> for ForegroundKey {
    fn from(metadata: &ForegroundMetadata) -> Self {
        Self {
            window_handle: metadata.window_handle,
            app_key: metadata.app_key.clone(),
            window_title: metadata.window_title.clone(),
        }
    }
}

struct SuccessfulCache {
    foreground: ForegroundKey,
    fingerprint: FrameFingerprint,
    ocr_text: String,
    stable_since: Instant,
}

struct Source<Ops> {
    ops: Ops,
    cache: Option<SuccessfulCache>,
    next_sleep: Option<Duration>,
}

impl<Ops> Source<Ops> {
    fn new(ops: Ops) -> Self {
        Self {
            ops,
            cache: None,
            next_sleep: None,
        }
    }
}

impl<Ops: WindowsSampleOps> Source<Ops> {
    async fn read(&mut self) -> Result<SampleRead> {
        if let Some(duration) = self.next_sleep {
            self.ops.sleep(duration).await;
        }

        // Ask whether there IS an interactive desktop before trying to capture
        // one. `probe_interactive_capability` existed for the test gate and had
        // no production caller at all, so a locked workstation surfaced as
        // `capture_unavailable` - the same code as a dead capture device.
        //
        // Two things went wrong with that. The run loop's gap ceiling, which
        // exists so a broken WGC path cannot spin until logoff, could not tell
        // an overnight lock from a real fault and would fire on the lock. And
        // the seam runbook had no durable evidence that a lock had happened at
        // all, so seam 2 could only be verified by watching a pid.
        if !self.ops.interactive_desktop_available() {
            self.next_sleep = Some(RETRY_CADENCE);
            return Ok(SampleRead::Gap(CaptureGap::DesktopLocked));
        }

        let (frame, metadata) = match self.ops.capture_foreground().await {
            Ok(capture) => capture,
            Err(_) => {
                self.next_sleep = Some(RETRY_CADENCE);
                // Re-probe before blaming capture.
                //
                // The pre-capture probe is not enough on its own: locking a
                // workstation stops capture working BEFORE `OpenInputDesktop`
                // starts refusing, so the probe still answers "available" for
                // the first few samples of a lock. Measured on a real lock,
                // that window was six samples - all recorded as
                // `capture_unavailable`, which is the code for a broken
                // capture device, and all of them counted toward the ceiling
                // that exists to catch one.
                //
                // Asking again after the failure closes the window: if the
                // desktop has gone by the time capture failed, the lock is the
                // explanation, not a fault.
                let gap = if self.ops.interactive_desktop_available() {
                    CaptureGap::CaptureUnavailable
                } else {
                    CaptureGap::DesktopLocked
                };
                return Ok(SampleRead::Gap(gap));
            }
        };
        let (captured_at, monotonic_now) = self.ops.now().await;
        let foreground = ForegroundKey::from(&metadata);
        let fingerprint = frame.fingerprint();
        let foreground_changed = self
            .cache
            .as_ref()
            .is_some_and(|cache| cache.foreground != foreground);
        let frame_changed = self
            .cache
            .as_ref()
            .is_some_and(|cache| cache.fingerprint != fingerprint);
        let unchanged = self.cache.as_ref().is_some_and(|cache| {
            cache.foreground == foreground && cache.fingerprint == fingerprint
        });

        let ocr_text = if unchanged {
            self.cache
                .as_ref()
                .expect("unchanged capture must have a successful cache")
                .ocr_text
                .clone()
        } else {
            match self.ops.recognize(&frame).await {
                Ok(text) if !text.trim().is_empty() => text,
                Ok(_) => {
                    drop(frame);
                    self.next_sleep = Some(RETRY_CADENCE);
                    return Ok(SampleRead::Gap(CaptureGap::EmptyOcr));
                }
                Err(_) => {
                    drop(frame);
                    self.next_sleep = Some(RETRY_CADENCE);
                    return Ok(SampleRead::Gap(CaptureGap::OcrUnavailable));
                }
            }
        };
        drop(frame);

        let input_idle = match self.ops.input_idle().await {
            Ok(duration) => duration,
            Err(error) => return Err(error),
        };
        let stable_since = if foreground_changed || frame_changed {
            monotonic_now
        } else {
            self.cache
                .as_ref()
                .map_or(monotonic_now, |cache| cache.stable_since)
        };
        let frame_stable_for = monotonic_now
            .checked_duration_since(stable_since)
            .context("monotonic clock moved backwards")?;
        let cadence = cadence_record(
            input_idle,
            frame_stable_for,
            foreground_changed,
            frame_changed,
        )?;
        let next_sleep = cadence
            .next_interval
            .to_std()
            .context("cadence interval must be nonnegative and in range")?;

        let browser_url = if matches!(
            metadata.app_key.to_ascii_lowercase().as_str(),
            "chrome.exe" | "msedge.exe"
        ) {
            self.ops.browser_url(&metadata).await.unwrap_or(None)
        } else {
            None
        };
        let sample = ObservationSample {
            captured_at,
            app_key: metadata.app_key,
            app_title: metadata.app_title,
            window_title: metadata.window_title,
            ocr_text: ocr_text.clone(),
            readable_text: ocr_text.clone(),
            browser_url,
        };

        self.cache = Some(SuccessfulCache {
            foreground,
            fingerprint,
            ocr_text,
            stable_since,
        });
        self.next_sleep = Some(next_sleep);

        Ok(SampleRead::Sample { sample, cadence })
    }
}

#[async_trait]
impl<Ops: WindowsSampleOps> SampleSource for Source<Ops> {
    async fn next_sample(&mut self) -> Result<SampleRead> {
        self.read().await
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct WindowsSampleSource {
    inner: Source<LiveWindowsOps>,
}

impl WindowsSampleSource {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new() -> Self {
        Self {
            inner: Source::new(LiveWindowsOps),
        }
    }
}

#[async_trait]
impl SampleSource for WindowsSampleSource {
    async fn next_sample(&mut self) -> Result<SampleRead> {
        self.inner.next_sample().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use screenpipe_memory::{CaptureGap, SampleRead, SampleSource};
    use screenpipe_screen::{ForegroundMetadata, TransientFrame};

    use super::{Source, WindowsSampleOps, WindowsSampleSource, cadence_record};

    type CaptureResult = Result<(TransientFrame, ForegroundMetadata)>;

    #[derive(Default)]
    struct CallLog {
        sleeps: Vec<Duration>,
        captures: usize,
        ocr: usize,
        input_idle: usize,
        browser_url: usize,
    }

    struct TestOps {
        clock: Arc<Mutex<(chrono::DateTime<Utc>, Instant)>>,
        calls: Arc<Mutex<CallLog>>,
        captures: VecDeque<CaptureResult>,
        ocr: VecDeque<Result<String>>,
        input_idle: VecDeque<Result<Duration>>,
        browser_urls: VecDeque<Result<Option<String>>>,
        interactive_desktop: bool,
        desktop_answers: VecDeque<bool>,
    }

    impl TestOps {
        fn new(
            clock: Arc<Mutex<(chrono::DateTime<Utc>, Instant)>>,
            calls: Arc<Mutex<CallLog>>,
            captures: impl IntoIterator<Item = CaptureResult>,
            ocr: impl IntoIterator<Item = Result<String>>,
            input_idle: impl IntoIterator<Item = Result<Duration>>,
            browser_urls: impl IntoIterator<Item = Result<Option<String>>>,
        ) -> Self {
            Self {
                clock,
                calls,
                captures: captures.into_iter().collect(),
                ocr: ocr.into_iter().collect(),
                input_idle: input_idle.into_iter().collect(),
                browser_urls: browser_urls.into_iter().collect(),
                interactive_desktop: true,
                desktop_answers: VecDeque::new(),
            }
        }

        /// Answer the desktop probe from a script, in order.
        fn with_desktop_answers(mut self, answers: impl IntoIterator<Item = bool>) -> Self {
            self.desktop_answers = answers.into_iter().collect();
            self
        }

        fn with_locked_desktop(mut self) -> Self {
            self.interactive_desktop = false;
            self
        }

        fn next<T>(queue: &mut VecDeque<Result<T>>, boundary: &str) -> Result<T> {
            queue
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected {boundary} boundary call"))
        }
    }

    #[async_trait]
    impl WindowsSampleOps for TestOps {
        fn interactive_desktop_available(&mut self) -> bool {
            // Every pre-existing test predates the desktop probe and asserts
            // behaviour that only happens on an unlocked desktop. Defaulting to
            // `true` keeps them meaning what they meant; the locked fixtures
            // below override it.
            //
            // A queue rather than a flag, because the defect this models is
            // precisely that the answer CHANGES between the pre-capture probe
            // and the post-failure one.
            self.desktop_answers
                .pop_front()
                .unwrap_or(self.interactive_desktop)
        }

        async fn sleep(&mut self, duration: Duration) {
            self.calls.lock().unwrap().sleeps.push(duration);
        }

        async fn now(&mut self) -> (chrono::DateTime<Utc>, Instant) {
            *self.clock.lock().unwrap()
        }

        async fn capture_foreground(&mut self) -> CaptureResult {
            self.calls.lock().unwrap().captures += 1;
            Self::next(&mut self.captures, "capture")
        }

        async fn recognize(&mut self, _frame: &TransientFrame) -> Result<String> {
            self.calls.lock().unwrap().ocr += 1;
            Self::next(&mut self.ocr, "OCR")
        }

        async fn input_idle(&mut self) -> Result<Duration> {
            self.calls.lock().unwrap().input_idle += 1;
            Self::next(&mut self.input_idle, "input-idle")
        }

        async fn browser_url(&mut self, _metadata: &ForegroundMetadata) -> Result<Option<String>> {
            self.calls.lock().unwrap().browser_url += 1;
            Self::next(&mut self.browser_urls, "browser URL")
        }
    }

    struct Harness {
        base_utc: chrono::DateTime<Utc>,
        base_instant: Instant,
        clock: Arc<Mutex<(chrono::DateTime<Utc>, Instant)>>,
        calls: Arc<Mutex<CallLog>>,
    }

    impl Harness {
        fn new() -> Self {
            let base_utc = Utc.with_ymd_and_hms(2026, 8, 3, 12, 0, 0).single().unwrap();
            let base_instant = Instant::now();
            Self {
                base_utc,
                base_instant,
                clock: Arc::new(Mutex::new((base_utc, base_instant))),
                calls: Arc::new(Mutex::new(CallLog::default())),
            }
        }

        fn at(&self, seconds: u64) {
            *self.clock.lock().unwrap() = (
                self.base_utc + chrono::Duration::seconds(seconds as i64),
                self.base_instant + Duration::from_secs(seconds),
            );
        }

        /// A desktop that still probes as available, then fails to capture,
        /// then probes as gone - the real transition when a screen locks.
        fn locking_source(&self) -> Source<TestOps> {
            Source::new(
                TestOps::new(
                    Arc::clone(&self.clock),
                    Arc::clone(&self.calls),
                    [Err(anyhow::anyhow!("foreground window is gone"))],
                    [],
                    [],
                    [],
                )
                .with_desktop_answers([true, false]),
            )
        }

        fn locked_source(&self) -> Source<TestOps> {
            // No capture, OCR, or input-idle results are queued ON PURPOSE.
            // TestOps::next panics on an unexpected boundary call, so if the
            // locked-desktop check is ever removed or moved below the capture
            // attempt, this test fails loudly instead of quietly reporting a
            // different gap kind.
            Source::new(
                TestOps::new(
                    Arc::clone(&self.clock),
                    Arc::clone(&self.calls),
                    [],
                    [],
                    [],
                    [],
                )
                .with_locked_desktop(),
            )
        }

        fn source(
            &self,
            captures: impl IntoIterator<Item = CaptureResult>,
            ocr: impl IntoIterator<Item = Result<String>>,
            input_idle: impl IntoIterator<Item = Result<Duration>>,
            browser_urls: impl IntoIterator<Item = Result<Option<String>>>,
        ) -> Source<TestOps> {
            Source::new(TestOps::new(
                Arc::clone(&self.clock),
                Arc::clone(&self.calls),
                captures,
                ocr,
                input_idle,
                browser_urls,
            ))
        }
    }

    fn frame(pixel: u8) -> TransientFrame {
        TransientFrame::from_bgra(1, 1, 4, vec![pixel, 2, 3, 4]).unwrap()
    }

    fn metadata(handle: isize, app_key: &str, window_title: &str) -> ForegroundMetadata {
        ForegroundMetadata {
            window_handle: handle,
            app_key: app_key.to_owned(),
            app_title: app_key.trim_end_matches(".exe").to_owned(),
            window_title: window_title.to_owned(),
            browser_url: None,
        }
    }

    fn sample(
        read: SampleRead,
    ) -> (
        screenpipe_memory::ObservationSample,
        screenpipe_memory::CadenceRecord,
    ) {
        match read {
            SampleRead::Sample { sample, cadence } => (sample, cadence),
            SampleRead::Gap(gap) => panic!("expected sample, got {gap:?}"),
        }
    }

    #[tokio::test]
    async fn first_sample_does_not_sleep_and_schedules_two_seconds() {
        let harness = Harness::new();
        let mut source = harness.source(
            [Ok((frame(1), metadata(10, "notepad.exe", "notes")))],
            [Ok("hello".to_owned())],
            [Ok(Duration::ZERO)],
            [],
        );

        let (actual, cadence) = sample(source.next_sample().await.unwrap());

        assert_eq!(actual.ocr_text, "hello");
        assert_eq!(actual.readable_text, "hello");
        assert_eq!(cadence.next_interval, chrono::Duration::seconds(2));
        assert!(harness.calls.lock().unwrap().sleeps.is_empty());
    }

    #[tokio::test]
    async fn identical_foreground_and_frame_sleep_then_reuse_ocr() {
        let harness = Harness::new();
        let mut source = harness.source(
            [
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
            ],
            [Ok("cached text".to_owned())],
            [Ok(Duration::ZERO), Ok(Duration::from_secs(10))],
            [],
        );

        source.next_sample().await.unwrap();
        harness.at(10);
        let (actual, cadence) = sample(source.next_sample().await.unwrap());

        assert_eq!(actual.ocr_text, "cached text");
        assert_eq!(
            cadence.input.frame_stable_for,
            chrono::Duration::seconds(10)
        );
        assert!(!cadence.input.foreground_changed);
        assert!(!cadence.input.frame_changed);
        let calls = harness.calls.lock().unwrap();
        assert_eq!(calls.sleeps, [Duration::from_secs(2)]);
        assert_eq!(calls.ocr, 1);
    }

    #[tokio::test]
    async fn each_foreground_identity_component_and_pixels_independently_force_ocr() {
        struct Case {
            name: &'static str,
            second: (TransientFrame, ForegroundMetadata),
            foreground_changed: bool,
            frame_changed: bool,
        }

        let cases = [
            Case {
                name: "HWND",
                second: (frame(1), metadata(11, "notepad.exe", "notes")),
                foreground_changed: true,
                frame_changed: false,
            },
            Case {
                name: "app key",
                second: (frame(1), metadata(10, "wordpad.exe", "notes")),
                foreground_changed: true,
                frame_changed: false,
            },
            Case {
                name: "window title",
                second: (frame(1), metadata(10, "notepad.exe", "other")),
                foreground_changed: true,
                frame_changed: false,
            },
            Case {
                name: "pixels",
                second: (frame(9), metadata(10, "notepad.exe", "notes")),
                foreground_changed: false,
                frame_changed: true,
            },
        ];

        for case in cases {
            let harness = Harness::new();
            let mut source = harness.source(
                [
                    Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                    Ok(case.second),
                ],
                [Ok("before".to_owned()), Ok("after".to_owned())],
                [Ok(Duration::ZERO), Ok(Duration::from_secs(30))],
                [],
            );
            source.next_sample().await.unwrap();
            harness.at(30);

            let (actual, cadence) = sample(source.next_sample().await.unwrap());

            assert_eq!(actual.ocr_text, "after", "{}", case.name);
            assert_eq!(
                cadence.input.foreground_changed, case.foreground_changed,
                "{}",
                case.name
            );
            assert_eq!(
                cadence.input.frame_changed, case.frame_changed,
                "{}",
                case.name
            );
            assert_eq!(
                cadence.input.frame_stable_for,
                chrono::Duration::zero(),
                "{}",
                case.name
            );
            assert_eq!(
                cadence.next_interval,
                chrono::Duration::seconds(2),
                "{}",
                case.name
            );
            assert_eq!(harness.calls.lock().unwrap().ocr, 2, "{}", case.name);
        }
    }

    #[tokio::test]
    async fn literal_stability_and_idle_thresholds_select_five_fifteen_thirty_then_recent_input_two()
     {
        let harness = Harness::new();
        let mut source = harness.source(
            [
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
            ],
            [Ok("stable".to_owned())],
            [
                Ok(Duration::ZERO),
                Ok(Duration::from_secs(30)),
                Ok(Duration::from_secs(120)),
                Ok(Duration::from_secs(600)),
                Ok(Duration::from_secs(1)),
            ],
            [],
        );

        let (_, initial) = sample(source.next_sample().await.unwrap());
        assert_eq!(initial.next_interval, chrono::Duration::seconds(2));
        for (at, expected) in [(30, 5), (120, 15), (600, 30), (601, 2)] {
            harness.at(at);
            let (_, cadence) = sample(source.next_sample().await.unwrap());
            assert_eq!(
                cadence.next_interval,
                chrono::Duration::seconds(expected),
                "at {at}s"
            );
        }
        assert_eq!(harness.calls.lock().unwrap().ocr, 1);
    }

    #[tokio::test]
    async fn capture_gap_preserves_successful_cache_and_recovery_uses_gap_cadence() {
        let harness = Harness::new();
        let mut source = harness.source(
            [
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Err(anyhow!("capture unavailable")),
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
            ],
            [Ok("cached".to_owned())],
            [Ok(Duration::ZERO), Ok(Duration::from_secs(4))],
            [],
        );

        source.next_sample().await.unwrap();
        harness.at(2);
        assert_eq!(
            source.next_sample().await.unwrap(),
            SampleRead::Gap(CaptureGap::CaptureUnavailable)
        );
        harness.at(4);
        let (recovered, cadence) = sample(source.next_sample().await.unwrap());

        assert_eq!(recovered.ocr_text, "cached");
        assert_eq!(cadence.input.frame_stable_for, chrono::Duration::seconds(4));
        let calls = harness.calls.lock().unwrap();
        assert_eq!(
            calls.sleeps,
            [Duration::from_secs(2), Duration::from_secs(2)]
        );
        assert_eq!(calls.ocr, 1);
    }

    #[tokio::test]
    async fn ocr_gaps_override_long_cadence_and_retry_changed_frame_without_stale_reuse() {
        for (name, failing_ocr, expected_gap) in [
            (
                "OCR error",
                Err(anyhow!("OCR unavailable")),
                CaptureGap::OcrUnavailable,
            ),
            ("empty OCR", Ok(" \r\n\t ".to_owned()), CaptureGap::EmptyOcr),
        ] {
            let harness = Harness::new();
            let mut source = harness.source(
                [
                    Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                    Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                    Ok((frame(2), metadata(10, "notepad.exe", "notes"))),
                    Ok((frame(2), metadata(10, "notepad.exe", "notes"))),
                ],
                [Ok("old".to_owned()), failing_ocr, Ok("fresh".to_owned())],
                [
                    Ok(Duration::ZERO),
                    Ok(Duration::from_secs(30)),
                    Ok(Duration::from_secs(37)),
                ],
                [],
            );

            source.next_sample().await.unwrap();
            harness.at(30);
            let (_, stable) = sample(source.next_sample().await.unwrap());
            assert_eq!(
                stable.next_interval,
                chrono::Duration::seconds(5),
                "{name} precondition"
            );
            harness.at(35);
            assert_eq!(
                source.next_sample().await.unwrap(),
                SampleRead::Gap(expected_gap),
                "{name}"
            );
            harness.at(37);
            let (actual, cadence) = sample(source.next_sample().await.unwrap());

            assert_eq!(actual.ocr_text, "fresh", "{name}");
            assert!(cadence.input.frame_changed, "{name}");
            assert_eq!(
                cadence.input.frame_stable_for,
                chrono::Duration::zero(),
                "{name}"
            );
            let calls = harness.calls.lock().unwrap();
            assert_eq!(
                calls.sleeps,
                [
                    Duration::from_secs(2),
                    Duration::from_secs(5),
                    Duration::from_secs(2),
                ],
                "{name}"
            );
            assert_eq!(calls.ocr, 3, "{name}");
        }
    }

    #[test]
    fn cadence_conversion_rejects_each_out_of_range_standard_duration() {
        let input_error = cadence_record(Duration::MAX, Duration::ZERO, false, false).unwrap_err();
        assert_eq!(
            input_error.to_string(),
            "input-idle duration exceeds chrono range"
        );

        let stability_error =
            cadence_record(Duration::ZERO, Duration::MAX, false, false).unwrap_err();
        assert_eq!(
            stability_error.to_string(),
            "frame-stability duration exceeds chrono range"
        );
    }

    #[tokio::test]
    async fn backwards_monotonic_clock_errors_without_committing_cache_or_reusing_stale_ocr() {
        let harness = Harness::new();
        let mut source = harness.source(
            [
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(2), metadata(10, "notepad.exe", "notes"))),
            ],
            [Ok("old".to_owned()), Ok("fresh".to_owned())],
            [
                Ok(Duration::ZERO),
                Ok(Duration::ZERO),
                Ok(Duration::from_secs(30)),
                Ok(Duration::from_secs(32)),
            ],
            [],
        );

        harness.at(30);
        source.next_sample().await.unwrap();
        harness.at(20);
        let error = source.next_sample().await.unwrap_err();
        assert_eq!(error.to_string(), "monotonic clock moved backwards");

        harness.at(60);
        let (recovered, cadence) = sample(source.next_sample().await.unwrap());
        assert_eq!(recovered.ocr_text, "old");
        assert_eq!(
            cadence.input.frame_stable_for,
            chrono::Duration::seconds(30)
        );
        harness.at(62);
        let (changed, _) = sample(source.next_sample().await.unwrap());
        assert_eq!(changed.ocr_text, "fresh");

        let calls = harness.calls.lock().unwrap();
        assert_eq!(calls.ocr, 2);
        assert_eq!(
            calls.sleeps,
            [
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(5),
            ]
        );
    }

    #[tokio::test]
    async fn chrome_url_is_attached_and_unavailable_url_is_none() {
        let harness = Harness::new();
        let mut source = harness.source(
            [
                Ok((frame(1), metadata(10, "chrome.exe", "tab"))),
                Ok((frame(1), metadata(10, "chrome.exe", "tab"))),
            ],
            [Ok("page".to_owned())],
            [Ok(Duration::ZERO), Ok(Duration::from_secs(1))],
            [
                Ok(Some("https://example.test/path?q=secret".to_owned())),
                Err(anyhow!("UIA unavailable")),
            ],
        );

        let (first, _) = sample(source.next_sample().await.unwrap());
        harness.at(1);
        let (second, _) = sample(source.next_sample().await.unwrap());

        assert_eq!(
            first.browser_url.as_deref(),
            Some("https://example.test/path?q=secret")
        );
        assert_eq!(second.browser_url, None);
        assert_eq!(harness.calls.lock().unwrap().browser_url, 2);
    }

    #[tokio::test]
    async fn every_supported_browser_app_key_crosses_the_boundary_case_insensitively() {
        // The app key comes from the process image name, whose case is
        // whatever the launching shell or shortcut recorded - a case-sensitive
        // match silently drops URL capture for the same browser started a
        // different way. Edge needs its own case: it was only ever exercised
        // through chrome.exe, so dropping it from the match was invisible.
        for app_key in ["chrome.exe", "CHROME.EXE", "msedge.exe", "MsEdge.exe"] {
            let harness = Harness::new();
            let mut source = harness.source(
                [Ok((frame(1), metadata(10, app_key, "tab")))],
                [Ok("page".to_owned())],
                [Ok(Duration::ZERO)],
                [Ok(Some("https://example.test/path".to_owned()))],
            );

            let (actual, _) = sample(source.next_sample().await.unwrap());

            assert_eq!(
                harness.calls.lock().unwrap().browser_url,
                1,
                "{app_key} did not reach the browser-URL boundary"
            );
            assert_eq!(
                actual.browser_url.as_deref(),
                Some("https://example.test/path"),
                "{app_key}"
            );
        }
    }

    #[tokio::test]
    async fn non_browser_sample_never_crosses_browser_url_boundary() {
        // chromium.exe and brave.exe are the near misses: Chromium-family
        // browsers whose URL bar this build does not know how to read. An
        // empty browser-URL queue turns any boundary call into a panic, so
        // reaching it at all fails the case.
        for app_key in ["chromium.exe", "brave.exe", "notepad.exe"] {
            let harness = Harness::new();
            let mut source = harness.source(
                [Ok((frame(1), metadata(10, app_key, "notes")))],
                [Ok("notes".to_owned())],
                [Ok(Duration::ZERO)],
                [],
            );

            let (actual, _) = sample(source.next_sample().await.unwrap());

            assert_eq!(actual.browser_url, None, "{app_key}");
            assert_eq!(harness.calls.lock().unwrap().browser_url, 0, "{app_key}");
        }
    }

    #[tokio::test]
    async fn initial_capture_gap_recovery_sleeps_two_seconds() {
        let harness = Harness::new();
        let mut source = harness.source(
            [
                Err(anyhow!("capture unavailable")),
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
            ],
            [Ok("recovered".to_owned())],
            [Ok(Duration::ZERO)],
            [],
        );

        assert_eq!(
            source.next_sample().await.unwrap(),
            SampleRead::Gap(CaptureGap::CaptureUnavailable)
        );
        let (actual, _) = sample(source.next_sample().await.unwrap());

        assert_eq!(actual.ocr_text, "recovered");
        assert_eq!(
            harness.calls.lock().unwrap().sleeps,
            [Duration::from_secs(2)]
        );
    }

    #[tokio::test]
    async fn input_idle_error_propagates_without_committing_changed_frame_or_text() {
        let harness = Harness::new();
        let mut source = harness.source(
            [
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(2), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(2), metadata(10, "notepad.exe", "notes"))),
            ],
            [
                Ok("old".to_owned()),
                Ok("must not cache".to_owned()),
                Ok("fresh".to_owned()),
            ],
            [
                Ok(Duration::ZERO),
                Err(anyhow!("GetLastInputInfo failed")),
                Ok(Duration::from_secs(4)),
            ],
            [],
        );

        source.next_sample().await.unwrap();
        harness.at(2);
        let error = source.next_sample().await.unwrap_err();
        assert_eq!(error.to_string(), "GetLastInputInfo failed");
        harness.at(4);
        let (actual, cadence) = sample(source.next_sample().await.unwrap());

        assert_eq!(actual.ocr_text, "fresh");
        assert!(cadence.input.frame_changed);
        assert_eq!(cadence.input.frame_stable_for, chrono::Duration::zero());
        assert_eq!(harness.calls.lock().unwrap().ocr, 3);
    }

    #[test]
    fn windows_source_and_source_future_satisfy_send_contract() {
        fn assert_send<T: Send>() {}
        fn assert_source<T: SampleSource + Send>() {}
        fn assert_send_value<T: Send>(_value: T) {}

        assert_send::<WindowsSampleSource>();
        assert_source::<WindowsSampleSource>();
        assert_send_value(WindowsSampleSource::new());
        assert_send::<Source<TestOps>>();
        assert_source::<Source<TestOps>>();
    }

    #[tokio::test]
    async fn a_locked_desktop_yields_a_typed_gap_without_attempting_capture() {
        // `probe_interactive_capability` shipped with the test gate and had NO
        // production caller, so a locked workstation reached the run loop as
        // `capture_unavailable` - the same code a dead capture device produces.
        // The loop could not tell an overnight lock from a real fault.
        //
        // The harness queues nothing, so any attempt to capture, OCR, or read
        // input idle panics. Passing therefore proves the probe short-circuits
        // BEFORE the capture attempt, not merely that the gap is relabelled.
        let harness = Harness::new();
        let mut source = harness.locked_source();

        let read = source.next_sample().await.unwrap();

        assert_eq!(
            read,
            SampleRead::Gap(CaptureGap::DesktopLocked),
            "a locked desktop must be its own gap kind, distinguishable from a              capture failure"
        );
        assert_ne!(
            read,
            SampleRead::Gap(CaptureGap::CaptureUnavailable),
            "a lock must not be reported as a capture failure"
        );
    }

    #[tokio::test]
    async fn a_lock_caught_mid_transition_is_not_blamed_on_capture() {
        // Locking a workstation stops capture working BEFORE OpenInputDesktop
        // starts refusing, so the pre-capture probe answers "available" for the
        // first few samples of a lock. Measured on a real lock on this machine,
        // that window was six samples - every one recorded as
        // `capture_unavailable`, the code for a broken capture device, and
        // every one counted toward the ceiling that exists to catch one.
        //
        // The fixture is that exact sequence: probe says available, capture
        // fails, probe now says gone.
        let harness = Harness::new();
        let mut source = harness.locking_source();

        let read = source.next_sample().await.unwrap();

        assert_eq!(
            read,
            SampleRead::Gap(CaptureGap::DesktopLocked),
            "a capture failure during a lock transition must be attributed to              the lock, not to the capture device"
        );
    }

    #[tokio::test]
    async fn a_capture_failure_on_a_live_desktop_is_still_a_capture_failure() {
        // The negative control for the test above. If the re-probe answered
        // "gone" unconditionally, every capture fault would be relabelled as a
        // lock and the ceiling that catches a dead capture device would never
        // fire again.
        let harness = Harness::new();
        let mut source = harness.source([Err(anyhow::anyhow!("WGC timed out"))], [], [], []);

        let read = source.next_sample().await.unwrap();

        assert_eq!(
            read,
            SampleRead::Gap(CaptureGap::CaptureUnavailable),
            "a genuine capture fault on a live desktop must not be excused as a lock"
        );
        assert_ne!(
            read,
            SampleRead::Gap(CaptureGap::DesktopLocked),
            "an available desktop and a locked desktop must remain distinct session gaps"
        );
    }
}
