//! Job compiler tests against the Soul Injector example YAML files.
use prost::Message;
use sidp_agent::compile::{Compiled, ImageHashes, RunEnds, ScriptRef, Sources, compile};
use sidp_agent::job::step::Op;
use sidp_agent::job::{FlashAlgorithm, Job, MAX_STEPS, RunEnd, SchemaVersion, Target};

const TARGET: &str = include_str!("fixtures/stm32wle5cc.yaml");
const PRE: &str = include_str!("fixtures/pre_prog.yaml");
const POST: &str = include_str!("fixtures/post_prog.yaml");

const FIRMWARE_HASH: [u8; 32] = [0x11; 32];
const BOOTLOADER_HASH: [u8; 32] = [0x22; 32];

const ESP32_TARGET: &str = r#"
family: esp32
control_pins:
  reset_assert_level: high
variants:
  - name: esp32s31
    chip: esp32s31
    images:
      - { path: /data/bootloader.bin, offset: 0x0 }
      - { path: /data/firmware.bin, offset: 0x10000 }
"#;

fn image_hashes(names: &[(&str, [u8; 32])]) -> ImageHashes {
    let mut hashes = ImageHashes::new();
    for (name, hash) in names {
        hashes.insert((*name).to_owned(), *hash);
    }
    hashes
}

fn compile_with(
    target: &str,
    name: Option<&str>,
    pre: Option<&str>,
    post: Option<&str>,
    images: &ImageHashes,
) -> Result<Compiled, String> {
    let sources = Sources {
        name,
        target,
        variant: None,
        pre_program: pre,
        post_program: post,
        images,
        script: None,
        run_end: RunEnds::default(),
    };
    compile(&sources).map_err(|e| e.to_string())
}

fn esp32_images() -> ImageHashes {
    image_hashes(&[
        ("firmware.bin", FIRMWARE_HASH),
        ("bootloader.bin", BOOTLOADER_HASH),
    ])
}

/// Compiles a Cortex-M target with its firmware image pinned.
fn compile_target(target: &str, pre: Option<&str>, post: Option<&str>) -> Result<Compiled, String> {
    let images = image_hashes(&[("firmware.bin", FIRMWARE_HASH)]);
    compile_with(target, None, pre, post, &images)
}

fn error_of(result: Result<Compiled, String>) -> String {
    match result {
        Ok(_) => panic!("compile unexpectedly succeeded"),
        Err(error) => error,
    }
}

fn compile_error(target: &str, pre: Option<&str>) -> String {
    error_of(compile_target(target, pre, None))
}

fn target_of(job: &Job) -> &Target {
    job.target.as_ref().unwrap()
}

fn algorithm_of(job: &Job) -> &FlashAlgorithm {
    target_of(job)
        .cortex_m
        .as_ref()
        .unwrap()
        .algorithm
        .as_ref()
        .unwrap()
}

fn write_steps(count: usize) -> String {
    let mut text = String::from("steps:\n");
    for _ in 0..count {
        text.push_str("  - { type: WRITE_32, addr: 0x20000000, data: 0x1 }\n");
    }
    text
}

#[test]
fn example_target_matches_firmware_parser_rules() {
    let job = compile_target(TARGET, Some(PRE), Some(POST)).unwrap().job;
    assert_eq!(job.schema_version, SchemaVersion::Current as i32);
    assert_eq!(job.name, "STM32WLE5CC");
    let target = target_of(&job);
    assert_eq!(target.variant_name, "STM32WLE5CC");
    assert_eq!(
        target.cortex_m.as_ref().unwrap().firmware_sha256,
        FIRMWARE_HASH
    );
    assert!(target.esp32.is_none());
    assert_eq!(target.memory_map.len(), 3);
    assert_eq!(target.self_tests[0].addr, 0xffff_f006);

    let algorithm = algorithm_of(&job);
    assert_eq!(algorithm.load_address, 0x2000_0020);
    // Entry points become absolute; data_section_offset is already absolute.
    assert_eq!(algorithm.pc_init, Some(0x2000_012d));
    assert_eq!(algorithm.pc_verify, Some(0x2000_0335));
    assert_eq!(algorithm.data_section_offset, Some(0x2000_1aa8));
    assert_eq!(algorithm.flash_start, Some(0x0800_0000));
    assert_eq!(algorithm.flash_end, Some(0x0804_0000));
    assert_eq!(algorithm.page_size, 0x800);
    assert_eq!(algorithm.instructions.len(), 6792);

    let pre = job.pre_program.as_ref().unwrap();
    let post = job.post_program.as_ref().unwrap();
    assert_eq!(pre.steps.len(), 14);
    assert_eq!(post.steps.len(), 14);
    assert!(pre.steps[11].ignore_error);
    match &post.steps[8].op {
        Some(Op::ReadModWrite32(step)) => assert_eq!(step.data, 0xbb),
        other => panic!("unexpected step {other:?}"),
    }
}

