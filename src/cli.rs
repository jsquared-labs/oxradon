//! The `oxradon` command line, mirroring radon's options. It is public only
//! for the `oxradon` binary; it is not part of the library's API.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use rayon::prelude::*;

use crate::cc::{self, Order};
use crate::cc_report;
use crate::colorama;
use crate::config::{self, Config};
use crate::discover::{self, Filters};
use crate::error::Error;
use crate::hal::{self, Halstead};
use crate::hal_report;
use crate::mi::{self, Mi};
use crate::mi_report;
use crate::raw::{self, Module};
use crate::raw_report;
use crate::report::{self, FileReport, Outcome, RankFilter, Sink, Stream, WriteFailure};
use crate::source::{self, Encoding, errno_error, os_error};

/// Parsing and visiting recurse once per nesting level, so all work runs on
/// pool threads with stacks this large.
const WORKER_STACK_BYTES: usize = 64 << 20;

#[derive(Parser)]
#[command(
    name = "oxradon",
    version,
    about = "A fast, drop-in Rust port of radon",
    // Like radon's argparse CLI, a repeated flag is accepted; the last wins.
    args_override_self = true,
    // argparse's `allow_abbrev`: an unambiguous prefix of a long option
    // (`--fun`, `--out=x`) is that option; an ambiguous one is a usage error.
    infer_long_args = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Analyze the given Python modules and compute Cyclomatic Complexity (CC).
    Cc(CcArgs),
    /// Analyze the given Python modules and compute their Halstead metrics.
    Hal(HalArgs),
    /// Analyze the given Python modules and compute raw metrics.
    #[command(mut_arg("json", |arg| arg.help("Format results in JSON (without the summary)")))]
    Raw(RawArgs),
    /// Analyze the given Python modules and compute the Maintainability Index.
    Mi(MiArgs),
}

/// The options every command has. Those without a value here may come from
/// radon's configuration files ([`CommonArgs::run_options`]).
#[derive(Args)]
struct CommonArgs {
    /// The paths where to find modules or packages to analyze.
    #[arg(required = true)]
    paths: Vec<OsString>,
    /// Exclude files whose path matches one of these comma-separated glob
    /// patterns.
    #[arg(short = 'e', long)]
    exclude: Option<String>,
    /// Ignore directories whose name matches one of these comma-separated glob
    /// patterns.
    #[arg(short = 'i', long)]
    ignore: Option<String>,
    /// Format results in JSON.
    #[arg(short = 'j', long)]
    json: bool,
    /// The output file (default to stdout).
    #[arg(short = 'O', long)]
    output_file: Option<OsString>,
    // radon's notebook options. Accepted and ignored: radon without
    // `nbformat` skips notebooks either way, as oxradon does.
    /// Include IPython Notebook files (accepted; notebooks are skipped).
    #[arg(long = "include-ipynb")]
    _include_ipynb: bool,
    /// Include reports for individual IPYNB cells (accepted; ignored).
    #[arg(long = "ipynb-cells")]
    _ipynb_cells: bool,
}

impl CommonArgs {
    /// What the command reads and where it reports. A configured value is the
    /// option's default, and a flag replaces it.
    fn run_options(&self, config: &Config) -> RunOptions<'_> {
        RunOptions {
            paths: &self.paths,
            exclude: or_configured(self.exclude.as_deref(), config.exclude.as_deref()),
            ignore: or_configured(self.ignore.as_deref(), config.ignore.as_deref()),
            output_file: (self.output_file.clone())
                .or_else(|| config.output_file.clone().map(OsString::from)),
            json: self.json,
        }
    }
}

/// An option's value: the one given, else the configured one.
fn or_configured(given: Option<&str>, configured: Option<&str>) -> Option<String> {
    given.or(configured).map(str::to_owned)
}

