//! Runs the admin panel's JavaScript tests as part of `cargo test`.
//!
//! `private/rdadmin.html` is a single file with its script inline: no build
//! step, no module boundary, nothing a Rust test can reach into. So the tests
//! in `tests/panel/` lift the code out of the page and run it against a small
//! DOM shim, and this target is what makes `cargo test` run them.
//!
//! One command covers the whole project, and CI needs no second job. See
//! `tests/panel/dom.js` for why the shim is hand-written rather than jsdom.
//!
//! To run just these, without waiting on the Rust suite:
//!
//!     cd src/tests/panel && node --test

use std::path::{Path, PathBuf};
use std::process::Command;

fn panel_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("panel")
}

/// Whether a usable `node` is on PATH.
fn node_version() -> Option<String> {
    let output = Command::new("node").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
fn admin_panel_javascript_passes_its_own_tests() {
    let dir = panel_dir();
    assert!(
        dir.join("polls.test.js").exists(),
        "expected the panel tests at {}",
        dir.display()
    );

    let Some(version) = node_version() else {
        // Missing node is a fair state for a machine that only touches the
        // Rust side, so it skips -- but never in CI, where skipping silently
        // would mean the panel ships with nothing checking it. GitHub Actions
        // sets CI=true, and the workflow installs node explicitly.
        if std::env::var("CI").is_ok() {
            panic!(
                "node is required to run the admin panel tests and CI is set. \
                 The workflow must install Node before `cargo test`."
            );
        }
        eprintln!(
            "skipping admin_panel_javascript_passes_its_own_tests: node is not on PATH.\n\
             Install Node 18 or newer to run the panel tests locally."
        );
        return;
    };

    // Run from inside the directory: `node --test` with no path argument
    // discovers every *.test.js beside it, so a new panel test file is picked
    // up without touching this harness.
    let output = Command::new("node")
        .arg("--test")
        .current_dir(&dir)
        .output()
        .expect("run node --test");

    if !output.status.success() {
        // node's own report is far more useful than any assertion message, so
        // hand it through rather than summarising it.
        panic!(
            "the admin panel's JavaScript tests failed (node {version})\n\
             \n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
