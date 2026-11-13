//! Plain-language descriptions of device answers, for people who do not
//! know the SIDP protocol.
use sidp_agent::{
    manage::proto::{Outcome, RunResult, Stage},
    session::{self, CAP_FPU, CAP_STEP, TargetInfo},
};

/// Explains a session error.
pub fn error(error: &session::Error) -> String {
    match error {
        session::Error::Status(code) => status(*code),
        session::Error::Io(io) => match io.kind() {
            std::io::ErrorKind::TimedOut => "The Soul Injector did not answer in time. Unplug it, plug it back in and try again.".into(),
            std::io::ErrorKind::NotFound => "The Soul Injector is no longer connected.".into(),
            std::io::ErrorKind::PermissionDenied => permission_hint(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe => {
                "The USB connection to the Soul Injector was lost.".into()
            }
            _ => format!("USB connection problem: {io}"),
        },
        session::Error::State(text) => capitalize(text),
    }
}

/// Explains an error from opening the serial port.
pub fn open_error(port: &str, error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => permission_hint(),
        std::io::ErrorKind::NotFound => format!("{port} is not connected."),
        _ => {
            let text = error.to_string();
            if text.contains("busy") || text.contains("Resource busy") {
                format!("{port} is in use by another program.")
            } else {
                format!("Cannot open {port}: {text}")
            }
        }
    }
}

fn permission_hint() -> String {
    if cfg!(target_os = "linux") {
        "Permission denied opening the USB port. Add yourself to the 'dialout' group (sudo usermod -aG dialout $USER), then log out and back in.".into()
    } else {
        "Permission denied opening the USB port.".into()
    }
}

/// Explains a SIDP status code.
pub fn status(code: i32) -> String {
    let text = match code {
        1 => "The Soul Injector reported an error (see its log).",
        2 => "The Soul Injector rejected the request: a file is missing or does not match the job.",
        3 => "This Soul Injector firmware does not support that. Update its firmware.",
        4 => {
            "The Soul Injector is busy programming a target. Wait for it to finish, or cancel the run."
        }
        5 => "The target did not respond in time.",
        10 => "The target is running.",
        11 => "The target is already halted.",
        12 => {
            "The target stopped answering. It may be asleep, unpowered, or have disabled its SWD pins; try \"Connect under reset\"."
        }
        13 => "The target's state changed during the request.",
        20 => {
            "SWD communication with the target failed. Check the wiring and power, or try \"Connect under reset\"."
        }
        21 => "The address is outside the target's memory.",
        22 => "All hardware breakpoints are in use. Delete one first.",
        23 => "All hardware watchpoints are in use.",
        24 => "The address is not aligned.",
        25 => "The connected target does not match the job.",
        sidp_agent::manage::STATUS_UNAUTHORIZED => {
            "The Soul Injector refused the password: it was mistyped, or the device's password was changed (set it again over USB with WiFi setup…)."
        }
        sidp_agent::manage::STATUS_LOG_FULL => {
            "The production log on the Soul Injector is full. Pull the log, then try again."
        }
        _ => return format!("The Soul Injector reported error {code}."),
    };
    text.to_owned()
}

/// The status code behind an error, if it came from the device.
pub fn status_code(error: &session::Error) -> Option<i32> {
    match error {
        session::Error::Status(code) => Some(*code),
        _ => None,
    }
}

/// Name of a programming stage.
pub fn stage(stage: Stage) -> &'static str {
    match stage {
        Stage::None => "an unknown stage",
        Stage::Load => "loading the job",
        Stage::PreProgram => "the pre-program steps",
        Stage::Detect => "target detection",
        Stage::Erase => "erase",
        Stage::Program => "programming",
        Stage::Verify => "verify",
        Stage::SelfTest => "the self test",
        Stage::PostProgram => "the post-program steps",
        Stage::CurrentTest => "the current test",
    }
}

/// Whether a run passed.
pub fn passed(result: &RunResult) -> bool {
    result.outcome == Outcome::Pass as i32
}

/// One sentence about a finished run.
pub fn run_result(result: &RunResult) -> String {
    let seconds = f64::from(result.duration_ms) / 1000.0;
    let failed = Stage::try_from(result.failed_stage).unwrap_or(Stage::None);
    match Outcome::try_from(result.outcome).unwrap_or(Outcome::None) {
        Outcome::Pass => format!("Programmed in {seconds:.1} s"),
        Outcome::Cancelled => format!("Cancelled during {} after {seconds:.1} s", stage(failed)),
        _ => format!("Failed at {} after {seconds:.1} s", stage(failed)),
    }
}

/// The core and features of an attached target.
pub fn target(info: &TargetInfo) -> String {
    let core = match info.profile {
        1 => "Cortex-M0/M0+",
        2 => "Cortex-M3",
        3 if info.capabilities & CAP_FPU != 0 => "Cortex-M4/M7 with FPU",
        3 => "Cortex-M4/M7",
        _ => "Cortex-M",
    };
    let mut text = format!(
        "{core}, {} hardware breakpoint{}",
        info.hardware_breakpoints,
        if info.hardware_breakpoints == 1 {
            ""
        } else {
            "s"
        }
    );
    if info.capabilities & CAP_STEP == 0 {
        text += ", no single-step";
    }
    text
}

/// `text` with its first letter in upper case.
pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_runs() {
        let mut result = RunResult {
            run_id: 3,
            outcome: Outcome::Fail as i32,
            failed_stage: Stage::Verify as i32,
            duration_ms: 2500,
        };
        assert_eq!(run_result(&result), "Failed at verify after 2.5 s");
        assert!(!passed(&result));
        result.outcome = Outcome::Pass as i32;
        assert_eq!(run_result(&result), "Programmed in 2.5 s");
        assert!(passed(&result));
    }

    #[test]
    fn describes_errors() {
        assert!(status(30).contains("Pull the log"));
        assert!(status(99).contains("99"));
        let error = session::Error::State("not attached");
        assert_eq!(super::error(&error), "Not attached");
    }
}
