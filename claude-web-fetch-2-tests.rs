use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Instant,
};

fn target(url: &str) -> Target {
    Target { given: url.into(), url: Url::parse(url).unwrap() }
}

fn quote(text: &str) -> Quote {
    Quote { given: text.into(), words: words(text).split_whitespace().map(str::to_owned).collect() }
}

fn claude() -> Claude {
    Claude {
        key: Secret::new("test-key".into()),
        model: "test-model".into(),
        prompt: DEFAULT_PROMPT.into(),
    }
}

fn fetched(url: &str, text: &str) -> Turn {
    Turn {
        text: "Claude says FOUND: expected words.".into(),
        stop_reason: "end_turn".into(),
        fetches: vec![(
            "fetch_1".into(),
            Fetch::WebFetchResult {
                url: url.into(),
                retrieved_at: Some("2026-10-01T00:00:00Z".into()),
                content: json!({"type": "document", "source": {"type": "text", "data": text}}),
            },
        )],
        ..Turn::default()
    }
}

#[test]
fn quote_verdict_uses_fetched_content_and_requires_the_requested_url() {
    let target = target("https://example.com/page");
    let quote = quote("Expected words");
    let (status, out) =
        report_summary(&target, fetched(target.url.as_str(), "Old content"), Some(&quote));
    assert_eq!(status, 1, "Claude repeating the quote must not count as evidence");
    assert!(out.contains("NOT FOUND"));
    assert_eq!(
        report_summary(&target, fetched(target.url.as_str(), "EXPECTED, words!"), Some(&quote)).0,
        0
    );
    assert_eq!(
        report_summary(
            &target,
            fetched("https://example.com/other", "expected words"),
            Some(&quote)
        )
        .0,
        FAILED
    );

    let mut redirected = fetched("https://example.com/final", "expected words");
    redirected.requested.insert("fetch_1".into(), target.url.to_string());
    assert_eq!(report_summary(&target, redirected, Some(&quote)).0, 0);

    let mut missing = fetched(target.url.as_str(), "expected words");
    if let Fetch::WebFetchResult { content, .. } = &mut missing.fetches[0].1 {
        *content = Value::Null;
    }
    let (status, out) = report_summary(&target, missing, Some(&quote));
    assert_eq!(status, FAILED);
    assert!(out.contains("UNPROVEN"));

    let mut truncated = fetched(target.url.as_str(), "expected words");
    truncated.stop_reason = "max_tokens".into();
    assert_eq!(report_summary(&target, truncated, Some(&quote)).0, FAILED);
    let error = serde_json::from_value(json!({
        "type": "web_fetch_tool_result_error", "error_code": "url_not_accessible"
    }))
    .unwrap();
    let mut failed = fetched(target.url.as_str(), "expected words");
    failed.fetches.push(("fetch_2".into(), error));
    assert_eq!(report_summary(&target, failed, Some(&quote)).0, FAILED);
    let no_fetch =
        Turn { text: "expected words".into(), stop_reason: "end_turn".into(), ..Turn::default() };
    assert_eq!(report_summary(&target, no_fetch, Some(&quote)).0, FAILED);
}

#[test]
fn direct_checks_preserve_search_evidence_and_incomplete_results() {
    let target = target("https://example.com/page");
    let quote = quote("expected words");
    let page = |body| Page {
        url: target.url.to_string(),
        status: 200,
        content_type: "text/html".into(),
        cache: vec!["x-cache: HIT".into()],
        body: Ok(body),
    };
    let body = br#"<script>{"quote":"expected\u0020words"}</script><p>Other content</p>"#;
    let (status, out) = report_page(&target, page(Captured::Complete(body.to_vec())), Some(&quote));
    assert_eq!(status, 0);
    assert!(out.contains("page source only") && out.contains("x-cache: HIT"));
    let absent = b"<p>different content</p>".to_vec();
    assert_eq!(report_page(&target, page(Captured::Complete(absent.clone())), Some(&quote)).0, 1);
    let (status, out) =
        report_page(&target, page(Captured::Truncated(absent.clone())), Some(&quote));
    assert_eq!(status, FAILED);
    assert!(out.contains("UNPROVEN"));
    let (status, out) = report_page(&target, page(Captured::Complete(absent)), None);
    assert_eq!(status, 0);
    assert!(out.contains("Response preview") && !out.contains("NOT FOUND"));
}

