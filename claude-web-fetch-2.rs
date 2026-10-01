#!/usr/bin/env rust-script
//! Fetch web pages through the Claude API's server-side web_fetch tool and
//! print Claude's write-up of each, always fresh (the server-side fetch cache
//! is bypassed), followed by what web_fetch retrieved. A write-up made
//! without retrieving any page is printed too, but counts as a failure.
//!
//! With --find, Claude is not involved: each page is fetched directly and its
//! HTML is searched for QUOTE. Matching ignores case, punctuation, whitespace,
//! tags and HTML/JSON escapes, and checks both the visible text and the full
//! source (scripts, JSON payloads, attributes). When the quote is absent, the
//! longest run of consecutive quote words that is present is reported.
//! Context snippets are shown normalised (lowercase, punctuation stripped).
//! Every report, for an HTTP error too, ends with the response's cache
//! metadata (Cache-Control, Age, CDN cache status and request ids, ...).
//!
//! Usage: claude-web-fetch-2.rs [--verbose] [--find QUOTE] URL [URL...]
//!
//! A failure is one line on stderr; --verbose adds the whole report, and
//! RUST_BACKTRACE=1 backtraces. Exit status: 0 on success (--find: QUOTE
//! found on every page), 1 if --find searched a whole page without finding
//! QUOTE, 2 if anything failed, 101 if a worker panicked.
//!
//! Environment (Claude mode; if set, a variable must be non-empty UTF-8):
//!   ANTHROPIC_API_KEY  API key; if unset, read from rageveil at $RAGEVEIL_KEY
//!   RAGEVEIL_KEY       rageveil entry (default: platform.claude.com/api/grim-monolith-key)
//!   MODEL              model id (default: claude-opus-5-5)
//!   PROMPT             instruction template; must contain {url}
//!
//! ```cargo
//! [dependencies]
//! offensive-rs = { git = "https://github.com/geosurge-ai/offensive-rs", features = ["ureq"] }
//! html-escape = "0.2"
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! url = "2"
//! ```

use offensive_rs::{
    Evidence, FAILED, Report, ResultExt, Secret, bail, batch, complain, env_or, env_secret,
    headline, http, report, secret_from,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::{Command, ExitCode};
use std::{fmt, num::NonZeroUsize, time::Duration};
use url::Url;

const USAGE: &str = "usage: claude-web-fetch-2.rs [--verbose] [--find QUOTE] URL [URL...]";
const DEFAULT_PROMPT: &str = "Fetch {url} and give me a thorough, faithful summary: \
    author, date, main sections, and the concrete practices, tools, and conclusions it describes.";
/// Targets in flight at once.
const JOBS: NonZeroUsize = NonZeroUsize::new(8).unwrap();
/// API requests per target: the first, then one for each pause_turn.
const MAX_REQUESTS: usize = 5;
/// Deadlines for a whole request, body included; an API call can take minutes.
const API_DEADLINE: Duration = Duration::from_secs(10 * 60);
const PAGE_DEADLINE: Duration = Duration::from_secs(2 * 60);
/// Only this much of a page is searched; absence from a longer page is unproven.
const MAX_PAGE_BYTES: usize = 10 << 20;
const CONTEXT_WORDS: usize = 12;

/// An http(s) URL, shown as it was given.
struct Target {
    given: String,
    url: Url,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.given)
    }
}

/// The --find quote as normalised words, if given, and the targets; neither empty.
fn parse_args(args: Vec<String>) -> Result<(Option<Vec<String>>, Vec<Target>), Report> {
    let (mut quote, mut targets) = (None, Vec::new());
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--find" => {
                let Some(text) = args.next() else { bail!("--find needs a QUOTE") };
                let words: Vec<_> = words(&text).split_whitespace().map(str::to_owned).collect();
                if words.is_empty() {
                    bail!("quote {text:?} contains no searchable words");
                }
                if quote.replace(words).is_some() {
                    bail!("--find given more than once");
                }
            }
            _ if arg.starts_with('-') => bail!("unknown option {arg}"),
            _ => match Url::parse(&arg).context_with(|| format!("target is not a URL: {arg}"))? {
                url if matches!(url.scheme(), "http" | "https") => {
                    targets.push(Target { given: arg, url })
                }
                _ => bail!("target is not an http(s) URL: {arg}"),
            },
        }
    }
    if targets.is_empty() {
        bail!("no URL given");
    }
    Ok((quote, targets))
}

