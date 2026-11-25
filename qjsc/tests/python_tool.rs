//! The containers in fixtures/ were made by esp-quickjs-ng's
//! `tools/qjs_esp_bc.py compile stm32wl-rdp.js --strip LEVEL`. Remake them
//! after moving the quickjs submodule to the firmware's new release.
use sidp_qjsc::{Strip, compile_container};

const SOURCE: &str = include_str!("fixtures/stm32wl-rdp.js");

fn check(strip: Strip, expected: &[u8]) {
    let blob = compile_container(SOURCE, "stm32wl-rdp.js", strip).unwrap();
    assert!(
        blob == expected,
        "{strip:?}: differs from qjs_esp_bc.py's output"
    );
}

#[test]
fn same_bytes_as_qjs_esp_bc_py() {
    check(Strip::None, include_bytes!("fixtures/stm32wl-rdp.none.qbc"));
    check(
        Strip::Source,
        include_bytes!("fixtures/stm32wl-rdp.source.qbc"),
    );
    check(
        Strip::Debug,
        include_bytes!("fixtures/stm32wl-rdp.debug.qbc"),
    );
}
