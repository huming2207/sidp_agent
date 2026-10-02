//! target.yaml conversion, keeping the checks the firmware parser made.
use super::source::{
    AlgorithmSource, FlashProperties, ImageSource, RangeSource, RegionSource, SelfTestSource,
    TargetFile, Variant,
};
use super::{CompileError, Result, check_count, check_len, optional_u32, to_u32};
use crate::job::memory_region::Kind;
use crate::job::self_test::Type;
use crate::job::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use yaml_serde::Value;

/// Baud rate used when an ESP32 variant does not set one.
const DEFAULT_ESP32_BAUD: u32 = 115_200;

enum Family {
    CortexM,
    Esp32,
}

/// Converts the selected variant of target.yaml.
pub fn convert(
    file: &TargetFile,
    variant_name: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<Target> {
    let family = parse_family(file.family.as_deref())?;
    let variant = select_variant(&file.variants, variant_name)?;
    let name = variant.name.clone().unwrap_or_default();
    check_len(&name, MAX_NAME_LEN, "variant name")?;

    let mut target = Target {
        variant_name: name,
        self_tests: convert_self_tests(&file.self_tests, warnings)?,
        ..Default::default()
    };
    match family {
        Family::CortexM => {
            target.memory_map = convert_memory_map(&variant.memory_map)?;
            target.cortex_m = Some(convert_cortex_m(file, variant, warnings)?);
        }
        Family::Esp32 => target.esp32 = Some(convert_esp32(file, variant)?),
    }
    Ok(target)
}

fn parse_family(name: Option<&str>) -> Result<Family> {
    match name.unwrap_or("") {
        "" | "cortex-m" | "swd" | "arm" => Ok(Family::CortexM),
        "esp32" | "esp32-serial" | "espressif" => Ok(Family::Esp32),
        other => Err(CompileError(format!(
            "unknown family '{other}' (expected cortex-m or esp32)"
        ))),
    }
}

fn select_variant<'a>(variants: &'a [Variant], wanted: Option<&str>) -> Result<&'a Variant> {
    match (variants, wanted) {
        ([], _) => Err(CompileError("'variants' is empty".into())),
        ([only], None) => Ok(only),
        (_, None) => Err(CompileError(format!(
            "target.yaml has {} variants; select one with --variant",
            variants.len()
        ))),
        (_, Some(wanted)) => variants
            .iter()
            .find(|v| v.name.as_deref() == Some(wanted))
            .ok_or_else(|| CompileError(format!("variant '{wanted}' not found"))),
    }
}

/// Keeps `!Ram` and `!Nvm` entries; other tags such as `!Generic` are skipped.
fn convert_memory_map(entries: &[Value]) -> Result<Vec<MemoryRegion>> {
    let mut regions = Vec::new();
    for entry in entries {
        let Value::Tagged(tagged) = entry else {
            continue;
        };
        let kind = if tagged.tag == "Ram" {
            Kind::Ram
        } else if tagged.tag == "Nvm" {
            Kind::Nvm
        } else {
            continue;
        };
        let body: RegionSource = yaml_serde::from_value(tagged.value.clone())
            .map_err(|e| CompileError(format!("memory_map {}: {e}", tagged.tag)))?;
        regions.push(convert_region(kind, &body.range)?);
    }

    check_count(regions.len(), MAX_MEMORY_REGIONS, "memory map entries")?;
    let ram_count = regions
        .iter()
        .filter(|r| r.kind == Kind::Ram as i32)
        .count();
    check_count(ram_count, MAX_RAM_REGIONS, "RAM regions")?;
    Ok(regions)
}

fn convert_region(kind: Kind, range: &RangeSource) -> Result<MemoryRegion> {
    let start = to_u32(range.start, "region start")?;
    let end = to_u32(range.end, "region end")?;
    if end <= start {
        return Err(CompileError(format!(
            "invalid region 0x{start:08x}-0x{end:08x}: end must be greater than start"
        )));
    }
    Ok(MemoryRegion {
        kind: kind as i32,
        start,
        end,
    })
}

fn convert_cortex_m(
    file: &TargetFile,
    variant: &Variant,
    warnings: &mut Vec<String>,
) -> Result<CortexM> {
    let algo_name = variant
        .flash_algorithms
        .first()
        .ok_or_else(|| CompileError("variant has no flash_algorithms".into()))?;
    if variant.flash_algorithms.len() > 1 {
        warnings.push(format!(
            "variant lists {} flash algorithms; only '{algo_name}' is used",
            variant.flash_algorithms.len()
        ));
    }
    let source = file
        .flash_algorithms
        .iter()
        .find(|a| &a.name == algo_name)
        .ok_or_else(|| {
            CompileError(format!(
                "flash algorithm '{algo_name}' not found at root level"
            ))
        })?;
    Ok(CortexM {
        algorithm: Some(convert_algorithm(source)?),
        ..Default::default()
    })
}

fn convert_algorithm(source: &AlgorithmSource) -> Result<FlashAlgorithm> {
    check_len(&source.name, MAX_NAME_LEN, "flash algorithm name")?;
    let load_address = to_u32(source.load_address, "load_address")?;
    let mut algorithm = FlashAlgorithm {
        name: source.name.clone(),
        load_address,
        pc_init: entry_point(load_address, source.pc_init, "pc_init")?,
        pc_uninit: entry_point(load_address, source.pc_uninit, "pc_uninit")?,
        pc_program_page: entry_point(load_address, source.pc_program_page, "pc_program_page")?,
        pc_erase_sector: entry_point(load_address, source.pc_erase_sector, "pc_erase_sector")?,
        pc_erase_all: entry_point(load_address, source.pc_erase_all, "pc_erase_all")?,
        pc_verify: entry_point(load_address, source.pc_verify, "pc_verify")?,
        data_section_offset: optional_u32(source.data_section_offset, "data_section_offset")?,
        instructions: decode_instructions(source)?,
        ..Default::default()
    };
    if let Some(properties) = &source.flash_properties {
        apply_flash_properties(&mut algorithm, properties)?;
    }
    if algorithm.page_size == 0 {
        return Err(CompileError(format!(
            "algorithm '{}': page_size is required and must be non-zero",
            source.name
        )));
    }
    Ok(algorithm)
}

