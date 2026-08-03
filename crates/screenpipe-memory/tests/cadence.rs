use chrono::Duration;
use screenpipe_memory::{CadenceInput, CadencePolicy};

#[test]
fn cadence_uses_idle_and_frame_stability_thresholds() {
    let cases = [
        (29, 600, false, false, 2),
        (600, 29, false, false, 2),
        (30, 30, false, false, 5),
        (119, 600, false, false, 5),
        (600, 119, false, false, 5),
        (120, 120, false, false, 15),
        (599, 600, false, false, 15),
        (600, 599, false, false, 15),
        (600, 600, false, false, 30),
        (600, 600, true, false, 2),
        (600, 600, false, true, 2),
    ];

    for (input_idle, frame_stable_for, foreground_changed, frame_changed, expected) in cases {
        let actual = CadencePolicy::next_interval(CadenceInput {
            input_idle: Duration::seconds(input_idle),
            frame_stable_for: Duration::seconds(frame_stable_for),
            foreground_changed,
            frame_changed,
        });

        assert_eq!(
            actual,
            Duration::seconds(expected),
            "input_idle={input_idle}, frame_stable_for={frame_stable_for}, foreground_changed={foreground_changed}, frame_changed={frame_changed}"
        );
    }
}