/// The `cc` options as given. `min`, `max` and `order` stay strings rather
/// than clap enums: radon compares ranks as strings and treats an unknown
/// order as `SCORE`. Options without a value here may come from radon's
/// configuration files ([`CcSettings`]).
#[derive(Args)]
#[allow(clippy::struct_excessive_bools)] // one bool per radon flag
struct CcArgs {
    /// The minimum complexity to display (default A).
    #[arg(short = 'n', long)]
    min: Option<String>,
    /// The maximum complexity to display (default F).
    #[arg(short = 'x', long)]
    max: Option<String>,
    /// Show the actual complexity score together with the A-F rank.
    #[arg(short = 's', long)]
    show_complexity: bool,
    /// Display the average complexity at the end of the analysis.
    #[arg(short = 'a', long)]
    average: bool,
    /// Like --average, but not influenced by --min and --max.
    #[arg(long)]
    total_average: bool,
    /// The ordering function: SCORE, LINES or ALPHA (default SCORE).
    #[arg(short = 'o', long)]
    order: Option<String>,
    /// Do not count assert statements when computing complexity.
    #[arg(long)]
    no_assert: bool,
    /// Add closures/inner classes to the output.
    #[arg(long)]
    show_closures: bool,
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    formats: UnsupportedFormats,
}

/// `cc`'s other output formats, not supported yet. They are declared so that
/// a long-option prefix is ambiguous exactly when it is for radon (`--m` could
/// be `--min`, `--max` or `--md`).
#[derive(Args)]
#[allow(clippy::struct_excessive_bools)] // one bool per radon flag
struct UnsupportedFormats {
    #[arg(long, hide = true)]
    xml: bool,
    #[arg(long, hide = true)]
    md: bool,
    #[arg(long, hide = true)]
    codeclimate: bool,
}

impl UnsupportedFormats {
    /// The first format given, if any.
    fn given(&self) -> Option<&'static str> {
        [
            ("--xml", self.xml),
            ("--md", self.md),
            ("--codeclimate", self.codeclimate),
        ]
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag))
    }
}

/// The `hal` options as given.
#[derive(Args)]
struct HalArgs {
    /// Analyze files by top-level functions instead of as a whole.
    #[arg(short = 'f', long)]
    functions: bool,
    #[command(flatten)]
    common: CommonArgs,
}

/// The `raw` options as given (radon's full set).
#[derive(Args)]
struct RawArgs {
    /// Display the summary of the gathered metrics at the end of the analysis.
    #[arg(short = 's', long)]
    summary: bool,
    #[command(flatten)]
    common: CommonArgs,
}

/// The `mi` options as given (radon's full set). `min` and `max` stay
/// strings: radon compares ranks as strings.
#[derive(Args)]
struct MiArgs {
    /// The minimum MI to display (default A).
    #[arg(short = 'n', long)]
    min: Option<String>,
    /// The maximum MI to display (default C).
    #[arg(short = 'x', long)]
    max: Option<String>,
    /// Do not count multiline strings as comments.
    #[arg(short = 'm', long)]
    multi: bool,
    /// Show the actual MI value in results.
    #[arg(short = 's', long)]
    show: bool,
    /// Sort results in ascending order.
    #[arg(long)]
    sort: bool,
    #[command(flatten)]
    common: CommonArgs,
}

/// `cc`'s own options once radon's configuration files have supplied
/// defaults.
#[allow(clippy::struct_excessive_bools)] // one bool per radon flag
struct CcSettings {
    min: String,
    max: String,
    order: String,
    show_complexity: bool,
    average: bool,
    total_average: bool,
    no_assert: bool,
    show_closures: bool,
}

impl CcSettings {
    /// A configured value is the option's default, and a flag replaces it.
    /// A configured boolean decides the flag's action too: mando declares
    /// `store_false` for a `True` default, so the flag then turns it off.
    fn new(args: &CcArgs, config: &Config) -> Self {
        let pick = |given, configured, default: &str| {
            or_configured(given, configured).unwrap_or_else(|| default.to_owned())
        };
        Self {
            min: pick(args.min.as_deref(), config.min.as_deref(), "A"),
            max: pick(args.max.as_deref(), config.max.as_deref(), "F"),
            order: pick(args.order.as_deref(), config.order.as_deref(), "SCORE"),
            show_complexity: args.show_complexity != config.show_complexity,
            average: args.average != config.average,
            total_average: args.total_average != config.total_average,
            no_assert: args.no_assert != config.no_assert,
            show_closures: args.show_closures != config.show_closures,
        }
    }
}

/// CPython's exit status when it cannot flush stdout or stderr at exit,
/// which is what radon ends with after any failed write to either.
const FLUSH_FAILED: u8 = 120;

