# offensive-rs

A small Rust library for offensive coding:

1. **Parse** foreign input into the types its consumers need. Reject what is
   unusable, with the evidence; never default it silently.
2. **Preserve** a failure together with what each layer knew about it. Show one
   line of that by default and all of it on request.
3. **Prevent** broken state from being used: contain what breaks and leave the
   decision to its owner.

Reports are [rootcause](https://docs.rs/rootcause) reports, re-exported. `?`
converts any error into a `Report`, `.context(…)` adds a layer, every layer
records where it was added, and the original error and its attachments stay
typed. This crate is what a program needs around that:

| Item | What it is for |
|---|---|
| `headline`, `complain`, `exit` | Two projections of one report: a single line for routine output, and the whole tree (layers, locations, evidence, backtraces) under `--verbose`. |
| `step`, `under` | Progress on stderr, on unless `--quiet`: a line before a step that may block and a line when it ends. The step's name is also the layer its failure gets. |
| `Evidence` | Attach any `Debug` value to a report layer. It stays typed and out of the headline. |
| `Captured` | A read with a limit that says when it is partial, and keeps what arrived before a failed read. |
| `scatter`, `batch`, `Panicked` | Work on many items, a bounded number at a time, results in input order. A panic costs its own item, not the run. |
| `start`, `env_var`, `env_or`, `env_secret` | Arguments and environment variables that are rejected when unusable (not UTF-8, set but empty) instead of panicking or falling back to a default. |
| `Secret`, `secret_from` | Credentials that cannot reach a report or a `Debug` dump, read from the environment or from a password manager's command. |
| `http` (feature `ureq`) | Requests with a deadline, an error status as a reply to read, bounded bodies, cache headers, and JSON that keeps what would not decode. |

## Using it

```toml
[dependencies]
offensive-rs = { git = "https://github.com/geosurge-ai/offensive-rs" }
# or, with offensive_rs::http and the ureq it is built on:
offensive-rs = { git = "https://github.com/geosurge-ai/offensive-rs", features = ["ureq"] }
```

```rust
use offensive_rs::{headline, Captured, Evidence, Report, ResultExt};

fn load(path: &str) -> Result<u16, Report> {
    let file = std::fs::File::open(path).context_with(|| format!("opening {path}"))?;
    let text = Captured::read(file, 64).context("reading port")?;
    let port = text.text().trim().parse();
    Ok(port.context("parsing port").map_err(|e| e.evidence(text))?)
}
```

`complain` prints an error headline, then the full report under `--verbose`.
For example, if a server closes a response early, the report keeps the HTTP
status and the bytes received before the failure. Set `RUST_BACKTRACE=1` to
have every layer carry a backtrace as well.

A run that hangs or is killed returns no report, so `step` says on stderr what
is about to be waited for, and then how it ended and how long it took:

```text
started: https://example.com/page: Claude — cache ON: API request 1
failed after 0.3s: https://example.com/page: Claude — cache ON: API request 1: HTTP 401 Unauthorized: …
```

A step is named once: the name starts its progress lines and is the layer a
failure in it gets. `under` puts a heading in front of the lines of the steps
inside it, which tells apart workers that report at the same time. `start`
turns the lines on, for whoever runs the program or reads its log, a person or
a model, unless it is given `--quiet`. A line that cannot be written (its
reader has gone) is dropped and the step carries on. A failure is on its
`failed` line as soon as it happens, and in `complain`'s `error:` line when
its owner reports it.

## The example

[`claude-web-fetch-2.rs`](claude-web-fetch-2.rs) audits each URL with three
independent checks, run in parallel:

1. **Direct HTTP:** an ordinary GET, reporting status, content type, a response
   preview or quote evidence, redirects, and cache headers.
2. **Claude, cache ON:** asks Claude what the URL returns, with
   `web_fetch.use_cache: true`.
3. **Claude, cache OFF:** asks the same question in a separate conversation,
   with `web_fetch.use_cache: false`.

Results are grouped under each URL in that order. All three checks finish
before the next URL starts. There is no URL worker pool or jobs setting. A
failure in one check does not suppress the others. Each request, and the
command that fetches the API key, is a `step`: its progress lines on stderr
name the URL and the check, so a slow or stuck check can be told from the
other two.

`--find QUOTE` applies to **all three checks**. Claude is asked whether the
quote occurs; the program also searches the actual text returned by
`web_fetch`, independently of Claude's answer. A quote echoed in the answer
does not establish a match. The direct HTTP body and the two Claude
conversations are never passed into one another.

Matching requires consecutive words, ignoring case, punctuation and whitespace.
Direct HTML is searched as stripped text and full source, decoding HTML/JSON
escapes. A direct source-only match may not be visible on the rendered page.
Claude's check searches the text extracted by its fetch tool; that can differ
from raw HTML. Unavailable text evidence is `UNPROVEN`. Direct searches stop
at 10 MiB; absence beyond that limit is also `UNPROVEN`.

Both Claude checks need `ANTHROPIC_API_KEY`, or the existing `rageveil`
fallback. `MODEL` defaults to `claude-opus-5-5`; `PROMPT` can replace the
description instruction and must contain `{url}`. The quote-check instruction
is appended when `--find` is present. A credential failure still leaves the
direct HTTP result available.

Cache ON permits a cached result; it does not guarantee a cache hit. Cache OFF
bypasses Anthropic's fetch cache, not every possible origin/CDN cache. See
[Anthropic's cache control documentation](https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-fetch-tool#cache-bypass).
The report shows the fetched URLs and retrieval timestamps when supplied.

Exit statuses: `0` for all checks succeeding (and all finding the quote when
requested), `1` for a missing quote, `2` for a failed or incomplete check, and
`101` for a panicked check. The highest status wins. Truncated Claude answers,
fetch errors, and answers without a fetch of the requested URL fail the check.

It is a [rust-script](https://rust-script.org) that depends on this
repository over git, so the file works wherever it is copied to:

```sh
./claude-web-fetch-2.rs --find 'a quote to look for' https://example.com/page
```

rust-script keeps a lockfile for each script, so the library stays at the
commit the script first built against. To move it, name the commit in the
script's manifest: `offensive-rs = { git = "…", rev = "…", features = ["ureq"] }`.
From a checkout the example also runs against the local library:

```sh
cargo run --features ureq --example claude-web-fetch -- --verbose https://example.com/page
```

## Left out on purpose

- **A failed cleanup after a failed operation.** rootcause's
  `ReportCollection` already keeps both failures under one layer, and
  `headline` prints them as `[a; b]`.
- **Retries and supervision.** Whether a failure may be retried, and from what
  state, belongs to the owner of that state. Nothing here guesses.
- **Argument parsing**, beyond the three flags `start` owns (`--verbose`,
  `--quiet` and `--help`).
- **A heartbeat.** A step says when it starts and when it ends, and nothing
  while it waits.

## Development

`cargo test --all-features` runs the library and auditor tests. The auditor
tests live in [`claude-web-fetch-2-tests.rs`](claude-web-fetch-2-tests.rs) and
use a local HTTP server, including a check that all three requests
arrive before any response is released; they make no live Claude calls.
`nix develop` (or direnv) provides cargo, rustc, rustfmt, clippy and rust-script.
