//! Support for offensive coding: a failure keeps its evidence, the reader
//! chooses how much of it to see, a bounded capture says when it is partial,
//! and a worker that breaks loses only its own item.
//!
//! Reports are [`rootcause`] reports, re-exported here. `?` converts any error
//! into a [`Report`], [`ResultExt::context`] adds a layer on top of it, and
//! every layer records where it was added (for a failure reported by this
//! crate, where it was called from). The original error and every attachment
//! stay typed, so an owner can get them back by downcasting.
//!
//! A report has two projections. [`headline`] is the one-line message for
//! routine output. The report's own `Display` is the whole tree: each layer
//! with its source location, its [`Evidence`], and its backtrace when
//! [`start`] saw `RUST_BACKTRACE`. [`complain`] prints the first, and the
//! second too for a reader who passed `--verbose`.
//!
//! A run that hangs or is killed returns no report. [`step`] therefore says on
//! stderr what is about to be waited for, and how it ended, once [`start`] has
//! run and unless the reader passed `--quiet`. The step's name is also the
//! layer a failure in it gets.
//!
//! ```
//! use offensive_rs::{headline, Captured, Evidence, Report, ResultExt};
//!
//! fn load(path: &str) -> Result<u16, Report> {
//!     let file = std::fs::File::open(path).context_with(|| format!("opening {path}"))?;
//!     let text = Captured::read(file, 64).context("reading port")?;
//!     let port = text.text().trim().parse();
//!     Ok(port.context("parsing port").map_err(|e| e.evidence(text))?)
//! }
//!
//! let failure = load("/nonexistent/port").unwrap_err();
//! assert!(headline(&failure).starts_with("opening /nonexistent/port: "));
//! println!("{failure}"); // every layer, location and attachment
//! ```

use rootcause::{
    IntoReportCollection, ReportRef,
    handlers::AttachmentHandler,
    hooks::Hooks,
    markers::{Dynamic, SendSync},
};
use rootcause_backtrace::BacktraceCollector;
use std::{
    any::Any,
    borrow::Cow,
    cell::RefCell,
    env,
    ffi::OsString,
    fmt,
    io::{self, Read, Write},
    mem,
    num::NonZeroUsize,
    process::{Command, ExitCode},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Instant,
};

pub use rootcause::{self, Report, bail, prelude::ResultExt, report};
#[cfg(feature = "ureq")]
pub use ureq;

/// Exit status of a run in which something failed.
pub const FAILED: u8 = 2;
/// Exit status of a run in which a worker panicked; Rust's own for a panic.
pub const DEFECT: u8 = 101;

static VERBOSE: AtomicBool = AtomicBool::new(false);
static PROGRESS: AtomicBool = AtomicBool::new(false);

/// Call first in `main`. Returns the command-line arguments after the program
/// name, minus the three flags handled here: `--verbose` makes [`complain`]
/// print whole reports, `--quiet` keeps each [`step`] from being announced
/// (from here on it is, otherwise), and `-h` or `--help` prints `usage` and
/// exits. (`std::env::args` panics on an argument that is not UTF-8; this
/// reports it.) When `RUST_BACKTRACE` is set (and not `0`), every report
/// layer captures a backtrace. Source locations are captured regardless.
pub fn start(usage: &str) -> Result<Vec<String>, Report> {
    if env::var_os("RUST_BACKTRACE").is_some_and(|value| value != "0") {
        // Err means the program installed its own rootcause hooks; those stand.
        let _ = Hooks::new().report_creation_hook(BacktraceCollector::new_from_env()).install();
    }
    let (flags, args) = split_flags(env::args_os().skip(1))?;
    if flags.contains(&"--help") {
        println!("{usage}");
        std::process::exit(0);
    }
    VERBOSE.store(flags.contains(&"--verbose"), Ordering::Relaxed);
    PROGRESS.store(!flags.contains(&"--quiet"), Ordering::Relaxed);
    Ok(args)
}