fn main() -> ExitCode {
    offensive_rs::exit(run())
}

fn run() -> Result<u8, Report> {
    let (quote, targets) = parse_args(offensive_rs::start(USAGE)?)?;
    // Workers share nothing mutable (each builds its own HTTP agent), so a
    // panic costs only its own target.
    Ok(match &quote {
        Some(quote) => batch(&targets, JOBS, |target| find(target, quote), report_page),
        None => {
            let claude = Claude::from_env()?;
            batch(&targets, JOBS, |target| summarise(&claude, target), report_summary)
        }
    })
}

struct Claude {
    key: Secret<String>,
    model: String,
    /// Contains `{url}`: web_fetch only fetches URLs present in the conversation.
    prompt: String,
}

impl Claude {
    fn from_env() -> Result<Self, Report> {
        let model = env_or("MODEL", "claude-opus-5-5")?;
        let prompt = env_or("PROMPT", DEFAULT_PROMPT)?;
        if !prompt.contains("{url}") {
            bail!("PROMPT must contain {{url}}: {prompt:?}");
        }
        let key = match env_secret("ANTHROPIC_API_KEY")? {
            Some(key) => key,
            None => {
                let entry = env_or("RAGEVEIL_KEY", "platform.claude.com/api/grim-monolith-key")?;
                secret_from(Command::new("rageveil").args(["show", &entry]))?
            }
        };
        Ok(Self { key, model, prompt })
    }
}

#[derive(Deserialize)]
struct Reply {
    content: Vec<Value>,
    stop_reason: String,
    stop_details: Option<Value>,
}

/// The content blocks this script reads; the rest are passed back untouched.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    WebFetchToolResult {
        content: Fetch,
    },
    #[serde(other)]
    Other,
}

/// One web_fetch call's result. Server-tool errors arrive as HTTP 200 with an
/// error object in the result block.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Fetch {
    WebFetchResult { url: String, retrieved_at: Option<String> },
    WebFetchToolError { error_code: String },
}

/// One assistant turn, across pause_turn continuations.
#[derive(Debug, Default)]
struct Turn {
    text: String,
    fetches: Vec<Fetch>,
    stop_reason: String,
}

/// Ask Claude to summarise the target. `Ok` is a turn that has text.
fn summarise(claude: &Claude, target: &Target) -> Result<Turn, Report> {
    let agent = http::agent(API_DEADLINE);
    let prompt = claude.prompt.replace("{url}", target.url.as_str());
    // The assistant turn so far, as received (to send back) and as read.
    let (mut blocks, mut turn) = (Vec::<Value>::new(), Turn::default());
    for request in 1..=MAX_REQUESTS {
        let mut messages = vec![json!({"role": "user", "content": prompt})];
        if !blocks.is_empty() {
            messages.push(json!({"role": "assistant", "content": blocks}));
        }
        let body = json!({
            "model": claude.model,
            "max_tokens": 16000,
            "fallbacks": "default",
            // use_cache: false bypasses the server's fetch cache (needs web_fetch_20260309+).
            "tools": [{"type": "web_fetch_20260318", "name": "web_fetch", "max_uses": 5, "use_cache": false}],
            "messages": messages,
        });
        let sent = agent
            .post("https://api.anthropic.com/v1/messages")
            .set("x-api-key", claude.key.expose())
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "server-side-fallback-2026-07-01")
            .send_json(body);
        let reply: Reply =
            http::json(sent, 64 << 20).context_with(|| format!("API request {request}"))?;
        for block in &reply.content {
            match Block::deserialize(block).context("malformed block in API reply")? {
                Block::Text { text } => turn.text += &text,
                Block::WebFetchToolResult { content } => turn.fetches.push(content),
                Block::Other => {}
            }
        }
        blocks.extend(reply.content);
        turn.stop_reason = reply.stop_reason;
        let failure = match turn.stop_reason.as_str() {
            // Server tool loop hit its iteration limit; resend to let it continue.
            "pause_turn" => continue,
            "refusal" => report!("refused: {}", reply.stop_details.unwrap_or_default()),
            _ if turn.text.trim().is_empty() => {
                report!("no text returned; web_fetch: {:?}", turn.fetches)
            }
            _ => return Ok(turn),
        };
        return Err(failure.evidence(turn));
    }
    Err(report!("turn still paused after {MAX_REQUESTS} requests").evidence(turn))
}

