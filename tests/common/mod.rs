//! Running the `oxradon` binary from integration tests.

// Each test binary compiles its own copy and uses only some of it.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

/// The binary under test: the one named by `OXRADON_BIN` when that is set, otherwise the one
/// Cargo built. The release workflow sets it to the binary a wheel installed, so the whole suite
/// checks what users get. A set `OXRADON_BIN` must name an absolute path to an executable file;
/// anything else stops the run, with no fallback to Cargo's build.
pub fn binary() -> PathBuf {
    match std::env::var_os("OXRADON_BIN") {
        Some(path) => checked_binary(Path::new(&path)).unwrap_or_else(|e| panic!("{e}")),
        None => PathBuf::from(env!("CARGO_BIN_EXE_oxradon")),
    }
}

/// `path` if it is an absolute path to an executable file, else why not.
pub fn checked_binary(path: &Path) -> Result<PathBuf, String> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(format!("OXRADON_BIN is not an absolute path: {shown}"));
    }
    let meta = std::fs::metadata(path).map_err(|e| format!("OXRADON_BIN {shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("OXRADON_BIN is not a file: {shown}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(format!("OXRADON_BIN is not executable: {shown}"));
        }
    }
    Ok(path.to_path_buf())
}

/// `oxradon`, run from the crate root, with color off and no
/// `RADONFILESENCODING` or `PYCHARM_HOSTED` (which turns on the exit reset).
pub fn command() -> Command {
    let mut command = Command::new(binary());
    configure(&mut command);
    command
}

/// `sh -c script` in the same setting as [`command`], with `$OXRADON`
/// naming the binary: for `ulimit` and redirections.
pub fn shell(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(script).env("OXRADON", binary());
    configure(&mut command);
    command
}

fn configure(command: &mut Command) {
    command
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("COLOR", "no")
        .env_remove("RADONFILESENCODING")
        .env_remove("PYCHARM_HOSTED");
}

/// Run `command` with piped stdout and stderr, feeding `stdin`.
pub fn output(command: &mut Command, stdin: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("oxradon starts");
    let mut pipe = child.stdin.take().expect("stdin is piped");
    // oxradon reads stdin only when told to; otherwise it may finish and close the pipe before
    // this write, which then fails with a broken pipe. That is not a failure: the caller checks
    // the output. (A race seen on macOS CI, 2026-11-14 and 2026-11-15.)
    match pipe.write_all(stdin) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        written => written.expect("stdin accepts input"),
    }
    drop(pipe);
    child.wait_with_output().expect("oxradon finishes")
}

/// Run [`command`] with `args` and extra environment, feeding `stdin`.
pub fn oxradon<A: AsRef<OsStr>>(args: &[A], stdin: &[u8], env: &[(&str, &str)]) -> Output {
    output(command().args(args).envs(env.iter().copied()), stdin)
}

/// Like [`oxradon`], but requires success and returns stdout as text.
pub fn run<A: AsRef<OsStr>>(args: &[A], stdin: &[u8], env: &[(&str, &str)]) -> String {
    let out = oxradon(args, stdin, env);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "oxradon failed: {stderr}");
    String::from_utf8(out.stdout).expect("output is UTF-8")
}

/// [`command`] run in `dir`, where only the directory's own configuration
/// applies: no `RADONCFG`, and `dir/home` (created here) as the home.
pub fn command_in(dir: &Path) -> Command {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("scratch home");
    let mut command = command();
    command
        .current_dir(dir)
        .env("HOME", home)
        .env_remove("RADONCFG");
    command
}

/// A fresh scratch directory under the target directory.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The recording `tests/golden/name`.
pub fn golden(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The entries of the recorded table `tests/golden/<table>.json` whose value differs on this
/// system, by index: `<table>.linux.json` on glibc Linux, `<table>.musl.json` on musl Linux,
/// `<table>.macos-x86_64.json` on an Intel Mac, none on an Apple silicon Mac. `mi` calls the C
/// library's `sin` and `log`, whose last bit differs between the Apple silicon Mac the tables
/// were recorded on, an Intel Mac, glibc and musl, and radon's results differ with them.
pub fn libm_overlay(table: &str) -> serde_json::Map<String, Value> {
    let suffix = if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "macos-x86_64"
    } else {
        return serde_json::Map::new();
    };
    serde_json::from_str(&golden(&format!("{table}.{suffix}.json"))).expect("overlay")
}