/// Run the command line and return the process exit code.
///
/// Configuration is read on the calling thread, which wants a main thread's
/// stack (a deeply nested `pyproject.toml` is parsed recursively, to a bounded
/// depth); the analysis runs on a pool of threads with larger stacks.
pub fn main() -> ExitCode {
    ignore_file_size_signal();
    let setup = StdoutSetup::detect();
    let code = run(setup);
    if colorama::exit_reset(setup.is_terminal).is_err() {
        // CPython's final flush of the reset fails, whatever the status was.
        return ExitCode::from(FLUSH_FAILED);
    }
    code
}

/// CPython ignores SIGXFSZ, so a write past `ulimit -f` fails with `EFBIG`
/// (and radon reports it) instead of killing the process.
fn ignore_file_size_signal() {
    #[cfg(unix)]
    {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        // A handler that only sets a flag: the write then fails instead.
        let _ = signal_hook::flag::register(
            signal_hook::consts::SIGXFSZ,
            Arc::new(AtomicBool::new(false)),
        );
    }
}

/// radon's stdout: whether it is a terminal, and whether it gets color
/// (otherwise colorama strips every write).
#[derive(Clone, Copy)]
struct StdoutSetup {
    is_terminal: bool,
    color: bool,
}

impl StdoutSetup {
    fn detect() -> Self {
        let is_terminal = io::stdout().is_terminal();
        Self {
            is_terminal,
            color: report::color_from_env(is_terminal),
        }
    }

    /// A sink for the lines radon writes to `sys.stdout`.
    fn sink<W: Write>(self, out: W) -> Sink<W> {
        Sink::stdout(out, self.color)
    }
}