fn report_summary(target: &Target, turn: Turn) -> u8 {
    let mut out = turn.text;
    if !matches!(turn.stop_reason.as_str(), "end_turn" | "stop_sequence") {
        out += &format!("\n\n[truncated: hit {}]", turn.stop_reason);
    }
    out += "\n\nweb_fetch:";
    let mut grounded = false;
    for fetch in &turn.fetches {
        out += &match fetch {
            Fetch::WebFetchResult { url, retrieved_at } => {
                grounded = true;
                format!("\n  fetched {url} at {}", retrieved_at.as_deref().unwrap_or("?"))
            }
            Fetch::WebFetchToolError { error_code } => format!("\n  error: {error_code}"),
        };
    }
    println!("# {target}\n\n{out}\n");
    if grounded {
        return 0;
    }
    eprintln!("error: {target}: Claude answered without retrieving any page");
    FAILED
}

/// A response to a direct fetch, error statuses included.
struct Page {
    /// Where the request ended up after redirects.
    url: String,
    cache: Vec<String>,
    verdict: Result<Verdict, Report>,
}

/// What a search established, with the passage that shows it.
enum Verdict {
    Found(String),
    Absent(String),
    /// Absent from the first MAX_PAGE_BYTES of a longer page.
    Unproven(String),
}

/// Fetch the target directly and search it for `quote`.
fn find(target: &Target, quote: &[String]) -> Result<Page, Report> {
    let sent = http::agent(PAGE_DEADLINE)
        .request_url("GET", &target.url)
        .set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) claude-web-fetch.rs")
        .set("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .set("Cache-Control", "no-cache")
        .call();
    let resp = http::arrived(sent)?;
    let (url, cache) = (resp.get_url().to_owned(), http::cache_headers(&resp));
    let body = http::body(resp, MAX_PAGE_BYTES);
    let verdict = body.map(|body| match search(&body.text(), quote) {
        Verdict::Absent(closest) if !body.is_complete() => Verdict::Unproven(closest),
        verdict => verdict,
    });
    Ok(Page { url, cache, verdict })
}

fn report_page(target: &Target, page: Page) -> u8 {
    // 1: absent from a page that was searched in full.
    let (status, mut out) = match &page.verdict {
        Ok(Verdict::Found(passage)) => (0, format!("FOUND {passage}")),
        Ok(Verdict::Absent(passage)) => (1, format!("NOT FOUND {passage}")),
        Ok(Verdict::Unproven(passage)) => (FAILED, format!("UNPROVEN, page cut short {passage}")),
        Err(failure) => (FAILED, format!("NOT SEARCHED: {}.", headline(failure))),
    };
    if page.url != target.url.as_str() {
        out += &format!("\n\nRedirected to {}.", page.url);
    }
    out += "\n\nCache metadata (response headers):";
    if page.cache.is_empty() {
        out += " none";
    }
    for line in &page.cache {
        out += &format!("\n  {line}");
    }
    println!("# {target}\n\n{out}\n");
    match page.verdict {
        Ok(Verdict::Unproven(_)) => eprintln!("error: {target}: quote not in what was searched"),
        Err(failure) => complain(&failure.context(target.to_string())),
        Ok(_) => {}
    }
    status
}