/// Whether any of `files` (relative to the crate root) is a directory, which radon walks.
pub fn walks(files: &[String]) -> bool {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    files.iter().any(|file| root.join(file).is_dir())
}

/// Output over a walked directory, in a form that does not depend on the order of the walk.
/// radon lists a directory's files in the order the system returns them, and so does oxradon;
/// the recordings have macOS's order. On macOS this returns `text` unchanged, so the order is
/// checked there. Elsewhere it sorts the per-file blocks (a block starts at each line that is
/// not indented), or, for JSON, the keys.
pub fn walk_order_free(text: &str) -> String {
    if cfg!(target_os = "macos") {
        return text.to_owned();
    }
    if text.starts_with('{') {
        let value: Value = serde_json::from_str(text).expect("JSON output");
        return sorted_keys(&value).to_string();
    }
    let mut blocks: Vec<String> = Vec::new();
    for line in text.split_inclusive('\n') {
        match blocks.last_mut() {
            Some(block) if line.starts_with(' ') => block.push_str(line),
            _ => blocks.push(line.to_owned()),
        }
    }
    blocks.sort();
    blocks.concat()
}

/// `value` with every object's keys in sorted order.
fn sorted_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), sorted_keys(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_keys).collect()),
        other => other.clone(),
    }
}

/// A JSON array of strings.
pub fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|s| s.as_str().expect("string").to_owned())
        .collect()
}

/// Make `file` unreadable (mode 000) and say whether it is. When it can still be read, because
/// the tests run as root or on a platform without these permissions, this prints a note to
/// stderr and returns `false`: the calling test should return early, and the note keeps that
/// skipped pass from being silent.
pub fn make_unreadable(file: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    }
    if std::fs::read(file).is_err() {
        return true;
    }
    let test = std::thread::current();
    eprintln!(
        "SKIPPED {}: {} is still readable (running as root?)",
        test.name().unwrap_or("a test"),
        file.display()
    );
    false
}

/// Replay the `config` cases recorded from radon in a manifest: each is a scratch project
/// (`files`), run from there with `HOME` there too and no `RADONCFG`, comparing stdout and, for
/// a `read` case, the `-O` file. `expected_file` turns the recorded file's text into what
/// oxradon should have written. Each project is walked, so off macOS the comparison does not
/// depend on the order of the files ([`walk_order_free`]).
pub fn replay_config(config: &Value, prefix: &str, expected_file: fn(&str) -> String) {
    let cases = config.as_object().expect("config cases");
    assert!(!cases.is_empty());
    for (name, case) in cases {
        let dir = scratch(&format!("{prefix}-{name}"));
        for (rel, text) in case["files"].as_object().expect("files") {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(&path, text.as_str().expect("text")).expect("write");
        }
        let mut command = command();
        // As recorded: no `RADONCFG`, `HOME` in the project.
        command
            .current_dir(&dir)
            .env("HOME", &dir)
            .env_remove("RADONCFG")
            .args(strings(&case["args"]));
        let out = output(&mut command, b"");
        let expected = &case["expected"];
        assert_eq!(
            walk_order_free(&String::from_utf8_lossy(&out.stdout)),
            walk_order_free(expected["stdout"].as_str().expect("stdout")),
            "case `{name}`"
        );
        if let Some(read) = case["read"].as_str() {
            let file = std::fs::read_to_string(dir.join(read)).expect("output file");
            let radon = expected_file(expected["file"].as_str().expect("file"));
            assert_eq!(
                walk_order_free(&file),
                walk_order_free(&radon),
                "case `{name}`"
            );
        }
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
