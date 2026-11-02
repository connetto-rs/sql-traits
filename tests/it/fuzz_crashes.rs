//! Anti-regression tests for crashes discovered by fuzzing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]
//! This test automatically discovers crash files from the cargo-fuzz artifacts
//! and verifies that they no longer cause panics.

use std::path::Path;

use arbitrary::{Arbitrary, Unstructured};
use sql_traits::prelude::ParserDB;
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

/// Test that parsing does not panic for both dialects.
/// Provides context about which crash file caused the failure.
fn should_not_panic_with_context(sql: &str, crash_file: &Path) {
    use std::panic;

    let result = panic::catch_unwind(|| {
        let _ = ParserDB::parse::<GenericDialect>(sql);
        let _ = ParserDB::parse::<PostgreSqlDialect>(sql);
    });

    if let Err(e) = result {
        panic!("Crash file {} caused a panic!\nSQL: {sql:?}\nPanic: {e:?}", crash_file.display());
    }
}

/// Copies the crash files from the cargo-fuzz artifacts
/// to the 'tests/fuzz_dialect' directory for testing and collect
/// a regression tests suite over time.
fn copy_crash_files() {
    let toml_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let crash_dir = Path::new(&toml_dir).join("fuzz/artifacts/fuzz_dialect");
    let test_dir = Path::new(&toml_dir).join("tests/fuzz_dialect");

    if !test_dir.exists() {
        std::fs::create_dir(&test_dir).unwrap();
    }

    let Ok(entries) = std::fs::read_dir(crash_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if path.is_file() && file_name.starts_with("crash-") {
            let dest_path = test_dir.join(file_name);
            let _ = std::fs::copy(&path, &dest_path);
        }
    }
}

/// Discover and test all crash files from the cargo-fuzz artifacts.
#[test]
fn test_fuzz_crashes() {
    copy_crash_files();

    // We load the SQL statements from the 'tests/fuzz_dialect' directory, which
    // should contain the crash files copied from the cargo-fuzz artifacts.
    let toml_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let test_dir = Path::new(&toml_dir).join("tests/fuzz_dialect");
    let Ok(entries) = std::fs::read_dir(test_dir) else {
        return;
    };

    let crash_files: Vec<_> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.is_file() {
                let bytes = std::fs::read(&path).ok()?;
                Some((path, bytes))
            } else {
                None
            }
        })
        .collect();

    if crash_files.is_empty() {
        return;
    }

    for (path, bytes) in crash_files {
        // Decode the string exactly as the libFuzzer target does
        let sql: &str = match <&str>::arbitrary_take_rest(Unstructured::new(&bytes)) {
            Ok(s) => s,
            Err(_) => {
                // If arbitrary can't extract a valid string, skip this file
                // (the crash was likely in the arbitrary extraction itself)
                continue;
            }
        };

        // This should NOT panic - if it does, the test fails
        should_not_panic_with_context(sql, &path);
    }
}
