//! YAML input shapes: probe-rs style target.yaml and procedure files.
//!
//! Only the keys the firmware used to parse are modelled; serde ignores the
//! rest (cores, package variants, probe-rs options).
use serde::Deserialize;
use yaml_serde::Value;

/// Root of target.yaml.
#[derive(Deserialize)]
pub struct TargetFile {
    pub family: Option<String>,
    pub variants: Vec<Variant>,
    #[serde(default)]
    pub flash_algorithms: Vec<AlgorithmSource>,
    pub control_pins: Option<ControlPins>,
    #[serde(default)]
    pub self_tests: Vec<SelfTestSource>,
}

/// One entry of `variants`.
#[derive(Deserialize)]
pub struct Variant {
    pub name: Option<String>,
    #[serde(default)]
    pub flash_algorithms: Vec<String>,
    /// Tagged entries such as `!Ram`, `!Nvm` or `!Generic`.
    #[serde(default)]
    pub memory_map: Vec<Value>,
    pub chip: Option<String>,
    pub flash_size_kb: Option<u64>,
    pub baud: Option<u64>,
    #[serde(default)]
    pub images: Vec<ImageSource>,
}

/// One entry of the root `flash_algorithms` list.
#[derive(Deserialize)]
pub struct AlgorithmSource {
    pub name: String,
    pub load_address: u64,
    pub pc_init: Option<u64>,
    pub pc_uninit: Option<u64>,
    pub pc_program_page: Option<u64>,
    pub pc_erase_sector: Option<u64>,
    pub pc_erase_all: Option<u64>,
    pub pc_verify: Option<u64>,
    pub data_section_offset: Option<u64>,
    pub flash_properties: Option<FlashProperties>,
    pub instructions: Option<String>,
}

/// `flash_properties` of a flash algorithm.
#[derive(Deserialize)]
pub struct FlashProperties {
    pub address_range: Option<RangeSource>,
    pub page_size: Option<u64>,
    pub erased_byte_value: Option<u64>,
    pub program_page_timeout: Option<u64>,
    pub erase_sector_timeout: Option<u64>,
}

/// Address range with an exclusive end.
#[derive(Deserialize)]
pub struct RangeSource {
    pub start: u64,
    pub end: u64,
}

/// Body of a `!Ram` or `!Nvm` memory map entry.
#[derive(Deserialize)]
pub struct RegionSource {
    pub range: RangeSource,
}

/// Board-level ESP32 control pin polarity.
#[derive(Deserialize)]
pub struct ControlPins {
    pub reset_assert_level: Option<String>,
    pub boot_assert_level: Option<String>,
}

/// One entry of `self_tests`.
#[derive(Deserialize)]
pub struct SelfTestSource {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub addr: u64,
    pub name: Option<String>,
}

/// One ESP32 image.
#[derive(Deserialize)]
pub struct ImageSource {
    pub path: String,
    pub offset: u64,
}

/// Root of pre_prog.yaml / post_prog.yaml.
#[derive(Deserialize)]
pub struct ProcedureFile {
    pub steps: Vec<StepSource>,
}

/// One procedure step; which fields are required depends on `type`.
#[derive(Deserialize)]
pub struct StepSource {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub ignore_error: bool,
    pub addr: Option<u64>,
    pub data: Option<u64>,
    pub mask: Option<u64>,
    pub expected: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub delay_ms: Option<u64>,
}
