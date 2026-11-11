//! Reads what the UI needs to know about a target.yaml, and builds the job
//! the device runs.
//!
//! The job itself is compiled by `sidp_agent::compile`, the same code the
//! command-line tool uses, so the app accepts exactly what the CLI accepts.
use crate::profile::Profile;
use prost::Message;
use serde::Deserialize;
use sidp_agent::{
    compile::{self, ImageHashes, Sources},
    manage,
};
use std::path::Path;

/// Programming family of a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// ARM Cortex-M over SWD; can be debugged.
    CortexM,
    /// Espressif over UART; programming only.
    Esp32,
}

impl Family {
    /// Short display name.
    pub fn label(self) -> &'static str {
        match self {
            Family::CortexM => "Cortex-M (SWD)",
            Family::Esp32 => "ESP32 (UART)",
        }
    }
}

/// The parts of target.yaml the UI shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSummary {
    /// Programming family.
    pub family: Family,
    /// Named variants, in file order.
    pub variants: Vec<VariantSummary>,
    /// Variants without a name; they can only be used when they are the only one.
    pub unnamed: usize,
}

/// One variant of target.yaml.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantSummary {
    /// Variant name; empty for an unnamed single variant.
    pub name: String,
    /// Device names of the images this variant programs.
    pub images: Vec<String>,
}

#[derive(Deserialize)]
struct TargetFile {
    family: Option<String>,
    #[serde(default)]
    variants: Vec<VariantFile>,
}

#[derive(Deserialize)]
struct VariantFile {
    name: Option<String>,
    #[serde(default)]
    images: Vec<ImageFile>,
}

#[derive(Deserialize)]
struct ImageFile {
    path: String,
}

impl TargetSummary {
    /// Reads the summary from target.yaml text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: TargetFile =
            yaml_serde::from_str(text).map_err(|e| format!("target.yaml: {e}"))?;
        let family = match file.family.as_deref().unwrap_or("") {
            "" | "cortex-m" | "swd" | "arm" => Family::CortexM,
            "esp32" | "esp32-serial" | "espressif" => Family::Esp32,
            other => {
                return Err(format!(
                    "target.yaml: unknown family '{other}' (expected cortex-m or esp32)"
                ));
            }
        };
        if file.variants.is_empty() {
            return Err("target.yaml lists no variants".into());
        }
        let only_one = file.variants.len() == 1;
        let mut unnamed = 0;
        let mut variants = Vec::new();
        for variant in file.variants {
            let images = match family {
                Family::CortexM => vec![manage::FIRMWARE_NAME.to_owned()],
                Family::Esp32 => variant
                    .images
                    .iter()
                    .map(|image| {
                        let path = image.path.as_str();
                        path.strip_prefix(manage::DEVICE_DIR)
                            .unwrap_or(path)
                            .to_owned()
                    })
                    .collect(),
            };
            match variant.name {
                Some(name) => variants.push(VariantSummary { name, images }),
                None if only_one => variants.push(VariantSummary {
                    name: String::new(),
                    images,
                }),
                None => unnamed += 1,
            }
        }
        Ok(Self {
            family,
            variants,
            unnamed,
        })
    }

    /// Reads target.yaml from disk.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Whether the variant must be chosen by name.
    pub fn needs_variant_choice(&self) -> bool {
        self.variants.len() + self.unnamed > 1
    }

    /// The variant the job would use, if it can be determined.
    pub fn variant(&self, wanted: Option<&str>) -> Option<&VariantSummary> {
        match wanted {
            Some(wanted) => self.variants.iter().find(|v| v.name == wanted),
            None if !self.needs_variant_choice() => self.variants.first(),
            None => None,
        }
    }
}

/// A compiled job and the images it pins.
#[derive(Clone)]
pub struct BuiltJob {
    /// Encoded job.
    pub bytes: Vec<u8>,
    /// Name the device shows.
    pub name: String,
    /// Device name and contents of each image.
    pub images: Vec<(String, Vec<u8>)>,
    /// Programming family.
    pub family: Family,
    /// Non-fatal compiler notes.
    pub warnings: Vec<String>,
}

impl std::fmt::Debug for BuiltJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let images: Vec<(&str, usize)> = self
            .images
            .iter()
            .map(|(name, data)| (name.as_str(), data.len()))
            .collect();
        f.debug_struct("BuiltJob")
            .field("name", &self.name)
            .field("family", &self.family)
            .field("bytes", &self.bytes.len())
            .field("images", &images)
            .field("warnings", &self.warnings)
            .finish()
    }
}

impl BuiltJob {
    /// Total bytes of all images.
    pub fn image_bytes(&self) -> usize {
        self.images.iter().map(|(_, data)| data.len()).sum()
    }
}

/// What the user chooses for the image with device name `name`: "firmware
/// to program" for a Cortex-M target's only image, else "file for <name>".
pub fn image_description(name: &str) -> String {
    if name == manage::FIRMWARE_NAME {
        "firmware to program".into()
    } else {
        format!("file for {name}")
    }
}

