// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! Helpers of the unit tests.

use std::path::{Path, PathBuf};

/// A fresh, empty directory `<target>/tmp/rusteda-workspace-unit/<name>`; test names are unique,
/// and a later run starts from an empty directory. Unit tests do not get `CARGO_TARGET_TMPDIR`,
/// so the target directory is found from the test executable: it is the nearest directory above
/// the executable that holds Cargo's `CACHEDIR.TAG`. Where the executable lies below it depends
/// on the Cargo version (`<profile>/deps/<test>` up to 1.99,
/// `<profile>/build/<package>/<hash>/out/<test>` from 1.100), so the depth is not assumed.
pub(crate) fn temp_dir(name: &str) -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let exe = std::env::current_exe().expect("test executable path");
            target_dir(&exe)
                .expect("test executable below the target directory")
                .join("tmp")
        });
    let dir = base.join("rusteda-workspace-unit").join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("remove an old test directory");
    }
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

/// The Cargo target directory that holds the test executable `exe`.
fn target_dir(exe: &Path) -> Option<&Path> {
    exe.ancestors()
        .skip(1)
        .find(|dir| dir.join("CACHEDIR.TAG").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The target directory is found at either depth of the test executable, and the unit-test
    /// directory of this very run lies directly below `<target>/tmp`.
    #[test]
    fn target_directory_is_found_at_any_depth() {
        let root = temp_dir("target_directory_is_found_at_any_depth");
        std::fs::write(
            root.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        for below in [
            "release/deps/unit-0123",
            "release/build/rusteda-workspace/0123/out/unit-0123",
        ] {
            let exe = root.join(below);
            assert_eq!(target_dir(&exe), Some(root.as_path()), "{below}");
        }
        assert_eq!(target_dir(Path::new("/unit-0123")), None);
        if std::env::var_os("CARGO_TARGET_TMPDIR").is_none() {
            let tmp = root.parent().and_then(Path::parent).unwrap();
            assert_eq!(tmp.file_name().unwrap(), "tmp");
            assert!(tmp.parent().unwrap().join("CACHEDIR.TAG").is_file());
        }
    }
}