#[test]
fn cache_requests_keep_identical_prompts_and_separate_conversations() {
    let claude = claude();
    let target = target("https://example.com/page");
    let quote = quote("Expected words");
    let cached = claude.request(&target, Some(&quote), true, &[]);
    let mut fresh = claude.request(&target, Some(&quote), false, &[]);
    assert_eq!(fresh["tools"][0]["use_cache"], false);
    fresh["tools"][0]["use_cache"] = json!(true);
    assert_eq!(cached, fresh, "cache setting must be the only request difference");
    assert_eq!(cached["messages"].as_array().unwrap().len(), 1);
    assert!(cached["messages"][0]["content"].as_str().unwrap().contains(&quote.given));
    let continuation = [json!({"type": "text", "text": "paused turn"})];
    let resumed = claude.request(&target, Some(&quote), false, &continuation);
    assert_eq!(resumed["tools"][0]["use_cache"], false);
    assert_eq!(resumed["messages"][1]["content"], json!(continuation));
    assert_eq!(claude.request(&target, None, true, &[])["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn each_url_starts_three_checks_together_and_one_failure_does_not_stop_them() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server_base = base.clone();
    let server = thread::spawn(move || {
        for round in 0..3 {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = Vec::new();
            // No response until all three arrive: sequential checks fail this test.
            while requests.len() < 3 {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        let mut header = Vec::new();
                        while !header.ends_with(b"\r\n\r\n") {
                            let mut byte = [0];
                            stream.read_exact(&mut byte).unwrap();
                            header.push(byte[0]);
                        }
                        let header = String::from_utf8(header).unwrap();
                        let length = header
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        let mut body = vec![0; length];
                        stream.read_exact(&mut body).unwrap();
                        requests.push((stream, header, body));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "three concurrent checks did not arrive"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
            let url = format!("{server_base}/page/{round}");
            let mut seen = Vec::new();
            for (mut stream, header, body) in requests {
                let (status, kind, response) = if header.starts_with("GET ") {
                    seen.push("direct");
                    assert!(header.starts_with(&format!("GET /page/{round} ")));
                    assert!(!header.to_ascii_lowercase().contains("cache-control:"));
                    (
                        "200 OK",
                        "text/html",
                        "<p>expected words; direct-only evidence</p>".to_owned(),
                    )
                } else {
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(request["messages"].as_array().unwrap().len(), 1);
                    let prompt = request["messages"][0]["content"].as_str().unwrap();
                    assert!(prompt.contains(&url) && !prompt.contains("direct-only evidence"));
                    assert_eq!(prompt.contains("Expected words"), round < 2);
                    let cached = request["tools"][0]["use_cache"].as_bool().unwrap();
                    seen.push(if cached { "cached" } else { "fresh" });
                    if round == 1 && cached {
                        ("503 Service Unavailable", "text/plain", "cached API unavailable".into())
                    } else {
                        let content = if cached { "old page content" } else { "expected words" };
                        let reply = json!({
                            "content": [
                                {"type": "server_tool_use", "id": "fetch_1", "name": "web_fetch", "input": {"url": url}},
                                {"type": "web_fetch_tool_result", "tool_use_id": "fetch_1", "content": {
                                    "type": "web_fetch_result", "url": url, "retrieved_at": "2026-10-01T00:00:00Z",
                                    "content": {"type": "document", "source": {"type": "text", "data": content}}
                                }},
                                {"type": "text", "text": "Claude says FOUND: expected words."}
                            ], "stop_reason": "end_turn"
                        });
                        ("200 OK", "application/json", reply.to_string())
                    }
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
            seen.sort_unstable();
            assert_eq!(seen, ["cached", "direct", "fresh"]);
        }
    });
    let claude = Ok(claude());
    let quote = quote("Expected words");
    for (round, expected) in [1, FAILED, 0].into_iter().enumerate() {
        assert_eq!(
            audit(
                &target(&format!("{base}/page/{round}")),
                (round < 2).then_some(&quote),
                &claude,
                &format!("{base}/messages")
            ),
            expected
        );
    }
    server.join().unwrap();
}
