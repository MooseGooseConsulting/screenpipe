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

#[async_trait]
trait WindowsSampleOps: Send {
    async fn sleep(&mut self, duration: Duration);
    async fn now(&mut self) -> (DateTime<Utc>, Instant);
    async fn capture_foreground(&mut self) -> Result<(TransientFrame, ForegroundMetadata)>;
    async fn recognize(&mut self, frame: &TransientFrame) -> Result<String>;
    async fn input_idle(&mut self) -> Result<Duration>;
    async fn browser_url(&mut self, metadata: &ForegroundMetadata) -> Result<Option<String>>;
}

struct LiveWindowsOps;

#[async_trait]
impl WindowsSampleOps for LiveWindowsOps {
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

        let (frame, metadata) = match self.ops.capture_foreground().await {
            Ok(capture) => capture,
            Err(_) => {
                self.next_sleep = Some(RETRY_CADENCE);
                return Ok(SampleRead::Gap(CaptureGap::CaptureUnavailable));
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
        let cadence = CadenceRecord::from_input(CadenceInput {
            input_idle: chrono::Duration::from_std(input_idle)
                .context("input-idle duration exceeds chrono range")?,
            frame_stable_for: chrono::Duration::from_std(frame_stable_for)
                .context("frame-stability duration exceeds chrono range")?,
            foreground_changed,
            frame_changed,
        });
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

pub(crate) struct WindowsSampleSource {
    inner: Source<LiveWindowsOps>,
}

impl WindowsSampleSource {
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

    use super::{Source, WindowsSampleOps, WindowsSampleSource};

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
            }
        }

        fn next<T>(queue: &mut VecDeque<Result<T>>, boundary: &str) -> Result<T> {
            queue
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected {boundary} boundary call"))
        }
    }

    #[async_trait]
    impl WindowsSampleOps for TestOps {
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
    async fn ocr_error_and_empty_text_retry_same_changed_frame_without_stale_reuse() {
        let harness = Harness::new();
        let mut source = harness.source(
            [
                Ok((frame(1), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(2), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(2), metadata(10, "notepad.exe", "notes"))),
                Ok((frame(2), metadata(10, "notepad.exe", "notes"))),
            ],
            [
                Ok("old".to_owned()),
                Err(anyhow!("OCR unavailable")),
                Ok(" \r\n\t ".to_owned()),
                Ok("fresh".to_owned()),
            ],
            [
                Ok(Duration::ZERO),
                Ok(Duration::from_secs(2)),
                Ok(Duration::from_secs(4)),
                Ok(Duration::from_secs(6)),
            ],
            [],
        );

        source.next_sample().await.unwrap();
        harness.at(2);
        assert_eq!(
            source.next_sample().await.unwrap(),
            SampleRead::Gap(CaptureGap::OcrUnavailable)
        );
        harness.at(4);
        assert_eq!(
            source.next_sample().await.unwrap(),
            SampleRead::Gap(CaptureGap::EmptyOcr)
        );
        harness.at(6);
        let (actual, cadence) = sample(source.next_sample().await.unwrap());

        assert_eq!(actual.ocr_text, "fresh");
        assert!(cadence.input.frame_changed);
        assert_eq!(cadence.input.frame_stable_for, chrono::Duration::zero());
        assert_eq!(harness.calls.lock().unwrap().ocr, 4);
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
    async fn non_browser_sample_never_crosses_browser_url_boundary() {
        let harness = Harness::new();
        let mut source = harness.source(
            [Ok((frame(1), metadata(10, "notepad.exe", "notes")))],
            [Ok("notes".to_owned())],
            [Ok(Duration::ZERO)],
            [],
        );

        let (actual, _) = sample(source.next_sample().await.unwrap());

        assert_eq!(actual.browser_url, None);
        assert_eq!(harness.calls.lock().unwrap().browser_url, 0);
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

        assert_send::<WindowsSampleSource>();
        assert_source::<WindowsSampleSource>();
        assert_send::<Source<TestOps>>();
        assert_source::<Source<TestOps>>();
    }
}
