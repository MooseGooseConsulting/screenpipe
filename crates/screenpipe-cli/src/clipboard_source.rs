use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use screenpipe_memory::{CadenceInput, CadenceRecord, ObservationSample, SampleRead, SampleSource};
use screenpipe_screen::{ClipboardRead, ClipboardWatcher};

/// How often the clipboard's sequence number is read.
///
/// The cadence floor, matching what `CadencePolicy` selects the moment
/// anything on screen changes. It does not back off with the screen cadence:
/// the screen backs off because capturing and OCR-ing an unchanged screen is
/// expensive, and this is one counter read that touches nothing else. Backing
/// it off to 30 seconds would only widen the window in which two copies
/// collapse into one observation.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The application title recorded on a clipboard event.
///
/// The app KEY is deliberately empty - see the writer's `upsert_app` - so this
/// never creates an `apps` row. It exists because it becomes the event's
/// `title`, the weight-A branch of the full-text index, and an event whose
/// only label is its own text is one a person cannot recognise in a list of
/// search results.
const CLIPBOARD_APP_TITLE: &str = "Clipboard";

/// Emitted when a clipboard generation was refused by its owner.
///
/// Fixed text. The line is printed on a path that has the clipboard's contents
/// one function call away, and pinning it here is what stops any of it - the
/// text, its length, its hash, the owning application - being interpolated
/// into the diagnostic later.
const CLIPBOARD_EXCLUDED: &str = "event=clipboard_excluded reason=owner_refused";

/// Emitted when the clipboard could not be opened. Printed once per outage,
/// not once per poll: the generation is retried every tick until it can be
/// read, and a locked or contended clipboard would otherwise produce a log
/// line every two seconds for as long as it lasts.
const CLIPBOARD_UNAVAILABLE: &str = "event=clipboard_unavailable reason=clipboard_locked";

#[async_trait]
trait ClipboardOps: Send {
    async fn sleep(&mut self, duration: Duration);
    fn now(&mut self) -> DateTime<Utc>;
    fn poll(&mut self) -> ClipboardRead;
}

struct LiveClipboardOps {
    watcher: ClipboardWatcher,
}

#[async_trait]
impl ClipboardOps for LiveClipboardOps {
    async fn sleep(&mut self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    fn now(&mut self) -> DateTime<Utc> {
        Utc::now()
    }

    fn poll(&mut self) -> ClipboardRead {
        self.watcher.poll()
    }
}

/// Turns clipboard generations into observation samples.
///
/// # Why this blocks instead of reporting "nothing happened"
///
/// `SampleSource` has two answers - a sample, or a typed capture gap - and
/// neither is what an idle clipboard is. A gap means capture was attempted and
/// produced nothing, which is a fault signal: it accumulates into the event's
/// `capture_gaps` and it advances the run loop's gap ceiling. An unchanged
/// clipboard is not a fault, it is the normal state, and reporting it as a gap
/// would fill every event's merge_meta with counts of nothing having happened
/// and abort the loop within minutes.
///
/// So this source absorbs its own idleness: `next_sample` polls, sleeps, and
/// polls again, and returns only when there is something real to return. That
/// keeps `Runner`'s contract intact - including the part that matters most,
/// which is that a failed durable write retains the exact sample and retries
/// it - at the cost of a `next_sample` that can be pending for hours. It is
/// driven by its own task for exactly that reason.
struct Source<Ops> {
    ops: Ops,
    poll_interval: Duration,
    /// True while an outage has already been announced.
    announced_unavailable: bool,
}

impl<Ops> Source<Ops> {
    fn new(ops: Ops) -> Self {
        Self {
            ops,
            poll_interval: POLL_INTERVAL,
            announced_unavailable: false,
        }
    }
}

impl<Ops: ClipboardOps> Source<Ops> {
    async fn read(&mut self) -> SampleRead {
        loop {
            match self.ops.poll() {
                ClipboardRead::Text(text) => {
                    self.announced_unavailable = false;
                    // The boundary already refuses blank text. Checked again
                    // here because the runner turns an empty observation into
                    // an `empty_ocr` capture gap, and a clipboard channel must
                    // not be able to write fault counters onto its own events.
                    if !text.trim().is_empty() {
                        return self.sample(text);
                    }
                }
                ClipboardRead::Excluded => {
                    self.announced_unavailable = false;
                    println!("{CLIPBOARD_EXCLUDED}");
                }
                ClipboardRead::Unavailable => {
                    if !self.announced_unavailable {
                        self.announced_unavailable = true;
                        println!("{CLIPBOARD_UNAVAILABLE}");
                    }
                }
                // A new generation carrying an image, files, or a format this
                // build does not read. Ignored entirely: no event, and no gap
                // either, because nothing was lost - there was never anything
                // here this channel records.
                ClipboardRead::NoText => self.announced_unavailable = false,
                ClipboardRead::Unchanged => {}
            }
            self.ops.sleep(self.poll_interval).await;
        }
    }

