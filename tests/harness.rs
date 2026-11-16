//! Checks of the test harness itself, not of radon's behaviour.

mod common;

use std::path::Path;

use common::{checked_binary, scratch};

#[test]
fn oxradon_bin_must_be_an_absolute_executable_file() {
    // The release workflow points `OXRADON_BIN` at a wheel's installed binary; a wrong value
    // must stop the run rather than test something else.
    let dir = scratch("harness-oxradon-bin");
    assert!(checked_binary(Path::new("target/release/oxradon")).is_err());
    assert!(checked_binary(&dir.join("missing")).is_err());
    assert!(checked_binary(&dir).is_err());
    let built = Path::new(env!("CARGO_BIN_EXE_oxradon"));
    assert_eq!(checked_binary(built), Ok(built.to_path_buf()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let plain = dir.join("plain");
        std::fs::write(&plain, "").expect("write");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(checked_binary(&plain).is_err());
    }
}
