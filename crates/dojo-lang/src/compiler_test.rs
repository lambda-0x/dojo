use dojo_test_utils::compiler::build_test_config;
use scarb::core::TargetKind;
use scarb::ops::CompileOpts;

use crate::scarb_internal;

#[test]
fn test_compiler() {
    let config = build_test_config("../../examples/spawn-and-move/Scarb.toml").unwrap();
    assert!(
        scarb_internal::compile_workspace(
            &config,
            CompileOpts { include_targets: vec![], exclude_targets: vec![TargetKind::TEST] },
        )
        .is_ok(),
        "compilation failed"
    );
}
