use std::time::Duration;

use anyhow::{Result, bail};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowsLastInput;

impl WindowsLastInput {
    pub fn idle_for() -> Result<Duration> {
        let last_input_ticks = query_last_input_ticks();
        let now_ticks = unsafe { GetTickCount64() };
        idle_duration_from_tick_result(last_input_ticks, now_ticks)
    }
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

fn idle_duration_from_tick_result(
    last_input_ticks: Result<u32>,
    now_ticks: u64,
) -> Result<Duration> {
    Ok(tick_delta(now_ticks, last_input_ticks?))
}

fn tick_delta(now_ticks: u64, last_input_ticks: u32) -> Duration {
    let elapsed_millis = (now_ticks as u32).wrapping_sub(last_input_ticks);
    Duration::from_millis(u64::from(elapsed_millis))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::anyhow;

    use super::{WindowsLastInput, idle_duration_from_tick_result, tick_delta};

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
    fn failed_last_input_query_returns_error_instead_of_idle_time() {
        let error =
            idle_duration_from_tick_result(Err(anyhow!("GetLastInputInfo failed")), 4_294_967_500)
                .unwrap_err();

        assert_eq!(error.to_string(), "GetLastInputInfo failed");
    }

    #[test]
    fn public_idle_for_has_the_expected_fallible_duration_contract() {
        let idle_for: fn() -> anyhow::Result<Duration> = WindowsLastInput::idle_for;
        let _ = idle_for;
    }
}
