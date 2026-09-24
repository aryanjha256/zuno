//! **Every snippet sends what Zuno sends**, checked by running it.
//!
//! A snippet that merely *looks* right is the failure this exists for: the curl exporter looked
//! right for a year while sending JSON as a form. So each scenario is sent twice to one local
//! server — once through Zuno's engine, once by running the generated code with the real
//! toolchain — and the two requests the server received are compared: method, path and query,
//! every header Zuno sent, the media type, and the body. Multipart is compared part by part,
//! because each client picks its own boundary.
//!
//! `#[ignore]`d, like the live network tests, because it needs node, python3 with `requests`, go,
//! java, ruby and dotnet. A missing toolchain is **skipped and named**, never failed, so it runs
//! anywhere and CI never depends on it:
//!
//! ```text
//! cargo test -p zuno-core --test codegen -- --ignored --nocapture
//! ```
//!
//! HTTPie, PHP and grpcurl have no runner here; their renderers are pinned by exact-text unit
//! tests instead.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use zuno_core::codegen::{Target, Wire};
use zuno_core::engine::{Engine, Event};
use zuno_core::request::{
    Body, FormField, GraphQlRequest, Header, Method, MultipartField, MultipartValue, QueryParam,
    RawKind, RequestKind, RequestSpec,
};