/// The flags [`start`] handles that are present, and the other arguments.
fn split_flags(
    args: impl Iterator<Item = OsString>,
) -> Result<(Vec<&'static str>, Vec<String>), Report> {
    let (mut flags, mut rest) = (Vec::new(), Vec::new());
    for arg in args {
        match arg.into_string() {
            Ok(arg) if arg == "--verbose" => flags.push("--verbose"),
            Ok(arg) if arg == "--quiet" => flags.push("--quiet"),
            Ok(arg) if arg == "--help" || arg == "-h" => flags.push("--help"),
            Ok(arg) => rest.push(arg),
            Err(arg) => bail!("argument is not UTF-8: {arg:?}"),
        }
    }
    Ok((flags, rest))
}

/// The one-line projection of a report: each layer's message from the
/// outermost down to the root cause and its `Error::source` chain, joined by
/// `: `. Independent failures under one layer are listed as `[a; b]`.
/// Attachments are left to the report's `Display`.
pub fn headline<C: ?Sized>(report: &Report<C>) -> String {
    describe(report.as_ref().into_dynamic())
}

fn describe<O>(node: ReportRef<'_, Dynamic, O, SendSync>) -> String {
    let mut line = node.format_current_context().to_string();
    let mut source = node.current_context_error_source();
    while let Some(cause) = source {
        append(&mut line, &cause.to_string());
        source = cause.source();
    }
    let children: Vec<String> = node.children().iter().map(describe).collect();
    match children.as_slice() {
        [] => {}
        [only] => append(&mut line, only),
        many => line = format!("{line}: [{}]", many.join("; ")),
    }
    line
}

/// Many errors print their cause, and many causes restate what they are about;
/// neither is said twice.
fn append(line: &mut String, cause: &str) {
    if cause.starts_with(&format!("{line}: ")) {
        *line = cause.to_owned();
    } else if line != cause && !line.ends_with(&format!(": {cause}")) {
        line.push_str(": ");
        line.push_str(cause);
    }
}

/// Prints `error: ` and the [`headline`] to stderr, then the whole report if
/// [`start`] saw `--verbose`.
pub fn complain<C: ?Sized>(failure: &Report<C>) {
    eprintln!("error: {}", headline(failure));
    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("{failure}");
    }
}

/// For `main`: the run's status as the exit code, or [`FAILED`] after
/// [`complain`]ing about why there was no run.
pub fn exit(status: Result<u8, Report>) -> ExitCode {
    ExitCode::from(status.unwrap_or_else(|failure| {
        complain(&failure);
        FAILED
    }))
}

thread_local! {
    /// What this thread's [`step`]s are part of: each heading it is [`under`],
    /// outermost first, each followed by `: `.
    static UNDER: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Runs `work` as a step that may block, saying so on stderr once [`start`]
/// has run without `--quiet`: `started: NAME` before it, then `finished in
/// 1.2s: NAME`, or `failed after 1.2s: ` and the failure's [`headline`]. A run
/// that hangs or is killed returns no report, and these lines are what is
/// left of it. The failure gets `name` as a layer, and steps inside this one
/// are reported [`under`] it. Never put a credential in a name.
#[track_caller]
pub fn step<T, E: IntoReportCollection<SendSync>>(
    name: impl fmt::Display,
    work: impl FnOnce() -> Result<T, E>,
) -> Result<T, Report> {
    let (outer, name) = (UNDER.with_borrow(String::clone), name.to_string());
    tell(format_args!("started: {outer}{name}"));
    // ponytail: says nothing while `work` waits; a ticker thread would show
    // that the run is still alive.
    let began = Instant::now();
    let result = under(&name, work).context_with(|| name.clone());
    let took = began.elapsed().as_secs_f64();
    match &result {
        Ok(_) => tell(format_args!("finished in {took:.1}s: {outer}{name}")),
        Err(failure) => tell(format_args!("failed after {took:.1}s: {outer}{}", headline(failure))),
    }
    Ok(result?)
}

/// Runs `work` with `heading` in front of the progress lines of the [`step`]s
/// it takes on this thread, as when several workers report at once. It prints
/// nothing itself, and leaves a failure as it is.
pub fn under<T>(heading: impl fmt::Display, work: impl FnOnce() -> T) -> T {
    struct Restore(String);
    impl Drop for Restore {
        fn drop(&mut self) {
            UNDER.set(mem::take(&mut self.0));
        }
    }
    let inner = format!("{}{heading}: ", UNDER.with_borrow(String::clone));
    let _outer = Restore(UNDER.replace(inner));
    work()
}

/// One line of progress on stderr, written whole so that a line from another
/// thread, or from stdout, cannot land inside it. A line that cannot be
/// written is dropped: a reader that went away does not fail the step.
fn tell(line: fmt::Arguments<'_>) {
    if PROGRESS.load(Ordering::Relaxed) {
        let _ = io::stderr().write_all(format!("{line}\n").as_bytes());
    }
}

/// Typed evidence on a report layer: any `Debug` value, shown pretty-printed in
/// the report's `Display`, left out of the [`headline`], and recoverable as
/// its own type with `downcast_inner`. Never attach a credential.
pub trait Evidence {
    #[must_use]
    fn evidence<E: fmt::Debug + Send + Sync + 'static>(self, value: E) -> Self;
}

impl<C: ?Sized> Evidence for Report<C> {
    fn evidence<E: fmt::Debug + Send + Sync + 'static>(self, value: E) -> Self {
        self.attach_custom::<Pretty, E>(value)
    }
}

