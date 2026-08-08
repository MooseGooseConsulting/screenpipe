use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};

use crate::cadence::CadenceRecord;
use crate::sample::{AudioMeta, ObservationEnvelope, ObservationSample};
use crate::text_hash::{TextIdentity, jaccard_overlap, normalize_text};

/// Bumped to 2 when the per-event hash ledger became bounded. Version 1 events
/// recorded every distinct OCR hash they ever saw; version 2 events record at
/// most `MAX_TRACKED_HASHES` and carry an eviction count, so `hashes_seen` is
/// no longer a complete census and must not be read as one.
///
/// Bumped to 4 when an event's span and sample count became bounded. Versions
/// below 4 could hold a row spanning the whole run, so an analysis that reads
/// `ended_at - started_at` as "how long that activity lasted" is only true from
/// 4 onward.
///
/// Bumped to 5 when the contract stopped describing one kind of event. Below 5
/// every row is a screen row and the content test that produced its boundary is
/// implied by the version alone; from 5 on, `events.kind` selects the test -
/// see [`EventKind::merge_rule`] - so the version no longer determines how a
/// row was segmented on its own.
///
/// Bumped to 6 when audio events stopped using transcript content alone as
/// their merge identity. From 6 on, an audio identity also includes the VAD
/// utterance window, so repeating the same words in a later utterance remains
/// a separately durable event.
///
/// Bumped to 7 when an observation window end that regresses behind the open
/// event's observed end becomes a durable boundary. Earlier versions could
/// merge it and produce an invalid event window with `ended_at < started_at`.
pub const MERGE_CONTRACT_VERSION: u32 = 7;

/// What a durable event is a record of.
///
/// The kind selects the CONTENT test that decides whether the next sample
/// extends the open event, and only that. The idle-gap boundary and the
/// ceilings below are shared, so both kinds land in the same table, under the
/// same `{slug}_{seq}` identifiers, with the same `merge_meta` shape - there is
/// one merge discipline here, not two.
///
/// It rides on the config and the event rather than on the writer because it
/// describes what was observed, not where it is stored. The writer binds
/// `events.kind` straight from the event, so a kind can only be decided in one
/// place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// The foreground screen, read by OCR. Content continues when this event
    /// has already seen the screen, or when the five-gram overlap with the
    /// previous sample is high enough to be a scroll through the same material.
    Screen,
    /// Text the operator copied. Content continues ONLY when the normalized
    /// text is byte-identical after normalization - that is, a re-copy of the
    /// same thing.
    ///
    /// Scroll overlap is deliberately not consulted: two clipboard entries that
    /// share most of their words are two separate copies of two different
    /// things, not one thing being scrolled past. The window title is not
    /// consulted either, because a clipboard capture has no window of its own.
    Clipboard,
    /// One utterance of speech, transcribed locally. Content continues only
    /// when both the normalized transcript and the VAD utterance window are
    /// identical. That deduplicates a retried chunk from one utterance without
    /// erasing a later occurrence of the same words.
    ///
    /// Speech does not repeat itself the way a screen does, so the overlap
    /// test that segments the screen has nothing to measure here - which is
    /// why the design called for silence to be the boundary instead. It is:
    /// the idle gap below IS the silence, because an utterance is only
    /// delivered after voice activity detection has closed it, so the interval
    /// between two samples on this channel is exactly the silence between two
    /// utterances.
    ///
    Audio,
}

impl EventKind {
    /// Stable code. Persisted verbatim as `events.kind`.
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Clipboard => "clipboard",
            Self::Audio => "audio",
        }
    }
}

/// Longest span one event may cover before it is forced to split.
///
/// Three of the merge path's four split tests describe a *change*: the app, the
/// window's idleness, the text. An event whose app never changes, whose window
/// never goes idle, and whose text keeps overlapping matches none of them and
/// merges forever. A dashboard, a video player, a clock, a terminal tailing a
/// log all behave exactly like that, so an unattended week produces one row
/// spanning the week - with a week-old screen in `ocr_text`, a duration that
/// describes nothing, and a `sample_count` no reader can act on.
///
/// An hour is far above any real activity that a person would call one thing,
/// and far below the span at which a row stops meaning anything. The split
/// carries its own reason code, so a reader can tell a forced boundary from an
/// observed one and never mistake it for a real change of activity.
pub const MAX_EVENT_DURATION_SECONDS: i64 = 3_600;

