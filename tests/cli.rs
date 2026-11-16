//! Command-line behaviours checked against radon 6.0.1 on CPython 3.14.
//! Each expected value was observed from radon itself (see the comments).

use std::ffi::OsStr;

mod common;

use common::{command, output, oxradon, run, scratch, shell};

#[test]
fn repeated_flags_are_accepted() {
    // radon: `cc -j -j file` prints normal JSON and exits 0.
    let out = run(&["cc", "-j", "-j", "tests/fixtures/crlf.py"], b"", &[]);
    assert!(out.contains("windows_newlines"));
}

#[test]
fn empty_output_file_means_stdout() {
    // radon: `-O ''` is falsy, so output goes to stdout.
    let out = run(
        &["cc", "-j", "--output-file", "", "-"],
        b"def f(): pass\n",
        &[],
    );
    assert!(out.starts_with("{\"-\": [{"));
}

#[test]
fn order_uses_full_unicode_upper_case() {
    // radon: `-o line\u{17f}` upper-cases to `LINES`, so blocks are in line order.
    let out = run(
        &["cc", "-o", "line\u{17f}", "tests/fixtures/branches.py"],
        b"",
        &[],
    );
    let lines: Vec<&str> = out.lines().skip(1).take(2).collect();
    assert_eq!(
        lines,
        ["    F 4:0 conditionals - B", "    F 17:0 loops - C"]
    );
}

