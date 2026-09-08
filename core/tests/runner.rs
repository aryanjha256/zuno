//! The collection runner, against real sockets.
//!
//! No window and no GPUI — which is the point of the runner living in `zuno-core` at all. A
//! mocked transport would prove nothing about a chained run, because the thing under test is
//! that step 2 resolves against what step 1 wrote to disk.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use zuno_core::runner::{self, Step};
use zuno_core::{Body, Engine, Header, Method, RawKind, RequestSpec};

/// Serve `bodies` in order, one connection each, recording every request text received.
///
/// **`Connection: close` on every response, and a bounded accept.** Left keep-alive, reqwest
/// pools the socket and a server that accepts once per response can block on a connection the
/// client decided to reuse — a race that only loses on a slow runner and once hung CI for six
/// hours.
fn serve(bodies: Vec<(u16, String)>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);

    std::thread::spawn(move || {
        for (status, body) in bodies {
            let Some(mut stream) = accept_before(&listener, Duration::from_secs(10)) else {
                return;
            };
            let request = read_head(&mut stream);
            recorder.lock().expect("lock").push(request);

            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    (format!("http://{addr}"), seen)
}

fn accept_before(listener: &TcpListener, within: Duration) -> Option<TcpStream> {
    listener.set_nonblocking(true).expect("nonblocking");
    let deadline = std::time::Instant::now() + within;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break None,
        }
    }?;
    listener.set_nonblocking(false).expect("blocking");
    stream.set_read_timeout(Some(within)).expect("timeout");
    Some(stream)
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buffer).to_string()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zuno-runner-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("environments")).expect("scratch");
    dir
}

fn get(label: &str, url: &str) -> Step {
    Step {
        label: label.to_string(),
        spec: Some(RequestSpec {
            url: url.to_string(),
            ..RequestSpec::default()
        }),
    }
}

/// A step expecting a status, so a run can fail on one.
fn expecting(label: &str, url: &str, status: u16) -> Step {
    Step {
        label: label.to_string(),
        spec: Some(RequestSpec {
            url: url.to_string(),
            expect_status: Some(status),
            ..RequestSpec::default()
        }),
    }
}

fn run(engine: &Engine, steps: Vec<Step>, root: &Path, env: Option<&str>) -> runner::Report {
    runner::run(engine, steps, root, env, &Arc::new(AtomicBool::new(false)))
}

