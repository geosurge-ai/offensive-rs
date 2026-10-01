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

This is what `complain` prints for a failure of the example below: the
headline, and after it (because of `--verbose`) the report it was projected
from. The server closed the connection ten bytes into the body.

```
error: http://127.0.0.1:47391/short-body: HTTP 200 OK: response body closed before all bytes were read

 ● http://127.0.0.1:47391/short-body
 ├ claude-web-fetch-2.rs:314
 │
 ● HTTP 200 OK
 ├ claude-web-fetch-2.rs:285
 │
 ● response body closed before all bytes were read
 ├ claude-web-fetch-2.rs:285
 ╰ Truncated(10 bytes: "<p>partial")
```

Set `RUST_BACKTRACE=1` to have every layer carry a backtrace as well.

## The example

[`claude-web-fetch-2.rs`](claude-web-fetch-2.rs) summarises web pages through
the Claude API's `web_fetch` tool, or with `--find` fetches them directly and
searches them for a quote. Its header comment documents the options, exit
statuses and environment.

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
- **Argument parsing**, beyond the two flags `start` owns (`--verbose` and
  `--help`).

## Development

`cargo test --all-features` runs the tests. `nix develop` (or direnv) provides
cargo, rustc, rustfmt, clippy and rust-script.
