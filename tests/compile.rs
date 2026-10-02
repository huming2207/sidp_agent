//! Job compiler tests against the Soul Injector example YAML files.
use prost::Message;
use sidp_agent::compile::{Compiled, Sources, compile};
use sidp_agent::job::step::Op;
use sidp_agent::job::{FlashAlgorithm, Job, MAX_STEPS, SchemaVersion, Target};

const TARGET: &str = include_str!("fixtures/stm32wle5cc.yaml");
const PRE: &str = include_str!("fixtures/pre_prog.yaml");
const POST: &str = include_str!("fixtures/post_prog.yaml");

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

fn compile_target(target: &str, pre: Option<&str>, post: Option<&str>) -> Result<Compiled, String> {
    let sources = Sources {
        target,
        variant: None,
        pre_program: pre,
        post_program: post,
    };
    compile(&sources).map_err(|e| e.to_string())
}

fn compile_error(target: &str, pre: Option<&str>) -> String {
    match compile_target(target, pre, None) {
        Ok(_) => panic!("compile unexpectedly succeeded"),
        Err(error) => error,
    }
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
    let target = target_of(&job);
    assert_eq!(target.variant_name, "STM32WLE5CC");
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
    let job = compile_target(ESP32_TARGET, None, None).unwrap().job;
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
