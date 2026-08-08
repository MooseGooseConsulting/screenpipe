use chrono::{Duration, TimeZone, Utc};
use screenpipe_memory::{
    AudioMeta, CadenceInput, CadenceRecord, CaptureGapSummary, EnvelopeMergeDecision, EventKind,
    MergeConfig, Merger, ObservationEnvelope, ObservationSample, SplitReason,
};

#[test]
fn audio_span_and_metadata_survive_observation_into_open_event() {
    let started_at = Utc.with_ymd_and_hms(2026, 8, 8, 12, 0, 0).unwrap();
    let ended_at = started_at + Duration::seconds(3);
    let sample = ObservationSample {
        captured_at: started_at,
        app_key: "audio:loopback".to_owned(),
        app_title: "System Audio".to_owned(),
        window_title: String::new(),
        ocr_text: "first rendering".to_owned(),
        readable_text: "first rendering".to_owned(),
        browser_url: None,
    };
    let observation = ObservationEnvelope::spanning(
        sample,
        ended_at,
        AudioMeta {
            channel: "system_audio",
            device_category: "communications",
            engine: "whisper-rs",
            model: "ggml-base.en".to_owned(),
            vad_engine: "webrtc-vad",
            vad_aggressiveness: "quality",
            language: Some("en".to_owned()),
            avg_no_speech_permille: Some(30),
            closed_by: "silence",
        },
    );
    let cadence = CadenceRecord::from_input(CadenceInput {
        input_idle: Duration::zero(),
        frame_stable_for: Duration::zero(),
        foreground_changed: false,
        frame_changed: false,
    });
    let mut merger = Merger::new(MergeConfig {
        kind: EventKind::Audio,
        idle_gap: Duration::seconds(30),
        scroll_overlap: 0.35,
    });

    let envelope = match merger.ingest_envelope_with_metadata(
        observation,
        cadence,
        CaptureGapSummary::default(),
    ) {
        EnvelopeMergeDecision::Start { reason, event } => {
            assert_eq!(reason, SplitReason::Initial);
            event
        }
        EnvelopeMergeDecision::Merge { .. } => panic!("expected initial event"),
    };

    assert_eq!(envelope.event().started_at, started_at);
    assert_eq!(envelope.event().ended_at, ended_at);
    let audio = envelope.audio().expect("audio metadata");
    assert_eq!(audio.channel, "system_audio");
    assert_eq!(audio.model, "ggml-base.en");
    assert_eq!(audio.avg_no_speech_permille, Some(30));
}