/// Most samples one event may absorb before it is forced to split.
///
/// Not redundant with `MAX_EVENT_DURATION_SECONDS`, because that ceiling is
/// measured on the wall clock and this one is not. A clock that stops or steps
/// backwards - a suspend/resume, an NTP correction, a VM restored from a
/// snapshot - leaves the duration ceiling unreachable while samples keep
/// arriving every two seconds. This is the bound that still holds when the
/// clock does not.
///
/// At the fastest cadence an hour is 1,800 samples, so on a healthy clock this
/// never binds; it exists for the case where the other ceiling cannot.
pub const MAX_EVENT_SAMPLES: u32 = 4_000;

/// Upper bound on distinct OCR hashes remembered inside one open event.
///
/// Unbounded, this was the worst defect in the merge path. Any window whose
/// text changes while staying above the five-gram overlap threshold - a log
/// tail, a build, a subtitled video, any UI with a clock - takes the merge
/// path on every sample and contributes a new 64-character key. At the 2s
/// cadence that is ~43,200 entries in 24 hours: several megabytes resident,
/// cloned twice per tick, and re-serialized in full into the `merge_meta`
/// JSONB on *every* write. The durable write volume is quadratic in sample
/// count; one event would rewrite a multi-megabyte JSONB every two seconds.
///
/// The cap also restores a behaviour that unbounded growth had quietly
/// removed. `contains` is what lets a previously-seen screen merge back in, so
/// with an unbounded ledger an event that had seen N screens would merge any
/// of those N back unconditionally, forever - `SplitReason::TextHashChange`
/// became progressively unreachable as the event aged.
pub const MAX_TRACKED_HASHES: usize = 64;

/// Bounded record of the OCR hashes seen inside one open event.
///
/// Eviction is by insertion order (oldest out first), which is what makes
/// "have I seen this screen recently" the question `contains` answers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HashLedger {
    counts: BTreeMap<String, u64>,
    order: VecDeque<String>,
    evicted: u64,
}

impl HashLedger {
    fn with_first(hash: String) -> Self {
        Self::from_hashes([hash])
    }

    /// Build a ledger by observing `hashes` in order. Eviction applies, so this
    /// cannot be used to fabricate a ledger larger than `MAX_TRACKED_HASHES`.
    pub fn from_hashes(hashes: impl IntoIterator<Item = String>) -> Self {
        let mut ledger = Self::default();
        for hash in hashes {
            ledger.record(hash);
        }
        ledger
    }

    /// True when this hash is still tracked. An evicted hash reads as unseen,
    /// which is the intended consequence of the bound.
    pub fn contains(&self, hash: &str) -> bool {
        self.counts.contains_key(hash)
    }

    fn record(&mut self, hash: String) {
        if let Some(count) = self.counts.get_mut(&hash) {
            *count = count.saturating_add(1);
            return;
        }
        if self.counts.len() >= MAX_TRACKED_HASHES
            && let Some(oldest) = self.order.pop_front()
        {
            self.counts.remove(&oldest);
            self.evicted = self.evicted.saturating_add(1);
        }
        self.order.push_back(hash.clone());
        self.counts.insert(hash, 1);
    }

    /// The tracked hashes and their observation counts.
    pub fn counts(&self) -> &BTreeMap<String, u64> {
        &self.counts
    }

    /// How many distinct hashes fell out of the window. Persisted alongside
    /// the counts so a reader can tell a complete census from a truncated one.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitReason {
    Initial,
    AppChange,
    WindowTitleChange,
    IdleGap,
    /// The observation window end moved behind the open event's observed end.
    TimestampRegression,
    TextHashChange,
    /// The open event reached `MAX_EVENT_DURATION_SECONDS`.
    ///
    /// Held apart from the observed reasons on purpose: nothing about the
    /// screen changed here. A reader that treats this boundary as a change of
    /// activity would be reading the ceiling, not the user.
    MaxDuration,
    /// The open event reached `MAX_EVENT_SAMPLES`.
    MaxSamples,
}

