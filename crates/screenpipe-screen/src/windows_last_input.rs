use std::time::Duration;

use anyhow::{Result, bail};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

type QueryLastInput = fn() -> Result<u32>;
type ReadTicks = fn() -> u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowsLastInput;

impl WindowsLastInput {
    pub fn idle_for() -> Result<Duration> {
        let (query_last_input, read_ticks) = input_functions();
        idle_for_with(query_last_input, read_ticks)
    }
}

fn input_functions() -> (QueryLastInput, ReadTicks) {
    #[cfg(test)]
    if let Some(seam) = TEST_SEAM.with(|slot| slot.get()) {
        return (seam.query_last_input, seam.read_ticks);
    }

    (query_last_input_ticks, current_ticks)
}

fn idle_for_with(query_last_input: QueryLastInput, read_ticks: ReadTicks) -> Result<Duration> {
    let last_input_ticks = query_last_input()?;
    Ok(tick_delta(read_ticks(), last_input_ticks))
}

fn query_last_input_ticks() -> Result<u32> {
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        bail!("GetLastInputInfo failed");
    }
    Ok(info.dwTime)
}

fn current_ticks() -> u64 {
    unsafe { GetTickCount64() }
}

fn tick_delta(now_ticks: u64, last_input_ticks: u32) -> Duration {
    let elapsed_millis = (now_ticks as u32).wrapping_sub(last_input_ticks);
    Duration::from_millis(u64::from(elapsed_millis))
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct TestSeam {
    query_last_input: QueryLastInput,
    read_ticks: ReadTicks,
}

#[cfg(test)]
thread_local! {
    static TEST_SEAM: std::cell::Cell<Option<TestSeam>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn install_test_seam(query_last_input: QueryLastInput, read_ticks: ReadTicks) -> TestSeamGuard {
    TEST_SEAM.with(|slot| {
        assert!(
            slot.replace(Some(TestSeam {
                query_last_input,
                read_ticks,
            }))
            .is_none(),
            "a test last-input seam is already installed on this thread"
        );
    });
    TestSeamGuard
}

#[cfg(test)]
struct TestSeamGuard;

#[cfg(test)]
impl Drop for TestSeamGuard {
    fn drop(&mut self) {
        TEST_SEAM.with(|slot| slot.set(None));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::anyhow;

    use super::{WindowsLastInput, install_test_seam, tick_delta};

    #[test]
    fn tick_delta_uses_literal_low_u32_millisecond_boundaries() {
        let cases = [
            (12_345_u64, 10_000_u32, 2_345_u64),
            ((3_u64 << 32) + 25, u32::MAX - 74, 100_u64),
            (1_u64 << 32, u32::MAX, 1_u64),
            (u32::MAX as u64, 0_u32, 4_294_967_295_u64),
        ];

        for (now_ticks, last_input_ticks, expected_millis) in cases {
            assert_eq!(
                tick_delta(now_ticks, last_input_ticks),
                Duration::from_millis(expected_millis),
                "now_ticks={now_ticks}, last_input_ticks={last_input_ticks}"
            );
        }
    }

    #[test]
    fn public_idle_for_propagates_query_failure_without_reading_the_clock() {
        let _seam = install_test_seam(
            || Err(anyhow!("GetLastInputInfo failed")),
            || panic!("clock must not be read after a failed last-input query"),
        );

        let error = WindowsLastInput::idle_for().unwrap_err();

        assert_eq!(error.to_string(), "GetLastInputInfo failed");
    }

    #[test]
    fn public_idle_for_uses_injected_query_and_clock_ticks() {
        let _seam = install_test_seam(|| Ok(u32::MAX - 74), || (3_u64 << 32) + 25);

        assert_eq!(
            WindowsLastInput::idle_for().unwrap(),
            Duration::from_millis(100)
        );
    }
}