fn run(setup: StdoutSetup) -> ExitCode {
    // A configuration error ends radon with a traceback and status 1.
    // radon reads its configuration while importing, before parsing arguments.
    let config = match config::load() {
        Ok(config) => config,
        Err(message) => {
            return match writeln!(io::stderr(), "oxradon: {message}") {
                Ok(()) => ExitCode::FAILURE,
                Err(_) => ExitCode::from(FLUSH_FAILED),
            };
        }
    };
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Usage errors, `--help` and `--version`: clap prints them, and the
        // exit reset must still run, so clap may not exit by itself.
        Err(e) => {
            // Help and version go to stdout, usage errors to stderr. If
            // either write fails, CPython's final flush of that stream fails
            // too, which overrides argparse's status.
            if e.print().is_err() {
                return ExitCode::from(FLUSH_FAILED);
            }
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let result = rayon::ThreadPoolBuilder::new()
        .stack_size(WORKER_STACK_BYTES)
        .build()
        .map_err(|e| Failure::Other(io::Error::other(e)))
        .and_then(|pool| {
            pool.install(|| match &cli.command {
                Command::Cc(args) => run_cc(args, &config, setup),
                Command::Hal(args) => run_hal(args, &config, setup),
                Command::Raw(args) => run_raw(args, &config, setup),
                Command::Mi(args) => run_mi(args, &config, setup),
            })
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Write(WriteFailure {
            stream: Stream::Stdout,
            ..
        })) => ExitCode::from(FLUSH_FAILED),
        // radon's `main` catches the OSError and prints it on stdout.
        Err(Failure::Write(WriteFailure {
            stream: Stream::File,
            error,
        })) => match report::write_abort(&mut setup.sink(io::stdout()), &errno_error(&error)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(FLUSH_FAILED),
        },
        Err(Failure::Unsupported(flag)) => {
            match writeln!(io::stderr(), "oxradon: {flag} is not supported yet") {
                Ok(()) => ExitCode::from(2),
                Err(_) => ExitCode::from(FLUSH_FAILED),
            }
        }
        Err(Failure::Other(e)) => {
            // `eprintln!` would panic if stderr is gone.
            let _ = writeln!(io::stderr(), "oxradon: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Why a run stopped early.
enum Failure {
    /// A write to stdout or the `-O` file failed.
    Write(WriteFailure),
    /// A failure radon has no counterpart for (an over-large pattern, or no
    /// thread pool).
    Other(io::Error),
    /// A radon option oxradon does not implement yet: a usage error.
    Unsupported(&'static str),
}

impl From<WriteFailure> for Failure {
    fn from(failure: WriteFailure) -> Self {
        Self::Write(failure)
    }
}

fn run_cc(args: &CcArgs, config: &Config, setup: StdoutSetup) -> Result<(), Failure> {
    if let Some(flag) = args.formats.given() {
        return Err(Failure::Unsupported(flag));
    }
    let settings = CcSettings::new(args, config);
    let options = cc::Options {
        no_assert: settings.no_assert,
        show_closures: settings.show_closures,
        order: Order::from_radon(&settings.order),
    };
    let filter = RankFilter::new(&settings.min, &settings.max);
    run_command(
        &args.common.run_options(config),
        setup,
        |text: &str| cc::analyze(text, &options),
        |out, reports| cc_report::write_json(out, reports, &filter),
        |out, stdout, reports, aborted| {
            let terminal = cc_report::TerminalOptions {
                show_complexity: settings.show_complexity,
                average: settings.average,
                total_average: settings.total_average,
            };
            cc_report::write_terminal(out, stdout, reports, &filter, terminal, aborted)
        },
    )
}

/// `hal`. Its one option of its own takes its default from configuration
/// as `cc`'s do ([`CcSettings::new`]).
fn run_hal(args: &HalArgs, config: &Config, setup: StdoutSetup) -> Result<(), Failure> {
    // mando's `store_false` for a configured `True` (see `CcSettings::new`).
    let by_function = args.functions != config.functions;
    run_command(
        &args.common.run_options(config),
        setup,
        hal::analyze,
        |out, reports: &[FileReport<Halstead>]| hal_report::write_json(out, reports),
        // An input that ends the run takes nothing away from `hal`'s listing.
        |out, stdout, reports, _aborted| {
            hal_report::write_terminal(out, stdout, reports, by_function)
        },
    )
}

/// `raw`. Only the options every command shares can be configured
/// ([`CommonArgs::run_options`]).
fn run_raw(args: &RawArgs, config: &Config, setup: StdoutSetup) -> Result<(), Failure> {
    run_command(
        &args.common.run_options(config),
        setup,
        raw::analyze,
        |out, reports: &[FileReport<Module>]| raw_report::write_json(out, reports),
        |out, stdout, reports, aborted| {
            raw_report::write_terminal(out, stdout, reports, args.summary, aborted)
        },
    )
}

/// `mi`. Its own options take their defaults from configuration as `cc`'s
/// do ([`CcSettings::new`]). radon's `multi` defaults to `True`, so
/// unconfigured, `-m` turns it off.
fn run_mi(args: &MiArgs, config: &Config, setup: StdoutSetup) -> Result<(), Failure> {
    let min =
        or_configured(args.min.as_deref(), config.mi_min.as_deref()).unwrap_or_else(|| "A".into());
    let max =
        or_configured(args.max.as_deref(), config.mi_max.as_deref()).unwrap_or_else(|| "C".into());
    let filter = RankFilter::new(&min, &max);
    let options = mi::Options {
        count_multi: args.multi != config.multi.unwrap_or(true),
    };
    let show = args.show != config.show_mi;
    run_command(
        &args.common.run_options(config),
        setup,
        |text: &str| mi::analyze(text, &options),
        |out, reports: &[FileReport<Mi>]| mi_report::write_json(out, reports, &filter),
        |out, stdout, reports, aborted| {
            mi_report::write_terminal(out, stdout, reports, &filter, show, args.sort, aborted)
        },
    )
}

/// The part of a run every command shares: what it reads, where it reports
/// and in which form, once configuration has supplied defaults.
struct RunOptions<'a> {
    paths: &'a [OsString],
    exclude: Option<String>,
    ignore: Option<String>,
    output_file: Option<OsString>,
    json: bool,
}

/// Where the report is written: the `-O` file or stdout, buffered.
type ReportWriter = BufWriter<Box<dyn Write>>;

/// A run as radon's harvesters make it: open the `-O` file (failing ends the
/// run), find and analyze the inputs, report those before any input that
/// could not be opened, then print that input's error.
///
/// The report is `write_json`'s with `-j`, and otherwise `write_terminal`'s,
/// which is told whether an input ended the run. radon builds its JSON after
/// reading every file, so then such an input leaves no JSON at all.
fn run_command<T: Send>(
    options: &RunOptions<'_>,
    setup: StdoutSetup,
    analysis: impl Fn(&str) -> Result<T, Error> + Sync,
    write_json: impl FnOnce(&mut ReportWriter, &[FileReport<T>]) -> io::Result<()>,
    write_terminal: impl FnOnce(
        &mut Sink<ReportWriter>,
        &mut Sink<io::Stdout>,
        &[FileReport<T>],
        bool,
    ) -> Result<(), WriteFailure>,
) -> Result<(), Failure> {
    let filters = Filters::new(options.exclude.as_deref(), options.ignore.as_deref())
        .map_err(|e| Failure::Other(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
    let mut stdout = setup.sink(io::stdout());
    // radon opens the `-O` file before looking at any input.
    let mut out = match open_output(options.output_file.as_deref(), setup) {
        Ok(out) => out,
        Err(message) => return Ok(report::write_abort(&mut stdout, &message)?),
    };
    let names = discover::python_files(options.paths, &filters);
    let reports = analyze_all(names, &Encoding::from_env(), analysis);
    let (reports, abort) = report::split_at_abort(&reports);
    if !options.json {
        write_terminal(&mut out, &mut stdout, reports, abort.is_some())?;
    } else if abort.is_none() {
        out.raw(|w| write_json(w, reports))?;
    }
    out.flush()?;
    if let Some(message) = abort {
        report::write_abort(&mut stdout, message)?;
    }
    Ok(())
}

/// Analyze `names` in radon's order, up to and including the first input
/// that ends radon's run.
///
/// Files are analyzed in parallel. Names that read stdin wait, so stdin is
/// read in order, by one thread, as radon reads it, and never after an
/// abort. Every input is read before any output is written.
fn analyze_all<T: Send>(
    names: Vec<PathBuf>,
    encoding: &Encoding,
    analysis: impl Fn(&str) -> Result<T, Error> + Sync,
) -> Vec<FileReport<T>> {
    let analyzed: Vec<Option<Option<Outcome<T>>>> = names
        .par_iter()
        .map(|name| (!source::is_stdin(name)).then(|| analyze(name, encoding, &analysis)))
        .collect();
    let mut reports = Vec::with_capacity(names.len());
    for (name, outcome) in names.into_iter().zip(analyzed) {
        let Some(outcome) = outcome.unwrap_or_else(|| analyze(&name, encoding, &analysis)) else {
            continue; // a notebook
        };
        let aborted = matches!(outcome, Outcome::Aborted(_));
        reports.push(FileReport {
            name: name.into_os_string(),
            outcome,
        });
        if aborted {
            break;
        }
    }
    reports
}

/// Open, read and analyze one input, sorting failures the way radon does.
///
/// A name ending in `.ipynb` (found through a shebang, or named) is opened,
/// so failing to open it still ends the run, and then skipped: radon without
/// `nbformat` reads no notebook (`Harvester.run`). That is `None`.
fn analyze<T>(
    name: &Path,
    encoding: &Encoding,
    analysis: &impl Fn(&str) -> Result<T, Error>,
) -> Option<Outcome<T>> {
    let opened = match source::open(name, encoding) {
        Ok(opened) => opened,
        Err(message) => return Some(Outcome::Aborted(message)),
    };
    if name.as_os_str().as_encoded_bytes().ends_with(b".ipynb") {
        return None;
    }
    let analyzed =
        (opened.read_text()).and_then(|text| analysis(&text).map_err(Error::into_message));
    Some(match analyzed {
        Ok(result) => Outcome::Analyzed(result),
        Err(message) => Outcome::Failed(message),
    })
}

/// The report's destination: stdout, or the `-O` file (an empty path means
/// stdout, as in radon, where `''` is falsy).
///
/// radon leaks color codes into `-O` files; oxradon treats a file as a
/// non-terminal, so it is colored only with `COLOR=yes`.
///
/// # Errors
/// Returns radon's message for a file it cannot create, which ends its run.
fn open_output(path: Option<&OsStr>, setup: StdoutSetup) -> Result<Sink<ReportWriter>, String> {
    if let Some(path) = path.filter(|p| !p.is_empty()) {
        let file = File::create(path).map_err(|e| os_error(&e, Path::new(path)))?;
        let file: Box<dyn Write> = Box::new(file);
        return Ok(Sink::file(
            BufWriter::new(file),
            report::color_from_env(false),
        ));
    }
    let lock: Box<dyn Write> = Box::new(io::stdout().lock());
    Ok(setup.sink(BufWriter::new(lock)))
}
