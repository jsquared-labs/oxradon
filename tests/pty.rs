//! colorama's exit-time reset, which needs stdout to
//! be a real terminal. Expected values observed from radon 6.0.1 through a
//! pseudo-terminal and through pipes.
#![cfg(unix)]

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::process::{Command, Stdio};

mod common;

use common::{command, output, scratch};

const RESET: &[u8] = b"\x1b[0m";

/// Run `cmd` with stdout on a pseudo-terminal; return what it printed.
fn on_terminal(mut cmd: Command) -> Vec<u8> {
    let pty = nix::pty::openpty(None, None).expect("a pseudo-terminal");
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(pty.slave))
        .stderr(Stdio::null())
        .spawn()
        .expect("oxradon starts");
    // `Command` keeps its copy of the terminal side open until dropped, and
    // the reader sees end of file only once every copy is closed.
    drop(cmd);
    let reader = std::thread::spawn(move || {
        let mut master = File::from(pty.master);
        let mut printed = Vec::new();
        let mut buf = [0; 4096];
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => printed.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break, // EIO once the last writer closes (Linux)
            }
        }
        printed
    });
    assert!(child.wait().expect("oxradon finishes").code().is_some());
    reader.join().unwrap()
}

fn cases() -> Vec<Vec<String>> {
    let report = scratch("pty").join("out.txt");
    let report = report.to_str().unwrap().to_owned();
    let fixture = "tests/fixtures/crlf.py".to_owned();
    let strs = |args: &[&str]| args.iter().map(|&a| a.to_owned()).collect();
    vec![
        strs(&["cc", "-j", &fixture]),
        strs(&["cc", &fixture]),
        strs(&["cc", "-O", &report, &fixture]),
        strs(&["cc", "-O", "missing-dir/out.txt", &fixture]), // the abort path
        strs(&["cc", "--bogus"]),                             // a usage error
        strs(&["cc", "--help"]),
    ]
}

#[test]
fn a_terminal_gets_the_reset_on_every_exit() {
    // radon: colorama's `atexit` hook writes ESC[0m to a terminal stdout,
    // whatever COLOR says.
    for args in cases() {
        for color in ["no", "auto"] {
            let mut cmd = command();
            cmd.args(&args).env("COLOR", color);
            let printed = on_terminal(cmd);
            assert!(
                printed.ends_with(RESET),
                "{args:?} COLOR={color}: {printed:?}"
            );
        }
    }
}

#[test]
fn a_pipe_gets_the_reset_only_with_pycharm_hosted() {
    for args in cases() {
        // No terminal, no `PYCHARM_HOSTED`: nothing extra.
        let out = output(command().args(&args), b"");
        assert!(!out.stdout.ends_with(RESET), "{args:?}: {:?}", out.stdout);
        // colorama takes stdout for a terminal when `PYCHARM_HOSTED` is set,
        // even to an empty value.
        for value in ["", "0"] {
            let out = output(command().args(&args).env("PYCHARM_HOSTED", value), b"");
            assert!(
                out.stdout.ends_with(RESET),
                "{args:?} {value:?}: {:?}",
                out.stdout
            );
        }
    }
}
