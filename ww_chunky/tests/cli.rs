//! The command line as `ww` and a shell see it: help, version, the device selection options, the completion answer
//! `ww` relays for `ww dfu flash <TAB>`.

use std::process::{Command, Output};

fn ww_chunky(args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ww-chunky"))
        .args(args)
        .envs(env.iter().copied())
        .output()
        .expect("ww-chunky runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn version_has_sha_and_build_time() {
    let out = stdout(&ww_chunky(&["--version"], &[]));
    assert!(out.starts_with("ww-chunky 0."), "{out}");
    assert!(out.contains(", built "), "{out}");
}

#[test]
fn help_lists_the_steps_and_ww_s_device_options() {
    let out = stdout(&ww_chunky(&["--help"], &[]));
    for word in [
        "flash",
        "status",
        "confirm",
        "revert",
        "reboot",
        "--serial",
        "--usb-path",
        "WW_SERIAL",
    ] {
        assert!(out.contains(word), "{word} missing:\n{out}");
    }
    let flash = stdout(&ww_chunky(&["flash", "--help"], &[]));
    for word in [
        "--fw-version",
        "--key",
        "--unsigned",
        "--chip",
        "--no-reboot",
        "--in-flight",
    ] {
        assert!(flash.contains(word), "{word} missing:\n{flash}");
    }
}

/// `ww` asks with `_WW_PLUGIN_COMPLETE` and the words after the plugin's name, the last one being completed.
#[test]
fn completion_for_ww() {
    let out = stdout(&ww_chunky(&["fl"], &[("_WW_PLUGIN_COMPLETE", "1")]));
    assert!(out.lines().any(|l| l.starts_with("flash")), "{out}");
    let flags = stdout(&ww_chunky(&["flash", "--un"], &[("_WW_PLUGIN_COMPLETE", "1")]));
    assert!(flags.lines().any(|l| l.starts_with("--unsigned")), "{flags}");
}

/// `ww` runs the plugin with the typed words only after this answer.
#[test]
fn answers_the_plugin_probe() {
    let out = stdout(&ww_chunky(&["--ww-plugin"], &[]));
    assert_eq!(out.trim(), "ww-plugin 1");
}