#[test]
fn encoded_job_round_trips() {
    let job = compile_target(TARGET, Some(PRE), None).unwrap().job;
    let decoded = Job::decode(job.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded, job);
    assert!(decoded.post_program.is_none());
}

#[test]
fn esp32_target_uses_defaults_and_control_pins() {
    let job = compile_with(ESP32_TARGET, None, None, None, &esp32_images())
        .unwrap()
        .job;
    let target = target_of(&job);
    assert!(target.cortex_m.is_none());
    assert!(target.memory_map.is_empty());
    let esp32 = target.esp32.as_ref().unwrap();
    assert_eq!(esp32.chip, "esp32s31");
    assert_eq!(esp32.baud, 115_200);
    assert_eq!(esp32.flash_size_kb, None);
    assert!(esp32.reset_assert_high);
    assert!(!esp32.boot_assert_high);
    assert_eq!(esp32.images[1].offset, 0x10000);
    assert_eq!(esp32.images[0].sha256, BOOTLOADER_HASH);
    assert_eq!(esp32.images[1].sha256, FIRMWARE_HASH);
}

#[test]
fn every_programmed_image_must_be_pinned() {
    let firmware_only = image_hashes(&[("firmware.bin", FIRMWARE_HASH)]);
    let error = error_of(compile_with(ESP32_TARGET, None, None, None, &firmware_only));
    assert!(error.contains("--image bootloader.bin="));
    let error = error_of(compile_with(TARGET, None, None, None, &ImageHashes::new()));
    assert!(error.contains("--image firmware.bin="));

    let outside = ESP32_TARGET.replace("/data/bootloader.bin", "/sd/bootloader.bin");
    let error = error_of(compile_with(&outside, None, None, None, &esp32_images()));
    assert!(error.contains("/data/<name>"));

    let compiled = compile_with(TARGET, None, None, None, &esp32_images()).unwrap();
    assert_eq!(
        compiled.warnings,
        ["image 'bootloader.bin' is not used by this job"]
    );
}

#[test]
fn job_name_overrides_variant_and_is_bounded() {
    let images = image_hashes(&[("firmware.bin", FIRMWARE_HASH)]);
    let job = compile_with(TARGET, Some("line-a"), None, None, &images)
        .unwrap()
        .job;
    assert_eq!(job.name, "line-a");
    let long = "x".repeat(32);
    let error = error_of(compile_with(TARGET, Some(&long), None, None, &images));
    assert!(error.contains("job name"));
}

#[test]
fn legacy_blob_steps_are_skipped_with_a_warning() {
    let pre = "steps:\n  - { type: READ_BLOB }\n  - { type: SWD_REINIT }\n";
    let compiled = compile_target(TARGET, Some(pre), None).unwrap();
    assert_eq!(compiled.job.pre_program.unwrap().steps.len(), 1);
    assert_eq!(compiled.warnings.len(), 1);
}