struct Pretty;

impl<A: fmt::Debug> AttachmentHandler<A> for Pretty {
    fn display(value: &A, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{value:#?}")
    }

    fn debug(value: &A, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{value:#?}")
    }
}

/// Bytes read from a source up to a limit, recording whether more followed.
#[derive(Clone, PartialEq, Eq)]
pub enum Captured {
    Complete(Vec<u8>),
    /// More followed, or the read failed after these.
    Truncated(Vec<u8>),
}

impl Captured {
    /// `Debug` shows this much; `Display` and [`bytes`](Self::bytes) show all.
    const DEBUG_BYTES: usize = 512;

    /// Reads at most `limit` bytes. A failed read reports the `io::Error` with
    /// the bytes that did arrive as [`Evidence`], as `Captured::Truncated`.
    #[track_caller]
    pub fn read(source: impl Read, limit: usize) -> Result<Self, Report<io::Error>> {
        let mut bytes = Vec::new();
        match source.take((limit as u64).saturating_add(1)).read_to_end(&mut bytes) {
            Ok(_) if bytes.len() > limit => {
                bytes.truncate(limit);
                Ok(Self::Truncated(bytes))
            }
            Ok(_) => Ok(Self::Complete(bytes)),
            Err(cause) => Err(report!(cause).evidence(Self::Truncated(bytes))),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Complete(bytes) | Self::Truncated(bytes) => bytes,
        }
    }

    /// The bytes as text, lossily decoded.
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.bytes())
    }

    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }

    /// The first `limit` bytes, `Truncated` unless this is all of a complete capture.
    #[must_use]
    pub fn prefix(&self, limit: usize) -> Self {
        let bytes = self.bytes();
        if self.is_complete() && bytes.len() <= limit {
            self.clone()
        } else {
            Self::Truncated(bytes[..bytes.len().min(limit)].to_vec())
        }
    }
}

/// The [`text`](Captured::text); a truncated capture says so.
impl fmt::Display for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.text(), if self.is_complete() { "" } else { " [... truncated]" })
    }
}

impl fmt::Debug for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = if self.is_complete() { "Complete" } else { "Truncated" };
        let bytes = self.bytes();
        let shown = String::from_utf8_lossy(&bytes[..bytes.len().min(Self::DEBUG_BYTES)]);
        let more = if bytes.len() > Self::DEBUG_BYTES { "..." } else { "" };
        write!(f, "{name}({} bytes: {shown:?}{more})", bytes.len())
    }
}

/// A worker's panic, kept as a value. The panic hook has already printed it;
/// the owner decides what losing that item means.
pub struct Panicked(pub Box<dyn Any + Send>);

impl Panicked {
    /// The panic message, when the payload is a string.
    pub fn message(&self) -> Option<&str> {
        let payload = &self.0;
        payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
    }
}

impl fmt::Display for Panicked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "worker panicked: {}", self.message().unwrap_or("non-string payload"))
    }
}