/// What the server saw.
#[derive(Debug, Clone)]
struct Received {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(seen, _)| seen.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    fn media_type(&self) -> Option<String> {
        self.header("content-type")
            .first()
            .map(|value| value.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
    }
}

/// An HTTP/1.1 server that records every request and answers `200 ok`.
fn serve() -> (u16, mpsc::Receiver<Received>) {
    let (port_tx, port_rx) = mpsc::channel();
    let (seen_tx, seen_rx) = mpsc::channel();

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let _ = port_tx.send(listener.local_addr().expect("addr").port());
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen_tx = seen_tx.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(
                        move |request: http::Request<hyper::body::Incoming>| {
                            let seen_tx = seen_tx.clone();
                            async move {
                                let method = request.method().to_string();
                                let target = request
                                    .uri()
                                    .path_and_query()
                                    .map(|pq| pq.to_string())
                                    .unwrap_or_default();
                                let headers = request
                                    .headers()
                                    .iter()
                                    .map(|(name, value)| {
                                        (
                                            name.to_string(),
                                            String::from_utf8_lossy(value.as_bytes()).into_owned(),
                                        )
                                    })
                                    .collect();
                                let body = request
                                    .into_body()
                                    .collect()
                                    .await
                                    .map(|collected| collected.to_bytes().to_vec())
                                    .unwrap_or_default();
                                let _ = seen_tx.send(Received {
                                    method,
                                    target,
                                    headers,
                                    body,
                                });
                                Ok::<_, Infallible>(http::Response::new(
                                    http_body_util::Full::new(Bytes::from_static(b"ok")),
                                ))
                            }
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the server must bind");
    (port, seen_rx)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zuno-codegen-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The scenarios: every body shape, a query string that needs encoding, a header, and text that
/// would break a careless literal in one language or another.
fn scenarios(port: u16, dir: &Path) -> Vec<(&'static str, RequestSpec)> {
    let base = format!("http://127.0.0.1:{port}");
    let upload = dir.join("upload.bin");
    // A NUL, a newline and non-UTF-8 bytes: a client that treated the file as text would mangle
    // exactly these.
    std::fs::write(&upload, b"bin\0ary\n\xff\xfe end").expect("fixture");

    let http = |method: Method, path: &str, body: Body| {
        let mut spec = RequestSpec::default();
        spec.url = format!("{base}{path}");
        spec.headers = vec![Header::new("X-Trace", "abc 123")];
        if let Some(http) = spec.http_mut() {
            http.method = method;
            http.body = body;
            http.query = vec![QueryParam::new("q", "a b&c"), QueryParam::new("page", "2")];
        }
        spec
    };

    let mut graphql = RequestSpec::default();
    graphql.url = format!("{base}/graphql");
    graphql.kind = RequestKind::GraphQl(GraphQlRequest {
        method: Method::Post,
        query: "query Me($id: ID!) { user(id: $id) { name } }".into(),
        variables: r#"{"id": "42"}"#.into(),
        ..GraphQlRequest::default()
    });

    vec![
        ("get", http(Method::Get, "/items", Body::Empty)),
        (
            "json",
            http(
                Method::Post,
                "/items",
                Body::Raw {
                    // Quotes, a backslash, a newline, `$` and `#{}` and `${}`, and non-ASCII.
                    text: "{\"name\": \"café \\\"q\\\"\", \"note\": \"line1\\nline2 $HOME #{x} ${y}\"}"
                        .into(),
                    kind: RawKind::Json,
                },
            ),
        ),
        (
            "form",
            http(
                Method::Put,
                "/token",
                Body::Form(vec![
                    FormField {
                        enabled: true,
                        name: "grant_type".into(),
                        value: "client_credentials".into(),
                    },
                    FormField {
                        enabled: true,
                        name: "scope".into(),
                        value: "read write".into(),
                    },
                ]),
            ),
        ),
        (
            "multipart",
            http(
                Method::Post,
                "/upload",
                Body::Multipart(vec![
                    MultipartField {
                        enabled: true,
                        name: "caption".into(),
                        value: MultipartValue::Text("a photo".into()),
                    },
                    MultipartField {
                        enabled: true,
                        name: "file".into(),
                        value: MultipartValue::File(upload.clone()),
                    },
                ]),
            ),
        ),
        ("binary", http(Method::Post, "/blob", Body::Binary(upload))),
        ("graphql", graphql),
    ]
}

/// How to run one target's code, and whether it can be run here at all.
struct Runner {
    target: Target,
    tool: &'static str,
}

const RUNNERS: [Runner; 7] = [
    Runner { target: Target::Curl, tool: "curl" },
    Runner { target: Target::Fetch, tool: "node" },
    Runner { target: Target::Python, tool: "python3" },
    Runner { target: Target::Go, tool: "go" },
    Runner { target: Target::Java, tool: "java" },
    Runner { target: Target::Ruby, tool: "ruby" },
    Runner { target: Target::CSharp, tool: "dotnet" },
];

fn available(tool: &str) -> bool {
    let arg = if tool == "go" { "version" } else { "--version" };
    let present = Command::new(tool)
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    // `requests` is not standard library, and a python without it is a missing toolchain.
    present
        && (tool != "python3"
            || Command::new("python3")
                .args(["-c", "import requests"])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success()))
}

/// Run `command` to completion, killing it past a generous deadline — `go run` and `dotnet run`
/// both compile first.
fn run(mut command: Command, what: &str) {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{what}: could not start: {error}"));
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            let output = child.wait_with_output().expect("output");
            assert!(
                status.success(),
                "{what} failed: {status}\n--- stdout\n{}\n--- stderr\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{what}: timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Write the snippet where its toolchain wants it and run it.
fn execute(runner: &Runner, code: &str, dir: &Path, csharp_project: &Path) {
    let what = format!("{:?}", runner.target);
    match runner.target {
        Target::Curl => {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("{code} --silent --output /dev/null")]);
            run(command, &what);
        }
        Target::Fetch => {
            let file = dir.join("snippet.mjs");
            std::fs::write(&file, code).expect("write");
            let mut command = Command::new("node");
            command.arg(&file);
            run(command, &what);
        }
        Target::Python => {
            let file = dir.join("snippet.py");
            std::fs::write(&file, code).expect("write");
            let mut command = Command::new("python3");
            command.arg(&file);
            run(command, &what);
        }
        Target::Go => {
            let go_dir = dir.join("go");
            std::fs::create_dir_all(&go_dir).expect("dir");
            std::fs::write(go_dir.join("main.go"), code).expect("write");
            let mut command = Command::new("go");
            command.args(["run", "main.go"]).current_dir(&go_dir);
            run(command, &what);
        }
        Target::Java => {
            let java_dir = dir.join("java");
            std::fs::create_dir_all(&java_dir).expect("dir");
            std::fs::write(java_dir.join("Main.java"), code).expect("write");
            let mut command = Command::new("java");
            command.arg("Main.java").current_dir(&java_dir);
            run(command, &what);
        }
        Target::Ruby => {
            let file = dir.join("snippet.rb");
            std::fs::write(&file, code).expect("write");
            let mut command = Command::new("ruby");
            command.arg(&file);
            run(command, &what);
        }
        Target::CSharp => {
            std::fs::write(csharp_project.join("Program.cs"), code).expect("write");
            let mut command = Command::new("dotnet");
            command.args(["run", "--project"]).arg(csharp_project);
            run(command, &what);
        }
        other => unreachable!("{other:?} has no runner"),
    }
}

/// `(name, filename, content)` for every part, from a multipart body and its content type.
fn parts(received: &Received) -> Vec<(String, Option<String>, Vec<u8>)> {
    let content_type = received.header("content-type").first().copied().unwrap_or_default();
    let boundary = content_type
        .split(';')
        .find_map(|param| param.trim().strip_prefix("boundary="))
        .map(|value| value.trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("no boundary in {content_type:?}"));
    let delimiter = format!("--{boundary}").into_bytes();

    let body = &received.body;
    let mut out = Vec::new();
    let mut starts = Vec::new();
    let mut at = 0;
    while let Some(found) = find(&body[at..], &delimiter) {
        starts.push(at + found);
        at += found + delimiter.len();
    }
    for pair in starts.windows(2) {
        let chunk = &body[pair[0] + delimiter.len()..pair[1]];
        let chunk = chunk.strip_prefix(b"\r\n").unwrap_or(chunk);
        let chunk = chunk.strip_suffix(b"\r\n").unwrap_or(chunk);
        let split = find(chunk, b"\r\n\r\n").expect("part headers");
        let head = String::from_utf8_lossy(&chunk[..split]).to_string();
        let content = chunk[split + 4..].to_vec();

        let disposition = head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-disposition"))
            .unwrap_or_default()
            .to_string();
        let param = |key: &str| {
            disposition.split(';').find_map(|piece| {
                let piece = piece.trim();
                piece
                    .strip_prefix(&format!("{key}="))
                    .map(|value| value.trim_matches('"').to_string())
            })
        };
        out.push((param("name").unwrap_or_default(), param("filename"), content));
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// The comparison, with a message naming the first thing that differs.
fn same_request(
    scenario: &str,
    target: Target,
    spec: &RequestSpec,
    code: &str,
    zuno: &Received,
    got: &Received,
) {
    let what = format!("{scenario} / {target:?}");
    assert_eq!(got.method, zuno.method, "{what}: method");
    assert_eq!(got.target, zuno.target, "{what}: path and query");

    let wire = Wire::of(spec).expect("an HTTP-shaped request");
    for (name, _) in wire
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("content-type"))
    {
        assert_eq!(got.header(name), zuno.header(name), "{what}: header {name}");
    }

    // **The one difference no snippet can remove, recorded rather than skipped.** Net::HTTP
    // labels any body without a type `application/x-www-form-urlencoded` — both of its body
    // paths call `supply_default_content_type`, and nothing public turns it off (read in Ruby
    // 3.2's `generic_request.rb`). So a typeless file body cannot go out typeless from Ruby, and
    // what is asserted instead is that the snippet says so.
    if target == Target::Ruby
        && matches!(wire.body, zuno_core::codegen::WireBody::File(_))
        && zuno.media_type().is_none()
    {
        assert!(
            code.contains("Net::HTTP will label it"),
            "{what}: the snippet must name the content type it cannot avoid\n{code}"
        );
    } else {
        assert_eq!(got.media_type(), zuno.media_type(), "{what}: content type");
    }

    if zuno.media_type().as_deref() == Some("multipart/form-data") {
        assert_eq!(parts(got), parts(zuno), "{what}: multipart parts");
    } else {
        assert_eq!(
            got.body,
            zuno.body,
            "{what}: body\n  zuno: {:?}\n  code: {:?}",
            String::from_utf8_lossy(&zuno.body),
            String::from_utf8_lossy(&got.body)
        );
    }
}

#[test]
#[ignore = "runs every language's toolchain; see the module docs"]
fn every_snippet_sends_what_zuno_sends() {
    let (port, seen) = serve();
    let dir = scratch("run");

    let runners: Vec<&Runner> = RUNNERS.iter().filter(|runner| available(runner.tool)).collect();
    for runner in &RUNNERS {
        if !runners.iter().any(|kept| kept.tool == runner.tool) {
            println!("skipped {:?}: `{}` is not installed", runner.target, runner.tool);
        }
    }

    // One project for every C# run: `dotnet new` is slow, and only `Program.cs` changes.
    let csharp_project = dir.join("csharp");
    if runners.iter().any(|runner| runner.target == Target::CSharp) {
        let mut command = Command::new("dotnet");
        command
            .args(["new", "console", "--force", "-o"])
            .arg(&csharp_project);
        run(command, "dotnet new console");
    }

    let engine = Engine::new().expect("engine");
    for (scenario, spec) in scenarios(port, &dir) {
        let (_job, events) = engine.send(spec.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "{scenario}: Zuno's own send timed out");
            match events.recv_blocking() {
                Ok(Event::Done { .. }) => break,
                Ok(Event::Failed { error, .. }) => panic!("{scenario}: Zuno failed: {error}"),
                Ok(_) => {}
                Err(_) => panic!("{scenario}: engine stopped"),
            }
        }
        let zuno = seen
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{scenario}: the server never saw Zuno's request"));

        for runner in &runners {
            let code = runner
                .target
                .render(&spec, None)
                .unwrap_or_else(|| panic!("{scenario}: {:?} cannot express it", runner.target));
            execute(runner, &code, &dir, &csharp_project);
            let got = seen.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| {
                panic!("{scenario}: {:?} ran but sent nothing\n{code}", runner.target)
            });
            same_request(scenario, runner.target, &spec, &code, &zuno, &got);
            println!("ok {scenario} / {:?}", runner.target);
        }
    }

    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}