    fn sample(&mut self, text: String) -> SampleRead {
        let captured_at = self.ops.now();
        SampleRead::Sample {
            sample: ObservationSample {
                captured_at,
                // No application. The foreground window at poll time is up to
                // one poll interval later than the copy and is not reliably
                // where it came from, and a guess in a durable row is worse
                // than an honest absence.
                app_key: String::new(),
                app_title: CLIPBOARD_APP_TITLE.to_owned(),
                window_title: String::new(),
                readable_text: text.clone(),
                ocr_text: text,
                browser_url: None,
                observed_until: None,
                audio: None,
            },
            // Reported, not computed. `CadencePolicy` answers a question about
            // screen frames and input idleness, and neither describes a
            // clipboard poll; what is true here is the interval, so that is
            // what this carries into merge_meta.
            cadence: CadenceRecord {
                input: CadenceInput {
                    input_idle: chrono::Duration::zero(),
                    frame_stable_for: chrono::Duration::zero(),
                    foreground_changed: false,
                    frame_changed: false,
                },
                next_interval: chrono::Duration::from_std(self.poll_interval)
                    .unwrap_or_else(|_| chrono::Duration::seconds(2)),
            },
        }
    }
}

#[async_trait]
impl<Ops: ClipboardOps> SampleSource for Source<Ops> {
    async fn next_sample(&mut self) -> Result<SampleRead> {
        Ok(self.read().await)
    }
}

pub(crate) struct ClipboardSampleSource {
    inner: Source<LiveClipboardOps>,
}

impl ClipboardSampleSource {
    pub(crate) fn new() -> Self {
        Self {
            inner: Source::new(LiveClipboardOps {
                watcher: ClipboardWatcher::new(),
            }),
        }
    }
}

#[async_trait]
impl SampleSource for ClipboardSampleSource {
    async fn next_sample(&mut self) -> Result<SampleRead> {
        self.inner.next_sample().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use screenpipe_memory::{SampleRead, SampleSource};
    use screenpipe_screen::ClipboardRead;

    use super::{
        CLIPBOARD_APP_TITLE, CLIPBOARD_EXCLUDED, CLIPBOARD_UNAVAILABLE, ClipboardSampleSource,
        Source,
    };

    struct TestOps {
        reads: VecDeque<ClipboardRead>,
        sleeps: Arc<Mutex<Vec<Duration>>>,
        polls: Arc<Mutex<usize>>,
        second: i64,
    }

    impl TestOps {
        fn new(reads: impl IntoIterator<Item = ClipboardRead>) -> Self {
            Self {
                reads: reads.into_iter().collect(),
                sleeps: Arc::new(Mutex::new(Vec::new())),
                polls: Arc::new(Mutex::new(0)),
                second: 0,
            }
        }
    }

    #[async_trait]
    impl super::ClipboardOps for TestOps {
        async fn sleep(&mut self, duration: Duration) {
            self.sleeps.lock().unwrap().push(duration);
        }

        fn now(&mut self) -> chrono::DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).single().unwrap()
                + chrono::Duration::seconds(self.second)
        }

        fn poll(&mut self) -> ClipboardRead {
            *self.polls.lock().unwrap() += 1;
            self.second += 2;
            self.reads
                .pop_front()
                .expect("the source polled the clipboard more times than the script allows")
        }
    }

    fn sample_of(read: SampleRead) -> screenpipe_memory::ObservationSample {
        match read {
            SampleRead::Sample { sample, .. } => sample,
            SampleRead::Gap(gap) => {
                panic!("the clipboard channel must never report a capture gap, got {gap:?}")
            }
        }
    }