impl fmt::Debug for Panicked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Runs `work` on every item, at most `jobs` at a time, yielding each item
/// with its result in input order. A panic costs only its own item, provided
/// `work` shares nothing its panic could leave broken.
pub fn scatter<'a, I: Sync, T: Send + 'a>(
    items: &'a [I],
    jobs: NonZeroUsize,
    work: impl Fn(&'a I) -> T + Sync + 'a,
) -> impl Iterator<Item = (&'a I, Result<T, Panicked>)> + 'a {
    // ponytail: fixed batches, so a slow item holds back the start of the next
    // batch; hand items to a pool of `jobs` workers if that idle time matters.
    items.chunks(jobs.get()).flat_map(move |batch| {
        let work = &work;
        let results: Vec<_> = thread::scope(|scope| {
            let workers: Vec<_> =
                batch.iter().map(|item| scope.spawn(move || work(item))).collect();
            workers.into_iter().map(|worker| worker.join().map_err(Panicked)).collect()
        });
        batch.iter().zip(results)
    })
}

/// [`scatter`] for a command-line run. Each success goes to `show`, in input
/// order, which reports it and returns its exit status. A failure is
/// [`complain`]ed about under its item's name and counts as [`FAILED`], a
/// panic as [`DEFECT`]. Returns the worst (highest) status, for the process to
/// exit with.
#[track_caller]
pub fn batch<I: fmt::Display + Sync, T: Send>(
    items: &[I],
    jobs: NonZeroUsize,
    work: impl Fn(&I) -> Result<T, Report> + Sync,
    mut show: impl FnMut(&I, T) -> u8,
) -> u8 {
    let mut worst = 0;
    for (item, result) in scatter(items, jobs, work) {
        worst = worst.max(match result {
            Ok(Ok(value)) => show(item, value),
            Ok(Err(failure)) => {
                complain(&failure.context(item.to_string()));
                FAILED
            }
            Err(panicked) => {
                eprintln!("error: {item}: defect: {panicked}");
                DEFECT
            }
        });
    }
    worst
}

/// An environment variable; `None` when unset. A value that is set but empty or
/// not UTF-8 is an error, never silently the caller's default.
#[track_caller]
pub fn env_var(name: &str) -> Result<Option<String>, Report> {
    checked(name, env::var_os(name), false)
}

/// [`env_var`], or `default` when the variable is unset.
#[track_caller]
pub fn env_or(name: &str, default: &str) -> Result<String, Report> {
    Ok(env_var(name)?.unwrap_or_else(|| default.to_owned()))
}

/// [`env_var`] for a credential: a rejected value stays out of the report.
#[track_caller]
pub fn env_secret(name: &str) -> Result<Option<Secret<String>>, Report> {
    Ok(checked(name, env::var_os(name), true)?.map(Secret::new))
}

#[track_caller]
fn checked(name: &str, raw: Option<OsString>, secret: bool) -> Result<Option<String>, Report> {
    match raw.map(OsString::into_string) {
        None => Ok(None),
        Some(Ok(value)) if !value.is_empty() => Ok(Some(value)),
        Some(Ok(_)) => bail!("{name} is set but empty"),
        Some(Err(_)) if secret => bail!("{name} is not UTF-8"),
        Some(Err(raw)) => bail!("{name} is not UTF-8: {raw:?}"),
    }
}

/// A credential from a command that prints it, such as a password manager:
/// the first line of its stdout. Running it is a [`step`], since it may wait
/// to be unlocked. Progress and a failure show the command line, a failure
/// its exit status and its stderr too, and neither its stdout.
#[track_caller]
pub fn secret_from(command: &mut Command) -> Result<Secret<String>, Report> {
    step(format!("running {command:?}"), || -> Result<_, Report> {
        let out = command.output()?;
        if !out.status.success() {
            bail!("{}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
        }
        let printed = String::from_utf8(out.stdout).ok();
        match printed.as_deref().and_then(|printed| printed.lines().next()) {
            Some(secret) if !secret.is_empty() => Ok(Secret::new(secret.to_owned())),
            _ => bail!("it printed no secret (an empty or non-UTF-8 first line)"),
        }
    })
}

/// A value that must not reach output: `Debug` is redacted and there is no
/// `Display`, so a type holding one can still derive `Debug`.
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// HTTP replies through [`ureq`]: every request has a deadline; an error
/// status is a reply to read, not an exception; bodies are read up to a limit;
/// a failure keeps what did arrive.
#[cfg(feature = "ureq")]
pub mod http {
    use crate::{Captured, Evidence, Report, ResultExt, bail, report};
    use serde::de::DeserializeOwned;
    use std::time::Duration;
    use ureq::{Agent, Error, Response, Transport};

    /// How much of an error body, or of a reply that would not decode, is kept.
    pub const EXCERPT_BYTES: usize = 4 << 10;

    /// Response headers that say how fresh a reply is and which cache served
    /// it, besides any whose name contains "cache" (cache-control,
    /// cache-status, cf-cache-status, x-cache, ...).
    const CACHE_HEADERS: &[&str] = &[
        "age",
        "date",
        "expires",
        "etag",
        "last-modified",
        "pragma",
        "vary",
        "via",
        "surrogate-control",
        "surrogate-key",
        "server-timing",
        "cf-ray",
        "x-nf-request-id",
        "x-vercel-id",
        "x-served-by",
        "x-timer",
        "x-varnish",
        "x-amz-cf-pop",
        "x-amz-cf-id",
    ];

    /// An agent whose every request, body included, must finish by `deadline`.
    pub fn agent(deadline: Duration) -> Agent {
        ureq::AgentBuilder::new().timeout(deadline).build()
    }

    /// The response's cache-related headers as `name: value` lines, in
    /// response order: the evidence of how fresh it is.
    pub fn cache_headers(resp: &Response) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for name in resp.headers_names() {
            let wanted = name.contains("cache") || CACHE_HEADERS.contains(&name.as_str());
            // A repeated header's values are all listed at its first occurrence.
            if wanted && !lines.iter().any(|line| line.starts_with(&format!("{name}: "))) {
                lines.extend(resp.all(&name).into_iter().map(|value| format!("{name}: {value}")));
            }
        }
        lines
    }

    /// The response, whatever its status; only a transport failure is an error.
    #[track_caller]
    pub fn arrived(result: Result<Response, Error>) -> Result<Response, Report<Transport>> {
        match result {
            Ok(resp) | Err(Error::Status(_, resp)) => Ok(resp),
            Err(Error::Transport(cause)) => Err(report!(cause)),
        }
    }

    /// The status line, with the request id when the server sent one.
    fn status(resp: &Response) -> String {
        let id = resp.header("request-id").or_else(|| resp.header("x-request-id"));
        let id = id.map(|id| format!(" (request-id {id})")).unwrap_or_default();
        format!("HTTP {} {}{id}", resp.status(), resp.status_text())
    }

    /// The body of a response, up to `limit` bytes, unless its status is an
    /// error. That failure is the status line and the start of the body: in
    /// the headline when it is JSON or plain text, where an API states what
    /// went wrong, and as [`Evidence`] otherwise (a page of markup, say).
    #[track_caller]
    pub fn body(resp: Response, limit: usize) -> Result<Captured, Report> {
        let status = status(&resp);
        if resp.status() < 400 {
            return Ok(Captured::read(resp.into_reader(), limit).context(status)?);
        }
        let kind = resp.header("content-type").unwrap_or_default();
        let stated = kind.starts_with("application/json") || kind.starts_with("text/plain");
        let body =
            Captured::read(resp.into_reader(), EXCERPT_BYTES).context_with(|| status.clone())?;
        Err(if stated {
            report!("{status}: {body}")
        } else {
            report!(status).evidence(body).into_dynamic()
        })
    }

    /// A reply decoded from JSON: its [`body`], complete within `limit` bytes.
    /// One that does not decode keeps its start as [`Evidence`].
    #[track_caller]
    pub fn json<T: DeserializeOwned>(
        result: Result<Response, Error>,
        limit: usize,
    ) -> Result<T, Report> {
        let resp = arrived(result)?;
        let status = status(&resp);
        let body = body(resp, limit)?;
        if !body.is_complete() {
            bail!("{status}: reply exceeds {limit} bytes");
        }
        let decoded = serde_json::from_slice(body.bytes())
            .context_with(|| format!("{status}: undecodable reply"));
        Ok(decoded.map_err(|failure| failure.evidence(body.prefix(EXCERPT_BYTES)))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootcause::report_collection::ReportCollection;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    #[test]
    fn headline_is_the_layers_and_evidence_stays_typed() {
        let failure = report!(io::Error::other("disk on fire"))
            .context("loading config")
            .evidence(vec![7u8, 9])
            .context("starting up");
        assert_eq!(headline(&failure), "starting up: loading config: disk on fire");

        let full = failure.to_string();
        assert!(full.contains("src/lib.rs") && full.contains("7,"), "{full}");

        let layer = failure.iter_reports().nth(1).unwrap();
        let kept = layer.attachments().iter().find_map(|a| a.downcast_inner::<Vec<u8>>());
        assert_eq!(kept, Some(&vec![7, 9]));
        assert!(
            failure.iter_reports().any(|n| n.downcast_current_context::<io::Error>().is_some())
        );
    }

    #[test]
    fn headline_follows_sources_without_repeating_them() {
        #[derive(Debug)]
        struct Outer(io::Error, bool);
        impl fmt::Display for Outer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                if self.1 { write!(f, "outer: {}", self.0) } else { write!(f, "outer") }
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        for prints_source in [false, true] {
            assert_eq!(
                headline(&report!(Outer(io::Error::other("inner"), prints_source))),
                "outer: inner"
            );
        }
        // Only a whole cause counts as already printed, not a matching tail.
        assert_eq!(headline(&report!(io::Error::other("ing")).context("parsing")), "parsing: ing");
        assert_eq!(headline(&report!("x.test: refused").context("x.test")), "x.test: refused");
    }

    #[test]
    fn headline_lists_independent_failures() {
        let mut both = ReportCollection::new();
        both.push(report!("write failed").into_cloneable());
        both.push(report!("close failed").into_cloneable());
        assert_eq!(headline(&both.context("saving")), "saving: [write failed; close failed]");
    }

    #[test]
    fn capture_marks_truncation_and_keeps_a_failed_read() {
        assert_eq!(Captured::read(&b"abc"[..], 3).unwrap(), Captured::Complete(b"abc".to_vec()));
        let cut = Captured::read(&b"abcd"[..], 3).unwrap();
        assert_eq!(cut, Captured::Truncated(b"abc".to_vec()));
        assert_eq!(cut.to_string(), "abc [... truncated]");
        assert_eq!(
            Captured::Complete(b"abc".to_vec()).prefix(2),
            Captured::Truncated(b"ab".to_vec())
        );

        struct Resets<'a>(&'a [u8]);
        impl Read for Resets<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() { Err(io::Error::other("reset")) } else { self.0.read(buf) }
            }
        }
        let failure = Captured::read(Resets(b"ab"), 9).unwrap_err();
        assert_eq!(failure.current_context().to_string(), "reset");
        let partial = failure.attachments().iter().find_map(|a| a.downcast_inner::<Captured>());
        assert_eq!(partial, Some(&Captured::Truncated(b"ab".to_vec())));
    }

    #[test]
    fn scatter_keeps_order_bounds_threads_and_contains_a_panic() {
        let (running, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let items: Vec<u32> = (0..7).collect();
        let results: Vec<_> = scatter(&items, NonZeroUsize::new(3).unwrap(), |&n| {
            peak.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(20));
            running.fetch_sub(1, Ordering::SeqCst);
            assert!(n != 4, "four is broken");
            n * 2
        })
        .collect();

        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(results.len(), items.len());
        for (expected, (item, result)) in items.iter().zip(results) {
            assert_eq!(item, expected);
            match result {
                Ok(doubled) => assert_eq!(doubled, item * 2),
                Err(panicked) => {
                    assert_eq!((*item, panicked.message()), (4, Some("four is broken")))
                }
            }
        }
    }

    #[test]
    fn batch_shows_successes_in_order_and_exits_with_the_worst_status() {
        let jobs = NonZeroUsize::new(2).unwrap();
        let work = |&n: &u32| match n {
            2 => bail!("two failed"),
            3 => panic!("three is broken"),
            n => Ok(n),
        };
        let mut shown = Vec::new();
        let mut show = |_: &u32, n: u32| {
            shown.push(n);
            u8::from(n == 1)
        };
        assert_eq!(batch(&[0, 1, 0], jobs, work, &mut show), 1);
        assert_eq!(batch(&[1, 2, 0], jobs, work, &mut show), FAILED);
        assert_eq!(batch(&[2, 3, 1], jobs, work, &mut show), DEFECT);
        assert_eq!(shown, [0, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn step_names_its_failure_and_is_under_the_headings_around_it_on_its_thread() {
        let under_now = || UNDER.with_borrow(String::clone);
        assert_eq!(step("parse", || "7".parse::<u8>()).unwrap(), 7);

        let failure = under("page 3", || {
            step("fetch", || {
                assert_eq!(under_now(), "page 3: fetch: ");
                thread::scope(|scope| scope.spawn(|| assert_eq!(under_now(), "")).join()).unwrap();
                step("read", || Err::<(), _>(io::Error::other("reset")))
            })
        })
        .unwrap_err();
        assert_eq!(headline(&failure), "fetch: read: reset");
        assert!(
            failure.iter_reports().any(|n| n.downcast_current_context::<io::Error>().is_some())
        );

        assert!(std::panic::catch_unwind(|| under::<()>("broken", || panic!("defect"))).is_err());
        assert_eq!(under_now(), "", "a heading outlived its work");
    }

    /// Runs itself in a child process, whose stderr is the progress to check.
    #[test]
    fn progress_is_a_line_before_and_after_each_step_unless_quiet() {
        const CHILD: &str = "OFFENSIVE_RS_PROGRESS_CHILD";
        if env::var_os(CHILD).is_some() {
            step("unseen", || "7".parse::<u8>()).unwrap();
            PROGRESS.store(true, Ordering::Relaxed);
            let _ = under("page 3", || step("read", || Err::<(), _>(io::Error::other("reset"))));
            step("parse", || "7".parse::<u8>()).unwrap();
            if cfg!(unix) {
                secret_from(Command::new("sh").args(["-c", "printf 'hunt%s\\n' er2"])).unwrap();
            }
            return;
        }
        let test = "tests::progress_is_a_line_before_and_after_each_step_unless_quiet";
        let child = Command::new(env::current_exe().unwrap())
            .args(["--exact", test])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let told = String::from_utf8(child.stderr).unwrap();
        assert!(child.status.success(), "{told}");
        let lines: Vec<&str> = told.lines().collect();
        let Some(&[started, failed, parsing, parsed]) = lines.get(..4) else { panic!("{told}") };
        assert_eq!((started, parsing), ("started: page 3: read", "started: parse"));
        // Between its two parts, a line that ends a step says how long it took.
        let ended = |line: &str, how, what| line.starts_with(how) && line.ends_with(what);
        assert!(ended(failed, "failed after ", "s: page 3: read: reset"), "{told}");
        assert!(ended(parsed, "finished in ", "s: parse"), "{told}");
        // Running a secret's command is a step: two more lines, without what it printed.
        assert_eq!(lines.len(), if cfg!(unix) { 6 } else { 4 }, "{told}");
        assert!(!told.contains("hunter2"), "{told}");
    }

    /// Runs itself in a child process, whose stderr nobody reads.
    #[test]
    #[cfg(unix)]
    fn a_step_does_not_fail_because_nobody_reads_its_progress() {
        use std::process::Stdio;
        const CHILD: &str = "OFFENSIVE_RS_UNHEARD_CHILD";
        if env::var_os(CHILD).is_some() {
            PROGRESS.store(true, Ordering::Relaxed);
            // Ends when the parent, having closed this stderr, closes this stdin.
            assert_eq!(step("wait", || io::read_to_string(io::stdin())).unwrap(), "");
            return;
        }
        let test = "tests::a_step_does_not_fail_because_nobody_reads_its_progress";
        let mut child = Command::new(env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stderr.take());
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success(), "the step failed for want of a reader");
    }

    #[test]
    #[cfg(unix)]
    fn arguments_and_environment_are_checked_and_a_rejected_secret_is_not_shown() {
        use std::os::unix::ffi::OsStringExt;
        let garbled = || Some(OsString::from_vec(b"hunter\xff".to_vec()));
        let args = ["--find", "--verbose", "x", "--quiet", "-h"].map(OsString::from);
        let (flags, rest) = split_flags(args.into_iter()).unwrap();
        assert_eq!(
            (flags, rest),
            (vec!["--verbose", "--quiet", "--help"], vec!["--find".to_owned(), "x".to_owned()])
        );
        assert!(
            headline(&split_flags(garbled().into_iter()).unwrap_err())
                .starts_with("argument is not UTF-8")
        );

        assert_eq!(checked("X", None, false).unwrap(), None);
        assert_eq!(checked("X", Some("v".into()), false).unwrap().as_deref(), Some("v"));
        assert_eq!(
            headline(&checked("X", Some("".into()), false).unwrap_err()),
            "X is set but empty"
        );
        assert!(headline(&checked("X", garbled(), false).unwrap_err()).contains("hunter"));
        assert_eq!(headline(&checked("X", garbled(), true).unwrap_err()), "X is not UTF-8");
    }

    #[test]
    #[cfg(unix)]
    fn a_secret_comes_from_a_command_and_never_from_its_failure() {
        let sh = |script| secret_from(Command::new("sh").args(["-c", script]));
        // printf, so that the command line itself (which is reported) lacks the secret.
        assert_eq!(sh("printf 'hunt%s\\nmore\\n' er2").unwrap().expose(), "hunter2");
        let failure = headline(&sh("printf 'hunt%s\\n' er2; echo locked >&2; exit 3").unwrap_err());
        assert!(
            failure.ends_with(": exit status: 3: locked") && !failure.contains("hunter2"),
            "{failure}"
        );
        assert!(headline(&sh("echo").unwrap_err()).contains("printed no secret"));
    }

    #[test]
    fn secret_is_redacted() {
        #[derive(Debug)]
        struct Config {
            #[allow(dead_code)]
            key: Secret<&'static str>,
        }
        let shown = format!("{:?}", Config { key: Secret::new("hunter2") });
        assert!(!shown.contains("hunter2") && shown.contains("redacted"), "{shown}");
    }

    #[test]
    #[cfg(feature = "ureq")]
    fn http_reads_an_error_status_as_a_reply_and_keeps_what_would_not_decode() {
        let reply = |head: &str, body: &str| -> ureq::Response {
            format!("HTTP/1.1 {head}\r\n\r\n{body}").parse().unwrap()
        };
        let ok: Vec<u8> = http::json(Ok(reply("200 OK", "[1, 2]")), 64).unwrap();
        assert_eq!(ok, [1, 2]);

        let stated = "400 Bad Request\r\nrequest-id: req_1\r\nContent-Type: application/json";
        let failure =
            http::json::<Vec<u8>>(Ok(reply(stated, r#"{"error":"no"}"#)), 64).unwrap_err();
        assert_eq!(
            headline(&failure),
            r#"HTTP 400 Bad Request (request-id req_1): {"error":"no"}"#
        );

        let page = reply("404 Not Found\r\nContent-Type: text/html", "<h1>gone</h1>");
        let failure = http::body(page, 64).unwrap_err();
        assert_eq!(headline(&failure), "HTTP 404 Not Found");
        let kept = failure.attachments().iter().find_map(|a| a.downcast_inner::<Captured>());
        assert_eq!(kept.map(Captured::text).as_deref(), Some("<h1>gone</h1>"));

        let failure = http::json::<Vec<u8>>(Ok(reply("200 OK", "[1, ")), 64).unwrap_err();
        assert!(headline(&failure).starts_with("HTTP 200 OK: undecodable reply: EOF"), "{failure}");
        let kept = failure.attachments().iter().find_map(|a| a.downcast_inner::<Captured>());
        assert_eq!(kept, Some(&Captured::Complete(b"[1, ".to_vec())));

        let failure = http::json::<Vec<u8>>(Ok(reply("200 OK", "[1, 2, 3]")), 4).unwrap_err();
        assert_eq!(headline(&failure), "HTTP 200 OK: reply exceeds 4 bytes");

        let cached =
            reply("200 OK\r\nAge: 7\r\nServer: x\r\nX-Cache: HIT\r\nX-Cache: MISS\r\nage: 9", "");
        assert_eq!(
            http::cache_headers(&cached),
            ["age: 7", "age: 9", "x-cache: HIT", "x-cache: MISS"]
        );
    }
}