impl SplitReason {
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::AppChange => "app_change",
            Self::WindowTitleChange => "window_title_change",
            Self::IdleGap => "idle_gap",
            Self::TimestampRegression => "timestamp_regression",
            Self::TextHashChange => "text_hash_change",
            Self::MaxDuration => "max_duration",
            Self::MaxSamples => "max_samples",
        }
    }

    /// True when the boundary was forced by a ceiling rather than observed on
    /// the screen.
    pub const fn is_forced(self) -> bool {
        matches!(self, Self::MaxDuration | Self::MaxSamples)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeDecisionKind {
    Start,
    Merge,
}

impl MergeDecisionKind {
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Merge => "merge",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureGap {
    CaptureUnavailable,
    OcrUnavailable,
    EmptyOcr,
    /// The interactive desktop is locked or absent.
    ///
    /// Distinct from `CaptureUnavailable` on purpose. A locked workstation is
    /// an ordinary, indefinite state - a machine left overnight produces
    /// nothing else - whereas `CaptureUnavailable` means capture was attempted
    /// against a live desktop and failed. Collapsing the two made them
    /// indistinguishable to the run loop, so the ceiling that exists to catch a
    /// dead capture device would fire on a normal night's sleep instead.
    DesktopLocked,
}

impl CaptureGap {
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::CaptureUnavailable => "capture_unavailable",
            Self::OcrUnavailable => "ocr_unavailable",
            Self::EmptyOcr => "empty_ocr",
            Self::DesktopLocked => "desktop_locked",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureGapSummary {
    pub capture_unavailable: u64,
    pub ocr_unavailable: u64,
    pub empty_ocr: u64,
    pub desktop_locked: u64,
}

impl CaptureGapSummary {
    pub fn record(&mut self, gap: CaptureGap) {
        let count = match gap {
            CaptureGap::CaptureUnavailable => &mut self.capture_unavailable,
            CaptureGap::OcrUnavailable => &mut self.ocr_unavailable,
            CaptureGap::EmptyOcr => &mut self.empty_ocr,
            CaptureGap::DesktopLocked => &mut self.desktop_locked,
        };
        *count = count.saturating_add(1);
    }

    fn add(self, other: Self) -> Self {
        Self {
            capture_unavailable: self
                .capture_unavailable
                .saturating_add(other.capture_unavailable),
            ocr_unavailable: self.ocr_unavailable.saturating_add(other.ocr_unavailable),
            empty_ocr: self.empty_ocr.saturating_add(other.empty_ocr),
            desktop_locked: self.desktop_locked.saturating_add(other.desktop_locked),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MergeDecision {
    Start {
        reason: SplitReason,
        event: OpenEvent,
    },
    Merge {
        event: OpenEvent,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MergeConfig {
    /// Which content test decides a boundary, and what the events this merger
    /// opens will be recorded as.
    pub kind: EventKind,
    /// Must be strictly greater than the maximum cadence interval. The split
    /// test compares the delta between *consecutive samples*, so if this
    /// equals the slowest cadence the sleep alone reaches the threshold and
    /// ordinary capture and OCR overhead pushes every idle sample past it -
    /// fragmenting a quiet window into one-sample events, which is the exact
    /// opposite of what an idle gap is for. See `MAX_CADENCE_INTERVAL`.
    pub idle_gap: Duration,
    /// Five-gram Jaccard overlap at which a changed screen still counts as the
    /// same material. Consulted only by [`EventKind::Screen`].
    pub scroll_overlap: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenEvent {
    pub kind: EventKind,
    pub merge_contract_version: u32,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub latest: ObservationSample,
    pub merge_hash: String,
    pub latest_exact_ocr_hash: String,
    pub start_reason: SplitReason,
    pub last_decision: MergeDecisionKind,
    pub latest_cadence: CadenceRecord,
    pub capture_gaps: CaptureGapSummary,
    pub sample_count: u32,
    pub hash_counts: HashLedger,
}

#[derive(Clone, Debug)]
pub struct Merger {
    config: MergeConfig,
    open: Option<EventEnvelope>,
}

impl Merger {
    pub fn new(config: MergeConfig) -> Self {
        Self { config, open: None }
    }

    pub fn ingest_with_metadata(
        &mut self,
        sample: ObservationSample,
        cadence: CadenceRecord,
        capture_gaps: CaptureGapSummary,
    ) -> MergeDecision {
        self.ingest_envelope_with_metadata(sample.into(), cadence, capture_gaps)
            .into_legacy()
    }

    pub fn ingest_envelope_with_metadata(
        &mut self,
        observation: ObservationEnvelope,
        cadence: CadenceRecord,
        capture_gaps: CaptureGapSummary,
    ) -> EnvelopeMergeDecision {
        let sample = observation.sample();
        let identity = TextIdentity::from_ocr(&sample.ocr_text);
        let merge_hash = match self.config.kind {
            EventKind::Audio => audio_merge_hash(&observation, &identity.exact_hash),
            EventKind::Screen | EventKind::Clipboard => identity.exact_hash.clone(),
        };
        let Some(open) = self.open.as_ref() else {
            return self.start(
                observation,
                merge_hash,
                identity.exact_hash,
                cadence,
                capture_gaps,
                SplitReason::Initial,
            );
        };

        let reason = if observation.observed_until() < open.event().ended_at {
            Some(SplitReason::TimestampRegression)
        } else {
            match self.config.kind {
                EventKind::Screen => self.screen_split_reason(open.event(), sample, &identity),
                EventKind::Clipboard => {
                    self.clipboard_split_reason(open.event(), sample, &identity)
                }
                EventKind::Audio => self.audio_split_reason(open.event(), sample, &merge_hash),
            }
        };

        // The ceilings are consulted last, so an observed reason always wins
        // and a forced boundary is only ever reported when nothing on the
        // screen explained it. Every check above describes a CHANGE; an event
        // that never changes matches none of them and would otherwise merge for
        // the length of the run.
        let reason = reason.or_else(|| {
            if (sample.captured_at - open.event().started_at).num_seconds()
                >= MAX_EVENT_DURATION_SECONDS
            {
                Some(SplitReason::MaxDuration)
            } else if open.event().sample_count >= MAX_EVENT_SAMPLES {
                Some(SplitReason::MaxSamples)
            } else {
                None
            }
        });

        if let Some(reason) = reason {
            return self.start(
                observation,
                merge_hash,
                identity.exact_hash,
                cadence,
                capture_gaps,
                reason,
            );
        }

        let open = self.open.as_mut().expect("open event checked above");
        open.event.ended_at = observation.observed_until();
        open.event.latest = observation.sample().clone();
        open.audio = observation.audio().cloned();
        open.event.latest_exact_ocr_hash = identity.exact_hash.clone();
        open.event.last_decision = MergeDecisionKind::Merge;
        open.event.latest_cadence = cadence;
        open.event.capture_gaps = open.event.capture_gaps.add(capture_gaps);
        open.event.sample_count = open.event.sample_count.saturating_add(1);
        open.event.hash_counts.record(identity.exact_hash);
        EnvelopeMergeDecision::Merge {
            event: open.clone(),
        }
    }

    /// The boundary test for [`EventKind::Screen`].
    ///
    /// CONTENT DECIDES. The window title is a hint, not the authority.
    ///
    /// This used to test the title second, before content was consulted at all,
    /// and a title change short-circuited straight to a split. A window title
    /// is a presentation surface: apps put spinners, unsaved-change markers,
    /// notification counts and download percentages in it. Every one of those
    /// became a semantic event boundary.
    ///
    /// Measured on 784 real events from this machine: 690 of them - 88% -
    /// started because of a title change, averaging 3.2 samples each. The
    /// control was in the same table. Events that started from an app change -
    /// an unambiguous, real change of activity - averaged 24.1 samples, 7.5x
    /// longer. Nothing about the underlying activity was that fragmented; the
    /// segmentation was. Content only ever got a vote 59 times, because the
    /// title check ran first and almost always fired.
    ///
    /// So the order is inverted. If the text is continuous - the same screen
    /// seen before, or enough five-gram overlap to be a scroll - this is the
    /// same activity and the title is decoration. A title change still splits,
    /// but only when the content changed too, and it keeps its own reason code
    /// because "the title changed" is the more informative description of what
    /// happened.
    fn screen_split_reason(
        &self,
        open: &OpenEvent,
        sample: &ObservationSample,
        identity: &TextIdentity,
    ) -> Option<SplitReason> {
        if sample.app_key != open.latest.app_key {
            Some(SplitReason::AppChange)
        } else if sample.captured_at - open.ended_at > self.config.idle_gap {
            Some(SplitReason::IdleGap)
        } else {
            let previous_identity = TextIdentity::from_ocr(&open.latest.ocr_text);
            let content_continues = open.hash_counts.contains(&identity.exact_hash)
                || jaccard_overlap(&previous_identity.five_grams, &identity.five_grams)
                    >= self.config.scroll_overlap;

            if content_continues {
                None
            } else if normalize_text(&sample.window_title)
                != normalize_text(&open.latest.window_title)
            {
                Some(SplitReason::WindowTitleChange)
            } else {
                Some(SplitReason::TextHashChange)
            }
        }
    }

    /// The boundary test for the discrete [`EventKind::Clipboard`] channel.
    ///
    /// Two closers, and deliberately no others: the text changed, or the
    /// channel went quiet for longer than the idle gap. A capture whose
    /// normalized text hash equals the open event's merges instead, which is
    /// what makes re-copying the same thing - hitting Ctrl-C twice, or a
    /// re-copy from a different app - one event with a higher `sample_count`
    /// rather than a second row saying the same thing.
    ///
    /// The comparison is against the OPEN EVENT'S OWN hash and not against its
    /// ledger. `hash_counts` answers "did this event ever see this screen",
    /// which is the right question for a screen scrolling back to something it
    /// showed before; an event on these channels only ever holds one distinct
    /// text, so consulting the ledger would answer the same question in a way
    /// that stops being true the moment the rule changes.
    ///
    /// The app key is not consulted. A clipboard capture is not attributed to
    /// an app at all - the foreground window at poll time is not reliably the
    /// window the copy came from, and asserting otherwise would put a guess in
    /// a durable row.
    fn clipboard_split_reason(
        &self,
        open: &OpenEvent,
        sample: &ObservationSample,
        identity: &TextIdentity,
    ) -> Option<SplitReason> {
        if sample.captured_at - open.ended_at > self.config.idle_gap {
            Some(SplitReason::IdleGap)
        } else if identity.exact_hash != open.latest_exact_ocr_hash {
            Some(SplitReason::TextHashChange)
        } else {
            None
        }
    }

    /// Audio deduplicates only a retry of the same VAD utterance. Transcript
    /// content alone cannot identify an occurrence: people can say the same
    /// words twice, and both occurrences must remain durable. The merge hash
    /// therefore binds normalized content to the utterance's start/end window.
    fn audio_split_reason(
        &self,
        open: &OpenEvent,
        sample: &ObservationSample,
        merge_hash: &str,
    ) -> Option<SplitReason> {
        if sample.captured_at - open.ended_at > self.config.idle_gap {
            Some(SplitReason::IdleGap)
        } else if merge_hash != open.merge_hash {
            Some(SplitReason::TextHashChange)
        } else {
            None
        }
    }

    fn start(
        &mut self,
        observation: ObservationEnvelope,
        merge_hash: String,
        latest_exact_ocr_hash: String,
        cadence: CadenceRecord,
        capture_gaps: CaptureGapSummary,
        reason: SplitReason,
    ) -> EnvelopeMergeDecision {
        let started_at = observation.sample().captured_at;
        let ended_at = observation.observed_until();
        let audio = observation.audio().cloned();
        let sample = observation.into_sample();
        let hash_counts = HashLedger::with_first(latest_exact_ocr_hash.clone());
        let event = OpenEvent {
            kind: self.config.kind,
            merge_contract_version: MERGE_CONTRACT_VERSION,
            started_at,
            // The observation's END, which is its start for everything that
            // happens at an instant and genuinely later for an utterance. This
            // is what the idle-gap test above measures FROM, so getting it
            // wrong would fold the length of one utterance into the silence
            // after it.
            ended_at,
            latest: sample,
            latest_exact_ocr_hash,
            merge_hash,
            start_reason: reason,
            last_decision: MergeDecisionKind::Start,
            latest_cadence: cadence,
            capture_gaps,
            sample_count: 1,
            hash_counts,
        };
        let event = match audio {
            Some(audio) => EventEnvelope::with_audio(event, audio),
            None => EventEnvelope::without_audio(event),
        };
        self.open = Some(event.clone());
        EnvelopeMergeDecision::Start { reason, event }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventEnvelope {
    event: OpenEvent,
    audio: Option<AudioMeta>,
}

impl EventEnvelope {
    pub fn without_audio(event: OpenEvent) -> Self {
        Self { event, audio: None }
    }

    pub fn with_audio(event: OpenEvent, audio: AudioMeta) -> Self {
        Self {
            event,
            audio: Some(audio),
        }
    }

    pub fn event(&self) -> &OpenEvent {
        &self.event
    }

    pub fn audio(&self) -> Option<&AudioMeta> {
        self.audio.as_ref()
    }

    fn into_event(self) -> OpenEvent {
        self.event
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvelopeMergeDecision {
    Start {
        reason: SplitReason,
        event: EventEnvelope,
    },
    Merge {
        event: EventEnvelope,
    },
}

impl EnvelopeMergeDecision {
    fn into_legacy(self) -> MergeDecision {
        match self {
            Self::Start { reason, event } => MergeDecision::Start {
                reason,
                event: event.into_event(),
            },
            Self::Merge { event } => MergeDecision::Merge {
                event: event.into_event(),
            },
        }
    }
}

/// Stable identity for one normalized transcript occurrence.
///
/// The timestamps are encoded as fixed-width seconds/nanoseconds pairs, so no
/// locale or formatter can alter the durable hash. Including the transcript
/// hash keeps different chunks in one window distinct; including both ends of
/// the VAD window keeps identical words in separate utterances distinct.
fn audio_merge_hash(observation: &ObservationEnvelope, transcript_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"screenpipe-audio-utterance-v1\0");
    hasher.update(transcript_hash.as_bytes());
    for instant in [
        observation.sample().captured_at,
        observation.observed_until(),
    ] {
        hasher.update(instant.timestamp().to_be_bytes());
        hasher.update(instant.timestamp_subsec_nanos().to_be_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
