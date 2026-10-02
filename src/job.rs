//! Programming job schema shared with the Soul Injector firmware.
//!
//! `proto/job.proto` and `proto/job.options` are copies of the files in
//! Soul Injector's `sidp_client/proto`; keep them identical. The firmware
//! decodes jobs with nanopb using the fixed limits below, so the compiler
//! must reject anything larger before encoding.
#![allow(missing_docs)]

include!(concat!(env!("OUT_DIR"), "/si.job.rs"));

/// Longest flash algorithm, variant or self-test name (nanopb max_size 32).
pub const MAX_NAME_LEN: usize = 31;
/// Longest ESP32 chip name (nanopb max_size 24).
pub const MAX_CHIP_LEN: usize = 23;
/// Longest ESP32 image path on the device (nanopb max_size 64).
pub const MAX_PATH_LEN: usize = 63;
/// Memory map entries per target.
pub const MAX_MEMORY_REGIONS: usize = 16;
/// RAM regions the firmware keeps for flash algorithm placement.
pub const MAX_RAM_REGIONS: usize = 4;
/// Self tests per target.
pub const MAX_SELF_TESTS: usize = 16;
/// ESP32 images per target.
pub const MAX_IMAGES: usize = 8;
/// Steps per pre/post procedure.
pub const MAX_STEPS: usize = 96;

#[cfg(test)]
mod tests {
    use super::*;

    fn option_value(options: &str, field: &str, key: &str) -> usize {
        let line = options
            .lines()
            .find(|line| line.split_whitespace().next() == Some(field))
            .unwrap_or_else(|| panic!("{field} missing from job.options"));
        let value = line
            .split_whitespace()
            .find_map(|word| word.strip_prefix(key))
            .unwrap_or_else(|| panic!("{field} has no {key}"));
        value.parse().unwrap()
    }

    const OPTIONS: &str = include_str!("../proto/job.options");

    /// String limit without the NUL terminator nanopb reserves.
    fn max_len(field: &str) -> usize {
        option_value(OPTIONS, field, "max_size:") - 1
    }

    fn max_count(field: &str) -> usize {
        option_value(OPTIONS, field, "max_count:")
    }

    #[test]
    fn limits_match_nanopb_options() {
        assert_eq!(max_len("si.job.FlashAlgorithm.name"), MAX_NAME_LEN);
        assert_eq!(max_len("si.job.Target.variant_name"), MAX_NAME_LEN);
        assert_eq!(max_len("si.job.SelfTest.name"), MAX_NAME_LEN);
        assert_eq!(max_len("si.job.Esp32.chip"), MAX_CHIP_LEN);
        assert_eq!(max_len("si.job.Esp32Image.path"), MAX_PATH_LEN);
        assert_eq!(max_count("si.job.Target.memory_map"), MAX_MEMORY_REGIONS);
        assert_eq!(max_count("si.job.Target.self_tests"), MAX_SELF_TESTS);
        assert_eq!(max_count("si.job.Esp32.images"), MAX_IMAGES);
        assert_eq!(max_count("si.job.Procedure.steps"), MAX_STEPS);
    }
}