/// Entry points in target.yaml are offsets from the load address.
fn entry_point(load_address: u32, offset: Option<u64>, what: &str) -> Result<Option<u32>> {
    let Some(offset) = offset else {
        return Ok(None);
    };
    load_address
        .checked_add(to_u32(offset, what)?)
        .map(Some)
        .ok_or_else(|| CompileError(format!("{what} overflows the 32-bit address space")))
}

fn apply_flash_properties(
    algorithm: &mut FlashAlgorithm,
    properties: &FlashProperties,
) -> Result<()> {
    if let Some(range) = &properties.address_range {
        algorithm.flash_start = Some(to_u32(range.start, "address_range.start")?);
        algorithm.flash_end = Some(to_u32(range.end, "address_range.end")?);
    }
    algorithm.page_size = optional_u32(properties.page_size, "page_size")?.unwrap_or(0);
    algorithm.erased_byte_value = optional_u32(properties.erased_byte_value, "erased_byte_value")?;
    algorithm.program_page_timeout_ms =
        optional_u32(properties.program_page_timeout, "program_page_timeout")?;
    algorithm.erase_sector_timeout_ms =
        optional_u32(properties.erase_sector_timeout, "erase_sector_timeout")?;
    Ok(())
}

/// Decodes the base64 algorithm blob; block scalars may contain whitespace.
fn decode_instructions(source: &AlgorithmSource) -> Result<Vec<u8>> {
    let text = source.instructions.as_deref().ok_or_else(|| {
        CompileError(format!(
            "algorithm '{}': missing 'instructions'",
            source.name
        ))
    })?;
    let clean: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let blob = STANDARD.decode(clean).map_err(|e| {
        CompileError(format!(
            "algorithm '{}': invalid base64 instructions: {e}",
            source.name
        ))
    })?;
    if blob.is_empty() {
        return Err(CompileError(format!(
            "algorithm '{}': instructions are empty",
            source.name
        )));
    }
    Ok(blob)
}

fn convert_esp32(file: &TargetFile, variant: &Variant) -> Result<Esp32> {
    let chip = variant.chip.clone().ok_or_else(|| {
        CompileError("esp32 family requires a 'chip' name (e.g. esp32s31)".into())
    })?;
    check_len(&chip, MAX_CHIP_LEN, "chip")?;

    let pins = file.control_pins.as_ref();
    let reset_level = pins.and_then(|p| p.reset_assert_level.as_deref());
    let boot_level = pins.and_then(|p| p.boot_assert_level.as_deref());
    Ok(Esp32 {
        chip,
        flash_size_kb: optional_u32(variant.flash_size_kb, "flash_size_kb")?,
        baud: optional_u32(variant.baud, "baud")?.unwrap_or(DEFAULT_ESP32_BAUD),
        reset_assert_high: is_assert_high(reset_level, "reset_assert_level")?,
        boot_assert_high: is_assert_high(boot_level, "boot_assert_level")?,
        images: convert_images(&variant.images)?,
    })
}

fn is_assert_high(level: Option<&str>, what: &str) -> Result<bool> {
    match level {
        None | Some("low") => Ok(false),
        Some("high") => Ok(true),
        Some(other) => Err(CompileError(format!(
            "control_pins.{what} must be 'low' or 'high' (got '{other}')"
        ))),
    }
}

fn convert_images(images: &[ImageSource]) -> Result<Vec<Esp32Image>> {
    if images.is_empty() {
        return Err(CompileError(
            "esp32 family requires a non-empty 'images' list".into(),
        ));
    }
    check_count(images.len(), MAX_IMAGES, "images")?;
    let mut converted = Vec::with_capacity(images.len());
    for image in images {
        check_len(&image.path, MAX_PATH_LEN, "image path")?;
        converted.push(Esp32Image {
            path: image.path.clone(),
            offset: to_u32(image.offset, "image offset")?,
            ..Default::default()
        });
    }
    Ok(converted)
}

fn convert_self_tests(
    tests: &[SelfTestSource],
    warnings: &mut Vec<String>,
) -> Result<Vec<SelfTest>> {
    check_count(tests.len(), MAX_SELF_TESTS, "self tests")?;
    let mut converted = Vec::with_capacity(tests.len());
    for (index, test) in tests.iter().enumerate() {
        let name = test.name.clone().unwrap_or_default();
        check_len(&name, MAX_NAME_LEN, "self test name")?;
        converted.push(SelfTest {
            r#type: test_type(test.kind.as_deref(), index, warnings) as i32,
            addr: to_u32(test.addr, "self test addr")?,
            name,
        });
    }
    Ok(converted)
}

fn test_type(kind: Option<&str>, index: usize, warnings: &mut Vec<String>) -> Type {
    match kind {
        None | Some("simple") => Type::Simple,
        Some("extend") => Type::Extend,
        Some("power") => Type::Power,
        Some(other) => {
            warnings.push(format!(
                "self_tests[{index}]: unknown type '{other}', using simple"
            ));
            Type::Simple
        }
    }
}
