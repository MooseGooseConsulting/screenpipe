use chrono::Duration;
use screenpipe_memory::{CadenceInput, CadencePolicy, CadenceRecord};

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
        // Both change at once - an app switch that also repaints, which is the
        // single most common real transition. Without this row the disjunction
        // can be weakened to an exclusive-or and every test still passes, so a
        // normal app switch would fall through to the idle ladder and back off
        // to the slowest interval instead of sampling fast.
        (600, 600, true, true, 2),
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

#[test]
fn cadence_record_carries_the_policy_interval_for_its_own_input() {
    // `CadenceRecord` is what the merger stores and what `merge_meta` persists.
    // Nothing else asserted that its `next_interval` comes from the policy at
    // all, so the constructor could return a zero interval - which downstream
    // reads as "sample immediately, forever" - with the whole suite green.
    let inputs = [
        CadenceInput {
            input_idle: Duration::seconds(600),
            frame_stable_for: Duration::seconds(600),
            foreground_changed: false,
            frame_changed: false,
        },
        CadenceInput {
            input_idle: Duration::seconds(45),
            frame_stable_for: Duration::seconds(45),
            foreground_changed: false,
            frame_changed: false,
        },
    ];

    for input in inputs {
        let record = CadenceRecord::from_input(input);

        assert_eq!(record.input, input, "the record must retain its own input");
        assert_eq!(
            record.next_interval,
            CadencePolicy::next_interval(input),
            "the record's interval must be the policy's decision for that input"
        );
        assert!(
            record.next_interval > Duration::zero(),
            "a cadence interval must never be zero: {:?}",
            record.next_interval
        );
    }

    // The two inputs above must not select the same interval, or the assertion
    // above would hold for a constructor that ignores its input entirely.
    assert_ne!(
        CadenceRecord::from_input(inputs[0]).next_interval,
        CadenceRecord::from_input(inputs[1]).next_interval
    );
}