#[test]
fn a_run_reports_every_step_and_does_not_stop_at_the_first_failure() {
    // A run exists to tell you everything wrong in one pass. Stopping at the first failure means
    // running it again to find the second, which is the whole cost the feature removes.
    let root = scratch("continues");
    let engine = Engine::new().expect("engine");
    let (url, _) = serve(vec![
        (200, r#"{"ok":true}"#.into()),
        (500, r#"{"ok":false}"#.into()),
        (200, r#"{"ok":true}"#.into()),
    ]);

    let steps = vec![
        expecting("one", &url, 200),
        expecting("two", &url, 200),
        expecting("three", &url, 200),
    ];

    let report = run(&engine, steps, &root, None);
    assert_eq!(report.outcomes.len(), 3, "every step ran");
    assert_eq!(report.passed(), 2);
    assert_eq!(report.failed(), 1);
    assert!(!report.outcomes[1].passed());
    assert_eq!(report.outcomes[1].label, "two", "and the report names which");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_captured_value_reaches_the_next_step_on_the_wire() {
    // **The one that justifies the whole slice.** Step 1 publishes a token into the environment
    // *on disk*; step 2 has to resolve against what step 1 wrote, which is only true because the
    // resolver is rebuilt per step. Asserted at the bytes the server received, because a resolver
    // that looks right and substitutes nothing looks identical from inside the process.
    let root = scratch("chain");
    std::fs::write(root.join("environments/dev.json"), "{}").expect("write");
    let engine = Engine::new().expect("engine");
    let (url, seen) = serve(vec![
        (200, r#"{"access_token":"abc123"}"#.into()),
        (200, r#"{"ok":true}"#.into()),
    ]);

    let login = Step {
        label: "auth/login".into(),
        spec: Some(RequestSpec {
            url: url.clone(),
            captures: vec![zuno_core::capture::Capture {
                path: "$.access_token".into(),
                name: "token".into(),
                ..Default::default()
            }],
            ..RequestSpec::default()
        }),
    };
    let call = Step {
        label: "users/get".into(),
        spec: Some(RequestSpec {
            url: url.clone(),
            headers: vec![Header::new("Authorization", "Bearer {{token}}")],
            ..RequestSpec::default()
        }),
    };

    let report = run(&engine, vec![login, call], &root, Some("dev"));
    assert_eq!(report.failed(), 0, "both steps should pass");
    assert_eq!(report.outcomes[0].captured, vec!["token".to_string()]);

    let requests = seen.lock().expect("lock").clone();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].to_lowercase().contains("authorization: bearer abc123"),
        "the second request must carry what the first published: {}",
        requests[1]
    );

    // Secret by default, so it landed in the gitignored half rather than the committed one.
    let committed = std::fs::read_to_string(root.join("environments/dev.json")).expect("read");
    assert!(!committed.contains("abc123"), "{committed}");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_step_that_fails_its_assertions_publishes_nothing() {
    // Otherwise a chain poisons itself: the token from an error body goes into `{{token}}` and
    // every later step fails for a reason nothing on screen points at.
    let root = scratch("no-publish-on-failure");
    std::fs::write(root.join("environments/dev.json"), "{}").expect("write");
    let engine = Engine::new().expect("engine");
    let (url, _) = serve(vec![(401, r#"{"access_token":"nope"}"#.into())]);

    let step = Step {
        label: "auth/login".into(),
        spec: Some(RequestSpec {
            url,
            expect_status: Some(200),
            captures: vec![zuno_core::capture::Capture {
                path: "$.access_token".into(),
                name: "token".into(),
                ..Default::default()
            }],
            ..RequestSpec::default()
        }),
    };

    let report = run(&engine, vec![step], &root, Some("dev"));
    assert_eq!(report.failed(), 1);
    assert!(report.outcomes[0].captured.is_empty());
    assert!(!root.join("environments/dev.local.json").exists());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn body_assertions_run_against_the_response_and_name_what_they_found() {
    let root = scratch("assertions");
    let engine = Engine::new().expect("engine");
    let (url, _) = serve(vec![(200, r#"{"status":"degraded","id":7}"#.into())]);

    let step = Step {
        label: "health".into(),
        spec: Some(RequestSpec {
            url,
            expect_status: Some(200),
            assertions: vec![
                zuno_core::assertion::Assertion {
                    path: "$.status".into(),
                    op: zuno_core::assertion::Op::Equals,
                    value: "ok".into(),
                    enabled: true,
                },
                zuno_core::assertion::Assertion {
                    path: "$.id".into(),
                    op: zuno_core::assertion::Op::Exists,
                    value: String::new(),
                    enabled: true,
                },
            ],
            ..RequestSpec::default()
        }),
    };

    let report = run(&engine, vec![step], &root, None);
    let outcome = &report.outcomes[0];
    assert_eq!(outcome.status, Some(200), "the status was fine");
    assert_eq!(outcome.failures.len(), 1, "only the body rule failed: {:?}", outcome.failures);

    let text = outcome.failures[0].to_string();
    assert!(text.contains("$.status") && text.contains("degraded"), "{text}");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_run_stops_when_it_is_cancelled() {
    // A forty-request run needs a stop, and a report that pretends it finished is a lie about
    // what was checked.
    let root = scratch("cancel");
    let engine = Engine::new().expect("engine");
    let (url, _) = serve(vec![(200, "{}".into()), (200, "{}".into())]);

    let cancel = Arc::new(AtomicBool::new(true));
    let report = runner::run(
        &engine,
        vec![get("one", &url), get("two", &url)],
        &root,
        None,
        &cancel,
    );

    assert!(report.cancelled);
    assert!(report.outcomes.is_empty(), "nothing ran");

    // **Read from the flag, not from whether the loop broke on it.** Cancelling during the last
    // step leaves nothing left to break out of, and a report that then claims it finished is the
    // one lie the panel must not tell. Asserted with *no steps*, because that is the only shape
    // where the loop cannot break and so the flag is the only thing that can answer.
    let empty = runner::run(&engine, Vec::new(), &root, None, &cancel);
    assert!(empty.cancelled, "a stopped run with nothing left to do still stopped");

    let never = Arc::new(AtomicBool::new(false));
    let ran = runner::run(&engine, Vec::new(), &root, None, &never);
    assert!(!ran.cancelled, "and one nobody stopped did not");

    cancel.store(false, Ordering::Relaxed);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_request_that_hangs_can_still_be_stopped() {
    // **The runner used to block on the event stream**, so the cancel flag was only ever read
    // between events — and a request that sends `Started` and then hangs produces none. The stop
    // button did nothing on exactly the request you would most want to stop.
    //
    // Here rather than in the app suite: `run_until_parked` waits for the background task, so no
    // mid-run state is observable from the headless platform at all. These are real threads.
    let root = scratch("hang");
    let engine = Engine::new().expect("engine");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            std::thread::sleep(Duration::from_secs(20));
            drop(stream);
        }
    });

    let step = Step {
        label: "hangs".into(),
        spec: Some(RequestSpec {
            url: format!("http://{addr}"),
            settings: zuno_core::RequestSettings {
                timeout: Some(Duration::from_secs(15)),
                ..Default::default()
            },
            ..RequestSpec::default()
        }),
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        flag.store(true, Ordering::Relaxed);
    });

    let started = std::time::Instant::now();
    let report = runner::run(&engine, vec![step], &root, None, &cancel);
    let elapsed = started.elapsed();

    // The margin is the assertion: blocked on the stream this takes the request's full 15s, and
    // polling stops it in well under a second.
    assert!(
        elapsed < Duration::from_secs(5),
        "a hung request has to be stoppable, took {elapsed:?}"
    );
    assert!(report.cancelled);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_folder_expands_to_steps_in_the_order_the_panel_shows() {
    // Filename order, recursive — the smoke-test producer. `scan` already sorts by relative
    // path, so this is the order the collection panel draws and nothing has to re-derive it.
    let root = scratch("folder");
    std::fs::create_dir_all(root.join("users")).expect("mkdir");
    for (path, url) in [
        ("01-login.json", "https://one.test"),
        ("users/02-create.json", "https://two.test"),
        ("users/03-get.json", "https://three.test"),
    ] {
        let spec = RequestSpec {
            url: url.into(),
            method: Method::Get,
            body: Body::Raw { text: String::new(), kind: RawKind::Json },
            ..RequestSpec::default()
        };
        std::fs::write(root.join(path), serde_json::to_vec(&spec).expect("json")).expect("write");
    }

    let steps = runner::steps_in_folder(&root, &root);
    let labels: Vec<&str> = steps.iter().map(|step| step.label.as_str()).collect();
    assert_eq!(labels, ["01-login.json", "users/02-create.json", "users/03-get.json"]);

    std::fs::remove_dir_all(&root).ok();
}