#[test]
fn stdin_ignores_radonfilesencoding() {
    // radon reads stdin through `sys.stdin` (UTF-8): `caf\u{e9}` survives,
    // and even an unknown encoding is never looked up.
    for encoding in ["latin1", "bogus"] {
        let env = [("RADONFILESENCODING", encoding)];
        let out = run(
            &["cc", "-j", "-"],
            "def caf\u{e9}(): pass\n".as_bytes(),
            &env,
        );
        let escaped = format!("\"name\": \"caf{}u00e9\"", '\\');
        assert!(out.contains(&escaped), "{encoding}: {out}");
    }
    // ... and keeps a BOM, which `ast.parse` rejects.
    let env = [("RADONFILESENCODING", "utf-8-sig")];
    let out = run(&["cc", "-j", "-"], b"\xef\xbb\xbfdef f(): pass\n", &env);
    assert!(out.starts_with(r#"{"-": {"error": "#), "{out}");
}

#[test]
fn radonfilesencoding_follows_python_codec_names() {
    // radon accepts these spellings; each analyzes the file. `locale` is
    // `open()`'s locale sentinel (UTF-8 here).
    for encoding in [
        "cp1252",
        "UTF 8",
        "L1",
        "utf",
        "windows-1252",
        "\u{130}utf8",
        "locale",
    ] {
        let env = [("RADONFILESENCODING", encoding)];
        let out = run(&["cc", "-j", "tests/fixtures/crlf.py"], b"", &env);
        assert!(out.contains("windows_newlines"), "{encoding}: {out}");
    }
}

#[test]
fn unknown_radonfilesencoding_aborts_the_run() {
    // radon: `open()` raises LookupError outside the per-file handler, so the
    // whole run ends with one line on stdout (no JSON) and exit status 0.
    for encoding in ["", "Locale", "bogus"] {
        let env = [("RADONFILESENCODING", encoding)];
        let out = run(&["cc", "-j", "tests/fixtures/crlf.py"], b"", &env);
        assert_eq!(out, format!("ERROR: unknown encoding: {encoding}\n"));
    }
}

#[test]
fn doubled_braces_in_file_names_collapse_like_str_format() {
    // radon prints `a{{b}}.py` as `a{b}.py` (its logger calls `str.format()`).
    let dir = scratch("braces");
    std::fs::write(dir.join("a{{b}}.py"), "def f(): pass\n").unwrap();
    let path = dir.join("a{{b}}.py");
    let out = run(&["cc", path.to_str().unwrap()], b"", &[]);
    let header = out.lines().next().unwrap();
    assert!(header.ends_with("a{b}.py"), "{header}");
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_are_paths() {
    // radon: a missing non-UTF-8 path yields `{}` and exit 0.
    let missing = os(b"nonexistent-\xff.py");
    let out = oxradon(&[OsStr::new("cc"), OsStr::new("-j"), missing], b"", &[]);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"{}\n");
}

/// Three files where the middle one cannot be opened, or `None` (with a note on stderr) when
/// the test runs with privileges that ignore file permissions.
#[cfg(unix)]
fn files_with_unreadable_middle(name: &str) -> Option<std::path::PathBuf> {
    let dir = scratch(name);
    for f in ["a", "b", "c"] {
        let source = format!("def {f}(x):\n    return x or 1\n");
        std::fs::write(dir.join(format!("{f}.py")), source).unwrap();
    }
    common::make_unreadable(&dir.join("b.py")).then_some(dir)
}

#[cfg(unix)]
#[test]
fn a_file_that_cannot_be_opened_aborts_the_run() {
    let Some(dir) = files_with_unreadable_middle("unreadable") else {
        return;
    };
    let path = |f: &str| dir.join(f).to_str().unwrap().to_owned();
    let (a, b, c) = (path("a.py"), path("b.py"), path("c.py"));
    let abort = format!("ERROR: [Errno 13] Permission denied: '{b}'\n");
    // radon: earlier files are listed, then the error; no later files and
    // no averages (`-a`).
    let out = run(&["cc", "-s", "-a", &a, &b, &c], b"", &[]);
    assert_eq!(out, format!("{a}\n    F 1:0 a - A (2)\n{abort}"));
    // radon builds JSON after all files, so only the error is printed.
    let out = run(&["cc", "-j", &a, &b, &c], b"", &[]);
    assert_eq!(out, abort);
    // The report goes to the `-O` file, the error to stdout.
    let report = dir.join("report.txt");
    let out = run(
        &["cc", "-s", "-O", report.to_str().unwrap(), &a, &b, &c],
        b"",
        &[],
    );
    assert_eq!(out, abort);
    let written = std::fs::read_to_string(&report).unwrap();
    assert_eq!(written, format!("{a}\n    F 1:0 a - A (2)\n"));
}

#[cfg(unix)]
#[test]
fn a_dangling_symlink_aborts_the_run() {
    // radon: the walk lists `link.py`, and opening it raises ENOENT.
    let dir = scratch("dangling");
    std::os::unix::fs::symlink(dir.join("nowhere.py"), dir.join("link.py")).unwrap();
    let out = run(&["cc", dir.to_str().unwrap()], b"", &[]);
    let link = dir.join("link.py");
    let expected = format!(
        "ERROR: [Errno 2] No such file or directory: '{}'\n",
        link.display()
    );
    assert_eq!(out, expected);
}

#[test]
fn an_output_file_that_cannot_be_created_aborts_the_run() {
    // radon opens `-O` before any input: nothing but the error is printed.
    let out = run(
        &["cc", "-O", "missing-dir/out.txt", "tests/fixtures/crlf.py"],
        b"",
        &[],
    );
    assert_eq!(
        out,
        "ERROR: [Errno 2] No such file or directory: 'missing-dir/out.txt'\n"
    );
}

// radon's name and stream routing: which names read stdin, and which stream
// each line goes to.

#[cfg(unix)]
fn os(bytes: &[u8]) -> &OsStr {
    std::os::unix::ffi::OsStrExt::from_bytes(bytes)
}

#[cfg(unix)]
#[test]
fn non_utf8_radonfilesencoding_aborts_the_run() {
    // radon: `os.environ` decodes the value with `surrogateescape`, and
    // `io.open` fails to encode it back, before opening anything (so it
    // wins over ENOENT), with exit status 0 and no JSON.
    let at = |position: usize| {
        format!(
            "ERROR: 'utf-8' codec can't encode character '{}udcff' in position {position}: \
             surrogates not allowed\n",
            '\\'
        )
    };
    let plural = "ERROR: 'utf-8' codec can't encode characters in position 0-1: \
                  surrogates not allowed\n";
    // Only the first run of surrogates is reported, at its code-point position.
    for (value, want) in [(&b"ut\xffx\xfe"[..], at(2)), (b"a\xffb\xfe\xfdc", at(1))] {
        let out = output(
            command()
                .args(["cc", "-j", "tests/fixtures/crlf.py"])
                .env("RADONFILESENCODING", os(value)),
            b"",
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{value:x?}");
    }
    let dir = scratch("encode-error");
    std::os::unix::fs::symlink(dir.join("nowhere.py"), dir.join("link.py")).unwrap();
    for (value, want) in [(&b"\xff"[..], at(0).as_str()), (b"\xff\xfe", plural)] {
        for args in [
            &["cc", "-j", "tests/fixtures/crlf.py"][..],
            &["cc", "tests/fixtures/crlf.py"],
            &["cc", "-j", dir.to_str().unwrap()],
        ] {
            let out = output(
                command().args(args).env("RADONFILESENCODING", os(value)),
                b"",
            );
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{args:?}");
        }
        // stdin is read through `sys.stdin`, which never sees the value.
        let out = output(
            command()
                .args(["cc", "-j", "-"])
                .env("RADONFILESENCODING", os(value)),
            b"def f(): pass\n",
        );
        assert!(out.stdout.starts_with(b"{\"-\": [{"), "{out:?}");
    }
}

/// The per-file `ERROR` line oxradon prints for the syntax-error fixture
/// (its wording is Ruff's, a documented divergence).
fn syntax_error_line() -> String {
    let out = run(&["cc", "-j", "tests/fixtures/syntax_error.py"], b"", &[]);
    let start = out.find("\"error\": \"").unwrap() + "\"error\": \"".len();
    let message = &out[start..out.rfind('"').unwrap()];
    format!("    ERROR: {message}\n")
}

#[test]
fn per_file_errors_go_to_stdout_under_output_file() {
    // radon's `log_result` calls `log_error` without the stream: with `-O`,
    // the file gets the name and stdout gets the indented `ERROR` line.
    let dir = scratch("per-file-error");
    let report = dir.join("out.txt");
    let report = report.to_str().unwrap();
    let (bad, good) = ("tests/fixtures/syntax_error.py", "tests/fixtures/crlf.py");
    let out = run(&["cc", "-O", report, bad, good], b"", &[]);
    assert_eq!(out, syntax_error_line());
    let written = std::fs::read_to_string(report).unwrap();
    let listing = run(&["cc", good], b"", &[]);
    assert_eq!(written, format!("{bad}\n{listing}"));

    // On stdout the line is colored by stdout's rule.
    let out = run(&["cc", "-O", report, bad], b"", &[("COLOR", "yes")]);
    let colored = syntax_error_line().replacen("ERROR", "\x1b[1m\x1b[31mERROR\x1b[0m", 1);
    assert_eq!(out, colored);

    // JSON keeps the error inside the report.
    let out = run(&["cc", "-j", "-O", report, bad], b"", &[]);
    assert_eq!(out, "");
    assert!(
        std::fs::read_to_string(report)
            .unwrap()
            .contains("\"error\"")
    );

    // Without `-O` both lines share stdout, in order.
    let out = run(&["cc", bad, good], b"", &[]);
    assert_eq!(out, format!("{bad}\n{}{listing}", syntax_error_line()));
}

/// A scratch directory holding `good.py` and, if `dash`, a file `-` whose
/// function is `from_disk`.
fn dash_dir(name: &str, dash: bool) -> std::path::PathBuf {
    let dir = scratch(name);
    std::fs::write(dir.join("good.py"), "def good(x):\n    return x or 1\n").unwrap();
    if dash {
        std::fs::write(dir.join("-"), "def from_disk(): pass\n").unwrap();
    }
    dir
}

/// Run `cc -j args` in `dir`, with stdin defining `from_stdin`.
fn json_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = output(
        command().current_dir(dir).args(["cc", "-j"]).args(args),
        b"def from_stdin(): pass\n",
    );
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn the_name_dash_reads_stdin_however_it_was_found() {
    // radon's `_open` compares the yielded name with `-`.
    let dir = dash_dir("dash-names", true);
    // An explicit `-` next to other paths, when a file `-` exists.
    let out = json_in(&dir, &["-", "good.py"]);
    assert!(
        out.starts_with("{\"-\": [{") && out.contains("from_stdin"),
        "{out}"
    );
    assert!(!out.contains("from_disk"), "{out}");
    // A walk whose root normalizes to `.` yields `-`, Python without a shebang.
    for root in [".", "./"] {
        let out = json_in(&dir, &[root]);
        assert!(
            out.contains("\"-\": [{") && out.contains("from_stdin"),
            "{root}: {out}"
        );
        assert!(!out.contains("from_disk"), "{root}: {out}");
    }
    std::fs::create_dir(dir.join("sub")).unwrap();
    let out = json_in(&dir, &["sub/.."]);
    assert!(out.contains("from_stdin"), "{out}");
    // `./-` is a different name: an ordinary file, Python only by shebang.
    assert_eq!(json_in(&dir, &["./-"]), "{}\n");
    std::fs::write(
        dir.join("-"),
        "#!/usr/bin/env python\ndef from_disk(): pass\n",
    )
    .unwrap();
    let out = json_in(&dir, &["./-"]);
    assert!(
        out.starts_with("{\"./-\": [{") && out.contains("from_disk"),
        "{out}"
    );
    // A walked `sub/-` is an ordinary file: no shebang, so not Python.
    std::fs::write(dir.join("sub/-"), "def in_sub(): pass\n").unwrap();
    assert_eq!(json_in(&dir, &["sub"]), "{}\n");
}

#[test]
fn a_repeated_dash_gets_what_is_left_of_stdin() {
    // radon: the first `-` reads all of stdin; the second reads nothing, so
    // it has no blocks and leaves the first entry in place.
    let dir = dash_dir("dash-repeated", true);
    let out = json_in(&dir, &["-", "-", "good.py"]);
    assert_eq!(out.matches("from_stdin").count(), 1, "{out}");
    assert!(
        out.starts_with("{\"-\": [{") && out.contains("\"good.py\""),
        "{out}"
    );
    // JSON collapses the repeated name; the terminal listing shows that the
    // second `-` got nothing (replaying stdin would list `from_stdin` twice).
    let out = output(
        command()
            .current_dir(&dir)
            .args(["cc", "-", "-", "good.py"]),
        b"def from_stdin(): pass\n",
    );
    let want = "-\n    F 1:0 from_stdin - A\ngood.py\n    F 1:0 good - A\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
    // All arguments `-`: stdin once, file `-` or not.
    let out = json_in(&dir, &["-", "-"]);
    assert_eq!(out.matches("from_stdin").count(), 1, "{out}");
}

#[test]
fn a_dash_that_names_nothing_is_skipped() {
    // radon: not a file, so `os.walk('-')`, which yields nothing.
    let dir = dash_dir("dash-missing", false);
    let out = json_in(&dir, &["-", "good.py"]);
    assert!(
        out.starts_with("{\"good.py\": [{") && !out.contains("from_stdin"),
        "{out}"
    );
    // A directory `-` is walked like any other.
    std::fs::create_dir(dir.join("-")).unwrap();
    std::fs::write(dir.join("-/inner.py"), "def inner(): pass\n").unwrap();
    let out = json_in(&dir, &["-", "good.py"]);
    assert!(
        out.starts_with("{\"-/inner.py\": [{") && !out.contains("from_stdin"),
        "{out}"
    );
}

#[cfg(unix)]
#[test]
fn stdin_is_not_read_after_an_abort() {
    // radon stops at the file it cannot open and never reaches the `-` after
    // it. With stdin left open, reading it would block forever.
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let Some(dir) = files_with_unreadable_middle("abort-before-stdin") else {
        return;
    };
    std::fs::write(dir.join("-"), "def from_disk(): pass\n").unwrap();
    let mut child = command()
        .current_dir(&dir)
        .args(["cc", "-j", "a.py", "b.py", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _stdin_left_open = child.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("oxradon waited for stdin after the abort");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let want = "ERROR: [Errno 13] Permission denied: 'b.py'\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
}

#[test]
fn an_output_file_that_is_also_an_input_reads_as_empty() {
    // Documented divergence: radon reads whatever CPython has flushed
    // so far, which is nothing for a small report; oxradon always reads the
    // truncated, empty file.
    let dir = dash_dir("output-is-input", false);
    let out = output(
        command()
            .current_dir(&dir)
            .args(["cc", "-O", "out.py", "good.py", "out.py"]),
        b"",
    );
    assert!(out.status.success());
    assert_eq!(out.stdout, b"");
    let written = std::fs::read_to_string(dir.join("out.py")).unwrap();
    assert_eq!(written, "good.py\n    F 1:0 good - A\n");
}

// colorama's stdout wrapper and output-stream failures. Expected values
// observed from radon.

/// Which of the child's output streams goes to a pipe whose reader is gone.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum Broken {
    Stdout,
    Stderr,
}

/// Run `command` with one output stream on a readerless pipe, so every write
/// to it fails with `EPIPE`; return the exit code.
#[cfg(unix)]
fn exit_code_with_broken(broken: Broken, command: &mut std::process::Command) -> Option<i32> {
    use std::io::Write;
    use std::process::Stdio;
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    match broken {
        Broken::Stdout => command.stdout(writer).stderr(Stdio::null()),
        Broken::Stderr => command.stdout(Stdio::null()).stderr(writer),
    };
    let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // Not every command reads stdin (`--help`, a usage error, a named file), and one that
    // does not can exit before this write, which then fails with `EPIPE`.
    let _ = stdin.write_all(b"def f(): pass\n");
    drop(stdin);
    child.wait().unwrap().code()
}

/// Run `command` with stdout on a readerless pipe; return the exit code.
#[cfg(unix)]
fn exit_code_with_readerless_stdout(command: &mut std::process::Command) -> Option<i32> {
    exit_code_with_broken(Broken::Stdout, command)
}

#[cfg(unix)]
#[test]
fn a_failed_stdout_write_exits_120() {
    // radon: CPython cannot flush stdout at exit and ends with status 120,
    // whatever was being written: the report, help, or only the exit reset.
    let fixture = "tests/fixtures/crlf.py";
    for args in [
        &["cc", "-j", fixture][..],
        &["cc", fixture],
        &["cc", "-j", "-"],
        &["cc", "--help"],
    ] {
        let code = exit_code_with_readerless_stdout(command().args(args));
        assert_eq!(code, Some(120), "{args:?}");
    }
    // Nothing to report, but the reset fails.
    let mut reset_only = command();
    reset_only
        .args(["cc", "missing.py"])
        .env("PYCHARM_HOSTED", "");
    assert_eq!(exit_code_with_readerless_stdout(&mut reset_only), Some(120));
    // A usage error goes to stderr, so its status stands...
    let code = exit_code_with_readerless_stdout(command().args(["cc", "--bogus"]));
    assert_eq!(code, Some(2));
    // ... unless stderr fails, whose final flush fails in CPython just the same.
    let code = exit_code_with_broken(Broken::Stderr, command().args(["cc", "--bogus"]));
    assert_eq!(code, Some(120));
}

#[cfg(unix)]
#[test]
fn an_abort_line_without_its_newline_is_still_flushed() {
    // colorama's stale OSC spans eat this abort line's newline (radon prints
    // `ERROR: unknown encoding: ESC]b BEL` with no newline). colorama flushes
    // each write, so a failing stdout still ends with 120.
    let value = "\x1b]a\x07\x1b]b\x07.py";
    let args = ["cc", "tests/fixtures/crlf.py"];
    let out = run(&args, b"", &[("RADONFILESENCODING", value)]);
    assert_eq!(out, "ERROR: unknown encoding: \x1b]b\x07");
    let code =
        exit_code_with_readerless_stdout(command().args(args).env("RADONFILESENCODING", value));
    assert_eq!(code, Some(120));
}

#[test]
fn colorama_strips_escapes_from_stdout_when_color_is_off() {
    let dir = scratch("strip-escapes");
    let name = "x\x1b[31m.py";
    std::fs::write(dir.join(name), "def f(): pass\n").unwrap();
    let listing = |color: &str| {
        let out = output(
            command()
                .current_dir(&dir)
                .args(["cc", name])
                .env("COLOR", color),
            b"",
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(listing("no"), "x.py\n    F 1:0 f - A\n");
    assert_eq!(
        listing("yes"),
        "x\x1b[31m.py\n    \x1b[1m\x1b[35mF \x1b[0m1:0 f - \x1b[32mA\x1b[0m\n"
    );
    // colorama never touches the `-O` file.
    let out = output(
        command()
            .current_dir(&dir)
            .args(["cc", "-O", "report.txt", name]),
        b"",
    );
    assert_eq!(out.stdout, b"");
    let written = std::fs::read_to_string(dir.join("report.txt")).unwrap();
    assert_eq!(written, format!("{name}\n    F 1:0 f - A\n"));
    // The abort line on stdout is stripped too.
    let out = run(
        &["cc", "tests/fixtures/crlf.py"],
        b"",
        &[("RADONFILESENCODING", "\x1b[31mx")],
    );
    assert_eq!(out, "ERROR: unknown encoding: x\n");
}

#[cfg(unix)]
#[test]
fn write_failures_under_a_file_size_limit() {
    // radon ignores SIGXFSZ (as CPython does), so writes past `ulimit -f`
    // fail with EFBIG. On the `-O` file that is radon's top-level `ERROR`
    // line on stdout and exit 0; on stdout it is exit 120.
    let dir = scratch("file-size-limit");
    let source = "def f(): pass\n".repeat(200);
    std::fs::write(dir.join("big.py"), source).unwrap();
    let to_file = output(
        shell(r#"ulimit -f 1; exec "$OXRADON" cc -O out.txt big.py"#).current_dir(&dir),
        b"",
    );
    assert_eq!(to_file.status.code(), Some(0), "{to_file:?}");
    assert_eq!(to_file.stdout, b"ERROR: [Errno 27] File too large\n");
    let to_stdout = output(
        shell(r#"ulimit -f 1; exec "$OXRADON" cc big.py > out2.txt"#).current_dir(&dir),
        b"",
    );
    assert_eq!(to_stdout.status.code(), Some(120), "{to_stdout:?}");
}

#[cfg(unix)]
#[test]
fn a_closed_stdout_behaves_like_dev_null() {
    // Documented divergence: radon exits 1 here, but Rust's runtime
    // reopens a closed fd 1 on /dev/null before `main`, so oxradon cannot
    // tell `>&-` from `>/dev/null`.
    let out = output(
        &mut shell(r#"exec "$OXRADON" cc tests/fixtures/crlf.py >&-"#),
        b"",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

// radon's configuration files. Expected values observed from radon; the full
// comparison is `scripts/diff_config.py`.

/// A scratch project with `src/f.py` (ranks A, B and C) and the given files,
/// plus an empty `$HOME`; returns the project directory.
fn config_project(name: &str, files: &[(&str, &[u8])]) -> std::path::PathBuf {
    use std::fmt::Write as _;
    let dir = scratch(name);
    let branches = |n: u32| {
        let mut body = String::new();
        for i in 0..n {
            let _ = writeln!(body, "    if x == {i}: return {i}");
        }
        body
    };
    let source = format!(
        "def a():\n    pass\n\ndef b(x):\n{}    return 0\n\ndef c(x):\n{}    return 0\n",
        branches(5),
        branches(10)
    );
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::write(dir.join("src/f.py"), source).unwrap();
    for (path, data) in files {
        std::fs::write(dir.join(path), data).unwrap();
    }
    dir
}

fn cc_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    output(common::command_in(dir).arg("cc").args(args), b"")
}

#[test]
fn config_files_supply_defaults_later_sources_winning() {
    let dir = config_project(
        "config-precedence",
        &[
            ("radon.cfg", b"[radon]\ncc_min = C\n"),
            ("pyproject.toml", b"[tool.radon]\ncc_min = \"B\"\n"),
        ],
    );
    let out = cc_in(&dir, &["src"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "src/f.py\n    F 12:0 c - C\n    F 4:0 b - B\n"
    );
    // A flag replaces the configured value.
    let out = cc_in(&dir, &["-n", "C", "src"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "src/f.py\n    F 12:0 c - C\n"
    );
}

#[test]
fn a_configured_true_turns_its_flag_into_off() {
    // mando declares `store_false` for a `True` default.
    let dir = config_project(
        "config-flip",
        &[("setup.cfg", b"[radon]\nshow_complexity = yes\ncc_min = C\n")],
    );
    let on = cc_in(&dir, &["src"]);
    assert_eq!(
        String::from_utf8_lossy(&on.stdout),
        "src/f.py\n    F 12:0 c - C (11)\n"
    );
    let off = cc_in(&dir, &["-s", "src"]);
    assert_eq!(
        String::from_utf8_lossy(&off.stdout),
        "src/f.py\n    F 12:0 c - C\n"
    );
}

#[test]
fn a_broken_config_ends_the_run_before_anything_else() {
    // radon reads config while importing, outside its error handling: a
    // traceback and status 1, even for `--help`, and for keys of other
    // commands. pyproject.toml's other tools count too (`%` interpolation).
    let cases: [(&str, &[u8]); 4] = [
        ("radon.cfg", b"[radon]\nshow_complexity = maybe\n"),
        ("radon.cfg", b"[radon]\nmulti = maybe\n"),
        (
            "pyproject.toml",
            b"[tool.pytest.ini_options]\nlog_date_format = \"%Y-%m-%d\"\n",
        ),
        ("pyproject.toml", b"[tool.radon\n"),
    ];
    for (i, (path, data)) in cases.into_iter().enumerate() {
        let dir = config_project(&format!("config-broken-{i}"), &[(path, data)]);
        for args in [&["src"][..], &["--help"]] {
            let out = cc_in(&dir, args);
            assert_eq!(out.status.code(), Some(1), "{path} {args:?}");
            assert_eq!(out.stdout, b"", "{path} {args:?}");
        }
    }
}

#[test]
fn an_unusable_pyproject_is_ignored() {
    // Not UTF-8, or too long a decimal integer: radon drops the TOML config
    // and uses the other sources.
    let mut long = b"[tool.radon]\ncc_min = \"C\"\n[tool.x]\nn = ".to_vec();
    long.extend(std::iter::repeat_n(b'9', 4301));
    long.push(b'\n');
    let cases: [&[u8]; 2] = [b"[tool.radon]\ncc_min = \"C\"\n# \xff\n", &long];
    for (i, pyproject) in cases.into_iter().enumerate() {
        let dir = config_project(
            &format!("config-ignored-{i}"),
            &[
                ("pyproject.toml", pyproject),
                ("radon.cfg", b"[radon]\ncc_min = B\n"),
            ],
        );
        let out = cc_in(&dir, &["src"]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "src/f.py\n    F 12:0 c - C\n    F 4:0 b - B\n",
            "case {i}"
        );
    }
}