    #[tokio::test]
    async fn a_copied_text_becomes_a_sample_with_no_application_attached() {
        let copied = "the text the operator copied";
        let mut source = Source::new(TestOps::new([ClipboardRead::Text(copied.to_owned())]));

        let read = source.next_sample().await.unwrap();

        let SampleRead::Sample { sample, cadence } = &read else {
            panic!("expected a sample");
        };
        assert_eq!(sample.ocr_text, copied);
        // Both text columns, exactly as the screen source fills them, so the
        // writer and the search index need no clipboard-specific path.
        assert_eq!(sample.readable_text, copied);
        assert_eq!(sample.app_key, "", "a clipboard capture has no application");
        assert_eq!(sample.app_title, CLIPBOARD_APP_TITLE);
        assert_eq!(sample.window_title, "");
        assert_eq!(sample.browser_url, None);
        assert_eq!(cadence.next_interval, chrono::Duration::seconds(2));
    }

    #[tokio::test]
    async fn an_unchanged_clipboard_is_polled_again_rather_than_reported_as_a_gap() {
        // A gap would accumulate into the event's capture_gaps and advance the
        // run loop's gap ceiling. An idle clipboard is the normal state, not a
        // fault, and this is what keeps the two apart.
        let mut source = Source::new(TestOps::new([
            ClipboardRead::Unchanged,
            ClipboardRead::Unchanged,
            ClipboardRead::Unchanged,
            ClipboardRead::Text("finally".to_owned()),
        ]));
        let sleeps = Arc::clone(&source.ops.sleeps);

        let sample = sample_of(source.next_sample().await.unwrap());

        assert_eq!(sample.ocr_text, "finally");
        assert_eq!(
            *sleeps.lock().unwrap(),
            vec![Duration::from_secs(2); 3],
            "an idle clipboard must wait a poll interval between reads"
        );
    }

    #[tokio::test]
    async fn a_refused_generation_produces_no_sample_and_no_gap() {
        let mut source = Source::new(TestOps::new([
            ClipboardRead::Excluded,
            ClipboardRead::Excluded,
            ClipboardRead::Text("something the operator did copy".to_owned()),
        ]));

        let sample = sample_of(source.next_sample().await.unwrap());

        assert_eq!(sample.ocr_text, "something the operator did copy");
    }

    #[tokio::test]
    async fn a_generation_without_text_is_ignored_entirely() {
        // An image, a file list, or text that is entirely whitespace. None of
        // them is an observation, and none of them is a fault either.
        let mut source = Source::new(TestOps::new([
            ClipboardRead::NoText,
            ClipboardRead::Text("   \t\r\n ".to_owned()),
            ClipboardRead::NoText,
            ClipboardRead::Text("real text".to_owned()),
        ]));

        let sample = sample_of(source.next_sample().await.unwrap());

        assert_eq!(sample.ocr_text, "real text");
    }

    #[tokio::test]
    async fn an_unopenable_clipboard_is_announced_once_per_outage() {
        // The generation is retried every tick until it can be read, so a
        // locked workstation would otherwise print this line every two seconds
        // for as long as the lock lasts.
        let mut source = Source::new(TestOps::new([
            ClipboardRead::Unavailable,
            ClipboardRead::Unavailable,
            ClipboardRead::Unavailable,
            ClipboardRead::Text("recovered".to_owned()),
        ]));

        sample_of(source.next_sample().await.unwrap());
        assert!(
            !source.announced_unavailable,
            "a recovered outage must be able to announce itself again"
        );
    }

    #[test]
    fn the_channel_diagnostics_carry_only_fixed_categories() {
        // These lines are printed by a loop that has the clipboard's contents
        // in scope. Pinning their exact text is what stops the text, its
        // length, or its hash being interpolated into them later.
        assert_eq!(
            CLIPBOARD_EXCLUDED,
            "event=clipboard_excluded reason=owner_refused"
        );
        assert_eq!(
            CLIPBOARD_UNAVAILABLE,
            "event=clipboard_unavailable reason=clipboard_locked"
        );
        for line in [CLIPBOARD_EXCLUDED, CLIPBOARD_UNAVAILABLE] {
            assert!(!line.contains('{'), "{line} carries a format placeholder");
        }
    }

    #[test]
    fn the_live_source_satisfies_the_send_contract_the_runner_requires() {
        fn assert_send<T: Send>() {}
        fn assert_source<T: SampleSource + Send>() {}

        assert_send::<ClipboardSampleSource>();
        assert_source::<ClipboardSampleSource>();
        assert_source::<Source<TestOps>>();
    }
}