/// Compiles the profile's job, reading every file it names.
///
/// Errors are sentences meant for the user.
pub fn build_job(profile: &Profile) -> Result<BuiltJob, String> {
    let target_path = profile
        .target
        .as_deref()
        .ok_or("Choose the target description (target.yaml).")?;
    let target_text = std::fs::read_to_string(target_path)
        .map_err(|e| format!("Cannot read {}: {e}", target_path.display()))?;
    let summary = TargetSummary::parse(&target_text)?;
    let variant_name = profile.variant.as_deref().filter(|v| !v.is_empty());
    let Some(variant) = summary.variant(variant_name) else {
        return Err(match variant_name {
            Some(name) => format!("target.yaml has no variant named '{name}'."),
            None => "Choose which variant of the target to use.".into(),
        });
    };

    let mut images = Vec::new();
    let mut hashes = ImageHashes::new();
    for name in &variant.images {
        let Some(path) = profile.images.get(name) else {
            return Err(format!("Choose the {}.", image_description(name)));
        };
        let data =
            std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        if data.is_empty() {
            return Err(format!("{} is empty.", path.display()));
        }
        hashes.insert(name.clone(), manage::sha256(&data));
        images.push((name.clone(), data));
    }

    let pre_program = read_optional(profile.pre_program.as_deref())?;
    let post_program = read_optional(profile.post_program.as_deref())?;
    let job_name = profile.job_name.as_deref().filter(|n| !n.trim().is_empty());
    let sources = Sources {
        name: job_name,
        target: &target_text,
        // An unnamed single variant is selected without a name.
        variant: variant_name,
        pre_program: pre_program.as_deref(),
        post_program: post_program.as_deref(),
        images: &hashes,
    };
    let compiled = compile::compile(&sources).map_err(|e| e.to_string())?;
    Ok(BuiltJob {
        bytes: compiled.job.encode_to_vec(),
        name: compiled.job.name,
        images,
        family: summary.family,
        warnings: compiled.warnings,
    })
}

fn read_optional(path: Option<&Path>) -> Result<Option<String>, String> {
    path.map(|path| {
        std::fs::read_to_string(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures");

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(FIXTURES).join(name)
    }

    #[test]
    fn summarizes_a_probe_rs_target() {
        let summary = TargetSummary::load(&fixture("stm32wle5cc.yaml")).unwrap();
        assert_eq!(summary.family, Family::CortexM);
        assert_eq!(summary.variants.len(), 1);
        assert_eq!(summary.variants[0].name, "STM32WLE5CC");
        assert_eq!(summary.variants[0].images, ["firmware.bin"]);
        assert!(!summary.needs_variant_choice());
        assert!(summary.variant(None).is_some());
    }

    #[test]
    fn summarizes_esp32_images() {
        let summary = TargetSummary::parse(
            "family: esp32\nvariants:\n  - name: a\n    chip: esp32s3\n    images:\n      - { path: /data/boot.bin, offset: 0 }\n      - { path: /data/app.bin, offset: 0x10000 }\n  - name: b\n  - chip: esp32\n",
        )
        .unwrap();
        assert_eq!(summary.family, Family::Esp32);
        assert_eq!(summary.variants[0].images, ["boot.bin", "app.bin"]);
        assert_eq!(summary.unnamed, 1);
        assert!(summary.needs_variant_choice());
        assert!(summary.variant(None).is_none());
        assert_eq!(summary.variant(Some("b")).unwrap().images.len(), 0);
    }

    #[test]
    fn rejects_unknown_family() {
        let error = TargetSummary::parse("family: riscv\nvariants: [{name: x}]").unwrap_err();
        assert!(error.contains("riscv"), "{error}");
    }

    #[test]
    fn builds_the_same_job_as_the_cli() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("app.bin");
        std::fs::write(&image, [1, 2, 3, 4]).unwrap();
        let mut profile = Profile::named("t");
        profile.target = Some(fixture("stm32wle5cc.yaml"));
        profile.pre_program = Some(fixture("pre_prog.yaml"));
        profile.post_program = Some(fixture("post_prog.yaml"));
        profile.images.insert("firmware.bin".into(), image.clone());

        let job = build_job(&profile).unwrap();
        assert_eq!(job.name, "STM32WLE5CC");
        assert_eq!(job.family, Family::CortexM);
        assert_eq!(job.images, [("firmware.bin".to_owned(), vec![1, 2, 3, 4])]);

        let mut hashes = ImageHashes::new();
        hashes.insert("firmware.bin".into(), manage::sha256(&[1, 2, 3, 4]));
        let read = |name| std::fs::read_to_string(fixture(name)).unwrap();
        let expected = compile::compile(&Sources {
            name: None,
            target: &read("stm32wle5cc.yaml"),
            variant: None,
            pre_program: Some(&read("pre_prog.yaml")),
            post_program: Some(&read("post_prog.yaml")),
            images: &hashes,
        })
        .unwrap();
        assert_eq!(job.bytes, expected.job.encode_to_vec());
    }

    #[test]
    fn explains_what_is_missing() {
        let mut profile = Profile::named("t");
        assert!(build_job(&profile).unwrap_err().contains("target.yaml"));
        profile.target = Some(fixture("stm32wle5cc.yaml"));
        assert_eq!(
            build_job(&profile).unwrap_err(),
            "Choose the firmware to program."
        );
        profile.variant = Some("nope".into());
        assert!(build_job(&profile).unwrap_err().contains("nope"));
    }
}