/// Search `html` for the quote: in the visible text, then in the full source.
fn search(html: &str, quote: &[String]) -> Verdict {
    let needle = format!(" {} ", quote.join(" "));
    // Escapes are decoded after the markup is stripped, so &lt; can't fabricate tags.
    let visible = words(&decode_escapes(&strip_markup(html)));
    let source = words(&decode_escapes(html));
    let haystacks = [
        ("visible page text", &visible),
        ("page source only (script, JSON or attribute; may not be displayed)", &source),
    ];
    for (label, hay) in haystacks {
        if let Some(pos) = hay.find(&needle) {
            return Verdict::Found(format!("in {label}:\n  {}", context(hay, pos, needle.len())));
        }
    }
    let runs = haystacks.iter().filter_map(|(_, hay)| longest_run(hay, quote));
    let closest = match runs.max_by_key(|run| run.0) {
        Some((len, at)) => format!("Longest run present: {len}/{} words:\n  {at}", quote.len()),
        None => "No two consecutive words of the quote appear together.".into(),
    };
    let scope = format!("({} bytes of HTML; searched visible text and full source).", html.len());
    Verdict::Absent(format!("{scope}\n  {closest}"))
}

/// Longest run (≥2 words, shorter than the full quote) of consecutive quote
/// words present in `hay`, with context.
fn longest_run(hay: &str, quote: &[String]) -> Option<(usize, String)> {
    (2..quote.len()).rev().flat_map(|len| quote.windows(len)).find_map(|run| {
        let phrase = format!(" {} ", run.join(" "));
        hay.find(&phrase).map(|pos| (run.len(), context(hay, pos, phrase.len())))
    })
}

fn context(hay: &str, pos: usize, len: usize) -> String {
    let mut before: Vec<_> = hay[..pos].split_whitespace().rev().take(CONTEXT_WORDS).collect();
    before.reverse();
    let after: Vec<_> = hay[pos + len..].split_whitespace().take(CONTEXT_WORDS).collect();
    format!("…{} >>{}<< {}…", before.join(" "), hay[pos..pos + len].trim(), after.join(" "))
}

/// Lowercase alphanumeric words, space-separated, with a leading and trailing
/// space so `" a b "` matches only on word boundaries.
fn words(s: &str) -> String {
    let words = s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty());
    words.fold(" ".to_owned(), |out, w| out + &w.to_lowercase() + " ")
}

/// Replace each tag with a space, and each script or style block along with its tags.
fn strip_markup(html: &str) -> String {
    // ASCII-lowercasing keeps byte offsets, so an index into `lower` is valid in `html`.
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut rest = 0;
    while let Some(open) = lower[rest..].find('<').map(|p| p + rest) {
        out.push_str(&html[rest..open]);
        out.push(' ');
        // A block runs to its closing tag, which ends, like any tag, at the next `>`.
        let block = ["script", "style"].iter().find(|name| lower[open + 1..].starts_with(*name));
        let end = match block {
            Some(name) => lower[open..].find(&format!("</{name}")).map(|p| p + open),
            None => Some(open),
        };
        rest = end.and_then(|end| Some(end + lower[end..].find('>')? + 1)).unwrap_or(html.len());
    }
    out + &html[rest..]
}

/// Decode JSON escapes (\u2019 \" \n ...), then HTML entities (&amp; &#8217; &#x27; ...).
fn decode_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('\\') {
        out.push_str(&rest[..pos]);
        let escape = &rest[pos + 1..];
        let code = escape.get(1..5).filter(|_| escape.starts_with('u'));
        let code = code.and_then(|hex| char::from_u32(u32::from_str_radix(hex, 16).ok()?));
        let (ch, len) = match code {
            Some(ch) => (ch, 6),
            // Any other escape is whitespace or punctuation: a word break either way.
            None if escape.starts_with(['n', 't', 'r', '"', '\'', '/', '\\']) => (' ', 2),
            None => (' ', 1),
        };
        out.push(ch);
        rest = &rest[pos + len..];
    }
    html_escape::decode_html_entities(&(out + rest)).into_owned()
}