#[test]
fn rejects_inputs_the_firmware_cannot_hold() {
    let two_variants = ESP32_TARGET.replace(
        "variants:\n",
        "variants:\n  - { name: other, chip: esp32 }\n",
    );
    assert!(compile_error(&two_variants, None).contains("--variant"));
    assert!(compile_error(TARGET, Some("steps:\n  - { type: JUMP }\n")).contains("unknown type"));
    assert!(
        compile_error(TARGET, Some("steps:\n  - { type: WRITE_32, addr: 0x1 }\n"))
            .contains("missing 'data'")
    );
    assert!(compile_error(TARGET, Some(&write_steps(MAX_STEPS + 1))).contains("exceed the limit"));
    assert!(compile_target(TARGET, Some(&write_steps(MAX_STEPS)), None).is_ok());

    let no_page_size = TARGET.replace("      page_size: 0x800\n", "");
    assert!(compile_error(&no_page_size, None).contains("page_size"));
    let wide_address = TARGET.replace("load_address: 0x20000020", "load_address: 0x120000020");
    assert!(compile_error(&wide_address, None).contains("32 bits"));
}

fn compile_script(
    pre: Option<&str>,
    asset: &str,
    sha256: Option<[u8; 32]>,
) -> Result<Compiled, String> {
    let images = image_hashes(&[("firmware.bin", FIRMWARE_HASH)]);
    let sources = Sources {
        name: None,
        target: TARGET,
        variant: None,
        pre_program: pre,
        post_program: None,
        images: &images,
        script: Some(ScriptRef {
            asset,
            sha256,
            hook_timeout_ms: 5000,
            network: true,
        }),
        run_end: RunEnds::default(),
    };
    compile(&sources).map_err(|e| e.to_string())
}

#[test]
fn script_job_carries_the_script() {
    let job = compile_script(None, "rdp.qbc", Some([0x33; 32]))
        .unwrap()
        .job;
    let script = job.script.as_ref().unwrap();
    assert_eq!(script.asset, "rdp.qbc");
    assert_eq!(script.sha256, [0x33; 32]);
    assert_eq!(script.hook_timeout_ms, 5000);
    assert!(script.network);
    assert!(job.pre_program.is_none() && job.post_program.is_none());
    assert_eq!(Job::decode(job.encode_to_vec().as_slice()).unwrap(), job);

    let unpinned = compile_script(None, "rdp.qbc", None).unwrap().job;
    assert!(unpinned.script.unwrap().sha256.is_empty());
}

#[test]
fn script_excludes_procedures_and_image_names() {
    let both = error_of(compile_script(Some(PRE), "rdp.qbc", None));
    assert!(both.contains("not both"), "{both}");
    let clash = error_of(compile_script(None, "firmware.bin", None));
    assert!(clash.contains("both named"), "{clash}");
}

fn compile_run_end(
    target: &str,
    images: &ImageHashes,
    run_end: RunEnds,
) -> Result<Compiled, String> {
    let sources = Sources {
        name: None,
        target,
        variant: None,
        pre_program: None,
        post_program: None,
        images,
        script: None,
        run_end,
    };
    compile(&sources).map_err(|e| e.to_string())
}

#[test]
fn run_end_defaults_to_the_device_choice() {
    let images = image_hashes(&[("firmware.bin", FIRMWARE_HASH)]);
    let job = compile_run_end(TARGET, &images, RunEnds::default())
        .unwrap()
        .job;
    // 0 on the wire: the device resets after a pass and leaves a failure as it is.
    assert_eq!(job.on_pass, RunEnd::Default as i32);
    assert_eq!(job.on_fail, RunEnd::Default as i32);
    assert_eq!(RunEnd::Default as i32, 0);
}

#[test]
fn run_end_is_carried_for_cortex_m() {
    let images = image_hashes(&[("firmware.bin", FIRMWARE_HASH)]);
    let run_end = RunEnds {
        on_pass: RunEnd::Resume,
        on_fail: RunEnd::Reset,
    };
    let job = compile_run_end(TARGET, &images, run_end).unwrap().job;
    let decoded = Job::decode(job.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded.on_pass(), RunEnd::Resume);
    assert_eq!(decoded.on_fail(), RunEnd::Reset);
}

#[test]
fn run_end_is_refused_for_esp32() {
    let run_end = RunEnds {
        on_pass: RunEnd::Default,
        on_fail: RunEnd::AsIs,
    };
    let error = error_of(compile_run_end(ESP32_TARGET, &esp32_images(), run_end));
    assert!(error.contains("ESP32"), "{error}");
    assert!(compile_run_end(ESP32_TARGET, &esp32_images(), RunEnds::default()).is_ok());
}
