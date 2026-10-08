//! End-to-end coverage of the native engine and binary. All model traffic goes to
//! a per-test loopback server; credentials are explicit, inert fixture values.

use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde_json::{Value, json};
use slopdex::engine::Engine;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const TEST_KEY: &str = "local-integration-placeholder";

#[derive(Clone, Debug)]
struct Request {
    path: String,
    body: Value,
}

#[derive(Default)]
struct MockState {
    requests: Vec<Request>,
    fail_embedding_containing: Option<String>,
    errors: Vec<String>,
    concurrent: Option<Arc<ConcurrentRequests>>,
    on_next_request: Option<Box<dyn FnOnce() -> Result<()> + Send>>,
}

struct Mock {
    base: String,
    cache: TempDir,
    state: Arc<Mutex<MockState>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Mock {
    fn start() -> Result<Self> {
        Self::start_with(None)
    }

    fn start_with(concurrent: Option<Arc<ConcurrentRequests>>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let base = format!("http://{}/v1", listener.local_addr()?);
        let cache = tempfile::tempdir()?;
        listener.set_nonblocking(true)?;
        let threaded = concurrent.is_some();
        let state = Arc::new(Mutex::new(MockState {
            concurrent,
            ..MockState::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            let mut handlers = Vec::new();
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = worker_state.clone();
                        let handle = move || {
                            if let Err(error) = serve(stream, &state) {
                                state.lock().unwrap().errors.push(error.to_string());
                            }
                        };
                        if threaded {
                            handlers.push(thread::spawn(handle));
                        } else {
                            handle();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => {
                        worker_state.lock().unwrap().errors.push(error.to_string());
                        break;
                    }
                }
            }
            for handler in handlers {
                if handler.join().is_err() {
                    worker_state
                        .lock()
                        .unwrap()
                        .errors
                        .push("request handler panicked".into());
                }
            }
        });
        Ok(Self {
            base,
            cache,
            state,
            stop,
            worker: Some(worker),
        })
    }

    fn config(&self) -> Value {
        json!({
            "embeddingProvider": "openai", "embeddingModel": "integration-embedding",
            "embeddingDimensions": 4, "embeddingBatchSize": 8,
            "descriptionProvider": "openai", "descriptionModel": "integration-description",
            "rerankerProvider": "cohere", "rerankerModel": "integration-reranker",
            "embeddingBaseUrl": self.base, "descriptionBaseUrl": self.base,
            "rerankerBaseUrl": self.base,
            "embeddingApiKey": TEST_KEY, "descriptionApiKey": TEST_KEY,
            "rerankerApiKey": TEST_KEY,
            "artifactCachePath": self.cache.path().join("artifacts.sqlite"),
            "providerMaxRetries": 0, "retryDelayMs": 0, "providerTimeoutMs": 5000
        })
    }

    fn requests(&self, suffix: &str) -> Vec<Request> {
        let state = self.state.lock().unwrap();
        assert!(
            state.errors.is_empty(),
            "mock server errors: {:?}",
            state.errors
        );
        state
            .requests
            .iter()
            .filter(|r| r.path.ends_with(suffix))
            .cloned()
            .collect()
    }

    fn count(&self) -> usize {
        self.requests("").len()
    }

    fn embedding_inputs(&self) -> Vec<String> {
        self.requests("/embeddings")
            .iter()
            .flat_map(|r| {
                r.body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_owned())
            })
            .collect()
    }

    fn fail_on(&self, marker: Option<&str>) {
        self.state.lock().unwrap().fail_embedding_containing = marker.map(str::to_owned);
    }

    fn on_next_request(&self, action: impl FnOnce() -> Result<()> + Send + 'static) {
        self.state.lock().unwrap().on_next_request = Some(Box::new(action));
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let joined = worker.join();
            if !thread::panicking() {
                assert!(joined.is_ok(), "mock server thread panicked");
                let state = self.state.lock().unwrap();
                assert!(
                    state.errors.is_empty(),
                    "mock protocol failure: {:?}",
                    state.errors
                );
            }
        }
    }
}

fn description_prompt(body: &Value) -> &str {
    body["input"].as_array().unwrap().last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
}

fn serve(mut stream: TcpStream, state: &Mutex<MockState>) -> Result<()> {
    // Windows accepted sockets inherit the listener's nonblocking mode.
    // Request parsing needs blocking I/O, bounded by the timeouts below.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    ensure!(parts.next() == Some("POST"), "expected a local POST");
    let path = parts.next().context("missing request path")?.to_owned();
    let mut length = None;
    let mut authorized = false;
    loop {
        line.clear();
        ensure!(reader.read_line(&mut line)? > 0, "truncated HTTP headers");
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>()?);
            }
            if name.eq_ignore_ascii_case("authorization") {
                authorized = value.trim() == format!("Bearer {TEST_KEY}");
            }
        }
    }
    // Never include received headers in diagnostics (even if a caller is broken).
    ensure!(
        authorized,
        "request did not use the explicit fixture credential"
    );
    let length = length.context("missing content length")?;
    ensure!(length <= 2_000_000, "unexpectedly large fixture request");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let body: Value = serde_json::from_slice(&bytes)?;
    let mut state = state.lock().unwrap();
    state.requests.push(Request {
        path: path.clone(),
        body: body.clone(),
    });
    let (status, response) = match path.as_str() {
        "/v1/embeddings" => {
            let dimensions = body["dimensions"]
                .as_u64()
                .context("missing embedding dimensions")? as usize;
            ensure!(
                matches!(dimensions, 2 | 4),
                "unexpected embedding dimensions: {dimensions}"
            );
            let inputs = body["input"]
                .as_array()
                .context("missing embedding inputs")?;
            if state
                .fail_embedding_containing
                .as_ref()
                .is_some_and(|marker| inputs.iter().any(|s| s.as_str().unwrap().contains(marker)))
            {
                // Deliberately echo the inert key to verify provider error redaction.
                (503, json!({"error": format!("fixture failure {TEST_KEY}")}))
            } else {
                // Reverse wire order to exercise the real provider's index mapping.
                let data: Vec<_> = inputs.iter().enumerate().rev().map(|(index, text)| {
                    json!({"index": index, "embedding": &embedding(text.as_str().unwrap())[..dimensions]})
                }).collect();
                (200, json!({"data": data}))
            }
        }
        "/v1/responses" => {
            let prompt = description_prompt(&body);
            let prefix = if prompt.starts_with("Describe the purpose") {
                "file-summary"
            } else if prompt.starts_with("Describe what") {
                "callable-summary"
            } else {
                "answer grounded in indexed context"
            };
            (
                200,
                json!({"output_text": format!("{prefix}: {}", slopdex::hash(body["input"].to_string()))}),
            )
        }
        "/v1/rerank" => {
            let documents = body["documents"]
                .as_array()
                .context("missing rerank documents")?;
            let mut ranking: Vec<_> = documents
                .iter()
                .enumerate()
                .map(|(index, doc)| {
                    let score = if doc.as_str().unwrap().contains("rerank_winner")
                        || doc.as_str().unwrap().contains("rerank winner")
                    {
                        0.99
                    } else {
                        0.1
                    };
                    json!({"index": index, "relevance_score": score})
                })
                .collect();
            ranking.sort_by(|a, b| {
                b["relevance_score"]
                    .as_f64()
                    .unwrap()
                    .total_cmp(&a["relevance_score"].as_f64().unwrap())
            });
            (200, json!({"results": ranking}))
        }
        _ => anyhow::bail!("unexpected local endpoint"),
    };
    let concurrent = state.concurrent.clone();
    let on_request = state.on_next_request.take();
    drop(state);
    if let Some(action) = on_request {
        action()?;
    }
    let request = Request { path, body };
    if let Some(concurrent) = concurrent
        && concurrent.work.matches(&request)
    {
        return concurrent.respond(&mut stream, request, response);
    }
    write_response(&mut stream, status, &response)
}

fn write_response(stream: &mut TcpStream, status: u16, response: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(&response)?;
    write!(
        stream,
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    )?;
    stream.write_all(&bytes)?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum ConcurrentWork {
    Embeddings,
    Callables,
}

impl ConcurrentWork {
    fn matches(self, request: &Request) -> bool {
        match self {
            Self::Embeddings => request.path.ends_with("/embeddings"),
            Self::Callables => {
                request.path.ends_with("/responses")
                    && description_prompt(&request.body).starts_with("Describe what")
            }
        }
    }

    fn inputs(self, request: &Request) -> Vec<String> {
        match self {
            Self::Embeddings => request.body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|input| input.as_str().unwrap().to_owned())
                .collect(),
            Self::Callables => vec![description_prompt(&request.body).to_owned()],
        }
    }

    fn paid_count(self, db: &Connection) -> Result<usize> {
        let sql = match self {
            Self::Embeddings => "SELECT count(*) FROM embeddings",
            Self::Callables => {
                "SELECT count(*) FROM cache WHERE kind='description' AND json_extract(value,'$.text') LIKE 'callable-summary:%'"
            }
        };
        let count: i64 = db.query_row(sql, [], |row| row.get(0))?;
        Ok(usize::try_from(count)?)
    }
}

#[derive(Default)]
struct ConcurrentProgress {
    arrived: usize,
    active: usize,
    maximum: usize,
    completed: Vec<(Request, u16)>,
}

/// Opt-in HTTP scheduling; the ordinary mock keeps its sequential accept loop.
/// Every wait is bounded so a serial/deferred-persistence regression fails rather
/// than deadlocking the test suite.
struct ConcurrentRequests {
    work: ConcurrentWork,
    limit: usize,
    fail_first: AtomicBool,
    index: PathBuf,
    progress: Mutex<ConcurrentProgress>,
    changed: Condvar,
}

impl ConcurrentRequests {
    fn new(work: ConcurrentWork, limit: usize, index: PathBuf, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            work,
            limit,
            fail_first: AtomicBool::new(fail),
            index,
            progress: Mutex::new(ConcurrentProgress::default()),
            changed: Condvar::new(),
        })
    }

    fn respond(&self, stream: &mut TcpStream, request: Request, response: Value) -> Result<()> {
        let mut progress = self.progress.lock().unwrap();
        let ordinal = progress.arrived;
        progress.arrived += 1;
        progress.active += 1;
        progress.maximum = progress.maximum.max(progress.active);
        self.changed.notify_all();
        // Hold the first window until all configured workers have reached HTTP.
        let (progress_after_wait, timeout) = self
            .changed
            .wait_timeout_while(progress, Duration::from_secs(3), |p| p.arrived < self.limit)
            .unwrap();
        progress = progress_after_wait;
        ensure!(
            !timeout.timed_out(),
            "configured HTTP workers did not overlap"
        );
        let fail = self.fail_first.load(Ordering::Relaxed);
        if fail && ordinal > 0 {
            let (next, timeout) = self
                .changed
                .wait_timeout_while(progress, Duration::from_secs(3), |p| {
                    !p.completed.iter().any(|(_, status)| *status == 503)
                })
                .unwrap();
            progress = next;
            ensure!(!timeout.timed_out(), "peer failure was not sent");
        }
        drop(progress);
        if fail && ordinal == 2 {
            // Keep one paid request outstanding until an earlier success is
            // visible through a separate SQLite connection. This also catches
            // collecting the whole window before persisting any of its results.
            let db = Connection::open(&self.index)?;
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.work.paid_count(&db)? == 0 {
                ensure!(
                    Instant::now() < deadline,
                    "successful peer was not persisted while HTTP work remained in flight"
                );
                thread::sleep(Duration::from_millis(5));
            }
        } else {
            // Give every launched worker time to arrive, including excess ones
            // if the engine accidentally ignores the configured bound.
            thread::sleep(Duration::from_millis(50));
        }
        let (status, response) = if fail && ordinal == 0 {
            (503, json!({"error": "concurrent fixture failure"}))
        } else {
            (200, response)
        };
        let mut progress = self.progress.lock().unwrap();
        write_response(stream, status, &response)?;
        progress.active -= 1;
        progress.completed.push((request, status));
        self.changed.notify_all();
        Ok(())
    }
}

// Independent, hand-selected unit vectors give exact ranking expectations. In
// particular, descriptions disagree with code, so averaging cannot pass by accident.
// Lowercase word forms let normalized symbol names use the same known geometry;
// plain existing queries such as "north" retain their original eastward vector.
fn embedding(text: &str) -> [f32; 4] {
    if text.starts_with("file-summary:") {
        [0.6, 0.0, 0.8, 0.0]
    } else if text.starts_with("callable-summary:") {
        [0.0, 1.0, 0.0, 0.0]
    } else if text.contains("VECTOR_MID") || text.contains("vector mid") {
        [0.8, 0.6, 0.0, 0.0]
    } else if text.contains("VECTOR_NORTH") || text.contains("vector north") {
        [0.0, 1.0, 0.0, 0.0]
    } else if text.contains("VECTOR_WEST") || text.contains("vector west") {
        [-1.0, 0.0, 0.0, 0.0]
    } else {
        [1.0, 0.0, 0.0, 0.0]
    }
}

struct Repo {
    _temp: TempDir,
    root: PathBuf,
    index: PathBuf,
    home: PathBuf,
}

impl Repo {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir(&root)?;
        fs::create_dir(&home)?;
        let index = temp.path().join("index.sqlite");
        Ok(Self {
            _temp: temp,
            root,
            index,
            home,
        })
    }

    fn write(&self, path: &str, text: &str) -> Result<()> {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, text)?;
        Ok(())
    }

    fn open(&self, config: &Value) -> Result<Engine> {
        // Concurrent tests launch Git/CLI children. A fork can briefly inherit a
        // just-dropped flock until exec closes CLOEXEC descriptors. Retry only
        // this lock-contention error; persistent contention still fails the test.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match Engine::open(&self.root, &self.index, config.clone()) {
                Err(error)
                    if error.to_string().starts_with("Index is in use")
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                result => return result,
            }
        }
    }

    fn db(&self) -> Result<Connection> {
        Ok(Connection::open(&self.index)?)
    }

    fn open_map(&self, config: &Value) -> Result<Engine> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match Engine::open_map(&self.root, &self.index, config.clone()) {
                Err(error)
                    if error.to_string().starts_with("Index is in use")
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                result => return result,
            }
        }
    }

    fn child(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("NO_PROXY", "*");
        // Winsock needs SystemRoot to load networking DLLs in Windows children.
        #[cfg(windows)]
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let output = self
            .child("git")
            .args([
                "-c",
                "user.name=Integration Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn cli(&self, args: &[&str]) -> Result<Output> {
        self.cli_format(args, "json")
    }

    fn cli_format(&self, args: &[&str], format: &str) -> Result<Output> {
        let output = self
            .child(env!("CARGO_BIN_EXE_slopdex"))
            .arg("--root")
            .arg(&self.root)
            .arg("--index")
            .arg(&self.index)
            .args(["--format", format])
            .args(args)
            .output()?;
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(TEST_KEY),
            "CLI leaked a credential to stdout"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(TEST_KEY),
            "CLI leaked a credential to stderr"
        );
        Ok(output)
    }

    fn cli_json(&self, args: &[&str]) -> Result<Value> {
        let output = self.cli(args)?;
        ensure!(
            output.status.success(),
            "CLI {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

fn function(name: &str, marker: &str) -> String {
    format!("fn {name}() -> i32 {{\n    // {marker}\n    42\n}}\n")
}

fn all() -> Value {
    json!({"minSimilarity": -1.0})
}

fn names(rows: &[Value]) -> BTreeSet<String> {
    rows.iter()
        .map(|r| r["function"]["qualifiedName"].as_str().unwrap().to_owned())
        .collect()
}

fn sources(rows: &[Value]) -> BTreeSet<String> {
    rows.iter()
        .map(|r| r["source"]["qualifiedName"].as_str().unwrap().to_owned())
        .collect()
}

fn strings(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| (*s).into()).collect()
}

fn assert_incomplete(engine: &Engine) {
    for kind in ["search", "search-code", "search-md"] {
        let error = engine.search("east", kind, &all()).unwrap_err();
        assert!(
            error.to_string().contains("incomplete"),
            "{kind}: {error:#}"
        );
    }
    let error = engine.cross_search(None, &all()).unwrap_err();
    assert!(error.to_string().contains("incomplete"), "{error:#}");
}

fn map_names(rows: &[Value]) -> BTreeSet<String> {
    rows.iter()
        .flat_map(|row| row["nodes"].as_array().unwrap())
        .map(|node| node["name"].as_str().unwrap().to_owned())
        .collect()
}

fn row<'a>(rows: &'a [Value], name: &str) -> &'a Value {
    rows.iter()
        .find(|r| r["function"]["qualifiedName"] == name)
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn near(actual: &Value, expected: f64) {
    let actual = actual.as_f64().expect("numeric similarity");
    assert!(
        (actual - expected).abs() < 0.002,
        "expected {expected}, got {actual}"
    );
}

fn file_record(repo: &Repo, path: &str) -> Result<Value> {
    let text: String =
        repo.db()?
            .query_row("SELECT data FROM files WHERE path=?", [path], |r| r.get(0))?;
    Ok(serde_json::from_str(&text)?)
}

fn artifact_counts(repo: &Repo) -> Result<(i64, i64, i64)> {
    let db = repo.db()?;
    Ok((
        db.query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))?,
        db.query_row("SELECT count(*) FROM cache WHERE kind='parse'", [], |r| {
            r.get(0)
        })?,
        db.query_row(
            "SELECT count(*) FROM cache WHERE kind='description'",
            [],
            |r| r.get(0),
        )?,
    ))
}

/// Include directories as well as file bytes, so even an empty cache/lock
/// directory or a write to an existing index is observable.
fn filesystem_snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
    fn visit(
        root: &Path,
        path: &Path,
        entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(root)?.to_owned();
            if entry.file_type()?.is_dir() {
                entries.insert(relative, None);
                visit(root, &path, entries)?;
            } else {
                // Empty lifetime-lock files can be held by another process;
                // avoid reading their locked byte range on Windows.
                let bytes = if entry.metadata()?.len() == 0 {
                    Vec::new()
                } else {
                    fs::read(path)?
                };
                entries.insert(relative, Some(bytes));
            }
        }
        Ok(())
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries)?;
    Ok(entries)
}

fn isolated_cli(repo: &Repo, index: Option<&Path>) -> Command {
    let mut command = repo.child(env!("CARGO_BIN_EXE_slopdex"));
    command
        .arg("--root")
        .arg(&repo.root)
        .env("XDG_CACHE_HOME", repo.home.join("cache"));
    if let Some(index) = index {
        command.arg("--index").arg(index);
    }
    command
}

/// Bound failures that must precede engine locking or provider work. Killing a
/// regressed child keeps the locked-source case from hanging the test suite.
fn quick_output(command: &mut Command) -> Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill()?;
            let output = child.wait_with_output()?;
            anyhow::bail!(
                "CLI did not fail promptly: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(child.wait_with_output()?)
}

fn assert_missing_index(output: &Output) {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.to_lowercase().contains("index"), "{stderr}");
    assert!(stderr.contains("slopdex update"), "{stderr}");
    assert!(!stderr.contains(TEST_KEY), "CLI leaked a credential");
}

fn concurrency_fixture(
    repo: &Repo,
    mock: &Mock,
    work: ConcurrentWork,
    limit: usize,
) -> Result<Value> {
    if matches!(work, ConcurrentWork::Callables) {
        // Callable turns are serial within each file; concurrency is across files.
        for i in 0..10 {
            repo.write(
                &format!("concurrent_{i}.rs"),
                &function(&format!("concurrent_{i}"), "VECTOR_EAST"),
            )?;
        }
    } else {
        let source: String = (0..10)
            .map(|i| function(&format!("concurrent_{i}"), "VECTOR_EAST"))
            .collect();
        repo.write("concurrent.rs", &source)?;
    }
    let mut config = mock.config();
    config["parallelism"] = json!(limit);
    config["embeddingBatchSize"] = json!(2);
    config["descriptionsEnabled"] = json!(matches!(work, ConcurrentWork::Callables));
    config["providerMaxRetries"] = json!(0);
    Ok(config)
}

fn assert_bounded_concurrency(work: ConcurrentWork) -> Result<()> {
    for limit in [1, 2, 3] {
        let repo = Repo::new()?;
        let concurrent = ConcurrentRequests::new(work, limit, repo.index.clone(), false);
        let mock = Mock::start_with(Some(concurrent.clone()))?;
        let config = concurrency_fixture(&repo, &mock, work, limit)?;
        let mut engine = repo.open(&config)?;
        engine.refresh()?;
        assert_eq!(engine.status()?["functionCount"], 10);
        let requests: Vec<_> = mock
            .requests("")
            .into_iter()
            .filter(|r| work.matches(r))
            .collect();
        let expected_requests = match work {
            ConcurrentWork::Embeddings => 5,
            ConcurrentWork::Callables => 10,
        };
        assert_eq!(requests.len(), expected_requests, "{work:?}, limit {limit}");
        let inputs: Vec<_> = requests.iter().flat_map(|r| work.inputs(r)).collect();
        assert_eq!(inputs.len(), 10);
        assert_eq!(inputs.iter().collect::<BTreeSet<_>>().len(), 10);
        if matches!(work, ConcurrentWork::Embeddings) {
            assert!(requests.iter().all(|r| work.inputs(r).len() == 2));
        }
        {
            let progress = concurrent.progress.lock().unwrap();
            assert_eq!(progress.active, 0, "refresh must join every HTTP worker");
            assert_eq!(progress.completed.len(), expected_requests);
            assert!(
                progress.maximum <= limit,
                "{work:?}: {} active requests exceeded limit {limit}",
                progress.maximum
            );
            if limit > 1 {
                assert!(progress.maximum > 1, "{work:?} unexpectedly ran serially");
            } else {
                assert_eq!(progress.maximum, 1);
            }
        }
        let calls = mock.count();
        assert_eq!(engine.refresh()?["filesUpdated"], 0);
        assert_eq!(
            mock.count(),
            calls,
            "completed concurrent work must be reusable"
        );
    }
    Ok(())
}

#[test]
fn concurrency_embedding_batches_respect_configured_parallelism() -> Result<()> {
    assert_bounded_concurrency(ConcurrentWork::Embeddings)
}

#[test]
fn concurrency_callable_descriptions_respect_configured_parallelism() -> Result<()> {
    assert_bounded_concurrency(ConcurrentWork::Callables)
}

#[test]
fn preparation_finishes_callable_descriptions_before_global_embeddings() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("a.rs", &function("alpha", "VECTOR_EAST"))?;
    repo.write("b.rs", &function("beta", "VECTOR_NORTH"))?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);

    let mut engine = repo.open(&config)?;
    engine.refresh()?;

    let requests = mock.requests("");
    let callable_indices: Vec<_> = requests
        .iter()
        .enumerate()
        .filter_map(|(index, request)| {
            (request.path.ends_with("/responses")
                && description_prompt(&request.body).starts_with("Describe what"))
            .then_some(index)
        })
        .collect();
    let embedding_indices: Vec<_> = requests
        .iter()
        .enumerate()
        .filter_map(|(index, request)| request.path.ends_with("/embeddings").then_some(index))
        .collect();
    assert_eq!(callable_indices.len(), 2);
    assert_eq!(embedding_indices.len(), 2);
    assert!(
        callable_indices.last() < embedding_indices.first(),
        "description and embedding phases must not interleave"
    );
    Ok(())
}

fn assert_concurrent_paid_work_survives_failure(work: ConcurrentWork) -> Result<()> {
    let repo = Repo::new()?;
    let file_count = if matches!(work, ConcurrentWork::Callables) {
        10
    } else {
        1
    };
    let limit = 3;
    let concurrent = ConcurrentRequests::new(work, limit, repo.index.clone(), true);
    let mock = Mock::start_with(Some(concurrent.clone()))?;
    let config = concurrency_fixture(&repo, &mock, work, limit)?;
    let mut engine = repo.open(&config)?;
    let error = engine
        .refresh()
        .expect_err("one request in the first window must fail");
    assert!(format!("{error:#}").contains("HTTP 503"));
    mock.requests(""); // Surface any timeout/protocol failure in the scheduling harness.
    assert_eq!(engine.status()?["functionCount"], 10);
    assert_eq!(engine.status()?["fileCount"], file_count);
    assert_eq!(map_names(&engine.map(&json!({"private":true}))?).len(), 10);
    assert_incomplete(&engine);
    let completed = {
        let progress = concurrent.progress.lock().unwrap();
        assert_eq!(
            progress.active, 0,
            "failure must still join successful peers"
        );
        assert!(progress.maximum > 1 && progress.maximum <= limit);
        assert_eq!(
            progress.arrived, limit,
            "no new window or retry may start after failure"
        );
        assert_eq!(progress.completed.len(), limit);
        progress.completed.clone()
    };
    assert_eq!(
        completed
            .iter()
            .filter(|(_, status)| *status == 503)
            .count(),
        1
    );
    let paid: BTreeSet<_> = completed
        .iter()
        .filter(|(_, status)| *status == 200)
        .flat_map(|(request, _)| work.inputs(request))
        .collect();
    let preserved = match work {
        ConcurrentWork::Embeddings => 4, // Two successful two-input batches.
        ConcurrentWork::Callables => 2,
    };
    assert_eq!(paid.len(), preserved);
    assert_eq!(
        work.paid_count(&repo.db()?)?,
        preserved,
        "both successes sent after the 503 must be durable, including the last peer"
    );
    drop(engine);

    let mut engine = repo.open(&config)?;
    assert_eq!(engine.status()?["functionCount"], 10);
    assert_incomplete(&engine);
    assert_eq!(work.paid_count(&repo.db()?)?, preserved);
    concurrent.fail_first.store(false, Ordering::Relaxed);
    let offset = mock.count();
    let retry = engine.refresh()?;
    assert_eq!(retry["filesUpdated"], 0, "structure was already published");
    assert_eq!(retry["filesPrepared"], file_count);
    assert_eq!(engine.status()?["functionCount"], 10);
    let retry_inputs: Vec<_> = mock.requests("")[offset..]
        .iter()
        .filter(|r| work.matches(r))
        .flat_map(|r| work.inputs(r))
        .collect();
    assert_eq!(
        retry_inputs.len(),
        10 - preserved,
        "restart should pay only for missing artifacts"
    );
    let retry_set: BTreeSet<_> = retry_inputs.into_iter().collect();
    assert_eq!(retry_set.len(), 10 - preserved);
    assert!(
        paid.is_disjoint(&retry_set),
        "successful peers must never be paid for again"
    );
    for (request, status) in &completed {
        if *status == 503 {
            assert!(
                work.inputs(request)
                    .iter()
                    .all(|input| retry_set.contains(input)),
                "the failed request must be retried on the next refresh"
            );
        }
    }
    let calls = mock.count();
    assert_eq!(engine.refresh()?["filesUpdated"], 0);
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn concurrency_embedding_successes_after_peer_failure_are_persisted_and_reused() -> Result<()> {
    assert_concurrent_paid_work_survives_failure(ConcurrentWork::Embeddings)
}

#[test]
fn concurrency_callable_successes_after_peer_failure_are_persisted_and_reused() -> Result<()> {
    assert_concurrent_paid_work_survives_failure(ConcurrentWork::Callables)
}

#[test]
fn query_threshold_endpoints_apply_to_code_and_markdown_and_survive_restart() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    for (name, marker) in [
        ("east", "VECTOR_EAST"),
        ("mid", "VECTOR_MID"),
        ("north", "VECTOR_NORTH"),
        ("west", "VECTOR_WEST"),
    ] {
        repo.write(&format!("{name}.rs"), &function(name, marker))?;
        repo.write(&format!("{name}.md"), &format!("# {name}\n\n{marker}\n"))?;
    }
    let config = mock.config();
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let mut saved = Vec::new();
    for (options, expected) in [
        (json!({"minSimilarity": 1.0}), vec!["east"]),
        (
            json!({"minSimilarity": 0.0, "maxSimilarity": 1.0}),
            vec!["mid", "north"],
        ),
        (
            json!({"minSimilarity": -1.0, "maxSimilarity": 0.0}),
            vec!["west"],
        ),
        (
            json!({"minSimilarity": -1.0, "maxSimilarity": -0.5}),
            vec!["west"],
        ),
        (json!({"minSimilarity": 0.9, "maxSimilarity": 1.0}), vec![]),
    ] {
        for kind in ["search-code", "search-md"] {
            let rows = engine.search("east", kind, &options)?;
            let actual: Vec<_> = rows
                .iter()
                .map(|r| {
                    if kind == "search-code" {
                        r["function"]["qualifiedName"].as_str().unwrap()
                    } else {
                        r["chunk"]["headingPath"][0].as_str().unwrap()
                    }
                })
                .collect();
            assert_eq!(actual, expected, "{kind}: {options}");
            assert_eq!(engine.search("east", kind, &options)?, rows);
            saved.push((kind, options.clone(), rows));
        }
    }
    let calls = mock.count();
    drop(engine);
    let engine = repo.open(&config)?;
    for (kind, options, expected) in saved {
        assert_eq!(engine.search("east", kind, &options)?, expected);
    }
    assert_eq!(mock.count(), calls);
    assert_eq!(
        mock.embedding_inputs()
            .iter()
            .filter(|s| *s == "east")
            .count(),
        1
    );
    Ok(())
}

#[test]
fn cross_search_default_separates_strong_groups_and_explicit_thresholds_can_bridge_them()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(".slopdex/config.json", &config.to_string())?;
    // Each family has cosine 1 internally, and 0.6 with the other family.
    // The old 0.3 default connects all four functions transitively.
    let source = [
        ("mid_a", "VECTOR_MID"),
        ("mid_b", "VECTOR_MID"),
        ("north_a", "VECTOR_NORTH"),
        ("north_b", "VECTOR_NORTH"),
    ]
    .into_iter()
    .map(|(name, marker)| function(name, marker))
    .collect::<String>();
    repo.write("groups.rs", &source)?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let broad = engine.cross_search(None, &json!({"minSimilarity": 0.3}))?;
    assert_eq!(
        broad
            .iter()
            .map(|r| r["matches"].as_array().unwrap().len())
            .sum::<usize>(),
        6
    );

    // Model the previous release's cached result for an omitted engine threshold.
    // The new effective default must not reuse that old low-threshold graph.
    let status = engine.status()?;
    let legacy_key = slopdex::hash(
        json!([
            "cross",
            status["generation"],
            repo.index.canonicalize()?,
            status["generation"],
            "code",
            null,
            status["gitCheckpoint"],
            {}
        ])
        .to_string(),
    );
    repo.db()?.execute(
        "INSERT OR REPLACE INTO search_cache VALUES(?, ?)",
        [legacy_key, serde_json::to_string(&broad)?],
    )?;
    let focused = engine.cross_search(None, &json!({}))?;
    assert_eq!(focused.len(), 2);
    for row in &focused {
        assert_eq!(row["matches"].as_array().unwrap().len(), 1);
        near(&row["matches"][0]["similarity"], 1.0);
    }
    assert_eq!(
        engine.cross_search(None, &json!({"minSimilarity": 0.8}))?,
        focused
    );
    let calls = mock.count();
    drop(engine);
    let engine = repo.open(&config)?;
    assert_eq!(engine.cross_search(None, &json!({}))?, focused);
    assert_eq!(
        engine.cross_search(None, &json!({"minSimilarity": 0.3}))?,
        broad
    );
    drop(engine);

    for (threshold_args, cluster_count) in [
        (vec![], 2),
        (vec!["--threshold", "0.8"], 2),
        (vec!["--threshold", "0.3"], 1),
        (vec!["--threshold", "0.3-0.8"], 1),
    ] {
        let args = [
            vec!["--no-reindex", "--format", "clusters", "cross-search"],
            threshold_args,
        ]
        .concat();
        let output = repo
            .child(env!("CARGO_BIN_EXE_slopdex"))
            .arg("--root")
            .arg(&repo.root)
            .arg("--index")
            .arg(&repo.index)
            .args(&args)
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout)?;
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("*** Cluster "))
                .count(),
            cluster_count,
            "{args:?}: {text}"
        );
        for name in ["mid_a", "mid_b", "north_a", "north_b"] {
            assert!(text.contains(name), "{args:?}: {text}");
        }
    }
    assert_eq!(mock.count(), calls, "threshold changes use saved vectors");
    Ok(())
}

#[test]
fn cross_search_threshold_endpoints_exclude_self_and_use_only_indexed_vectors() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    for (name, marker) in [
        ("source", "VECTOR_EAST"),
        ("twin", "VECTOR_EAST"),
        ("north", "VECTOR_NORTH"),
        ("west", "VECTOR_WEST"),
    ] {
        repo.write(&format!("{name}.rs"), &function(name, marker))?;
    }
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let calls = mock.count();
    for (minimum, maximum, expected, score) in [
        (1.0, None, "twin", 1.0),
        (0.0, Some(1.0), "north", 0.0),
        (-1.0, Some(0.0), "west", -1.0),
    ] {
        let options = json!({"regexp": "^source$", "matches": 1,
            "minSimilarity": minimum, "maxSimilarity": maximum});
        let rows = engine.cross_search(None, &options)?;
        assert_eq!(sources(&rows), strings(&["source"]));
        let matches = rows[0]["matches"].as_array().unwrap();
        assert_eq!(names(matches), strings(&[expected]));
        assert_eq!(matches[0]["similarity"], score);
        assert_eq!(engine.cross_search(None, &options)?, rows);
    }
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn mixed_search_limits_are_global_and_explicit_modes_select_only_requested_streams() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(
        "code.rs",
        &(function("east", "VECTOR_EAST") + &function("north", "VECTOR_NORTH")),
    )?;
    repo.write("mid.md", "# Mid\n\nVECTOR_MID\n")?;
    repo.write("west.md", "# West\n\nVECTOR_WEST\n")?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let rows = engine.search("east", "search", &all())?;
    assert_eq!(
        rows.iter()
            .map(|r| r["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["function", "markdown", "function", "markdown"]
    );
    assert_eq!(rows[0]["function"]["qualifiedName"], "east");
    assert_eq!(rows[1]["chunk"]["headingPath"], json!(["Mid"]));
    for limit in [1, 2, 3, 4, 10] {
        assert_eq!(
            engine.search(
                "east",
                "search",
                &json!({"limit": limit, "minSimilarity": -1})
            )?,
            rows[..limit.min(rows.len())]
        );
    }
    for (flag, kind) in [("code", "search-code"), ("md", "search-md")] {
        let mut options = all();
        options[flag] = json!(true);
        assert_eq!(
            engine.search("east", "search", &options)?,
            engine.search("east", kind, &all())?
        );
    }
    assert_eq!(
        engine.search(
            "east",
            "search",
            &json!({"code": true, "md": true, "minSimilarity": -1})
        )?,
        rows
    );
    Ok(())
}

#[test]
fn empty_search_caches_are_invalidated_when_items_are_added_or_removed() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.refresh()?["generation"], 0);
    assert!(engine.cross_search(None, &all())?.is_empty());
    assert_eq!(mock.count(), 0);
    for kind in ["search", "search-code", "search-md"] {
        assert!(engine.search("east", kind, &all())?.is_empty());
    }
    assert_eq!(mock.embedding_inputs(), ["east"]);
    repo.write("new.rs", &function("added", "VECTOR_EAST"))?;
    let populated_generation = engine.refresh()?["generation"].as_u64().unwrap();
    assert!(populated_generation > 0);
    assert_eq!(
        names(&engine.search("east", "search", &all())?),
        strings(&["added"])
    );
    fs::remove_file(repo.root.join("new.rs"))?;
    assert_eq!(engine.refresh()?["generation"], populated_generation + 1);
    assert!(engine.search("east", "search", &all())?.is_empty());
    let calls = mock.count();
    drop(engine);
    let mut engine = repo.open(&config)?;
    assert!(engine.search("east", "search", &all())?.is_empty());
    repo.write("new.rs", &function("added", "VECTOR_EAST"))?;
    engine.refresh()?;
    assert_eq!(
        names(&engine.search("east", "search", &all())?),
        strings(&["added"])
    );
    assert_eq!(
        mock.count(),
        calls,
        "reintroduced content and query reuse artifacts"
    );
    Ok(())
}

#[test]
fn include_exclude_changes_reconcile_snapshots_and_invalid_globs_preserve_them() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    for (path, name) in [
        ("src/keep.rs", "keep"),
        ("src/skip.rs", "skip"),
        ("other.rs", "other"),
    ] {
        repo.write(path, &function(name, "VECTOR_EAST"))?;
    }
    repo.write("guide.md", "# Guide\n\nVECTOR_EAST\n")?;
    let mut config = mock.config();
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let all_rows = engine.search("east", "search", &all())?;
    let calls = mock.count();
    drop(engine);

    config["include"] = json!(["src/**"]);
    config["exclude"] = json!(["**/skip.rs"]);
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.refresh()?["filesDeleted"], 3);
    assert_eq!(
        names(&engine.search("east", "search", &all())?),
        strings(&["keep"])
    );
    let status = engine.status()?;
    drop(engine);

    config["include"] = json!(["["]);
    let mut engine = repo.open(&config)?;
    assert!(engine.refresh().is_err());
    assert_eq!(engine.status()?, status);
    drop(engine);

    let mut engine = repo.open(&mock.config())?;
    assert_eq!(engine.refresh()?["filesUpdated"], 3);
    let restored = engine.search("east", "search", &all())?;
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["keep", "skip", "other"])
    );
    assert_eq!(restored.len(), all_rows.len());
    assert_eq!(mock.count(), calls, "filter changes only change membership");
    Ok(())
}

#[test]
fn max_file_size_is_inclusive_and_oversize_files_recover_without_losing_peers() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let source = function("boundary", "VECTOR_EAST");
    repo.write("boundary.rs", &source)?;
    repo.write("peer.rs", "fn peer() {}")?;
    let mut config = mock.config();
    config["maxFileSize"] = json!(source.len());
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    assert!(engine.errors()?.is_empty());
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["boundary", "peer"])
    );
    let calls = mock.count();
    repo.write("boundary.rs", &format!("{source}\n"))?;
    engine.refresh()?;
    let errors = engine.errors()?;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["path"], "boundary.rs");
    assert!(
        errors[0]["message"]
            .as_str()
            .unwrap()
            .contains("exceeds maxFileSize")
    );
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["peer"])
    );
    repo.write("boundary.rs", &source)?;
    engine.refresh()?;
    assert!(engine.errors()?.is_empty());
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["boundary", "peer"])
    );
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn source_change_during_embedding_keeps_structure_pending_and_retains_paid_artifacts() -> Result<()>
{
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("old.rs", &function("old", "VECTOR_EAST"))?;
    let config = mock.config();
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    engine.search("east", "search-code", &all())?;
    let status = engine.status()?;
    let source = function("added", "VECTOR_MID");
    repo.write("new.rs", &source)?;
    let path = repo.root.join("new.rs");
    mock.on_next_request(move || {
        fs::write(path, function("changed_again", "VECTOR_NORTH"))?;
        Ok(())
    });
    assert!(
        engine
            .refresh()
            .unwrap_err()
            .to_string()
            .contains("Source changed during indexing")
    );
    assert!(engine.status()?["generation"].as_u64() > status["generation"].as_u64());
    assert_eq!(
        map_names(&engine.map(&json!({"private":true}))?),
        strings(&["old", "added"])
    );
    assert_eq!(file_record(&repo, "new.rs")?["source"], source);
    assert_incomplete(&engine);
    let calls = mock.count();
    drop(engine);
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.status()?["functionCount"], 2);
    repo.write("new.rs", &source)?;
    engine.refresh()?;
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["old", "added"])
    );
    assert_eq!(
        mock.count(),
        calls,
        "completed embedding survives rejected snapshot and restart"
    );
    Ok(())
}

#[test]
fn git_head_change_during_embedding_keeps_structure_pending_and_can_retry() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.git(&["init", "--quiet"])?;
    repo.write("old.rs", &function("old", "VECTOR_EAST"))?;
    repo.git(&["add", "."])?;
    repo.git(&["commit", "--quiet", "-m", "initial"])?;
    let config = mock.config();
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let status = engine.status()?;
    repo.write("new.rs", &function("added", "VECTOR_MID"))?;
    let mut commit = repo.child("git");
    commit.args([
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "changed during request",
    ]);
    mock.on_next_request(move || {
        ensure!(commit.output()?.status.success(), "fixture commit failed");
        Ok(())
    });
    assert!(
        engine
            .refresh()
            .unwrap_err()
            .to_string()
            .contains("Git HEAD changed during indexing")
    );
    assert_eq!(engine.status()?["gitCheckpoint"], status["gitCheckpoint"]);
    assert_eq!(
        map_names(&engine.map(&json!({"private":true}))?),
        strings(&["old", "added"])
    );
    assert_incomplete(&engine);
    let calls = mock.count();
    engine.refresh()?;
    assert_eq!(engine.status()?["functionCount"], 2);
    assert_eq!(
        engine.status()?["gitCheckpoint"],
        repo.git(&["rev-parse", "HEAD"])?
    );
    assert_ne!(engine.status()?["gitCheckpoint"], status["gitCheckpoint"]);
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn force_rebuild_for_moved_root_reuses_code_markdown_and_description_artifacts() -> Result<()> {
    let mock = Mock::start()?;
    let mut repo = Repo::new()?;
    repo.write("code.rs", &function("example", "VECTOR_EAST"))?;
    repo.write("guide.md", "# Guide\n\nVECTOR_MID\n")?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let before = engine.search("east", "search", &all())?;
    let artifacts = artifact_counts(&repo)?;
    let calls = mock.count();
    drop(engine);
    let moved = repo.root.with_file_name("moved");
    fs::rename(&repo.root, &moved)?;
    repo.root = moved;
    config["forceReindex"] = json!(true);
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.status()?["functionCount"], 0);
    assert_eq!(engine.status()?["generation"], 0);
    assert_eq!(artifact_counts(&repo)?, artifacts);
    engine.refresh()?;
    let after = engine.search("east", "search", &all())?;
    // A rebuild may allocate fresh IDs; source content and scores must agree.
    let without_ids = |rows: Vec<Value>| {
        rows.into_iter()
            .map(|mut r| {
                let field = if r["type"] == "function" {
                    "function"
                } else {
                    "chunk"
                };
                r[field].as_object_mut().unwrap().remove("id");
                r
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(without_ids(after), without_ids(before));
    assert_eq!(engine.status()?["descriptionCount"], 1);
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn description_settings_are_restored_from_the_index_when_omitted_from_config() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("code.rs", &function("example", "VECTOR_EAST"))?;
    let mut config = mock.config();
    let mut engine = repo.open(&config)?;
    engine.set_descriptions(true)?;
    let expected = engine.search("east", "search-descriptions", &all())?;
    let status = engine.status()?;
    drop(engine);
    for key in [
        "descriptionsEnabled",
        "descriptionProvider",
        "descriptionModel",
    ] {
        config.as_object_mut().unwrap().remove(key);
    }
    let calls = mock.count();
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.status()?, status);
    assert_eq!(engine.refresh()?["filesUpdated"], 0);
    assert_eq!(
        engine.search("east", "search-descriptions", &all())?,
        expected
    );
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn source_path_filters_preserve_leading_dots_and_normalize_current_directory_components()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".hidden/source.rs", &function("source", "VECTOR_EAST"))?;
    repo.write("peer.rs", &function("peer", "VECTOR_MID"))?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let expected =
        engine.cross_search(None, &json!({"regexp": "^source$", "minSimilarity": -1}))?;
    assert_eq!(expected.len(), 1);
    for path in [
        ".hidden",
        "./.hidden",
        ".hidden/./source.rs",
        "./.hidden/source.rs",
    ] {
        assert_eq!(
            engine.cross_search(None, &json!({"sourcePath": path, "minSimilarity": -1}))?,
            expected,
            "{path}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn repository_scan_does_not_follow_symlink_files_or_directories() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let outside = tempfile::tempdir()?;
    fs::write(
        outside.path().join("outside.rs"),
        function("outside", "VECTOR_EAST"),
    )?;
    repo.write("inside.rs", &function("inside", "VECTOR_MID"))?;
    std::os::unix::fs::symlink(outside.path(), repo.root.join("linked"))?;
    std::os::unix::fs::symlink(
        outside.path().join("outside.rs"),
        repo.root.join("linked.rs"),
    )?;
    std::os::unix::fs::symlink(repo.root.join("inside.rs"), repo.root.join("alias.rs"))?;
    std::os::unix::fs::symlink(&repo.root, repo.root.join("cycle"))?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    assert_eq!(engine.status()?["fileCount"], 1);
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["inside"])
    );
    assert!(
        mock.embedding_inputs()
            .iter()
            .all(|s| !s.contains("outside"))
    );
    assert!(
        engine
            .cross_search(None, &json!({"sourcePath": repo.root.join("linked")}))
            .is_err()
    );
    Ok(())
}

#[test]
fn incremental_refresh_reuses_artifacts_and_reconciles_edits_deletes_and_renames() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    let alpha = function("alpha", "VECTOR_EAST");
    repo.write("alpha.rs", &alpha)?;
    repo.write("beta.rs", &function("beta", "VECTOR_MID"))?;
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.status()?["functionCount"], 0);
    let initial = engine.refresh()?;
    assert_eq!(initial["filesUpdated"], 2);
    assert!(initial["generation"].as_u64().unwrap() > 0);
    let before = engine.search("east", "search-code", &all())?;
    assert_eq!(names(&before), strings(&["alpha", "beta"]));
    let alpha_id = row(&before, "alpha")["function"]["id"].clone();
    let calls = mock.count();
    assert_eq!(engine.search("east", "search-code", &all())?, before);
    let noop = engine.refresh()?;
    assert_eq!(noop["filesUpdated"], 0);
    assert_eq!(noop["generation"], initial["generation"]);
    assert_eq!(mock.count(), calls);

    repo.write("alpha.rs", &format!("\n\n{alpha}"))?;
    assert_eq!(engine.refresh()?["filesUpdated"], 1);
    let shifted = engine.search("east", "search-code", &all())?;
    assert_eq!(row(&shifted, "alpha")["function"]["id"], alpha_id);
    assert_eq!(row(&shifted, "alpha")["function"]["startLine"], 3);
    assert_eq!(
        mock.count(),
        calls,
        "line shifts must reuse document and query embeddings"
    );

    repo.write("alpha.rs", &function("alpha", "VECTOR_NORTH"))?;
    repo.write("new.rs", &function("newcomer", "VECTOR_WEST"))?;
    fs::remove_file(repo.root.join("beta.rs"))?;
    let updated = engine.refresh()?;
    assert_eq!(updated["filesUpdated"], 2);
    assert_eq!(updated["filesDeleted"], 1);
    let edited = engine.search("east", "search-code", &all())?;
    assert_eq!(names(&edited), strings(&["alpha", "newcomer"]));
    assert_eq!(row(&edited, "alpha")["function"]["id"], alpha_id);
    near(&row(&edited, "alpha")["similarity"], 0.0);
    assert_eq!(
        mock.embedding_inputs().len(),
        5,
        "two original documents, one query, two new documents"
    );

    let calls = mock.count();
    fs::rename(repo.root.join("alpha.rs"), repo.root.join("renamed.rs"))?;
    let renamed = engine.refresh()?;
    assert_eq!(renamed["filesUpdated"], 1);
    assert_eq!(renamed["filesDeleted"], 1);
    let rows = engine.search("east", "search-code", &all())?;
    assert_eq!(row(&rows, "alpha")["function"]["path"], "renamed.rs");
    assert_eq!(
        mock.count(),
        calls,
        "a path-only rename reuses callable embeddings"
    );
    assert_eq!(engine.status()?["fileCount"], 2);
    Ok(())
}

#[test]
fn nested_gitignores_apply_without_git_and_changes_remove_indexed_files() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".gitignore", "root_ignored.rs\nignored_dir/\n")?;
    repo.write("nested/.gitignore", "*.rs\n!keep.rs\n")?;
    for (path, name) in [
        ("visible.rs", "visible"),
        ("root_ignored.rs", "root_ignored"),
        ("ignored_dir/hidden.rs", "hidden"),
        ("nested/skip.rs", "skip"),
        ("nested/keep.rs", "keep"),
        ("target/build.rs", "build_output"),
        (".hidden/visible.rs", "hidden_directory_visible"),
    ] {
        repo.write(path, &function(name, "VECTOR_EAST"))?;
    }
    assert!(!repo.root.join(".git").exists());
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["visible", "keep", "hidden_directory_visible"])
    );
    assert_eq!(engine.status()?["gitCheckpoint"], Value::Null);
    assert_eq!(mock.embedding_inputs().len(), 4);
    let calls = mock.count();
    repo.write("nested/.gitignore", "*.rs\n")?;
    assert_eq!(engine.refresh()?["filesDeleted"], 1);
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["visible", "hidden_directory_visible"])
    );
    assert_eq!(mock.count(), calls);
    repo.write("nested/.gitignore", "*.rs\n!keep.rs\n")?;
    assert_eq!(engine.refresh()?["filesUpdated"], 1);
    assert_eq!(engine.status()?["functionCount"], 3);
    assert_eq!(
        mock.count(),
        calls,
        "unignoring a known file reuses artifacts"
    );
    Ok(())
}

#[test]
fn restrictive_query_filters_and_threshold_ranges_backfill_past_nearest_neighbors() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut source = (0..48)
        .map(|i| function(&format!("distractor_{i}"), "VECTOR_EAST"))
        .collect::<String>();
    source.push_str(&function("selected_mid", "VECTOR_MID"));
    source.push_str(&function("selected_north", "VECTOR_NORTH"));
    repo.write("functions.rs", &source)?;
    repo.write("guide.md", "# Guide\n\nVECTOR_EAST documentation.\n")?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let options = json!({"regexp": "^selected_north$", "limit": 1, "minSimilarity": -1});
    let selected = engine.search("east", "search-code", &options)?;
    assert_eq!(names(&selected), strings(&["selected_north"]));
    near(&selected[0]["similarity"], 0.0);
    let range = engine.search(
        "east",
        "search-code",
        &json!({"limit": 1, "minSimilarity": 0.7, "maxSimilarity": 0.9}),
    )?;
    assert_eq!(names(&range), strings(&["selected_mid"]));
    near(&range[0]["similarity"], 0.8);
    let boundary = engine.search(
        "east",
        "search-code",
        &json!({"minSimilarity": 0.0, "maxSimilarity": 1.0}),
    )?;
    assert_eq!(
        names(&boundary),
        strings(&["selected_mid", "selected_north"]),
        "inclusive minimum, exclusive maximum"
    );
    assert!(
        engine
            .search(
                "east",
                "search-code",
                &json!({"regexp": "^absent$", "limit": 1})
            )?
            .is_empty()
    );
    assert!(
        engine
            .search("east", "search-code", &json!({"regexp": "["}))
            .is_err()
    );
    let mixed = engine.search("east", "search", &all())?;
    assert_eq!(mixed.len(), 51);
    assert_eq!(mixed.iter().filter(|r| r["type"] == "markdown").count(), 1);
    let md = engine.search("east", "search", &json!({"md": true, "minSimilarity": -1}))?;
    assert_eq!(md.len(), 1);
    assert_eq!(md[0]["chunk"]["headingPath"], json!(["Guide"]));
    assert_eq!(engine.search("east", "search-md", &all())?, md);
    assert_eq!(
        mock.embedding_inputs()
            .iter()
            .filter(|s| s.as_str() == "east")
            .count(),
        1,
        "changing filters must reuse the query embedding"
    );
    Ok(())
}

#[test]
fn cross_search_applies_candidate_filters_before_limit_and_deduplicates_pairs() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut same_file = function("source", "VECTOR_EAST");
    for i in 0..40 {
        same_file.push_str(&function(&format!("same_file_{i}"), "VECTOR_EAST"));
    }
    repo.write("src/source.rs", &same_file)?;
    let short = (0..40)
        .map(|i| format!("fn short_{i}() -> i32 {{ 42 }}\n"))
        .collect::<String>();
    repo.write("other/short.rs", &short)?;
    repo.write("other/long.rs", &function("eligible", "VECTOR_MID"))?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let calls = mock.count();
    let options = json!({"regexp": "^source$", "sourcePath": "src", "minLines": 3,
        "crossFileOnly": true, "matches": 1, "minSimilarity": 0.7, "maxSimilarity": 0.9});
    let rows = engine.cross_search(None, &options)?;
    assert_eq!(sources(&rows), strings(&["source"]));
    assert_eq!(rows[0]["matches"].as_array().unwrap().len(), 1);
    assert_eq!(
        rows[0]["matches"][0]["function"]["qualifiedName"],
        "eligible"
    );
    near(&rows[0]["matches"][0]["similarity"], 0.8);
    assert_eq!(engine.cross_search(None, &options)?, rows);
    assert_eq!(
        mock.count(),
        calls,
        "cross-search should only use indexed embeddings"
    );
    let mut none = options.clone();
    none["minLines"] = json!(5);
    assert!(engine.cross_search(None, &none)?.is_empty());
    none = options.clone();
    none["maxLines"] = json!(4);
    assert!(
        engine.cross_search(None, &none)?.is_empty(),
        "the maximum line count is exclusive for sources and candidates"
    );
    let mut bounded = options.clone();
    bounded["maxLines"] = json!(5);
    assert_eq!(engine.cross_search(None, &bounded)?, rows);
    none = options.clone();
    none["sourcePath"] = json!("sr");
    assert!(
        engine.cross_search(None, &none)?.is_empty(),
        "directory filters respect component boundaries"
    );
    none["sourcePath"] = json!("../outside");
    assert!(engine.cross_search(None, &none).is_err());
    none = options.clone();
    none["sourcePath"] = json!(repo.root.join("src/source.rs"));
    assert_eq!(engine.cross_search(None, &none)?, rows);

    let pairs = engine.cross_search(
        None,
        &json!({"minLines": 3, "crossFileOnly": true,
        "matches": 100, "minSimilarity": -1}),
    )?;
    let mut seen = BTreeSet::new();
    for source in &pairs {
        let a = source["source"]["id"].as_u64().unwrap();
        for target in source["matches"].as_array().unwrap() {
            let b = target["function"]["id"].as_u64().unwrap();
            assert_ne!(a, b);
            assert_ne!(source["source"]["path"], target["function"]["path"]);
            assert!(
                seen.insert((a.min(b), a.max(b))),
                "duplicate symmetric pair"
            );
        }
    }
    assert_eq!(seen.len(), 41);
    let symmetric = engine.cross_search(
        None,
        &json!({"minLines": 3, "crossFileOnly": true,
        "matches": 100, "minSimilarity": -1, "includeSymmetricDuplicates": true}),
    )?;
    assert_eq!(
        symmetric
            .iter()
            .map(|r| r["matches"].as_array().unwrap().len())
            .sum::<usize>(),
        82
    );
    Ok(())
}

#[test]
fn sqlite_restart_recovers_deleted_and_corrupt_vector_sidecars_without_provider_calls() -> Result<()>
{
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    repo.write("a.rs", &function("east", "VECTOR_EAST"))?;
    repo.write("b.rs", &function("mid", "VECTOR_MID"))?;
    repo.write("guide.md", "# Guide\n\nVECTOR_EAST docs.\n")?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let expected = engine.search("east", "search", &all())?;
    let expected_status = engine.status()?;
    let calls = mock.count();
    drop(engine);
    let sidecars: Vec<PathBuf> = ["code", "markdown", "descriptions", "combined"]
        .iter()
        .flat_map(|kind| {
            let path = format!("{}.{kind}.usearch", repo.index.display());
            [
                PathBuf::from(&path),
                PathBuf::from(format!("{path}.manifest.json")),
            ]
        })
        .collect();
    for round in 0..3 {
        for path in &sidecars {
            assert!(path.is_file());
            match round {
                0 => fs::remove_file(path)?,
                1 if path.extension().is_some_and(|e| e == "usearch") => {
                    // Leave the valid manifest in place: its binary hash must
                    // reject the corrupt binary before USearch tries to load it.
                    fs::write(path, b"not a valid binary")?;
                }
                2 if path.extension().is_some_and(|e| e == "json") => {
                    fs::write(path, b"not a valid manifest")?;
                }
                _ => {}
            }
        }
        let engine = repo.open(&config)?;
        assert_eq!(engine.status()?, expected_status);
        assert_eq!(
            engine.search("east", "search", &all())?,
            expected,
            "persisted result cache survives restart"
        );
        // Different options bypass the result cache, proving rebuilt ANN indexes work.
        let options = json!({"minSimilarity": -1, "limit": 20 + round});
        assert_eq!(engine.search("east", "search", &options)?, expected);
        assert_eq!(engine.search("east", "search-code", &options)?.len(), 2);
        assert_eq!(
            engine
                .search("east", "search-descriptions", &options)?
                .len(),
            2
        );
        assert_eq!(engine.search("east", "search-md", &options)?.len(), 1);
        for path in &sidecars {
            assert!(fs::metadata(path)?.len() > 20);
        }
        assert_eq!(
            mock.count(),
            calls,
            "SQLite artifacts must suffice for recovery"
        );
    }
    Ok(())
}

#[test]
fn description_lifecycle_fuses_scores_preserves_stale_files_and_reindexes_on_request() -> Result<()>
{
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write("a.rs", &function("alpha", "VECTOR_EAST"))?;
    repo.write("b.rs", &function("beta", "VECTOR_MID"))?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    assert!(
        engine
            .search("east", "search-descriptions", &all())
            .is_err()
    );
    assert!(engine.reindex_files(false).is_err());
    let enabled = engine.set_descriptions(true)?;
    assert_eq!(enabled["descriptionCount"], 2);
    assert_eq!(enabled["fileDescriptionCount"], 2);
    assert_eq!(enabled["staleFileDescriptionCount"], 0);
    assert_eq!(mock.requests("/responses").len(), 4);
    let fused = engine.search("east", "search", &all())?;
    let alpha = row(&fused, "alpha");
    near(&alpha["codeSimilarity"], 1.0);
    near(&alpha["descriptionSimilarity"], 0.0);
    near(&alpha["fileDescriptionSimilarity"], 0.6);
    near(&alpha["similarity"], 1.6 / 3.0);
    near(&row(&fused, "beta")["similarity"], 1.4 / 3.0);
    let descriptions = engine.search("east", "search-descriptions", &all())?;
    near(&descriptions[0]["similarity"], 0.3);
    assert!(descriptions[0].get("codeSimilarity").is_none());
    let cross = engine.cross_search(None, &json!({"minSimilarity": -1, "regexp": "^alpha$"}))?;
    assert_eq!(
        cross[0]["scoring"]["similarityMode"],
        "code-description-file-average"
    );
    near(&cross[0]["matches"][0]["similarity"], 2.8 / 3.0);
    let old_file = file_record(&repo, "a.rs")?;
    let old_callable = alpha["function"]["description"].clone();
    repo.write("a.rs", &function("alpha", "VECTOR_NORTH"))?;
    engine.refresh()?;
    assert_eq!(engine.status()?["staleFileDescriptionCount"], 1);
    assert_eq!(
        file_record(&repo, "a.rs")?["description"],
        old_file["description"]
    );
    assert_eq!(
        mock.requests("/responses").len(),
        5,
        "only the changed callable is redescribed"
    );
    let changed = engine.search("east", "search", &all())?;
    assert_ne!(
        row(&changed, "alpha")["function"]["description"],
        old_callable
    );
    let callable = row(&changed, "alpha")["function"]["description"].clone();
    near(&row(&changed, "alpha")["similarity"], 0.2);
    assert_eq!(engine.reindex_files(false)?["filesReindexed"], 1);
    assert_eq!(engine.status()?["staleFileDescriptionCount"], 0);
    assert_ne!(
        file_record(&repo, "a.rs")?["description"],
        old_file["description"]
    );
    assert_eq!(mock.requests("/responses").len(), 6);
    let refreshed = engine.search("east", "search", &all())?;
    assert_eq!(
        row(&refreshed, "alpha")["function"]["description"],
        callable
    );

    // A file-only edit makes its context stale while callable source stays identical.
    repo.write(
        "a.rs",
        &format!(
            "{}\nconst EXTRA: i32 = 7;\n",
            function("alpha", "VECTOR_NORTH")
        ),
    )?;
    engine.refresh()?;
    assert_eq!(mock.requests("/responses").len(), 6);
    assert_eq!(engine.reindex_files(true)?["filesReindexed"], 1);
    assert_eq!(
        mock.requests("/responses").len(),
        8,
        "--callables regenerates against the new file context"
    );
    let regenerated = engine.search("east", "search", &all())?;
    assert_ne!(
        row(&regenerated, "alpha")["function"]["description"],
        callable
    );
    let calls = mock.count();
    assert_eq!(engine.reindex_files(true)?["filesReindexed"], 0);
    assert_eq!(
        engine.set_descriptions(false)?["descriptionsEnabled"],
        false
    );
    let code_only = engine.search("east", "search", &all())?;
    assert!(
        code_only
            .iter()
            .all(|r| r.get("descriptionSimilarity").is_none())
    );
    assert!(
        engine
            .search("east", "search-descriptions", &all())
            .is_err()
    );
    assert_eq!(engine.set_descriptions(true)?["descriptionCount"], 2);
    assert_eq!(
        mock.count(),
        calls,
        "toggle preserves reusable descriptions"
    );
    drop(engine);
    // The caller omitted descriptionsEnabled: the persisted setting must win.
    let reopened = repo.open(&config)?;
    assert_eq!(reopened.status()?["descriptionsEnabled"], true);
    assert_eq!(reopened.search("east", "search", &all())?, regenerated);
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn describe_uses_indexed_search_context_without_whole_files() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let original = format!(
        "const FILE_CONTEXT_SENTINEL: i32 = 9;\n{}",
        function("alpha", "VECTOR_EAST")
    );
    repo.write("a.rs", &original)?;
    repo.write("unrelated.rs", &function("other", "VECTOR_WEST"))?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    // describe must use its indexed snapshot, not a fresh read of the working tree.
    repo.write("a.rs", "fn not_indexed_yet() {}\n")?;
    let answer = engine.describe("east", &json!({"minSimilarity": 0.9}))?;
    assert!(
        answer["description"]
            .as_str()
            .unwrap()
            .starts_with("answer grounded")
    );
    assert_eq!(answer["query"], "east");
    assert_eq!(answer["files"].as_array().unwrap().len(), 1);
    assert_eq!(answer["files"][0]["path"], "a.rs");
    assert!(answer["files"][0].get("content").is_none());
    assert_eq!(answer["functions"][0]["qualifiedName"], "alpha");
    assert!(answer["functions"][0].get("source").is_none());
    assert!(answer["functions"][0].get("embeddingInput").is_none());
    let requests = mock.requests("/responses");
    let prompt = requests.last().unwrap().body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(prompt.starts_with("Task: east\n\n*** a.rs\n"), "{prompt}");
    assert!(prompt.contains("@@ 2-"), "{prompt}");
    assert!(prompt.contains("fn alpha"), "{prompt}");
    assert!(!prompt.contains("FILE_CONTEXT_SENTINEL"), "{prompt}");
    assert!(!prompt.contains("Full source code for best matching files"));
    assert!(!prompt.contains("not_indexed_yet"));
    assert!(!prompt.starts_with('{'));
    Ok(())
}

#[test]
fn describe_keeps_search_skeletons_without_appending_large_files() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    for (path, name, size) in [
        ("a.rs", "first", 90 * 1024),
        ("b.rs", "second", 12 * 1024),
        ("c.rs", "third", 2 * 1024),
        ("z.rs", "oversized", 110 * 1024),
    ] {
        repo.write(
            path,
            &(function(name, "VECTOR_EAST") + &format!("// {}\n", "X".repeat(size))),
        )?;
    }
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    engine.describe("east", &json!({"minSimilarity":0.9}))?;
    let requests = mock.requests("/responses");
    let prompt = requests.last().unwrap().body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(prompt.len() <= 128 * 1024, "{} bytes", prompt.len());
    for name in ["first", "second", "third", "oversized"] {
        assert!(prompt.contains(&format!("fn {name}")), "missing {name}");
    }
    assert!(!prompt.contains(&"X".repeat(100)), "{prompt}");
    assert!(!prompt.contains("Full source code for best matching files"));
    Ok(())
}

#[test]
fn describe_bounds_expanded_markdown_search_on_utf8_boundary() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let sections = (0..20)
        .map(|index| format!("## Section {index}\nVECTOR_EAST {}\n", "📖".repeat(1800)))
        .collect::<String>();
    repo.write("guide.md", &format!("# Guide\n{sections}"))?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    engine.describe("east", &json!({"minSimilarity":0.9}))?;
    let requests = mock.requests("/responses");
    let prompt = requests.last().unwrap().body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(prompt.len() <= 128 * 1024);
    assert!(prompt.contains("*** guide.md\n"));
    assert!(prompt.contains("## Section 0"));
    assert!(prompt.contains("[Search results truncated to fit the prompt size limit]"));
    Ok(())
}

#[test]
fn failed_provider_publishes_structure_and_partial_artifacts_survive_restart() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    repo.write("baseline.rs", &function("baseline", "VECTOR_EAST"))?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    engine.search("east", "search", &all())?;
    let status = engine.status()?;
    let counts = artifact_counts(&repo)?;
    fs::remove_file(repo.root.join("baseline.rs"))?;
    repo.write("a_completed.rs", &function("completed", "VECTOR_MID"))?;
    repo.write(
        "z_failing.rs",
        &function("failing", "BROKEN_EMBED VECTOR_NORTH"),
    )?;
    mock.fail_on(Some("BROKEN_EMBED"));
    let error = engine
        .refresh()
        .expect_err("injected provider failure must abort refresh");
    let error = format!("{error:#}");
    assert!(error.contains("HTTP 503"));
    assert!(!error.contains(TEST_KEY));
    assert_eq!(engine.status()?["functionCount"], 2);
    assert!(engine.status()?["generation"].as_u64() > status["generation"].as_u64());
    assert_eq!(
        map_names(&engine.map(&json!({"private":true}))?),
        strings(&["completed", "failing"])
    );
    assert!(file_record(&repo, "baseline.rs").is_err());
    assert_eq!(
        file_record(&repo, "a_completed.rs")?["source"],
        function("completed", "VECTOR_MID")
    );
    assert_incomplete(&engine);
    let partial = artifact_counts(&repo)?;
    assert!(
        partial.0 > counts.0 && partial.1 > counts.1 && partial.2 > counts.2,
        "successful embeddings, parses, and descriptions survive the failure"
    );
    drop(engine);
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.status()?["functionCount"], 2);
    assert_incomplete(&engine);
    let request_offset = mock.count();
    mock.fail_on(None);
    let retried = engine.refresh()?;
    assert_eq!(retried["filesUpdated"], 0);
    assert_eq!(retried["filesDeleted"], 0);
    assert!(retried["generation"].as_u64() > status["generation"].as_u64());
    assert_eq!(
        names(&engine.search("east", "search", &all())?),
        strings(&["completed", "failing"])
    );
    let retry_requests = &mock.requests("")[request_offset..];
    assert!(
        retry_requests
            .iter()
            .all(|r| !r.body.to_string().contains("symbol: completed"))
    );
    assert!(
        retry_requests
            .iter()
            .filter(|r| r.path.ends_with("/responses"))
            .all(|r| description_prompt(&r.body).starts_with("Describe what")),
        "completed file descriptions must not be regenerated"
    );
    assert_eq!(
        mock.requests("/responses").len(),
        6,
        "baseline, completed, and failing each described exactly twice"
    );
    Ok(())
}

#[test]
fn sqlite_structure_failure_rolls_back_deletes_and_updates_before_provider_calls() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write("old.rs", &function("old", "VECTOR_EAST"))?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let before = engine.search("east", "search-code", &all())?;
    let status = engine.status()?;
    let db = repo.db()?;
    db.execute_batch(
        "CREATE TRIGGER integration_abort BEFORE INSERT ON search_units
        WHEN NEW.path='z_abort.rs' BEGIN SELECT RAISE(ABORT, 'fixture publication failure'); END;",
    )?;
    fs::remove_file(repo.root.join("old.rs"))?;
    repo.write("a_new.rs", &function("new_a", "VECTOR_MID"))?;
    repo.write("z_abort.rs", &function("new_z", "VECTOR_NORTH"))?;
    let calls = mock.count();
    let error = engine
        .refresh()
        .expect_err("publication trigger should abort");
    assert!(error.to_string().contains("fixture publication failure"));
    assert_eq!(engine.status()?, status);
    assert_eq!(engine.search("east", "search-code", &all())?, before);
    let live_paths: Vec<String> = db
        .prepare("SELECT path FROM files ORDER BY path")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(live_paths, ["old.rs"]);
    assert_eq!(
        mock.count(),
        calls,
        "structural failure precedes paid preparation"
    );
    assert_eq!(
        map_names(&engine.map(&json!({"private":true}))?),
        strings(&["old"])
    );
    db.execute_batch("DROP TRIGGER integration_abort")?;
    drop(engine);
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.status()?, status);
    engine.refresh()?;
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["new_a", "new_z"])
    );
    assert_eq!(
        mock.embedding_inputs().len(),
        4,
        "old document, query, and two new documents"
    );
    Ok(())
}

#[test]
fn cross_repository_search_tracks_target_generation_and_rejects_incompatible_profiles() -> Result<()>
{
    let mock = Mock::start()?;
    let source_repo = Repo::new()?;
    let target_repo = Repo::new()?;
    source_repo.write("same.rs", &function("source", "VECTOR_EAST"))?;
    target_repo.write("same.rs", &function("target", "VECTOR_MID"))?;
    let mut source = source_repo.open(&mock.config())?;
    let mut target = target_repo.open(&mock.config())?;
    source.refresh()?;
    target.refresh()?;
    let options = json!({"crossFileOnly": true, "minSimilarity": -1, "matches": 1});
    let first = source.cross_search(Some(&target), &options)?;
    assert_eq!(
        first.len(),
        1,
        "equal relative paths in different repositories are different physical files"
    );
    near(&first[0]["matches"][0]["similarity"], 0.8);
    assert_eq!(
        first[0]["matches"][0]["function"]["qualifiedName"],
        "target"
    );
    target_repo.write("same.rs", &function("target", "VECTOR_NORTH"))?;
    target.refresh()?;
    let changed = source.cross_search(Some(&target), &options)?;
    near(&changed[0]["matches"][0]["similarity"], 0.0);
    let other_repo = Repo::new()?;
    let mut incompatible = mock.config();
    incompatible["embeddingModel"] = json!("another-model");
    let other = other_repo.open(&incompatible)?;
    let error = source.cross_search(Some(&other), &options).unwrap_err();
    assert!(error.to_string().contains("identical embedding profiles"));
    Ok(())
}

#[test]
fn reranking_uses_local_provider_preserves_cosine_and_caches_results() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(
        "code.rs",
        &(function("cosine_winner", "VECTOR_EAST") + &function("rerank_winner", "VECTOR_NORTH")),
    )?;
    let mut config = mock.config();
    config["rerankingEnabled"] = json!(true);
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let options = json!({"minSimilarity": -1, "limit": 1});
    let ranked = engine.search("east", "search-code", &options)?;
    assert_eq!(names(&ranked), strings(&["rerank_winner"]));
    near(&ranked[0]["similarity"], 0.0);
    near(&ranked[0]["rerankScore"], 0.99);
    let requests = mock.requests("/rerank");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["documents"].as_array().unwrap().len(), 2);
    let calls = mock.count();
    assert_eq!(engine.search("east", "search-code", &options)?, ranked);
    drop(engine);
    let engine = repo.open(&config)?;
    assert_eq!(engine.search("east", "search-code", &options)?, ranked);
    assert_eq!(mock.count(), calls);
    let filtered = engine.search(
        "east",
        "search-code",
        &json!({"minSimilarity": 0.9, "limit": 1}),
    )?;
    assert_eq!(
        names(&filtered),
        strings(&["cosine_winner"]),
        "reranking cannot restore a threshold-excluded candidate"
    );
    Ok(())
}

#[test]
fn git_changed_since_filters_symbols_while_uncommitted_includes_staged_and_untracked_files()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.git(&["init", "--quiet"])?;
    repo.write(
        "tracked.rs",
        &(function("changed", "VECTOR_EAST") + &function("untouched", "VECTOR_MID")),
    )?;
    repo.write("peer.rs", &function("peer", "VECTOR_EAST"))?;
    repo.git(&["add", "."])?;
    repo.git(&["commit", "--quiet", "-m", "base"])?;
    let base = repo.git(&["rev-parse", "HEAD"])?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    assert_eq!(engine.status()?["gitCheckpoint"], base);
    let options = json!({"minSimilarity": -1, "matches": 20, "includeSymmetricDuplicates": true, "uncommitted": true});
    assert!(engine.cross_search(None, &options)?.is_empty());
    repo.write(
        "tracked.rs",
        &(function("changed", "VECTOR_NORTH") + &function("untouched", "VECTOR_MID")),
    )?;
    repo.git(&["add", "tracked.rs"])?;
    repo.git(&["commit", "--quiet", "-m", "change one callable"])?;
    let head = repo.git(&["rev-parse", "HEAD"])?;
    engine.refresh()?;
    assert_eq!(engine.status()?["gitCheckpoint"], head);
    let changed_options = json!({"minSimilarity": -1, "matches": 20, "includeSymmetricDuplicates": true, "changedSince": base});
    assert_eq!(
        sources(&engine.cross_search(None, &changed_options)?),
        strings(&["changed"])
    );
    assert!(engine.cross_search(None, &options)?.is_empty());
    repo.write(
        "tracked.rs",
        &(function("changed", "VECTOR_NORTH") + &function("untouched", "VECTOR_WEST")),
    )?;
    repo.git(&["add", "tracked.rs"])?;
    repo.write("peer.rs", &function("peer", "VECTOR_MID"))?;
    repo.write("untracked.rs", &function("new_untracked", "VECTOR_EAST"))?;
    engine.refresh()?;
    assert_eq!(
        sources(&engine.cross_search(None, &options)?),
        strings(&["changed", "untouched", "peer", "new_untracked"])
    );
    let since_head = json!({"minSimilarity": -1, "matches": 20, "includeSymmetricDuplicates": true, "changedSince": head});
    assert_eq!(
        sources(&engine.cross_search(None, &since_head)?),
        strings(&["untouched", "peer", "new_untracked"])
    );
    assert!(
        engine
            .cross_search(None, &json!({"changedSince": "missing-fixture-ref"}))
            .is_err()
    );
    repo.git(&["add", "."])?;
    repo.git(&["commit", "--quiet", "-m", "commit working tree"])?;
    let calls = mock.count();
    engine.refresh()?;
    assert!(
        engine.cross_search(None, &options)?.is_empty(),
        "source-mode changes invalidate cached cross-search"
    );
    assert_eq!(
        mock.count(),
        calls,
        "committing changes only metadata, not embedding inputs"
    );
    Ok(())
}

#[test]
fn cli_missing_resolved_source_indexes_fail_without_artifacts_or_provider_calls() -> Result<()> {
    for selection in ["default", "explicit", "config"] {
        let mock = Mock::start()?;
        let repo = Repo::new()?;
        let mut config = mock.config();
        config["descriptionsEnabled"] = json!(true);
        config["rerankingEnabled"] = json!(true);
        if selection == "config" {
            config["indexPath"] = json!("uncreated/configured.sqlite");
        }
        repo.write(".slopdex/config.json", &config.to_string())?;
        repo.write("api.rs", &function("alpha", "VECTOR_EAST"))?;
        repo.write("guide.md", "# Guide\n\nVECTOR_EAST documentation.\n")?;
        // A lower-priority existing index must not mask the missing selected
        // path: --index beats config, and config beats the XDG default.
        if selection != "default" {
            let decoy = if selection == "explicit" {
                repo.index.clone()
            } else {
                repo.home
                    .join("cache/slopdex/workspaces")
                    .join(slopdex::hash(
                        repo.root.canonicalize()?.as_os_str().as_encoded_bytes(),
                    ))
                    .join("index.sqlite")
            };
            Engine::open_map(&repo.root, &decoy, config.clone())?.refresh_structure()?;
            if selection == "explicit" {
                config["indexPath"] = json!(decoy);
                repo.write(".slopdex/config.json", &config.to_string())?;
            }
        }
        let explicit = Path::new("uncreated/explicit.sqlite");
        let index = (selection == "explicit").then_some(explicit);
        let before = filesystem_snapshot(repo._temp.path())?;
        let cache_before = filesystem_snapshot(mock.cache.path())?;
        for no_reindex in [false, true] {
            for kind in [
                "search",
                "search-code",
                "search-md",
                "search-descriptions",
                "cross-search",
            ] {
                let mut command = isolated_cli(&repo, index);
                command.args(["--format", "json"]);
                if no_reindex {
                    command.arg("--no-reindex");
                }
                command.arg(kind);
                if kind != "cross-search" {
                    command.arg("east");
                }
                let output = quick_output(&mut command)?;
                assert_missing_index(&output);
                assert_eq!(mock.count(), 0, "{selection}, {kind}, {no_reindex}");
                assert_eq!(
                    filesystem_snapshot(repo._temp.path())?,
                    before,
                    "{selection}, {kind}, {no_reindex} created workspace artifacts"
                );
                assert_eq!(filesystem_snapshot(mock.cache.path())?, cache_before);
            }
        }
    }
    Ok(())
}

#[test]
fn cli_missing_cross_target_fails_before_source_open_or_refresh() -> Result<()> {
    let mock = Mock::start()?;
    let source = Repo::new()?;
    let mut target = Repo::new()?;
    target.index = target._temp.path().join("uncreated/target.sqlite");
    let config = mock.config();
    source.write(".slopdex/config.json", &config.to_string())?;
    target.write(".slopdex/config.json", &config.to_string())?;
    source.write("source.rs", &function("saved", "VECTOR_EAST"))?;
    target.write("target.rs", &function("target", "VECTOR_EAST"))?;
    source.cli_json(&["update"])?;
    source.write(
        "source.rs",
        &function("edited", "BROKEN_EMBED VECTOR_NORTH"),
    )?;
    mock.fail_on(Some("BROKEN_EMBED"));
    let calls = mock.count();

    // Holding the source's exclusive lock makes opening it before target
    // validation observable, even if opening would otherwise be read-only.
    let guard = source.open(&config)?;
    let status = guard.status()?;
    let check = || -> Result<()> {
        let before = filesystem_snapshot(source._temp.path())?;
        let target_before = filesystem_snapshot(target._temp.path())?;
        let cache_before = filesystem_snapshot(mock.cache.path())?;
        for no_reindex in [false, true] {
            let mut command = isolated_cli(&source, Some(&source.index));
            if no_reindex {
                command.arg("--no-reindex");
            }
            let output = quick_output(command.args([
                "cross-search",
                "--target-root",
                target.root.to_str().unwrap(),
                "--target-index",
                target.index.to_str().unwrap(),
            ]))?;
            assert_missing_index(&output);
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(target.index.to_str().unwrap())
            );
            assert_eq!(filesystem_snapshot(source._temp.path())?, before);
            assert_eq!(filesystem_snapshot(target._temp.path())?, target_before);
            assert_eq!(filesystem_snapshot(mock.cache.path())?, cache_before);
            assert_eq!(mock.count(), calls);
        }
        Ok(())
    };
    check()?;
    assert_eq!(guard.status()?, status);
    drop(guard);
    check()?;
    assert_eq!(
        file_record(&source, "source.rs")?["source"],
        function("saved", "VECTOR_EAST")
    );
    Ok(())
}

#[test]
fn cli_refresh_search_no_reindex_jsonl_and_provider_failure_are_end_to_end() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write(
        "code.rs",
        &(function("alpha", "VECTOR_EAST") + &function("beta", "VECTOR_MID")),
    )?;
    let status = repo.cli_json(&["status"])?;
    assert_eq!(status["functionCount"], 2);
    assert_eq!(status["storageBackend"], "sqlite");
    assert_eq!(status["vectorBackend"], "usearch");
    let range = repo.cli_json(&[
        "search-code",
        "east",
        "--threshold",
        "0.7-0.9",
        "--limit",
        "1",
    ])?;
    assert_eq!(names(range.as_array().unwrap()), strings(&["beta"]));
    let calls = mock.count();
    assert_eq!(
        repo.cli_json(&[
            "search-code",
            "east",
            "--threshold",
            "0.7-0.9",
            "--limit",
            "1"
        ])?,
        range
    );
    assert_eq!(mock.count(), calls);
    repo.write("new.rs", &function("new_one", "VECTOR_NORTH"))?;
    assert_eq!(
        repo.cli_json(&["--no-reindex", "status"])?["functionCount"],
        2
    );
    assert_eq!(mock.count(), calls);
    assert_eq!(repo.cli_json(&["status"])?["functionCount"], 3);
    let output = repo.cli(&[
        "cross-search",
        "--regexp",
        "^alpha$",
        "--threshold",
        "-1",
        "--matches",
        "2",
    ])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<Value> = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<_>>()?;
    assert_eq!(sources(&lines), strings(&["alpha"]));
    assert_eq!(lines[0]["matches"].as_array().unwrap().len(), 2);
    let calls = mock.count();
    let bad = repo.cli(&["search-code", "east", "--threshold", "0.9-0.1"])?;
    assert!(!bad.status.success());
    assert_eq!(
        mock.count(),
        calls,
        "argument validation must precede provider requests"
    );
    repo.write("z_failure.rs", &function("failure", "BROKEN_EMBED"))?;
    mock.fail_on(Some("BROKEN_EMBED"));
    let failed = repo.cli(&["status"])?;
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("HTTP 503"));
    assert_eq!(
        repo.cli_json(&["--no-reindex", "status"])?["functionCount"],
        4
    );
    let incomplete = repo.cli(&["--no-reindex", "search-code", "east"])?;
    assert!(!incomplete.status.success());
    assert!(String::from_utf8_lossy(&incomplete.stderr).contains("incomplete"));
    Ok(())
}

#[test]
fn cli_redirected_progress_preserves_json_plain_diagnostics_and_offline_mode() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write("code.rs", &function("alpha", "VECTOR_EAST"))?;
    fs::write(repo.root.join("invalid.rs"), [0xff])?;

    let output = repo.cli(&["status"])?;
    assert!(
        output.status.success(),
        "CLI status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(status["functionCount"], 1);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("slopdex: notice: external model call:"));
    assert!(stderr.contains("saved indexing error(s)"));
    assert!(!stderr.contains('\x1b'));
    assert!(!stderr.contains("Refreshing index"));
    assert!(!stderr.contains("Embedding batches"));
    assert!(!stderr.contains("Generating embeddings"));
    assert!(!stderr.contains("Indexing files"));
    assert!(!stderr.contains("% ("));

    let requests = mock.count();
    repo.write("new.rs", &function("beta", "VECTOR_NORTH"))?;
    let output = repo.cli(&["--no-reindex", "--ignore-errors", "status"])?;
    assert!(output.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&output.stdout)?, status);
    assert!(output.stderr.is_empty());
    assert_eq!(mock.count(), requests);

    mock.fail_on(Some("VECTOR_NORTH"));
    let output = repo.cli(&["status"])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("slopdex:"));
    assert!(stderr.contains("HTTP 503"));
    assert!(!stderr.contains('\x1b'));
    assert!(!stderr.contains("Refreshing index"));
    Ok(())
}

#[test]
fn read_errors_are_persisted_and_clear_when_the_source_is_repaired() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("good.rs", &function("good", "VECTOR_EAST"))?;
    fs::write(repo.root.join("invalid.rs"), [0xff, 0xfe])?;
    let mut config = mock.config();
    config["ignoreErrors"] = json!(true);
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    assert_eq!(engine.status()?["fileCount"], 2);
    assert_eq!(engine.status()?["functionCount"], 1);
    assert_eq!(engine.status()?["failedFileCount"], 1);
    let errors = engine.errors()?;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["path"], "invalid.rs");
    assert_eq!(errors[0]["code"], "read-error");
    drop(engine);
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.errors()?, errors);
    repo.write("invalid.rs", &function("repaired", "VECTOR_MID"))?;
    engine.refresh()?;
    assert!(engine.errors()?.is_empty());
    assert_eq!(engine.status()?["failedFileCount"], 0);
    assert_eq!(
        names(&engine.search("east", "search-code", &all())?),
        strings(&["good", "repaired"])
    );
    Ok(())
}

#[test]
fn open_waits_for_exclusive_ownership_and_no_reindex_uses_the_persisted_snapshot() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write("code.rs", &function("indexed", "VECTOR_EAST"))?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let expected = engine.search("east", "search-code", &all())?;
    let status = engine.status()?;
    #[cfg(not(unix))]
    let second_index = repo.index.clone();
    #[cfg(unix)]
    let second_index = {
        let alias = repo.index.with_file_name("alias.sqlite");
        std::os::unix::fs::symlink(&repo.index, &alias)?;
        alias
    };
    let second_root = repo.root.clone();
    let second_config = config.clone();
    let waiter =
        std::thread::spawn(move || Engine::open(&second_root, &second_index, second_config));
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(
        !waiter.is_finished(),
        "a writer must wait while another owns the index"
    );
    drop(engine);
    drop(waiter.join().unwrap()?);

    let shared_search = Engine::open_readonly(&repo.root, &repo.index, config.clone())?;
    assert_eq!(
        shared_search.search("east", "search-code", &all())?,
        expected
    );
    assert!(
        shared_search
            .search("north", "search-code", &all())
            .is_err()
    );
    drop(shared_search);

    let first_reader = Engine::open_map_readonly(&repo.root, &repo.index, config.clone())?;
    let second_reader = Engine::open_map_readonly(&repo.root, &repo.index, config.clone())?;
    assert_eq!(first_reader.status()?, second_reader.status()?);
    drop(first_reader);
    drop(second_reader);

    let mut offline_config = config.clone();
    offline_config["noReindex"] = json!(true);
    repo.write("code.rs", &function("not_indexed", "VECTOR_NORTH"))?;
    let calls = mock.count();
    let mut offline = repo.open(&offline_config)?;
    let refresh = offline.refresh()?;
    assert_eq!(refresh["skipped"], true);
    assert_eq!(refresh["generation"], status["generation"]);
    assert_eq!(offline.status()?, status);
    assert_eq!(offline.search("east", "search-code", &all())?, expected);
    assert_eq!(mock.count(), calls);
    drop(offline);

    let mut resumed = repo.open(&config)?;
    assert_eq!(resumed.status()?, status);
    resumed.refresh()?;
    assert_eq!(
        names(&resumed.search("east", "search-code", &all())?),
        strings(&["not_indexed"])
    );
    Ok(())
}

#[test]
fn current_map_reads_share_the_lock_and_stale_maps_wait_for_writers() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("source.rs", "pub fn first() {}\n")?;
    let mut writer = Engine::open_map(&repo.root, &repo.index, json!({}))?;
    writer.refresh_structure()?;
    drop(writer);

    let mut reader = Engine::open_map_readonly(&repo.root, &repo.index, json!({}))?;
    let generation = reader.status()?["generation"].clone();
    assert_eq!(reader.refresh_structure()?["filesUpdated"], 0);
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--index")
        .arg(&repo.index)
        .args(["map", "source.rs"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout)?.contains("pub fn first()"));
    assert_eq!(reader.status()?["generation"], generation);

    repo.write("source.rs", "pub fn second() {}\n")?;
    assert!(reader.refresh_structure().is_err());
    drop(reader);
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--index")
        .arg(&repo.index)
        .args(["map", "source.rs"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout)?.contains("pub fn second()"));
    Ok(())
}

#[test]
fn edits_while_descriptions_are_disabled_invalidate_only_changed_callable_descriptions()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(
        "code.rs",
        &(function("edited", "VECTOR_EAST") + &function("untouched", "VECTOR_MID")),
    )?;
    let mut engine = repo.open(&mock.config())?;
    engine.set_descriptions(true)?;
    let before = engine.search("east", "search", &all())?;
    let saved_file = file_record(&repo, "code.rs")?;
    assert_eq!(mock.requests("/responses").len(), 3);
    engine.set_descriptions(false)?;
    repo.write(
        "code.rs",
        &(function("edited", "VECTOR_NORTH") + &function("untouched", "VECTOR_MID")),
    )?;
    engine.refresh()?;
    let disabled = engine.search("east", "search-code", &all())?;
    assert_eq!(
        row(&disabled, "edited")["function"]["description"],
        Value::Null
    );
    assert_eq!(
        row(&disabled, "untouched")["function"]["description"],
        row(&before, "untouched")["function"]["description"]
    );
    assert_eq!(engine.status()?["descriptionCount"], 1);
    assert_eq!(engine.status()?["staleFileDescriptionCount"], 1);
    assert_eq!(mock.requests("/responses").len(), 3);
    engine.set_descriptions(true)?;
    assert_eq!(engine.status()?["descriptionCount"], 2);
    assert_eq!(mock.requests("/responses").len(), 4);
    assert_eq!(
        file_record(&repo, "code.rs")?["description"],
        saved_file["description"]
    );
    let enabled = engine.search("east", "search", &all())?;
    assert_ne!(
        row(&enabled, "edited")["function"]["description"],
        row(&before, "edited")["function"]["description"]
    );
    near(&row(&enabled, "edited")["similarity"], 0.2);
    Ok(())
}

#[test]
fn regression_descriptions_with_markdown_refresh_is_noop_and_keeps_query_caches() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    config["rerankingEnabled"] = json!(true);
    repo.write("code.rs", &function("alpha", "VECTOR_EAST"))?;
    repo.write("guide.md", "# Guide\n\nVECTOR_MID documentation.\n")?;
    let mut engine = repo.open(&config)?;
    let initial = engine.refresh()?;
    assert_eq!(initial["filesUpdated"], 2);
    assert_eq!(mock.requests("/responses").len(), 2);
    let mut expected = Vec::new();
    for kind in ["search", "search-code", "search-descriptions", "search-md"] {
        let rows = engine.search("east", kind, &all())?;
        assert!(!rows.is_empty());
        expected.push((kind, rows));
    }
    assert_eq!(mock.requests("/rerank").len(), 4);
    let status = engine.status()?;
    let markdown = file_record(&repo, "guide.md")?;
    assert_eq!(markdown["description"], Value::Null);
    let artifacts = artifact_counts(&repo)?;
    let calls = mock.count();
    // A cache miss would try to write the recomputed result, even for a query
    // whose embedding is already cached. Make that observable independently of reranking.
    repo.db()?.execute_batch(
        "CREATE TRIGGER regression_cache_miss BEFORE INSERT ON search_cache
         BEGIN SELECT RAISE(ABORT, 'unexpected query cache miss'); END;",
    )?;
    for _ in 0..2 {
        let noop = engine.refresh()?;
        assert_eq!(noop["filesUpdated"], 0);
        assert_eq!(noop["filesDeleted"], 0);
        assert_eq!(noop["generation"], initial["generation"]);
        assert_eq!(engine.status()?, status);
        assert_eq!(file_record(&repo, "guide.md")?, markdown);
        for (kind, rows) in &expected {
            assert_eq!(engine.search("east", kind, &all())?, *rows);
        }
        assert_eq!(artifact_counts(&repo)?, artifacts);
        assert_eq!(mock.count(), calls);
        drop(engine);
        engine = repo.open(&config)?;
    }
    Ok(())
}

#[test]
fn generated_descriptions_share_a_growing_file_conversation_in_source_order() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    let source = "pub fn run() -> i32 { helper() }\npub fn helper() -> i32 { 42 }\n";
    repo.write("api.rs", source)?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let requests = mock.requests("/responses");
    assert_eq!(requests.len(), 3);
    assert!(description_prompt(&requests[0].body).starts_with("Describe the purpose"));
    assert!(description_prompt(&requests[1].body).contains("Symbol: run\n"));
    assert!(description_prompt(&requests[2].body).contains("Symbol: helper\n"));
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            request.body["instructions"],
            requests[0].body["instructions"]
        );
        let messages = request.body["input"].as_array().unwrap();
        assert_eq!(messages.len(), index * 2 + 1);
        assert!(
            messages[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(source)
        );
        if index > 0 {
            let previous = requests[index - 1].body["input"].as_array().unwrap();
            assert_eq!(
                &messages[..previous.len()],
                previous.as_slice(),
                "previous turn must be an identical cacheable prefix"
            );
            let answer = &messages[previous.len()];
            assert_eq!(answer["role"], "assistant");
            assert_eq!(answer["content"][0]["type"], "output_text");
            let expected_prefix = if index == 1 {
                "file-summary"
            } else {
                "callable-summary"
            };
            assert_eq!(
                answer["content"][0]["text"],
                format!(
                    "{expected_prefix}: {}",
                    slopdex::hash(requests[index - 1].body["input"].to_string())
                )
            );
            assert!(
                !description_prompt(&request.body).contains(source),
                "callable source is already in the shared context"
            );
        }
    }
    let file = requests
        .iter()
        .find(|request| description_prompt(&request.body).starts_with("Describe the purpose"))
        .unwrap();
    let callable = requests
        .iter()
        .find(|request| description_prompt(&request.body).starts_with("Describe what"))
        .unwrap();
    assert!(
        file.body["instructions"]
            .as_str()
            .unwrap()
            .contains("exactly one concise paragraph")
    );
    assert!(
        callable.body["instructions"]
            .as_str()
            .unwrap()
            .contains("exactly one sentence")
    );
    Ok(())
}

#[test]
fn regression_description_model_change_retains_unchanged_and_regenerates_only_edited_callable()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    let untouched = function("untouched", "VECTOR_MID");
    repo.write("code.rs", &(function("edited", "VECTOR_EAST") + &untouched))?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let before = engine.search("east", "search", &all())?;
    let saved_file = file_record(&repo, "code.rs")?;
    let generation = engine.status()?["generation"].clone();
    let calls = mock.count();
    assert_eq!(mock.requests("/responses").len(), 3);
    drop(engine);

    config["descriptionModel"] = json!("replacement-description-model");
    let mut engine = repo.open(&config)?;
    let noop = engine.refresh()?;
    assert_eq!(noop["filesUpdated"], 0);
    assert_eq!(noop["generation"], generation);
    assert_eq!(
        engine.status()?["descriptionProfile"]["model"],
        config["descriptionModel"]
    );
    assert_eq!(engine.search("east", "search", &all())?, before);
    assert_eq!(mock.count(), calls);

    // Both callables are reparsed together, including a line shift for the
    // untouched one, so retaining it cannot rely only on skipping its file.
    repo.write(
        "code.rs",
        &(function("edited", "VECTOR_NORTH") + "\n\n" + &untouched),
    )?;
    assert_eq!(engine.refresh()?["filesUpdated"], 1);
    let after = engine.search("east", "search", &all())?;
    assert_eq!(names(&after), strings(&["edited", "untouched"]));
    for name in ["edited", "untouched"] {
        assert_eq!(
            row(&after, name)["function"]["id"],
            row(&before, name)["function"]["id"]
        );
    }
    assert_eq!(
        row(&after, "untouched")["function"]["description"],
        row(&before, "untouched")["function"]["description"]
    );
    assert_ne!(
        row(&after, "untouched")["function"]["startLine"],
        row(&before, "untouched")["function"]["startLine"]
    );
    assert_ne!(
        row(&after, "edited")["function"]["description"],
        row(&before, "edited")["function"]["description"]
    );
    let requests = mock.requests("/responses");
    assert_eq!(
        requests.len(),
        4,
        "only the edited callable needs a new description"
    );
    assert_eq!(requests[3].body["model"], config["descriptionModel"]);
    let prompt = description_prompt(&requests[3].body);
    assert!(prompt.starts_with("Describe what"));
    assert!(prompt.contains("Symbol: edited\n"));
    assert!(!prompt.contains("Symbol: untouched"));
    assert_eq!(
        requests[3].body["input"][1]["content"][0]["text"],
        saved_file["description"]
    );
    assert!(
        requests[3].body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("VECTOR_NORTH")
    );
    let file = file_record(&repo, "code.rs")?;
    assert_eq!(file["description"], saved_file["description"]);
    assert_eq!(file["description_hash"], saved_file["description_hash"]);
    assert_eq!(engine.status()?["staleFileDescriptionCount"], 1);
    let calls = mock.count();
    drop(engine);
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.refresh()?["filesUpdated"], 0);
    assert_eq!(engine.search("east", "search", &all())?, after);
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn regression_changed_since_moving_branch_invalidates_cross_cache_and_rechecks_ancestry()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.git(&["init", "--quiet"])?;
    repo.write(
        "code.rs",
        &(function("changed", "VECTOR_EAST") + &function("peer", "VECTOR_MID")),
    )?;
    repo.git(&["add", "."])?;
    repo.git(&["commit", "--quiet", "-m", "base"])?;
    let base = repo.git(&["rev-parse", "HEAD"])?;
    repo.git(&["branch", "comparison", &base])?;
    repo.write(
        "code.rs",
        &(function("changed", "VECTOR_NORTH") + &function("peer", "VECTOR_MID")),
    )?;
    repo.git(&["add", "."])?;
    repo.git(&["commit", "--quiet", "-m", "indexed change"])?;
    let indexed = repo.git(&["rev-parse", "HEAD"])?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let status = engine.status()?;
    let calls = mock.count();
    let options = json!({"changedSince": "comparison", "minSimilarity": -1, "includeSymmetricDuplicates": true});
    let changed = engine.cross_search(None, &options)?;
    assert_eq!(sources(&changed), strings(&["changed"]));
    assert_eq!(engine.cross_search(None, &options)?, changed);
    repo.git(&["branch", "-f", "comparison", &indexed])?;
    assert!(
        engine.cross_search(None, &options)?.is_empty(),
        "moving the same symbolic ref must bypass cached results without a refresh"
    );
    repo.git(&["branch", "-f", "comparison", &base])?;
    assert_eq!(engine.cross_search(None, &options)?, changed);

    // A descendant of the indexed checkpoint is an ancestor of live HEAD, but
    // is still invalid for the persisted snapshot being queried.
    repo.git(&[
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "future checkpoint",
    ])?;
    let future = repo.git(&["rev-parse", "HEAD"])?;
    repo.git(&["branch", "-f", "comparison", &future])?;
    let error = engine
        .cross_search(None, &options)
        .expect_err("cached branch must still validate ancestry");
    assert!(error.to_string().contains("ancestor of the indexed commit"));
    repo.git(&["checkout", "--quiet", "--detach", &base])?;
    repo.git(&[
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "divergent checkpoint",
    ])?;
    let divergent = repo.git(&["rev-parse", "HEAD"])?;
    repo.git(&["branch", "-f", "comparison", &divergent])?;
    let error = engine
        .cross_search(None, &options)
        .expect_err("divergent branch is not an ancestor");
    assert!(error.to_string().contains("ancestor of the indexed commit"));
    repo.git(&["branch", "-D", "comparison"])?;
    let error = engine
        .cross_search(None, &options)
        .expect_err("deleted cached branch must be resolved again");
    assert!(
        error
            .to_string()
            .contains("Cannot resolve --changed-since commit")
    );
    assert_eq!(engine.status()?, status);
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn regression_git_subdirectory_root_scopes_uncommitted_and_changed_since_paths() -> Result<()> {
    let mock = Mock::start()?;
    let mut repo = Repo::new()?;
    repo.git(&["init", "--quiet"])?;
    repo.write(
        "workspace/pkg/tracked.rs",
        &(function("edited", "VECTOR_EAST") + &function("untouched", "VECTOR_MID")),
    )?;
    repo.write("workspace/pkg/peer.rs", &function("peer", "VECTOR_EAST"))?;
    // Same relative filename outside the indexed root catches missing Git prefixes.
    repo.write("tracked.rs", &function("outside", "VECTOR_NORTH"))?;
    repo.git(&["add", "."])?;
    repo.git(&["commit", "--quiet", "-m", "base"])?;
    let base = repo.git(&["rev-parse", "HEAD"])?;
    repo.root = repo.root.join("workspace/pkg");
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let dirty = json!({"uncommitted": true, "minSimilarity": -1, "matches": 20, "includeSymmetricDuplicates": true});
    let since = json!({"changedSince": base, "minSimilarity": -1, "matches": 20, "includeSymmetricDuplicates": true});
    assert!(engine.cross_search(None, &dirty)?.is_empty());
    assert!(engine.cross_search(None, &since)?.is_empty());
    repo.write("../../tracked.rs", &function("outside", "VECTOR_WEST"))?;
    repo.write(
        "../../outside_new.rs",
        &function("outside_new", "VECTOR_EAST"),
    )?;
    assert_eq!(engine.refresh()?["filesUpdated"], 0);
    assert!(engine.cross_search(None, &dirty)?.is_empty());
    repo.write(
        "tracked.rs",
        &(function("edited", "VECTOR_NORTH") + &function("untouched", "VECTOR_MID")),
    )?;
    repo.git(&["add", "tracked.rs"])?;
    repo.write("peer.rs", &function("peer", "VECTOR_MID"))?;
    repo.write("nested/new.rs", &function("untracked", "VECTOR_EAST"))?;
    assert_eq!(engine.refresh()?["filesUpdated"], 3);
    assert_eq!(
        sources(&engine.cross_search(None, &dirty)?),
        strings(&["edited", "untouched", "peer", "untracked"])
    );
    let changed = engine.cross_search(None, &since)?;
    assert_eq!(sources(&changed), strings(&["edited", "peer", "untracked"]));
    assert_eq!(engine.status()?["fileCount"], 3);
    for row in &changed {
        assert_eq!(row["source"]["sourceMode"], "working-tree");
        assert!(matches!(
            row["source"]["path"].as_str(),
            Some("tracked.rs" | "peer.rs" | "nested/new.rs")
        ));
    }
    let calls = mock.count();
    repo.git(&["add", "."])?;
    repo.git(&["commit", "--quiet", "-m", "commit package changes"])?;
    let head = repo.git(&["rev-parse", "HEAD"])?;
    engine.refresh()?;
    assert_eq!(engine.status()?["gitCheckpoint"], head);
    assert!(
        engine.cross_search(None, &dirty)?.is_empty(),
        "outside-root dirty paths do not mark package files uncommitted"
    );
    let committed = engine.cross_search(None, &since)?;
    assert_eq!(
        sources(&committed),
        strings(&["edited", "peer", "untracked"])
    );
    assert!(committed.iter().all(|r| r["source"]["sourceMode"] == "git"));
    let mut since_head = since.clone();
    since_head["changedSince"] = json!(head);
    assert!(engine.cross_search(None, &since_head)?.is_empty());
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn regression_failed_reindex_files_callables_reuses_completed_descriptions_after_generation_change()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    repo.write(".slopdex/config.json", &config.to_string())?;
    let source = function("alpha", "VECTOR_EAST") + &function("beta", "VECTOR_MID");
    repo.write("code.rs", &source)?;
    repo.write("guide.md", "# Guide\n\nOriginal context.\n")?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let before = engine.search("east", "search", &all())?;
    repo.write("code.rs", &(source + "\nconst NEW_CONTEXT: i32 = 7;\n"))?;
    engine.refresh()?;
    let status = engine.status()?;
    let stale_file = file_record(&repo, "code.rs")?;
    assert_eq!(status["staleFileDescriptionCount"], 1);
    assert_eq!(mock.requests("/responses").len(), 3);
    repo.db()?.execute_batch(
        "CREATE TRIGGER regression_reindex_abort BEFORE INSERT ON search_units
         WHEN NEW.path='code.rs' BEGIN SELECT RAISE(ABORT, 'fixture forced reindex failure'); END;",
    )?;
    drop(engine);
    let failed = repo.cli(&["--no-reindex", "index", "reindex-files", "--callables"])?;
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("fixture forced reindex failure"),
        "CLI did not reach the forced publication failure: {stderr}"
    );
    assert_eq!(
        mock.requests("/responses").len(),
        6,
        "file and both forced callable descriptions completed before publication failed"
    );
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.status()?, status);
    assert_eq!(file_record(&repo, "code.rs")?, stale_file);
    assert_eq!(engine.search("east", "search", &all())?, before);
    repo.db()?
        .execute_batch("DROP TRIGGER regression_reindex_abort")?;
    // Move generation independently of the failed file's source. A force cache
    // key tied to generation instead of source would repeat the completed calls.
    repo.write("guide.md", "# Guide\n\nUpdated context.\n")?;
    assert_eq!(engine.refresh()?["filesUpdated"], 1);
    assert!(
        engine.status()?["generation"].as_u64().unwrap() > status["generation"].as_u64().unwrap()
    );
    assert_eq!(engine.status()?["staleFileDescriptionCount"], 1);
    let calls = mock.count();
    drop(engine);
    assert_eq!(
        repo.cli_json(&["--no-reindex", "index", "reindex-files", "--callables"])?["filesReindexed"],
        1
    );
    assert_eq!(
        mock.count(),
        calls,
        "rerun must reuse completed forced descriptions and embeddings"
    );
    let engine = repo.open(&config)?;
    assert_eq!(engine.status()?["staleFileDescriptionCount"], 0);
    let file = file_record(&repo, "code.rs")?;
    assert_eq!(file["description_hash"], file["hash"]);
    assert_ne!(file["description"], stale_file["description"]);
    let after = engine.search("east", "search", &all())?;
    for name in ["alpha", "beta"] {
        assert_ne!(
            row(&after, name)["function"]["description"],
            row(&before, name)["function"]["description"]
        );
    }
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn cli_unindexed_map_matches_indexed_filters_expansion_and_rendering_without_artifacts()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    assert!(!repo.root.join(".git").exists());
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    config["rerankingEnabled"] = json!(true);
    config["include"] = json!(["**/*.py", "**/*.ts", "**/*.md"]);
    config["exclude"] = json!(["excluded/**"]);
    config["maxFileSize"] = json!(2048);
    repo.write(".gitignore", "ignored.py\n")?;
    repo.write(".ignore", "rule-ignored.py\n")?;
    repo.write("src/.gitignore", "nested-ignored.py\n")?;
    repo.write("a.py", "from b import middle\n\ndef outer():\n    return middle()\n\ndef outermost():\n    return outer()\n")?;
    repo.write(
        "b.py",
        "from c import leaf\n\ndef middle():\n    return leaf()\n",
    )?;
    repo.write(
        "c.py",
        "from d import deepest\n\ndef leaf():\n    return deepest()\n",
    )?;
    repo.write("d.py", "def deepest():\n    return 42\n")?;
    repo.write("src/api.ts", "export class Api {\n  public run() { return 1; }\n  private stop() {}\n}\nfunction hidden() {}\n")?;
    repo.write("src/skip.ts", "export class Skip {}\n")?;
    for path in [
        "ignored.py",
        "rule-ignored.py",
        "src/nested-ignored.py",
        "excluded/ignored.py",
        "generated/ignored.py",
    ] {
        repo.write(path, "def ignored():\n    return 0\n")?;
    }
    repo.write("not-included.rs", "pub fn not_included() {}\n")?;
    for (name, size) in [("at_limit", 2048), ("over_limit", 2049)] {
        let mut source = format!("def {name}():\n    return 1\n");
        source.push_str(&" ".repeat(size - source.len()));
        repo.write(&format!("{name}.py"), &source)?;
    }
    let body = format!("{}End of selected body.", "Café 🚀 ".repeat(50));
    let markdown = format!(
        "Guide\n=====\nGuide intro.\n\n## Setup\n{body}\n\n### Details\n```md\n# Literal heading\n```\nDetails body.\n\n## Other\nOther body.\n"
    );
    repo.write("docs/guide.md", &markdown.replace('\n', "\r\n"))?;

    let call_args = [
        "map",
        "b.py",
        "-g",
        "b.py",
        "-e",
        "^MIDDLE$",
        "-i",
        "-k",
        "fns",
        "--callers",
        "2",
        "--callees",
        "2",
        "--expand-callers",
        "1",
        "--expand-callees",
        "1",
    ];
    let cases: [(&str, &[&str]); 8] = [
        ("json", &["map", "--private", "-k", "fns,types,headings"]),
        (
            "json",
            &[
                "map",
                "src",
                "-g",
                "**/*.ts",
                "-g",
                "!**/skip.ts",
                "-e",
                "^API\\.RUN$",
                "-e",
                "^absent$",
                "-i",
                "-k",
                "methods",
                "-k",
                "headings",
            ],
        ),
        ("json", &call_args),
        ("text", &call_args),
        (
            "text",
            &[
                "map",
                "docs",
                "-g",
                "**/*.md",
                "-e",
                "^GUIDE\\.SETUP$",
                "-i",
                "-k",
                "headings",
                "--detail",
                "expanded",
            ],
        ),
        ("text", &["map", "docs", "--detail", "expanded"]),
        ("json", &["map", "src/api.ts"]),
        (
            "json",
            &[
                "map", "b.py", "-e", "^middle$", "-k", "fns", "--detail", "expanded",
            ],
        ),
    ];
    let mut expected = Vec::new();
    for selection in ["default", "explicit", "config"] {
        if selection == "config" {
            config["indexPath"] = json!("uncreated/configured.sqlite");
        }
        repo.write(".slopdex/config.json", &config.to_string())?;
        let explicit = Path::new("uncreated/explicit.sqlite");
        let index = (selection == "explicit").then_some(explicit);
        let before = filesystem_snapshot(repo._temp.path())?;
        let cache_before = filesystem_snapshot(mock.cache.path())?;
        for no_reindex in [false, true] {
            for (i, (format, args)) in cases.iter().enumerate() {
                let mut command = isolated_cli(&repo, index);
                command.args(["--format", *format]);
                if no_reindex {
                    command.arg("--no-reindex");
                }
                let output = command.args(*args).output()?;
                ensure!(
                    output.status.success(),
                    "{selection}, {args:?}, {no_reindex}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if selection == "default" && !no_reindex {
                    expected.push(output.stdout);
                } else {
                    assert_eq!(
                        output.stdout, expected[i],
                        "{selection}, {args:?}, {no_reindex}"
                    );
                }
                assert_eq!(filesystem_snapshot(repo._temp.path())?, before);
                assert_eq!(filesystem_snapshot(mock.cache.path())?, cache_before);
                assert_eq!(mock.count(), 0);
            }
        }
    }

    let all_rows: Value = serde_json::from_slice(&expected[0])?;
    let paths: BTreeSet<_> = all_rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from([
            "a.py",
            "at_limit.py",
            "b.py",
            "c.py",
            "d.py",
            "docs/guide.md",
            "src/api.ts",
            "src/skip.ts"
        ])
    );
    let all_names = map_names(all_rows.as_array().unwrap());
    for name in ["at_limit", "hidden", "stop"] {
        assert!(all_names.contains(name), "missing {name}");
    }
    let selected: Value = serde_json::from_slice(&expected[1])?;
    assert_eq!(
        map_names(selected.as_array().unwrap()),
        strings(&["Api", "run"])
    );
    let expanded: Value = serde_json::from_slice(&expected[2])?;
    assert_eq!(
        map_names(expanded.as_array().unwrap()),
        strings(&["outermost", "outer", "middle", "leaf", "deepest"])
    );
    let expanded_text = String::from_utf8(expected[3].clone())?;
    for name in ["outer", "leaf"] {
        assert!(
            expanded_text.contains(&format!("@ code:\ndef {name}():")),
            "{expanded_text}"
        );
    }
    assert!(
        expanded_text.contains("\ndef middle():\n  # callees: c.py:3-4:leaf\n"),
        "{expanded_text}"
    );
    for name in ["outermost", "middle", "deepest"] {
        assert!(
            !expanded_text.contains(&format!("@ code:\ndef {name}():")),
            "{expanded_text}"
        );
    }
    let filtered_md = String::from_utf8(expected[4].clone())?;
    assert!(filtered_md.contains("Guide\n=====") && filtered_md.contains(&body));
    for omitted in [
        "Guide intro.",
        "### Details",
        "Details body.",
        "Other body.",
    ] {
        assert!(!filtered_md.contains(omitted), "{omitted}");
    }
    let full_md = String::from_utf8(expected[5].clone())?;
    for text in [
        body.as_str(),
        "Guide intro.",
        "```md\n# Literal heading\n```\nDetails body.",
        "Other body.",
    ] {
        assert_eq!(full_md.matches(text).count(), 1, "{text}");
    }
    let public: Value = serde_json::from_slice(&expected[6])?;
    assert_eq!(
        map_names(public.as_array().unwrap()),
        strings(&["Api", "run"])
    );
    let default_expansion: Value = serde_json::from_slice(&expected[7])?;
    assert_eq!(
        map_names(default_expansion.as_array().unwrap()),
        strings(&["outer", "middle", "leaf"])
    );

    // Engine::open_map still creates structure-only fixtures. Compare CLI output
    // byte for byte with that indexed path, including persisted offline reads.
    repo.open_map(&config)?.refresh_structure()?;
    for no_reindex in [false, true] {
        for (i, (format, args)) in cases.iter().enumerate() {
            let mut command = isolated_cli(&repo, Some(&repo.index));
            command.args(["--format", *format]);
            if no_reindex {
                command.arg("--no-reindex");
            }
            let output = command.args(*args).output()?;
            ensure!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, expected[i], "indexed {args:?}, {no_reindex}");
        }
    }
    assert_eq!(mock.count(), 0);
    Ok(())
}

#[test]
fn cli_unindexed_map_requires_update_before_search_and_existing_indexes_refresh() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("api.rs", "pub fn alpha() -> i32 { 42 }\n")?;
    let before = filesystem_snapshot(repo._temp.path())?;
    for no_reindex in [false, true] {
        let mut command = isolated_cli(&repo, None);
        command.args(["--format", "json"]);
        if no_reindex {
            command.arg("--no-reindex");
        }
        let output = command.arg("map").output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(map_names(rows.as_array().unwrap()), strings(&["alpha"]));
        let output = quick_output(isolated_cli(&repo, None).args(["search", "east"]))?;
        assert_missing_index(&output);
        assert_eq!(filesystem_snapshot(repo._temp.path())?, before);
        assert_eq!(mock.count(), 0);
    }
    assert!(!repo.root.join(".slopdex").exists());

    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    let update = isolated_cli(&repo, None).args(["update"]).output()?;
    ensure!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&update.stdout)?["filesUpdated"],
        1
    );
    assert!(
        !repo.root.join(".git").exists(),
        "update also works without Git"
    );
    let search = || -> Result<Value> {
        let output = isolated_cli(&repo, None)
            .args([
                "--format",
                "json",
                "search-code",
                "east",
                "--threshold",
                "-1",
            ])
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    assert_eq!(names(search()?.as_array().unwrap()), strings(&["alpha"]));
    let calls = mock.count();
    repo.write("new.rs", "pub fn beta() -> i32 { 7 }\n")?;
    let output = isolated_cli(&repo, None)
        .args(["--no-reindex", "--format", "json", "map"])
        .output()?;
    ensure!(output.status.success());
    let rows: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(map_names(rows.as_array().unwrap()), strings(&["alpha"]));
    let output = isolated_cli(&repo, None)
        .args(["--format", "json", "map"])
        .output()?;
    ensure!(output.status.success());
    let rows: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        map_names(rows.as_array().unwrap()),
        strings(&["alpha", "beta"])
    );
    assert_eq!(mock.count(), calls, "indexed map refresh stays local");
    assert_eq!(
        names(search()?.as_array().unwrap()),
        strings(&["alpha", "beta"])
    );
    assert!(
        mock.count() > calls,
        "existing search automatically prepares new code"
    );
    Ok(())
}

#[test]
fn map_publishes_schema3_structure_without_models_or_vector_sidecars_and_reopens_offline()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    config["rerankingEnabled"] = json!(true);
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("src/api.ts", "export class Api {\n  run(value: string): string {\n    const BODY_ONLY_SENTINEL = value;\n    return BODY_ONLY_SENTINEL;\n  }\n}\n")?;
    repo.write(
        "guide.md",
        "# Guide\n\nMARKDOWN_BODY_SENTINEL\n\n## Usage\n\nExamples.\n",
    )?;
    let mut engine = repo.open_map(&config)?;
    assert_eq!(engine.refresh_structure()?["filesUpdated"], 2);
    let expected = engine.map(&json!({}))?;
    assert_eq!(expected.len(), 2);
    assert_eq!(
        map_names(&expected),
        strings(&["Api", "run", "Guide", "Usage"])
    );
    let serialized = serde_json::to_string(&expected)?;
    assert!(!serialized.contains("BODY_ONLY_SENTINEL"));
    assert!(!serialized.contains("MARKDOWN_BODY_SENTINEL"));
    for node in expected
        .iter()
        .flat_map(|row| row["nodes"].as_array().unwrap())
    {
        for field in [
            "source",
            "content",
            "embeddingInput",
            "embedding",
            "description",
        ] {
            assert!(node.get(field).is_none(), "map exposes {field}: {node}");
        }
    }
    let db = repo.db()?;
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        3
    );
    let identity: String =
        db.query_row("SELECT value FROM metadata WHERE key='identity'", [], |r| {
            r.get(0)
        })?;
    assert_eq!(
        serde_json::from_str::<Value>(&identity)?,
        json!({"schema":3,"root":repo.root.canonicalize()?})
    );
    assert!(db.query_row("SELECT count(*) FROM symbols", [], |r| r.get::<_, i64>(0))? >= 4);
    assert!(
        db.query_row("SELECT count(*) FROM search_units", [], |r| r
            .get::<_, i64>(0))?
            > 0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM embeddings", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM unit_embeddings", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    assert!(
        db.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none()
    );
    drop(engine);

    repo.write("src/api.ts", "export function not_indexed() {}\n")?;
    fs::remove_file(repo.root.join("guide.md"))?;
    // Neither the parse cache nor compatibility JSON is an authoritative map source.
    db.execute_batch("DELETE FROM cache WHERE kind='parse'; UPDATE files SET data='{}';")?;
    config["noReindex"] = json!(true);
    let mut engine = repo.open_map(&config)?;
    assert_eq!(engine.refresh_structure()?["skipped"], true);
    assert_eq!(engine.map(&json!({}))?, expected);
    assert_eq!(
        engine.map(&json!({"paths":["guide.md"]}))?,
        vec![expected[0].clone()]
    );
    drop(engine);
    assert_eq!(repo.cli_json(&["--no-reindex", "map"])?, json!(expected));
    let refreshed = repo.cli_json(&["map", "src"])?;
    assert_eq!(
        map_names(refreshed.as_array().unwrap()),
        strings(&["not_indexed"])
    );
    assert_eq!(mock.count(), 0);
    for kind in ["code", "markdown", "descriptions", "combined"] {
        let path = PathBuf::from(format!("{}.{kind}.usearch", repo.index.display()));
        assert!(
            !path.exists(),
            "map created a vector sidecar: {}",
            path.display()
        );
        assert!(!PathBuf::from(format!("{}.manifest.json", path.display())).exists());
    }
    Ok(())
}

#[test]
fn map_call_graph_expands_cross_file_chains_and_rebuilds_after_edits() -> Result<()> {
    let repo = Repo::new()?;
    repo.write(
        "a.py",
        "from b import middle\n\ndef outer():\n    return middle()\n\ndef outermost():\n    return outer()\n",
    )?;
    repo.write(
        "b.py",
        "from c import leaf\n\ndef middle():\n    return leaf()\n",
    )?;
    repo.write(
        "c.py",
        "from d import deepest\n\ndef leaf():\n    return deepest()\n",
    )?;
    repo.write("d.py", "def deepest():\n    return 1\n")?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    let selected = json!({"kinds":["functions"],"regexp":["^middle$"]});
    assert_eq!(map_names(&engine.map(&selected)?), strings(&["middle"]));
    let options = json!({"kinds":["functions"],"regexp":["^middle$"],"glob":["b.py"],"callers":1,"callees":1});
    let rows = engine.map(&options)?;
    assert_eq!(map_names(&rows), strings(&["outer", "middle", "leaf"]));
    let middle = rows
        .iter()
        .find(|row| row["path"] == "b.py")
        .context("middle file")?;
    assert_eq!(middle["nodes"][0]["callees"][0], "c.py:3-4:leaf");
    let outer = rows
        .iter()
        .find(|row| row["path"] == "a.py")
        .context("outer file")?;
    assert_eq!(outer["nodes"][0]["callees"][0], "b.py:3-4:middle");
    let two_levels = engine.map(&json!({"kinds":["functions"],"regexp":["^middle$"],"glob":["b.py"],"callers":2,"callees":2}))?;
    assert_eq!(
        map_names(&two_levels),
        strings(&["outermost", "outer", "middle", "leaf", "deepest"])
    );
    drop(engine);
    let json_rows = repo.cli_json(&[
        "--no-reindex",
        "map",
        "b.py",
        "-k",
        "fns",
        "-e",
        "^middle$",
        "--callers",
        "1",
        "--callees",
        "1",
    ])?;
    assert_eq!(
        map_names(json_rows.as_array().unwrap()),
        strings(&["outer", "middle", "leaf"])
    );
    repo.write("b.py", "def middle():\n    return 2\n")?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    assert_eq!(
        map_names(&engine.map(&options)?),
        strings(&["outer", "middle"])
    );
    Ok(())
}

#[test]
fn expanded_call_depths_force_code_only_within_their_own_levels() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write("a.py", "from b import middle\n\ndef outer():\n    return middle()\n\ndef outermost():\n    return outer()\n")?;
    repo.write(
        "b.py",
        "from c import leaf\n\ndef middle():\n    return leaf()\n",
    )?;
    repo.write(
        "c.py",
        "from d import deepest\n\ndef leaf():\n    return deepest()\n",
    )?;
    repo.write("d.py", "def deepest():\n    return 1\n")?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    let just_expanded = engine
        .map(&json!({"kinds":["fns"],"regexp":["^middle$"],"expandCallers":2,"expandCallees":1}))?;
    assert_eq!(
        map_names(&just_expanded),
        strings(&["outermost", "outer", "middle", "leaf"])
    );
    drop(engine);

    let text = |command: &str, extra: &[&str]| -> Result<String> {
        let output = repo
            .child(env!("CARGO_BIN_EXE_slopdex"))
            .arg("--root")
            .arg(&repo.root)
            .arg("--index")
            .arg(&repo.index)
            .args(["--no-reindex", "--expand-code-threshold", "1"])
            .arg(command)
            .args(extra)
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    };
    let map = text(
        "map",
        &[
            "b.py",
            "-k",
            "fns",
            "-e",
            "^middle$",
            "--callers",
            "2",
            "--expand-callers",
            "1",
            "--callees",
            "2",
            "--expand-callees",
            "1",
        ],
    )?;
    for name in ["outermost", "outer", "middle", "leaf", "deepest"] {
        assert!(map.contains(&format!("def {name}():")), "{map}");
    }
    assert!(map.contains("@ code:\ndef outer():"), "{map}");
    assert!(map.contains("@ code:\ndef leaf():"), "{map}");
    for name in ["outermost", "middle", "deepest"] {
        assert!(!map.contains(&format!("@ code:\ndef {name}():")), "{map}");
    }
    let reversed = text(
        "map",
        &[
            "b.py",
            "-k",
            "fns",
            "-e",
            "^middle$",
            "--callers",
            "1",
            "--expand-callers",
            "2",
            "--callees",
            "0",
            "--expand-callees",
            "1",
        ],
    )?;
    assert!(reversed.contains("@ code:\ndef outermost():"), "{reversed}");
    assert!(reversed.contains("@ code:\ndef leaf():"), "{reversed}");
    assert!(!reversed.contains("def deepest():"), "{reversed}");
    let overlapping_roots = repo.cli_json(&[
        "map",
        "-k",
        "fns",
        "-e",
        "^outer$",
        "-e",
        "^middle$",
        "--callers",
        "0",
        "--expand-callees",
        "1",
    ])?;
    let middle = overlapping_roots
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == "b.py")
        .context("middle file")?;
    assert_eq!(middle["nodes"][0]["callDepth"]["callee"], 0);
    assert_eq!(middle["nodes"][0]["expandedCode"], true);
    assert!(
        middle["nodes"][0]["source"]
            .as_str()
            .unwrap()
            .contains("return leaf()")
    );

    repo.cli_json(&[
        "search-code",
        "middle",
        "-e",
        "^middle$",
        "--threshold",
        "-1",
    ])?;
    let related = repo.cli_json(&[
        "--expand-code-threshold",
        "1",
        "search-code",
        "middle",
        "-e",
        "^middle$",
        "--threshold",
        "-1",
        "--expand-callers",
        "1",
        "--expand-callees",
        "1",
    ])?;
    let related = related[0]["relatedCallables"].as_array().unwrap();
    assert!(related.iter().any(|item| {
        item["node"]["name"] == "outer"
            && item["node"]["expandedCode"] == true
            && item["node"]["source"]
                .as_str()
                .unwrap()
                .contains("return middle()")
    }));
    assert!(related.iter().any(|item| {
        item["node"]["name"] == "leaf"
            && item["node"]["expandedCode"] == true
            && item["node"]["source"]
                .as_str()
                .unwrap()
                .contains("return deepest()")
    }));
    let search = text(
        "search-code",
        &[
            "middle",
            "-e",
            "^middle$",
            "--threshold",
            "-1",
            "--callers",
            "2",
            "--expand-callers",
            "1",
            "--callees",
            "2",
            "--expand-callees",
            "1",
        ],
    )?;
    assert!(search.contains("@ code:\ndef outer():"), "{search}");
    assert!(search.contains("@ code:\ndef leaf():"), "{search}");
    assert!(!search.contains("@ code:\ndef middle():"), "{search}");
    assert!(!search.contains("@ code:\ndef outermost():"), "{search}");
    assert!(!search.contains("@ code:\ndef deepest():"), "{search}");

    let clusters = text(
        "cross-search",
        &[
            "--lines",
            "1",
            "--threshold",
            "-1",
            "--matches",
            "1",
            "--expand-callers",
            "1",
            "--expand-callees",
            "1",
        ],
    )?;
    assert!(clusters.contains("a.py:\n  3-4:outer\n"), "{clusters}");
    assert!(!clusters.contains("@ code:"), "{clusters}");
    repo.cli_json(&[
        "describe",
        "middle",
        "-e",
        "^middle$",
        "--threshold",
        "-1",
        "--expand-callers",
        "1",
        "--callees",
        "0",
        "--expand-code-threshold",
        "1",
    ])?;
    let responses = mock.requests("/responses");
    let prompt = responses.last().unwrap().body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(prompt.contains("@ code:\ndef outer():"), "{prompt}");
    assert!(!prompt.contains("@ code:\ndef middle():"), "{prompt}");
    Ok(())
}

#[test]
fn describe_prompt_defaults_to_two_expanded_call_levels_with_explicit_overrides() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write("a.py", "from b import middle\n\ndef outer():\n    return middle()\n\ndef outermost():\n    return outer()\n")?;
    repo.write(
        "b.py",
        "from c import leaf\n\ndef middle():\n    return leaf()\n",
    )?;
    repo.write(
        "c.py",
        "from d import deepest\n\ndef leaf():\n    return deepest()\n",
    )?;
    repo.write("d.py", "def deepest():\n    return 1\n")?;
    repo.cli_json(&["update"])?;
    let base = [
        "describe",
        "middle",
        "-e",
        "^middle$",
        "--threshold",
        "-1",
        "--expand-code-threshold",
        "1",
    ];
    let result = repo.cli_json(&base)?;
    assert!(result["functions"][0].get("relatedCallables").is_none());
    let requests = mock.requests("/responses");
    let prompt = requests.last().unwrap().body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    for name in ["outer", "outermost", "leaf", "deepest"] {
        assert!(
            prompt.contains(&format!("@ code:\ndef {name}():")),
            "{prompt}"
        );
    }
    assert!(!prompt.contains("@ code:\ndef middle():"), "{prompt}");

    let mut disabled = base.to_vec();
    disabled.extend(["--expand-callers", "0", "--expand-callables", "0"]);
    repo.cli_json(&disabled)?;
    let requests = mock.requests("/responses");
    let prompt = requests.last().unwrap().body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(!prompt.contains("@ code:"), "{prompt}");

    let mut one_callee = base.to_vec();
    one_callee.extend(["--expand-callers", "0", "--expand-callables", "1"]);
    repo.cli_json(&one_callee)?;
    let requests = mock.requests("/responses");
    let prompt = requests.last().unwrap().body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(prompt.contains("@ code:\ndef leaf():"), "{prompt}");
    assert!(!prompt.contains("@ code:\ndef deepest():"), "{prompt}");
    assert!(!prompt.contains("@ code:\ndef outer():"), "{prompt}");
    Ok(())
}

#[test]
fn call_graph_depths_handle_cycles_and_ambiguous_calls() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("chain.py", "def first():\n    return second()\n\ndef second():\n    return third()\n\ndef third():\n    return first()\n")?;
    repo.write("other.py", "def third():\n    return 0\n")?;
    repo.write("unknown.py", "def unknown():\n    return third()\n")?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    let rows = engine.map(&json!({"regexp":["^first$"],"kinds":["fns"],"callees":2}))?;
    assert_eq!(map_names(&rows), strings(&["first", "second", "third"]));
    let unresolved = engine.map(&json!({"regexp":["^unknown$"],"kinds":["fns"],"callees":2}))?;
    assert_eq!(map_names(&unresolved), strings(&["unknown"]));
    assert_eq!(rows.len(), 1);
    let rows = engine
        .map(&json!({"regexp":["^third$"],"glob":["chain.py"],"kinds":["fns"],"callers":2}))?;
    assert_eq!(map_names(&rows), strings(&["first", "second", "third"]));
    drop(engine);
    let rows = repo.cli_json(&[
        "--no-reindex",
        "map",
        "chain.py",
        "-e",
        "^first$",
        "-k",
        "fns",
        "--callees",
        "1",
    ])?;
    assert_eq!(
        map_names(rows.as_array().unwrap()),
        strings(&["first", "second"])
    );
    Ok(())
}

#[test]
fn call_graph_resolves_explicit_js_imports_and_rust_module_calls() -> Result<()> {
    let repo = Repo::new()?;
    repo.write(
        "src/a.ts",
        "import { run as execute } from './b';\nexport function entry() { execute(); }\n",
    )?;
    repo.write("src/b.ts", "export function run() { return 1; }\n")?;
    repo.write(
        "src/namespace.ts",
        "import * as module from './b';\nexport function delegate() { module.run(); }\n",
    )?;
    repo.write(
        "src/lib.rs",
        "mod util;\nuse crate::util::work;\npub fn start() { work(); }\n",
    )?;
    repo.write("src/util.rs", "pub fn work() {}\n")?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    let js = engine.map(&json!({"kinds":["fns"],"regexp":["^entry$"],"callees":1}))?;
    assert_eq!(map_names(&js), strings(&["entry", "run"]));
    let namespace = engine.map(&json!({"kinds":["fns"],"regexp":["^delegate$"],"callees":1}))?;
    assert_eq!(map_names(&namespace), strings(&["delegate", "run"]));
    let rust = engine.map(&json!({"kinds":["fns"],"regexp":["^start$"],"callees":1}))?;
    assert_eq!(map_names(&rust), strings(&["start", "work"]));
    Ok(())
}

#[test]
fn call_graph_ignores_type_only_overload_declarations() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("overloads.ts", "export function run(x: string): string;\nexport function run(x: string) { return x; }\nexport function main() { return run('a'); }\n")?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    let rows = engine.map(&json!({"kinds":["fns"],"regexp":["^main$"],"callees":1}))?;
    assert_eq!(map_names(&rows), strings(&["main", "run"]));
    assert_eq!(
        rows[0]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["name"] == "run")
            .count(),
        1
    );
    Ok(())
}

#[test]
fn call_graph_tracks_constructor_calls() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("classes.py", "class Worker:\n    def __init__(self):\n        pass\n\ndef build():\n    return Worker()\n")?;
    repo.write(
        "objects.ts",
        "export class Box { constructor() {} }\nexport function create() { return new Box(); }\n",
    )?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    let python = engine.map(&json!({"regexp":["^build$"],"kinds":["fns"],"callees":1}))?;
    assert_eq!(
        map_names(&python),
        strings(&["build", "Worker", "__init__"])
    );
    let js = engine.map(&json!({"regexp":["^create$"],"kinds":["fns"],"callees":1}))?;
    assert_eq!(map_names(&js), strings(&["create", "Box", "constructor"]));
    Ok(())
}

#[test]
fn callable_search_json_and_text_include_associated_callables() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write(
        "a.py",
        "from b import middle\n\ndef outer():\n    return middle()\n",
    )?;
    repo.write("b.py", "def middle():\n    return 1\n")?;
    repo.cli_json(&["update"])?;
    let rows = repo.cli_json(&[
        "search-code",
        "middle",
        "-e",
        "^middle$",
        "--threshold",
        "-1",
        "--callers",
        "1",
    ])?;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["relatedCallables"][0]["node"]["name"], "outer");
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--index")
        .arg(&repo.index)
        .args([
            "--no-reindex",
            "search-code",
            "middle",
            "-e",
            "^middle$",
            "--threshold",
            "-1",
            "--callers",
            "1",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    assert!(
        text.contains("*** a.py")
            && text.contains("\ndef outer():\n  # callees: b.py:1-2:middle\n"),
        "{text}"
    );
    assert!(
        text.contains("*** b.py") && text.contains("def middle():"),
        "{text}"
    );
    let map_text = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--index")
        .arg(&repo.index)
        .args([
            "--no-reindex",
            "map",
            "b.py",
            "-e",
            "^middle$",
            "-k",
            "fns",
            "--callers",
            "1",
        ])
        .output()?;
    ensure!(
        map_text.status.success(),
        "{}",
        String::from_utf8_lossy(&map_text.stderr)
    );
    let map_text = String::from_utf8(map_text.stdout)?;
    assert!(
        map_text.contains("\ndef outer():\n  # callees: b.py:1-2:middle\n"),
        "{map_text}"
    );
    let describe = repo.cli_json(&[
        "describe",
        "middle",
        "-e",
        "^middle$",
        "--threshold",
        "-1",
        "--callers",
        "1",
    ])?;
    assert!(
        describe["functions"]
            .as_array()
            .is_some_and(|fs| fs.iter().any(|f| f["relatedCallables"]
                .as_array()
                .is_some_and(|related| related.iter().any(|r| r["node"]["name"] == "outer"))))
    );
    let cross = repo.cli(&[
        "cross-search",
        "--lines",
        "1",
        "--threshold",
        "-1",
        "--matches",
        "1",
        "--callers",
        "1",
    ])?;
    ensure!(
        cross.status.success(),
        "{}",
        String::from_utf8_lossy(&cross.stderr)
    );
    let cross_rows: Vec<Value> = String::from_utf8(cross.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<_>>()?;
    assert!(cross_rows.iter().any(|row| {
        row["relatedCallables"]
            .as_array()
            .is_some_and(|related| !related.is_empty())
            || row["matches"].as_array().is_some_and(|matches| {
                matches.iter().any(|m| {
                    m["relatedCallables"]
                        .as_array()
                        .is_some_and(|related| !related.is_empty())
                })
            })
    }));
    let cluster = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--index")
        .arg(&repo.index)
        .args([
            "--no-reindex",
            "cross-search",
            "--lines",
            "1",
            "--threshold",
            "-1",
            "--matches",
            "1",
            "--callers",
            "1",
        ])
        .output()?;
    ensure!(
        cluster.status.success(),
        "{}",
        String::from_utf8_lossy(&cluster.stderr)
    );
    let cluster = String::from_utf8(cluster.stdout)?;
    assert!(
        cluster.contains("a.py:3-4:outer\nb.py:1-2:middle\n"),
        "{cluster}"
    );
    assert!(!cluster.contains("callees:"), "{cluster}");
    Ok(())
}

#[test]
fn expanded_detail_defaults_to_one_edge_and_includes_high_similarity_code() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write(
        "a.py",
        "from b import middle\n\ndef outer():\n    return middle()\n",
    )?;
    repo.write("b.py", "def middle():\n    return 42\n")?;

    let expanded = repo.cli_json(&[
        "--detail", "expanded", "map", "b.py", "-e", "^middle$", "-k", "fns",
    ])?;
    assert!(
        expanded
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == "a.py")
    );
    let disabled = repo.cli_json(&[
        "--detail",
        "expanded",
        "map",
        "b.py",
        "-e",
        "^middle$",
        "-k",
        "fns",
        "--callers",
        "0",
        "--callees",
        "0",
    ])?;
    assert_eq!(disabled.as_array().unwrap().len(), 1);

    let query = [
        "search-code",
        "middle",
        "-e",
        "^middle$",
        "--threshold",
        "-1",
    ];
    repo.cli_json(&["update"])?;
    let compact = repo.cli_json(&query)?;
    assert!(compact[0].get("relatedCallables").is_none());
    let expanded_json = repo.cli_json(&[
        "--detail",
        "expanded",
        "search-code",
        "middle",
        "-e",
        "^middle$",
        "--threshold",
        "-1",
    ])?;
    assert_eq!(
        expanded_json[0]["relatedCallables"][0]["node"]["name"],
        "outer"
    );
    let run = |threshold: &str| -> Result<String> {
        let output = repo
            .child(env!("CARGO_BIN_EXE_slopdex"))
            .arg("--root")
            .arg(&repo.root)
            .arg("--index")
            .arg(&repo.index)
            .args([
                "--detail",
                "expanded",
                "--expand-code-threshold",
                threshold,
                "search-code",
                "middle",
                "-e",
                "^middle$",
                "--threshold",
                "-1",
            ])
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    };
    let with_code = run("-1")?;
    assert!(
        with_code.contains("@ code:\ndef middle():\n    return 42"),
        "{with_code}"
    );
    assert!(
        with_code.contains("\ndef outer():\n  # callees: b.py:1-2:middle\n"),
        "{with_code}"
    );
    let without_code = run("1")?;
    assert!(!without_code.contains("@ code:"), "{without_code}");
    let score = expanded_json[0]["similarity"].as_f64().unwrap();
    let at_score = run(&score.to_string())?;
    assert!(!at_score.contains("@ code:"), "{at_score}");
    let cross = |threshold: &str| -> Result<String> {
        let output = repo
            .child(env!("CARGO_BIN_EXE_slopdex"))
            .arg("--root")
            .arg(&repo.root)
            .arg("--index")
            .arg(&repo.index)
            .args([
                "--detail",
                "expanded",
                "--expand-code-threshold",
                threshold,
                "--format",
                "text",
                "cross-search",
                "--cohesion",
                "--lines",
                "1",
                "--threshold",
                "-1",
                "--matches",
                "1",
            ])
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    };
    assert!(cross("-1")?.contains("@ code:"));
    assert!(!cross("1")?.contains("@ code:"));
    Ok(())
}

#[test]
fn map_filters_keep_ancestors_without_siblings_and_cli_combines_paths_kinds_and_repeated_filters()
-> Result<()> {
    let repo = Repo::new()?;
    repo.write("src/api.ts", "export class Api {\n  run(): number { return 1; }\n  skip(): number { return 2; }\n}\nexport class Other {\n  run(): number { return 3; }\n}\n")?;
    repo.write("src/skip.ts", "export class Skip {}\n")?;
    repo.write("outside.ts", "export class Outside {}\n")?;
    repo.write(
        "docs/guide.md",
        "# Guide\n\n## Setup\n\nDetails.\n\n## Other\n\nOther details.\n",
    )?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    let options = json!({"paths":["src"], "glob":["**/*.ts", "!**/skip.ts"], "regexp":["^API\\.RUN$", "^absent$"], "ignoreCase":true, "kinds":["methods"]});
    let selected = engine.map(&options)?;
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["path"], "src/api.ts");
    let nodes = selected[0]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0]["name"], "Api");
    assert_eq!(nodes[1]["name"], "run");
    assert_eq!(nodes[1]["parentId"], nodes[0]["id"]);
    assert_eq!(nodes[1]["kind"], "method");
    let parent = engine.map(&json!({"paths":["src/api.ts"], "regexp":"^Api$", "kinds":"class"}))?;
    assert_eq!(
        map_names(&parent),
        strings(&["Api"]),
        "matching a parent must not expand its children"
    );
    let markdown =
        engine.map(&json!({"glob":"docs/**", "regexp":"^Guide\\.Setup$", "kinds":"headings"}))?;
    assert_eq!(map_names(&markdown), strings(&["Guide", "Setup"]));
    let md_nodes = markdown[0]["nodes"].as_array().unwrap();
    assert_eq!(md_nodes[1]["parentId"], md_nodes[0]["id"]);
    assert!(engine.map(&json!({"paths":["sr"]}))?.is_empty());
    assert!(engine.map(&json!({"paths":["../outside"]})).is_err());
    drop(engine);
    assert_eq!(
        repo.cli_json(&[
            "--no-reindex",
            "map",
            "src",
            "-g",
            "**/*.ts",
            "-g",
            "!**/skip.ts",
            "-e",
            "^API\\.RUN$",
            "-e",
            "^absent$",
            "-i",
            "-k",
            "methods",
            "-k",
            "headings"
        ])?,
        json!(selected)
    );
    Ok(())
}

#[test]
fn map_excludes_private_symbols_unless_requested() -> Result<()> {
    let repo = Repo::new()?;
    repo.write(
        "api.ts",
        "export class Api { public run() {} private stop() {} }\nexport function visible() {}\nfunction hidden() {}\n",
    )?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    assert_eq!(
        map_names(&engine.map(&json!({}))?),
        strings(&["Api", "run", "visible"])
    );
    assert_eq!(
        map_names(&engine.map(&json!({"private":true}))?),
        strings(&["Api", "run", "stop", "visible", "hidden"])
    );
    drop(engine);
    let rows = repo.cli_json(&["--no-reindex", "map", "--private"])?;
    assert_eq!(
        map_names(rows.as_array().unwrap()),
        strings(&["Api", "run", "stop", "visible", "hidden"])
    );
    Ok(())
}

#[test]
fn map_cli_closed_stdout_succeeds_for_text_and_json() -> Result<()> {
    let repo = Repo::new()?;
    // Long public signatures produce substantial output in both formats without
    // requiring a semantic index or any provider traffic.
    let source: String = (0..2048)
        .map(|i| format!("pub fn symbol_{i}_{}() {{}}\n", "x".repeat(128)))
        .collect();
    repo.write("large.rs", &source)?;
    let rows = repo.cli_json(&["map"])?;
    assert_eq!(rows.as_array().unwrap().len(), 1);

    for format in ["summary", "json"] {
        let mut command = repo.child(env!("CARGO_BIN_EXE_slopdex"));
        command
            .arg("--root")
            .arg(&repo.root)
            .arg("--index")
            .arg(&repo.index)
            .args(["--no-reindex", "--format", format, "map"]);
        let complete = command.output()?;
        ensure!(complete.status.success());
        assert!(complete.stdout.len() > 256 * 1024);
        assert!(complete.stderr.is_empty());

        // Close the only read end before spawning. This deterministically fails
        // the child's first write without sleeps or pipe-buffer-size assumptions.
        let (reader, writer) = std::io::pipe()?;
        drop(reader);
        let output = command.stdout(writer).stderr(Stdio::piped()).output()?;
        assert_eq!(
            output.status.code(),
            Some(0),
            "{format}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty(), "{format}: {:?}", output.stderr);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn map_cli_stdout_full_still_fails_for_text_and_json() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("api.rs", "pub fn api() {}\n")?;
    repo.cli_json(&["map"])?;

    for format in ["summary", "json"] {
        let output = repo
            .child(env!("CARGO_BIN_EXE_slopdex"))
            .arg("--root")
            .arg(&repo.root)
            .arg("--index")
            .arg(&repo.index)
            .args(["--no-reindex", "--format", format, "map"])
            .stdout(fs::OpenOptions::new().write(true).open("/dev/full")?)
            .output()?;
        assert_eq!(output.status.code(), Some(1), "{format}");
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("slopdex:"), "{format}: {stderr}");
        assert!(
            stderr.contains("No space left on device"),
            "{format}: {stderr}"
        );
    }
    Ok(())
}

#[test]
fn map_cli_defaults_to_terse_source_order_excerpts() -> Result<()> {
    let repo = Repo::new()?;
    repo.write(
        "api.rs",
        "pub struct Api { pub count: usize }\nimpl Api {\n  pub fn run(&self) { secret(); }\n}\n",
    )?;
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--index")
        .arg(&repo.index)
        .args(["map", "--private"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    assert!(
        text.starts_with("*** api.rs\n\n@@ 1 @@\npub struct Api\n"),
        "{text}"
    );
    assert!(
        text.contains("@@ 2-4 @@\nimpl Api\n  pub fn run(&self)\n"),
        "{text}"
    );
    assert!(!text.contains("secret") && !text.contains("implementation omitted"));
    Ok(())
}

#[test]
fn map_cli_does_not_fold_filtered_out_members_into_parent_range() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("pipe.ts", "export interface PipeAddress {\n  readonly read: string; readonly hidden: string\n  readonly write: string\n}\n")?;
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--index")
        .arg(&repo.index)
        .args(["map", "pipe.ts", "-e", "^PipeAddress\\.(read|write)$"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    assert!(
        text.contains("@@ 1-4 @@\nexport interface PipeAddress\n  readonly read: string\n"),
        "{text}"
    );
    assert!(
        text.contains("@@ 3 @@\n  readonly write: string\n"),
        "{text}"
    );
    assert!(!text.contains("hidden"));
    Ok(())
}

#[test]
fn map_cli_folds_markdown_heading_paths_from_indexed_source() -> Result<()> {
    let repo = Repo::new()?;
    repo.write(
        "guide.md",
        "Guide\n=====\n\n## Setup\nBody.\n\n### Advanced\nMore prose.\n",
    )?;
    repo.open_map(&json!({}))?.refresh_structure()?;
    // An offline map should still use the saved source when deciding whether
    // gaps between headings contain only whitespace.
    repo.write("guide.md", "# Changed\n")?;
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--index")
        .arg(&repo.index)
        .args(["--no-reindex", "map", "guide.md"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    assert!(
        text.contains("@@ 1-8 @@\nGuide\n=====\n  ## Setup\n"),
        "{text}"
    );
    assert!(text.contains("@@ 7-8 @@\n    ### Advanced\n"), "{text}");
    assert_eq!(text.matches("Guide").count(), 1);
    assert!(!text.contains("Body.") && !text.contains("Changed"));
    Ok(())
}

#[test]
fn map_cli_expanded_markdown_prints_indexed_bodies_once_and_respects_filters() -> Result<()> {
    let repo = Repo::new()?;
    let long_body = format!("{}End of long body.", "Café 🚀 ".repeat(1500));
    let source = format!(
        "Guide\n=====\nGuide intro.\n\n## Setup\n{long_body}\n\n### Details\n```md\n# Literal heading\n```\nDetails body.\n\n## Other\nOther body."
    );
    repo.write("guide.md", &source.replace('\n', "\r\n"))?;
    repo.open_map(&json!({}))?.refresh_structure()?;
    repo.write("guide.md", "# Changed\nChanged body.\n")?;

    let render = |args: &[&str]| -> Result<String> {
        let output = repo
            .child(env!("CARGO_BIN_EXE_slopdex"))
            .arg("--root")
            .arg(&repo.root)
            .arg("--index")
            .arg(&repo.index)
            .args(["--no-reindex", "map", "guide.md"])
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    };
    for detail in ["compact", "standard"] {
        let text = render(&["--detail", detail])?;
        assert!(!text.contains("Guide intro.") && !text.contains("End of long body."));
    }
    let expanded = render(&["--detail", "expanded"])?;
    assert!(expanded.contains(&long_body));
    for text in [
        "Guide\n=====",
        "Guide intro.",
        "## Setup",
        "End of long body.",
        "### Details",
        "```md\n# Literal heading\n```\nDetails body.",
        "## Other",
        "Other body.",
    ] {
        assert_eq!(expanded.matches(text).count(), 1, "{text}");
    }
    assert!(!expanded.contains("Changed"));
    let filtered = render(&["--detail", "expanded", "-e", "^Guide\\.Setup$"])?;
    assert!(filtered.contains("Guide\n=====") && filtered.contains(&long_body));
    for omitted in [
        "Guide intro.",
        "### Details",
        "Details body.",
        "Other body.",
    ] {
        assert!(!filtered.contains(omitted), "{omitted}");
    }
    Ok(())
}

#[test]
fn map_cli_warns_and_ignores_missing_paths() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("src/api.ts", "export function api() {}\n")?;
    repo.write("other.ts", "export function other() {}\n")?;
    repo.cli_json(&["map"])?;

    let absolute_missing = repo.root.join("absent-file.ts");
    let output = repo.cli(&[
        "--no-reindex",
        "map",
        "src",
        "absent-directory",
        absolute_missing.to_str().unwrap(),
    ])?;
    ensure!(
        output.status.success(),
        "map with missing paths failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["path"], "src/api.ts");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("warning: map path does not exist; ignoring: absent-directory"));
    assert!(stderr.contains(&absolute_missing.display().to_string()));

    let output = repo.cli(&["--no-reindex", "map", "absent-directory"])?;
    ensure!(output.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&output.stdout)?, json!([]));
    assert!(String::from_utf8(output.stderr)?.contains("absent-directory"));
    Ok(())
}

#[test]
fn map_then_semantic_prepare_and_structure_edit_never_reuses_stale_vectors() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write("code.rs", &function("alpha", "VECTOR_EAST"))?;
    let mut map = repo.open_map(&config)?;
    map.refresh_structure()?;
    assert_eq!(mock.count(), 0);
    drop(map);
    let mut engine = repo.open(&config)?;
    assert_incomplete(&engine);
    assert_eq!(
        mock.count(),
        0,
        "opening a pending index must not prepare it"
    );
    let prepared = engine.refresh()?;
    assert_eq!(prepared["filesUpdated"], 0);
    assert_eq!(prepared["filesPrepared"], 1);
    let before = engine.search("east", "search-code", &all())?;
    near(&before[0]["similarity"], 1.0);
    let id = before[0]["function"]["id"].clone();
    let calls = mock.count();
    drop(engine);

    repo.write("code.rs", &function("alpha", "BROKEN_EMBED VECTOR_NORTH"))?;
    let mut map = repo.open_map(&config)?;
    map.refresh_structure()?;
    assert_eq!(
        map_names(&map.map(&json!({"private":true}))?),
        strings(&["alpha"])
    );
    assert_eq!(mock.count(), calls);
    drop(map);
    let mut offline = config.clone();
    offline["noReindex"] = json!(true);
    let engine = repo.open(&offline)?;
    assert_incomplete(&engine);
    assert_eq!(
        mock.count(),
        calls,
        "cached search results cannot bypass readiness"
    );
    drop(engine);
    mock.fail_on(Some("BROKEN_EMBED"));
    let mut engine = repo.open(&config)?;
    assert!(engine.refresh().is_err());
    assert_incomplete(&engine);
    drop(engine);
    mock.fail_on(None);
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let after = engine.search("east", "search-code", &all())?;
    assert_eq!(after[0]["function"]["id"], id);
    near(&after[0]["similarity"], 0.0);
    assert!(
        after[0]["function"]["source"]
            .as_str()
            .unwrap()
            .contains("VECTOR_NORTH")
    );
    Ok(())
}

#[test]
fn embedding_profiles_switch_without_rebuild_and_reuse_each_warm_profile() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let a = mock.config();
    let mut b = a.clone();
    b["embeddingModel"] = json!("second-embedding-profile");
    repo.write("code.rs", &function("alpha", "VECTOR_EAST"))?;
    let mut engine = repo.open(&a)?;
    engine.refresh()?;
    let expected = engine.search("east", "search-code", &all())?;
    let calls = mock.count();
    drop(engine);
    let mut offline_b = b.clone();
    offline_b["noReindex"] = json!(true);
    let engine = repo.open(&offline_b)?;
    assert_eq!(engine.status()?["functionCount"], 1);
    assert_incomplete(&engine);
    assert_eq!(mock.count(), calls);
    drop(engine);
    let mut engine = repo.open(&b)?;
    assert_eq!(engine.refresh()?["filesUpdated"], 0);
    assert_eq!(engine.search("east", "search-code", &all())?, expected);
    drop(engine);
    let calls = mock.count();
    for config in [&a, &b, &a] {
        let mut engine = repo.open(config)?;
        assert_eq!(engine.search("east", "search-code", &all())?, expected);
        assert_eq!(engine.refresh()?["filesUpdated"], 0);
        assert_eq!(engine.search("east", "search-code", &all())?, expected);
        assert_eq!(
            mock.count(),
            calls,
            "warm profile switch must not repeat paid work"
        );
    }
    for model in ["integration-embedding", "second-embedding-profile"] {
        let inputs: Vec<_> = mock
            .requests("/embeddings")
            .into_iter()
            .filter(|request| request.body["model"] == model)
            .flat_map(|request| request.body["input"].as_array().unwrap().clone())
            .collect();
        assert_eq!(
            inputs.len(),
            2,
            "one document and query per profile: {model}"
        );
    }
    Ok(())
}

#[test]
fn partially_prepared_profile_rejects_search_and_reuses_successful_batches_on_retry() -> Result<()>
{
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("a.rs", &function("ready", "VECTOR_EAST"))?;
    repo.write("z.rs", &function("pending", "PROFILE_FAILURE VECTOR_NORTH"))?;
    let a = mock.config();
    let mut engine = repo.open(&a)?;
    engine.refresh()?;
    let expected = engine.search("east", "search-code", &all())?;
    drop(engine);
    let mut b = a.clone();
    b["embeddingModel"] = json!("partial-profile");
    mock.fail_on(Some("PROFILE_FAILURE"));
    let mut engine = repo.open(&b)?;
    assert!(engine.refresh().is_err());
    assert_incomplete(&engine);
    drop(engine);
    let calls = mock.count();
    let mut offline = b.clone();
    offline["noReindex"] = json!(true);
    let engine = repo.open(&offline)?;
    assert_incomplete(&engine);
    assert_eq!(mock.count(), calls);
    drop(engine);
    let engine = repo.open(&a)?;
    assert_eq!(engine.search("east", "search-code", &all())?, expected);
    assert_eq!(
        mock.count(),
        calls,
        "a failed new profile must not damage the old profile"
    );
    drop(engine);
    mock.fail_on(None);
    let mut engine = repo.open(&b)?;
    engine.refresh()?;
    assert_eq!(engine.search("east", "search-code", &all())?, expected);
    let inputs: Vec<_> = mock
        .requests("/embeddings")
        .into_iter()
        .filter(|request| request.body["model"] == "partial-profile")
        .flat_map(|request| request.body["input"].as_array().unwrap().clone())
        .collect();
    assert_eq!(
        inputs
            .iter()
            .filter(|input| input.as_str().unwrap().contains("symbol: ready"))
            .count(),
        1
    );
    assert_eq!(
        inputs
            .iter()
            .filter(|input| input.as_str().unwrap().contains("PROFILE_FAILURE"))
            .count(),
        2
    );
    Ok(())
}

#[test]
fn symbol_map_normalizes_bare_names_and_indexes_noncallables_without_content() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(
        "api.ts",
        "export const VECTOR_NORTH_LIMIT = 1;\nexport type VectorNorthType = number;\nexport function HTTP2VectorNorth() { return 'VECTOR_WEST'; }\nexport function http2_vector_north() { return 'VECTOR_WEST'; }\nexport function Ordinary() { return 'VECTOR_NORTH VECTOR_WEST'; }\nexport class VectorNorthContainer { ordinaryMember() { return 'VECTOR_NORTH'; } }\n",
    )?;
    repo.write("guide.md", "# Guide\n\nVECTOR_WEST body only.\n\n## Vector North\n\nVECTOR_WEST selected body.\n\n### Ordinary\n\nVECTOR_NORTH body only.\n")?;
    // The public Engine API must honor its configured provider after a structure-only refresh.
    let mut engine = repo.open(&config)?;
    engine.refresh_structure()?;
    let ordinary = engine.map(&json!({}))?;
    assert!(map_names(&ordinary).contains("Ordinary"));
    assert_eq!(mock.count(), 0);
    assert!(!PathBuf::from(format!("{}.symbols.usearch", repo.index.display())).exists());

    let options = json!({"symbolQuery":"VECTOR_NORTH"});
    let selected = engine.map(&options)?;
    assert_eq!(
        map_names(&selected),
        strings(&[
            "VECTOR_NORTH_LIMIT",
            "VectorNorthType",
            "HTTP2VectorNorth",
            "http2_vector_north",
            "VectorNorthContainer",
            "Guide",
            "Vector North"
        ])
    );
    assert!(
        engine
            .map(&json!({"symbolQuery":"VECTOR_WEST"}))?
            .is_empty(),
        "body-only terms must not select declarations or headings"
    );
    let mut union_expected = map_names(&selected);
    union_expected.extend(strings(&["Ordinary", "ordinaryMember"]));
    assert_eq!(
        map_names(
            &engine.map(
                &json!({"symbolQuery":"VECTOR_NORTH", "regexp":"ordinary", "ignoreCase":true})
            )?
        ),
        union_expected,
        "regex-only children join semantic matches without inheriting parent semantics"
    );

    let inputs = mock.embedding_inputs();
    assert!(
        inputs.iter().all(|input| !input.contains("VECTOR_")
            && !input.contains('\n')
            && !input.contains("return")),
        "name/query inputs only: {inputs:?}"
    );
    assert_eq!(
        inputs
            .iter()
            .filter(|input| *input == "http 2 vector north")
            .count(),
        1,
        "equivalent identifier spellings share one vocabulary vector"
    );
    for normalized in [
        "vector north limit",
        "vector north type",
        "vector north container",
    ] {
        assert!(
            inputs.iter().any(|input| input == normalized),
            "missing {normalized}: {inputs:?}"
        );
    }
    assert_eq!(
        inputs
            .iter()
            .filter(|input| *input == "vector north")
            .count(),
        2,
        "heading and query normalize identically, with separate document/query keys"
    );
    let calls = mock.count();
    for query in [
        "HTTP2VectorNorth",
        "http2_vector_north",
        "http 2 vector north",
    ] {
        assert_eq!(engine.map(&json!({"symbolQuery":query}))?, selected);
    }
    let normalized_query_calls = mock.count();
    assert_eq!(
        normalized_query_calls,
        calls + 1,
        "one normalized query embedding for all spellings"
    );
    drop(engine);
    let engine = repo.open(&config)?;
    assert_eq!(engine.map(&options)?, selected);
    assert_eq!(
        engine.map(&json!({"symbolQuery":"http2_vector_north"}))?,
        selected
    );
    assert_eq!(mock.count(), normalized_query_calls);
    assert!(mock.requests("/responses").is_empty());
    assert_eq!(artifact_counts(&repo)?.2, 0);
    let unit_embeddings: i64 =
        repo.db()?
            .query_row("SELECT count(*) FROM unit_embeddings", [], |r| r.get(0))?;
    assert_eq!(
        unit_embeddings, 0,
        "symbol map must not prepare content vectors"
    );
    assert_incomplete(&engine);
    Ok(())
}

#[test]
fn symbol_map_saved_import_aliases_share_the_normalized_vocabulary() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("api.ts", "import { original as HTTP2VectorNorthAlias } from './dependency';\nexport function Ordinary() {}\n")?;
    let config = mock.config();
    let mut engine = repo.open(&config)?;
    engine.refresh_structure()?;
    let selected =
        engine.map(&json!({"symbolQuery":"HTTP2VectorNorthAlias", "kinds":"imports"}))?;
    assert_eq!(selected.len(), 1);
    let nodes = selected[0]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["kind"], "import");
    assert!(
        nodes[0]["names"]
            .as_array()
            .unwrap()
            .contains(&json!("HTTP2VectorNorthAlias"))
    );
    assert_eq!(
        mock.embedding_inputs()
            .iter()
            .filter(|input| *input == "http 2 vector north alias")
            .count(),
        2
    );
    let calls = mock.count();
    drop(engine);
    assert_eq!(
        repo.open(&config)?
            .map(&json!({"symbolQuery":"http_2_vector_north_alias", "kinds":"imports"}))?,
        selected
    );
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn symbol_map_cli_unions_queries_and_regexes_with_glob_kind_and_visibility_restrictions()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(".slopdex/config.json", &config.to_string())?;
    let source = "export class Api {\n public VectorNorth() {}\n public VectorWest() {}\n public VectorMid() {}\n private VectorNorthSecret() {}\n ordinary() { return 'VECTOR_NORTH'; }\n}\nexport const VECTOR_NORTH_CONSTANT = 1;\n";
    for path in ["src/api.ts", "src/excluded.ts", "outside/api.ts"] {
        repo.write(path, source)?;
    }
    repo.open(&config)?.refresh_structure()?;
    let selector = [
        "--no-reindex",
        "map",
        "src",
        "-q",
        "VECTOR_NORTH",
        "--symbol-query",
        "VECTOR_WEST",
        "-e",
        "^API\\.VECTORMID$",
        "-e",
        "^absent$",
        "-i",
        "-g",
        "**/*.ts",
        "-g",
        "!**/excluded.ts",
        "-k",
        "methods",
    ];
    let selected = repo.cli_json(&selector)?;
    assert_eq!(selected.as_array().unwrap().len(), 1);
    assert_eq!(selected[0]["path"], "src/api.ts");
    assert_eq!(
        map_names(selected.as_array().unwrap()),
        strings(&["Api", "VectorNorth", "VectorWest", "VectorMid"]),
        "regex admits mid, and repeated semantic queries admit north/west"
    );
    let calls = mock.count();
    assert_eq!(repo.cli_json(&selector)?, selected);
    assert_eq!(
        mock.count(),
        calls,
        "cached CLI repeat/reopen must not call providers"
    );
    let mut strict = selector.to_vec();
    strict.extend(["--symbol-threshold", "0.7"]);
    assert_eq!(
        map_names(repo.cli_json(&strict)?.as_array().unwrap()),
        strings(&["Api", "VectorNorth", "VectorWest", "VectorMid"]),
        "raising the semantic threshold must not remove the regex-only match"
    );
    strict.push("--private");
    assert_eq!(
        map_names(repo.cli_json(&strict)?.as_array().unwrap()),
        strings(&[
            "Api",
            "VectorNorth",
            "VectorWest",
            "VectorMid",
            "VectorNorthSecret"
        ])
    );
    assert_eq!(
        mock.count(),
        calls,
        "threshold/visibility changes reuse paid name and query vectors"
    );
    assert!(mock.requests("/responses").is_empty());
    Ok(())
}

#[test]
fn symbol_map_cli_no_reindex_populates_lazy_cache_and_expands_only_direct_selected_body()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("guide.md", "# Guide\n\nAncestor body VECTOR_WEST.\n\n## Vector North\n\nSelected café 🚀 body VECTOR_WEST.\n\n### Ordinary\n\nDescendant body VECTOR_NORTH.\n\n## Other\n\nSibling body VECTOR_NORTH.\n")?;
    repo.write(
        "code.ts",
        "export function Ordinary() { return 'VECTOR_NORTH'; }\n",
    )?;
    let mut engine = repo.open_map(&config)?;
    engine.refresh_structure()?;
    let generation = engine.status()?["generation"].clone();
    let saved = file_record(&repo, "guide.md")?;
    let before = artifact_counts(&repo)?;
    assert_eq!(before.0, 0);
    assert_eq!(mock.count(), 0);
    drop(engine);
    repo.write("guide.md", "# Vector West\n\nChanged live body.\n")?;
    repo.write("code.ts", "export function VectorNorthNew() {}\n")?;

    let args = ["--no-reindex", "map", "-q", "VECTOR_NORTH"];
    let selected = repo.cli_json(&args)?;
    assert_eq!(
        map_names(selected.as_array().unwrap()),
        strings(&["Guide", "Vector North"])
    );
    assert!(
        mock.count() > 0,
        "noReindex still permits lazy symbol/query cache population"
    );
    let calls = mock.count();
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--index")
        .arg(&repo.index)
        .args([
            "--no-reindex",
            "--format",
            "summary",
            "--detail",
            "expanded",
            "map",
            "-q",
            "VECTOR_NORTH",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    for included in [
        "# Guide",
        "## Vector North",
        "Selected café 🚀 body VECTOR_WEST.",
    ] {
        assert_eq!(text.matches(included).count(), 1, "{text}");
    }
    for omitted in [
        "Ancestor body",
        "Descendant body",
        "Sibling body",
        "### Ordinary",
        "## Other",
        "Changed live body",
        "VectorNorthNew",
    ] {
        assert!(!text.contains(omitted), "{omitted}: {text}");
    }
    assert_eq!(repo.cli_json(&args)?, selected);
    assert_eq!(mock.count(), calls);
    assert_eq!(file_record(&repo, "guide.md")?, saved);
    assert_eq!(
        repo.cli_json(&["--no-reindex", "status"])?["generation"],
        generation
    );
    let after = artifact_counts(&repo)?;
    assert!(after.0 > before.0);
    assert_eq!((after.1, after.2), (before.1, before.2));
    assert!(mock.requests("/responses").is_empty());
    for kind in ["code", "markdown", "descriptions", "combined"] {
        assert!(!PathBuf::from(format!("{}.{kind}.usearch", repo.index.display())).exists());
    }
    Ok(())
}

#[test]
fn symbol_search_filters_before_content_limits_with_independent_thresholds_and_unchanged_scores()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    repo.write(".slopdex/config.json", &config.to_string())?;
    let distractors: String = (0..48)
        .map(|i| function(&format!("ordinary_{i}"), "VECTOR_EAST"))
        .collect();
    repo.write("other.rs", &distractors)?;
    repo.write(
        "selected.rs",
        &(function("VectorNorth", "VECTOR_NORTH")
            + &function("VectorMid", "VECTOR_MID")
            + &function("VectorWest", "VECTOR_EAST")),
    )?;
    repo.write("guide.md", "# Guide\n\nOverview VECTOR_EAST.\n\n## Vector North\n\nVECTOR_NORTH.\n\n## Vector Mid\n\nVECTOR_MID.\n\n## Ordinary\n\nVECTOR_EAST VECTOR_NORTH.\n")?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let strict =
        json!({"symbolQuery":"VECTOR_NORTH", "symbolThreshold":0.7, "minSimilarity":-1, "limit":1});
    for kind in ["search-code", "search-descriptions", "search-md", "search"] {
        let unfiltered = engine.search("east", kind, &all())?;
        let selected = engine.search("east", kind, &strict)?;
        assert_eq!(selected.len(), 1, "{kind}: {selected:?}");
        let expected = unfiltered
            .iter()
            .find(|row| {
                row["function"]["name"] == "VectorNorth"
                    || row["chunk"]["headingPath"] == json!(["Guide", "Vector North"])
            })
            .unwrap();
        assert_eq!(
            &selected[0], expected,
            "{kind}: symbol similarity must not replace or modify content scores"
        );
        let expected_score = match kind {
            "search-descriptions" => 0.3, // callable 0 plus file .6, averaged
            "search" => 0.2,              // code 0, callable 0, file .6, averaged
            _ => 0.0,
        };
        near(&selected[0]["similarity"], expected_score);
        let mut content_strict = strict.clone();
        content_strict["minSimilarity"] = json!(0.4);
        assert!(
            engine.search("east", kind, &content_strict)?.is_empty(),
            "{kind}: independent content threshold"
        );
    }
    let relaxed = json!({"symbolQuery":"VECTOR_NORTH", "minSimilarity":0.7, "limit":1});
    let code = engine.search("east", "search-code", &relaxed)?;
    assert_eq!(names(&code), strings(&["VectorMid"]));
    near(&code[0]["similarity"], 0.8);
    let md = engine.search("east", "search-md", &relaxed)?;
    assert_eq!(
        md[0]["chunk"]["headingPath"],
        json!(["Guide", "Vector Mid"])
    );
    near(&md[0]["similarity"], 0.8);
    let mixed = engine.search("east", "search", &json!({"code":true, "md":true, "symbolQuery":["VECTOR_NORTH", "VECTOR_WEST"], "symbolThreshold":0.7, "minSimilarity":-1, "limit":1}))?;
    assert_eq!(names(&mixed), strings(&["VectorWest"]));
    near(&mixed[0]["similarity"], 1.0);
    let composition = json!({"symbolQuery":["VECTOR_NORTH", "VECTOR_WEST"], "symbolThreshold":0.7, "regexp":["^vectOrmid$", "^absent$"], "ignoreCase":true, "glob":"selected.rs", "minSimilarity":-1, "limit":1});
    let composed = engine.search("east", "search-code", &composition)?;
    assert_eq!(names(&composed), strings(&["VectorWest"]));
    near(&composed[0]["similarity"], 1.0);
    let mut unlimited = composition.clone();
    unlimited.as_object_mut().unwrap().remove("limit");
    assert_eq!(
        names(&engine.search("east", "search-code", &unlimited)?),
        strings(&["VectorNorth", "VectorMid", "VectorWest"]),
        "regex-only and semantic-only matches are both eligible before limiting"
    );
    let markdown = engine.search(
        "east",
        "search-md",
        &json!({
            "symbolQuery":"VECTOR_NORTH", "symbolThreshold":0.7,
            "regexp":"^Guide\\.Ordinary$", "glob":"guide.md", "minSimilarity":-1
        }),
    )?;
    assert_eq!(
        markdown
            .iter()
            .map(|row| row["chunk"]["headingPath"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned())
            .collect::<BTreeSet<_>>(),
        strings(&["Vector North", "Ordinary"])
    );
    let mut cross_options = unlimited.clone();
    cross_options["minLines"] = json!(0);
    cross_options["crossFileOnly"] = json!(true);
    cross_options["matches"] = json!(1);
    assert_eq!(
        engine
            .cross_search(None, &cross_options)?
            .iter()
            .map(|row| row["source"]["name"].as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>(),
        strings(&["VectorNorth", "VectorMid", "VectorWest"]),
        "cross-search restricts the union to source globs, not target neighbors"
    );
    let calls = mock.count();
    assert_eq!(
        engine.search("east", "search-code", &composition)?,
        composed
    );
    drop(engine);
    assert_eq!(
        repo.open(&config)?
            .search("east", "search-code", &composition)?,
        composed
    );
    assert_eq!(
        repo.cli_json(&[
            "--no-reindex",
            "search-code",
            "east",
            "-q",
            "VECTOR_NORTH",
            "--symbol-query",
            "VECTOR_WEST",
            "--symbol-threshold",
            "0.7",
            "-e",
            "^vectOrmid$",
            "-e",
            "^absent$",
            "-i",
            "-g",
            "selected.rs",
            "--threshold",
            "-1",
            "--limit",
            "1"
        ])?,
        json!(composed)
    );
    assert_eq!(
        mock.count(),
        calls,
        "cached Engine/CLI search repeats and reopen must make no requests"
    );
    Ok(())
}

fn symbol_names(rows: &[Value]) -> BTreeSet<String> {
    rows.iter()
        .map(|row| row["symbol"]["name"].as_str().unwrap().to_owned())
        .collect()
}

fn assert_name_only_work(repo: &Repo, mock: &Mock) -> Result<()> {
    assert!(mock.requests("/responses").is_empty());
    assert_eq!(artifact_counts(repo)?.2, 0);
    let unit_embeddings: i64 =
        repo.db()?
            .query_row("SELECT count(*) FROM unit_embeddings", [], |row| row.get(0))?;
    assert_eq!(
        unit_embeddings, 0,
        "symbols must not prepare content vectors"
    );
    let inputs = mock.embedding_inputs();
    assert!(
        inputs.iter().all(|input| !input.contains('\n')
            && !input.contains("BODY_ONLY")
            && !input.contains("body only")
            && !input.contains("return")
            && input == &input.to_lowercase()),
        "normalized name/query inputs only: {inputs:?}"
    );
    Ok(())
}

#[test]
fn search_symbols_engine_ranks_all_declarations_and_preserves_duplicate_occurrences() -> Result<()>
{
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write("api.ts", "import { original as HTTP2VectorNorthAlias, other as VectorWestAlias } from './dependency';\nexport const VECTOR_NORTH_LIMIT = 1;\nexport class VectorNorthContainer {\n public VectorNorthField = 1;\n ordinaryMember() { return 'VECTOR_NORTH BODY_ONLY'; }\n}\nexport class First { VectorNorth() {} }\nexport class Second { VectorNorth() {} }\nexport function HTTP2VectorNorth() { return 'VECTOR_WEST BODY_ONLY'; }\nexport function http2_vector_north() { return 'VECTOR_WEST BODY_ONLY'; }\nexport function Ordinary() { return 'VECTOR_NORTH BODY_ONLY'; }\n")?;
    repo.write("other.ts", "export function HTTP2VectorNorth() {}\n")?;
    repo.write("guide.md", "# Guide\n\nVECTOR_NORTH BODY_ONLY parent.\n\n## Vector North Empty\n\n## Ordinary\n\nVECTOR_NORTH BODY_ONLY section.\n")?;
    let mut engine = repo.open(&config)?;
    engine.refresh_structure()?;
    assert_eq!(mock.count(), 0);

    let options = json!({"minSimilarity":0.9});
    let rows = engine.search("VECTOR_NORTH", "search-symbols", &options)?;
    assert_eq!(
        rows.len(),
        10,
        "one row per declaration, including aliases: {rows:?}"
    );
    let selected = symbol_names(&rows);
    for name in [
        "VECTOR_NORTH_LIMIT",
        "VectorNorthContainer",
        "VectorNorthField",
        "VectorNorth",
        "HTTP2VectorNorth",
        "http2_vector_north",
        "Vector North Empty",
    ] {
        assert!(selected.contains(name), "missing {name}: {rows:?}");
    }
    for name in ["Guide", "Ordinary", "ordinaryMember", "First", "Second"] {
        assert!(
            !selected.contains(name),
            "bare names exclude bodies and parents: {name}"
        );
    }
    assert_eq!(
        rows.iter()
            .filter(|r| r["symbol"]["name"] == "VectorNorth")
            .count(),
        2
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r["symbol"]["name"] == "HTTP2VectorNorth")
            .count(),
        2
    );
    let identities: BTreeSet<_> = rows
        .iter()
        .map(|row| {
            (
                row["symbol"]["path"].as_str().unwrap(),
                row["symbol"]["id"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(identities.len(), rows.len());
    for row in &rows {
        assert_eq!(row["type"], "symbol");
        near(&row["similarity"], 1.0);
        near(&row["symbolSimilarity"], 1.0);
        assert!(row.get("codeSimilarity").is_none());
        let symbol = &row["symbol"];
        let path = symbol["path"].as_str().unwrap();
        let structure = engine.presentation_structure(path)?.unwrap();
        let node = structure
            .nodes
            .iter()
            .find(|node| json!(node.id) == symbol["id"])
            .unwrap();
        let mut expected = serde_json::to_value(node)?;
        expected["path"] = json!(path);
        expected["sourceMode"] = json!("working-tree");
        assert_eq!(
            *symbol, expected,
            "preserve serialized StructureNode fields"
        );
    }
    let import = rows
        .iter()
        .find(|row| row["symbol"]["kind"] == "import")
        .unwrap();
    assert!(
        import["symbol"]["names"]
            .as_array()
            .unwrap()
            .contains(&json!("HTTP2VectorNorthAlias"))
    );
    // The declaration has eastward and westward names too: use the maximum,
    // rather than an average or only the primary import name.
    let west = engine.search(
        "VectorWest",
        "search-symbols",
        &json!({"minSimilarity":0.9}),
    )?;
    assert_eq!(west.len(), 1, "body-only VECTOR_WEST must not add hits");
    assert_eq!(west[0]["symbol"], import["symbol"]);
    near(&west[0]["similarity"], 1.0);
    let mut selector = options.clone();
    selector["symbols"] = json!(true);
    assert_eq!(engine.search("vector north", "search", &selector)?, rows);
    for limit in [1, 6, 9, 10, 20] {
        let mut limited = options.clone();
        limited["limit"] = json!(limit);
        assert_eq!(
            engine.search("VECTOR_NORTH", "search-symbols", &limited)?,
            rows[..limit.min(rows.len())],
            "limit counts occurrences, rather than vocabulary names"
        );
    }
    assert_name_only_work(&repo, &mock)?;
    Ok(())
}

#[test]
fn search_symbols_filters_and_threshold_ranges_precede_global_occurrence_limit() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(
        "nearest.rs",
        &(0..48)
            .map(|i| function(&format!("ordinary_{i}"), "BODY_ONLY"))
            .collect::<String>(),
    )?;
    let source = function("VectorEast", "BODY_ONLY")
        + &function("VectorMid", "BODY_ONLY")
        + &function("VectorNorth", "BODY_ONLY")
        + &function("VectorWest", "BODY_ONLY");
    repo.write("src/selected.rs", &source)?;
    repo.write(
        "src/excluded.rs",
        &function("VectorEastExcluded", "BODY_ONLY"),
    )?;
    let mut engine = repo.open(&config)?;
    engine.refresh_structure()?;
    for (minimum, maximum, expected) in [
        (1.0, None, vec!["VectorEast"]),
        (0.0, Some(1.0), vec!["VectorMid", "VectorNorth"]),
        (-1.0, Some(0.0), vec!["VectorWest"]),
        (-1.0, Some(-0.5), vec!["VectorWest"]),
        (0.9, Some(1.0), vec![]),
    ] {
        let options =
            json!({"glob":"src/selected.rs", "minSimilarity":minimum, "maxSimilarity":maximum});
        let rows = engine.search("east", "search-symbols", &options)?;
        assert_eq!(
            rows.iter()
                .map(|row| row["symbol"]["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected,
            "{options}"
        );
        let mut limited = options.clone();
        limited["limit"] = json!(1);
        assert_eq!(
            engine.search("east", "search-symbols", &limited)?,
            rows[..rows.len().min(1)]
        );
    }
    let selected = engine.search(
        "east",
        "search-symbols",
        &json!({
            "glob":["src/**", "!src/excluded.rs"], "regexp":["^vectOrnorth$", "^absent$"],
            "ignoreCase":true, "minSimilarity":-1, "limit":1
        }),
    )?;
    assert_eq!(symbol_names(&selected), strings(&["VectorNorth"]));
    near(&selected[0]["similarity"], 0.0);
    let strict = json!({"glob":"src/selected.rs", "symbolQuery":"VECTOR_NORTH", "symbolThreshold":0.7, "minSimilarity":-1, "limit":1});
    let north = engine.search("east", "search-symbols", &strict)?;
    assert_eq!(symbol_names(&north), strings(&["VectorNorth"]));
    near(&north[0]["similarity"], 0.0);
    near(&north[0]["symbolSimilarity"], 0.0);
    let mut relaxed = strict.clone();
    relaxed["symbolThreshold"] = json!(0.5);
    let mid = engine.search("east", "search-symbols", &relaxed)?;
    assert_eq!(symbol_names(&mid), strings(&["VectorMid"]));
    near(&mid[0]["similarity"], 0.8);
    let mut intersection = strict.clone();
    intersection["minSimilarity"] = json!(0.7);
    assert!(
        engine
            .search("east", "search-symbols", &intersection)?
            .is_empty()
    );
    assert_name_only_work(&repo, &mock)?;
    Ok(())
}

#[test]
fn search_symbols_cli_command_selector_parity_and_normalized_q_regex_composition() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("src/api.ts", "import { original as HTTP2VectorNorthAlias } from './dependency';\nexport class Api { HTTP2VectorNorth() {} ordinary() { return 'VECTOR_NORTH BODY_ONLY'; } }\nexport function VectorWest() {}\nexport function VectorMid() {}\n")?;
    repo.write("outside.ts", "export function HTTP2VectorNorth() {}\n")?;
    repo.write("guide.md", "# Guide\n\n## HTTP2VectorNorth\n")?;
    repo.open_map(&config)?.refresh_structure()?;
    let filters = [
        "-q",
        "http_2_vector_north",
        "--symbol-threshold",
        "0.7",
        "-e",
        "^api\\.http2vectornorth$",
        "-e",
        "^http2vectornorthalias$",
        "-i",
        "-g",
        "src/**",
        "--threshold",
        "0.9",
        "--limit",
        "10",
    ];
    let command_args = [
        vec!["--no-reindex", "search-symbols", "HTTP2VectorNorth"],
        filters.to_vec(),
    ]
    .concat();
    let command = repo.cli_json(&command_args)?;
    assert_eq!(
        command.as_array().unwrap().len(),
        2,
        "qualified method and import alias: {command}"
    );
    assert!(
        command
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["symbol"]["qualifiedName"] == "Api.HTTP2VectorNorth")
    );
    assert!(
        command
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["symbol"]["kind"] == "import")
    );
    let calls = mock.count();
    let union_args = [
        vec!["--no-reindex", "search-symbols", "HTTP2VectorNorth"],
        vec![
            "-q",
            "http_2_vector_north",
            "--symbol-threshold",
            "0.7",
            "-e",
            "^VectorWest$",
            "-g",
            "src/**",
            "--threshold",
            "-1",
        ],
    ]
    .concat();
    assert_eq!(
        symbol_names(repo.cli_json(&union_args)?.as_array().unwrap()),
        strings(&["HTTP2VectorNorth", "./dependency.original", "VectorWest"])
    );
    let selector_args = [
        vec!["--no-reindex", "search", "http 2 vector north", "--symbols"],
        filters.to_vec(),
    ]
    .concat();
    assert_eq!(repo.cli_json(&selector_args)?, command);
    assert_eq!(repo.cli_json(&command_args)?, command);
    for command in [
        vec!["search-symbols", "HTTP2VectorNorth"],
        vec!["search", "http_2_vector_north", "--symbols"],
    ] {
        let args = [
            vec!["--no-reindex"],
            command,
            vec!["-g", "src/**", "--threshold", "0.5-1", "--limit", "1"],
        ]
        .concat();
        let range = repo.cli_json(&args)?;
        assert_eq!(
            symbol_names(range.as_array().unwrap()),
            strings(&["VectorMid"])
        );
        near(&range[0]["similarity"], 0.6);
    }
    assert_eq!(
        mock.count(),
        calls,
        "repeat/reopen and alternate spelling share vectors"
    );
    assert_eq!(
        mock.embedding_inputs()
            .iter()
            .filter(|input| *input == "http 2 vector north")
            .count(),
        2,
        "one vocabulary vector plus one query shared by positional query and -q"
    );
    assert_name_only_work(&repo, &mock)?;
    Ok(())
}

#[test]
fn search_symbols_native_dimensions_share_map_vectors_and_support_cached_readonly_reopen()
-> Result<()> {
    let mock = Mock::start()?;
    let first = Repo::new()?;
    let second = Repo::new()?;
    let mut config = mock.config();
    config["symbolDimensions"] = json!(2);
    for repo in [&first, &second] {
        repo.write(
            "api.ts",
            "export function HTTP2VectorNorth() { return 'BODY_ONLY'; }\n",
        )?;
    }
    let mut engine = first.open(&config)?;
    engine.refresh_structure()?;
    let mapped = engine.map(&json!({"symbolQuery":"HTTP2VectorNorth"}))?;
    assert_eq!(map_names(&mapped), strings(&["HTTP2VectorNorth"]));
    let manifest = symbol_manifest(&first)?;
    assert_eq!(manifest["dimensions"], 2);
    let calls = mock.count();
    assert_eq!(calls, 2, "one symbol vocabulary batch and one query");
    let rows = engine.search("http_2_vector_north", "search-symbols", &all())?;
    assert_eq!(rows.len(), 1);
    near(&rows[0]["similarity"], 1.0);
    assert_eq!(
        symbol_manifest(&first)?,
        manifest,
        "reuse the existing .symbols.usearch vocabulary"
    );
    let selector = json!({"symbols":true, "minSimilarity":-1});
    assert_eq!(
        engine.search("HTTP2VectorNorth", "search", &selector)?,
        rows
    );
    assert_eq!(
        mock.count(),
        calls,
        "positional symbol queries reuse map -q embeddings"
    );
    let mut profile = slopdex::providers::Providers::new(&config)?.embedding_profile();
    profile["dimensions"] = json!(2);
    profile["symbolNormalizationVersion"] = json!("symbols-v1");
    for query in [false, true] {
        let key = slopdex::storage::Database::embedding_key(&profile, query, "http 2 vector north");
        let bytes: i64 = first.db()?.query_row(
            "SELECT length(vector) FROM embeddings WHERE key=?",
            [key],
            |row| row.get(0),
        )?;
        assert_eq!(bytes, 8);
    }
    for request in mock.requests("/embeddings") {
        assert_eq!(request.body["dimensions"], 2, "request native dimensions");
        assert_eq!(request.body["input"], json!(["http 2 vector north"]));
    }
    drop(engine);
    for _ in 0..2 {
        let reader = Engine::open_readonly(&first.root, &first.index, config.clone())?;
        let before = filesystem_snapshot(first._temp.path())?;
        assert_eq!(
            reader.search("http_2_vector_north", "search-symbols", &all())?,
            rows
        );
        assert_eq!(
            reader.search("HTTP2VectorNorth", "search", &selector)?,
            rows
        );
        assert_eq!(filesystem_snapshot(first._temp.path())?, before);
        assert_eq!(mock.count(), calls);
    }
    let mut other = second.open(&config)?;
    other.refresh_structure()?;
    let shared = other.search("http 2 vector north", "search-symbols", &all())?;
    assert_eq!(symbol_names(&shared), strings(&["HTTP2VectorNorth"]));
    near(&shared[0]["similarity"], 1.0);
    assert_eq!(
        mock.count(),
        calls,
        "a different index hydrates shared symbol document/query artifacts"
    );
    assert_eq!(symbol_manifest(&second)?["dimensions"], 2);
    assert_name_only_work(&first, &mock)?;
    assert_name_only_work(&second, &mock)?;
    Ok(())
}

#[test]
fn search_symbols_mixed_modes_rank_separate_streams_with_global_limits_and_explicit_selection()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write(
        "code.rs",
        &(function("VectorNorth", "VECTOR_EAST")
            + &function("VectorMid", "VECTOR_NORTH")
            + &function("VectorWest", "VECTOR_MID")),
    )?;
    repo.write(
        "guide.md",
        "# Ordinary\n\nVECTOR_MID\n\n## Vector North Empty\n",
    )?;
    repo.write("config.json", "{\"setting\": 42}\n")?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let plain = engine.search("east", "search", &all())?;
    assert!(plain.iter().all(|row| row["type"] != "symbol"));
    assert!(plain.iter().any(|row| row["type"] == "document"));
    assert!(
        !PathBuf::from(format!("{}.symbols.usearch", repo.index.display())).exists(),
        "plain search does not implicitly initialize symbols"
    );
    let options = json!({"glob":["*.rs", "*.md"], "minSimilarity":-1});
    let code = engine.search("east", "search-code", &options)?;
    let markdown = engine.search("east", "search-md", &options)?;
    let symbols = engine.search("east", "search-symbols", &options)?;
    for mask in 1..8 {
        let mut selected = options.clone();
        selected["code"] = json!(mask & 1 != 0);
        selected["md"] = json!(mask & 2 != 0);
        selected["symbols"] = json!(mask & 4 != 0);
        let rows = engine.search("east", "search", &selected)?;
        let mut expected = Vec::new();
        for (bit, stream) in [(1, &code), (2, &markdown), (4, &symbols)] {
            if mask & bit != 0 {
                expected.extend(stream.iter().map(Value::to_string));
            }
        }
        let mut actual: Vec<_> = rows.iter().map(Value::to_string).collect();
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "select only requested streams: {selected}"
        );
        assert!(
            rows.windows(2)
                .all(|pair| pair[0]["similarity"].as_f64().unwrap()
                    >= pair[1]["similarity"].as_f64().unwrap())
        );
        for limit in [1, 2, 4, 20] {
            let mut limited = selected.clone();
            limited["limit"] = json!(limit);
            assert_eq!(
                engine.search("east", "search", &limited)?,
                rows[..limit.min(rows.len())],
                "one global occurrence limit: {limited}"
            );
        }
    }
    near(
        &code
            .iter()
            .find(|row| row["function"]["name"] == "VectorNorth")
            .unwrap()["similarity"],
        1.0,
    );
    near(
        &symbols
            .iter()
            .find(|row| row["symbol"]["name"] == "VectorNorth")
            .unwrap()["similarity"],
        0.0,
    );
    assert!(
        symbols
            .iter()
            .any(|row| row["symbol"]["name"] == "Vector North Empty")
    );
    assert_eq!(
        markdown.len(),
        1,
        "empty headings exist only in the structural stream"
    );
    let mixed_options = json!({"code":true, "md":true, "symbols":true, "glob":["*.rs", "*.md"], "minSimilarity":-1, "limit":4});
    let mixed = engine.search("east", "search", &mixed_options)?;
    drop(engine);
    assert_eq!(
        repo.cli_json(&[
            "--no-reindex",
            "search",
            "east",
            "--code",
            "--md",
            "--symbols",
            "-g",
            "*.rs",
            "-g",
            "*.md",
            "--threshold",
            "-1",
            "--limit",
            "4"
        ])?,
        json!(mixed)
    );
    Ok(())
}

#[test]
fn search_symbols_reranking_uses_names_without_bodies_or_descriptions() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["rerankingEnabled"] = json!(true);
    config["descriptionsEnabled"] = json!(true);
    repo.write("api.rs", "pub fn Ordinary() { let rerank_winner = \"BODY_ONLY_RERANK_SECRET\"; }\npub fn rerank_winner() { let marker = \"BODY_ONLY_OTHER\"; }\n")?;
    let mut engine = repo.open(&config)?;
    engine.refresh_structure()?;
    let options = json!({"minSimilarity":-1, "limit":1});
    let rows = engine.search("east", "search-symbols", &options)?;
    assert_eq!(symbol_names(&rows), strings(&["rerank_winner"]));
    near(&rows[0]["similarity"], 1.0);
    near(&rows[0]["symbolSimilarity"], 1.0);
    near(&rows[0]["rerankScore"], 0.99);
    let requests = mock.requests("/rerank");
    assert_eq!(requests.len(), 1);
    let documents = requests[0].body["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 2);
    for document in documents {
        let document = document.as_str().unwrap();
        for omitted in [
            "BODY_ONLY",
            "pub fn",
            "let marker",
            "signature",
            "description",
            "code",
        ] {
            assert!(
                !document.contains(omitted),
                "name-only symbol rerank data excludes {omitted}: {document}"
            );
        }
    }
    assert_eq!(
        documents
            .iter()
            .filter(|document| {
                let document = document.as_str().unwrap();
                document.contains("rerank_winner") || document.contains("rerank winner")
            })
            .count(),
        1,
        "body-only winner text must not influence ranking"
    );
    let calls = mock.count();
    assert_eq!(engine.search("east", "search-symbols", &options)?, rows);
    drop(engine);
    assert_eq!(
        repo.open(&config)?
            .search("east", "search-symbols", &options)?,
        rows
    );
    assert_eq!(mock.count(), calls);
    assert_name_only_work(&repo, &mock)?;
    Ok(())
}

#[test]
fn search_symbols_cli_saved_snapshot_json_summary_and_expanded_callable_heading_context()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("guide.md", "# Guide\n\nBODY_ONLY ancestor.\n\n## Vector North\n\nSaved café 🚀 BODY_ONLY selected.\n\n### Vector North Empty\n\n## Other\n\nBODY_ONLY sibling.\n")?;
    repo.write(
        "a.py",
        "from b import HTTP2VectorNorth\n\ndef outer():\n    return HTTP2VectorNorth()\n",
    )?;
    repo.write("b.py", "def HTTP2VectorNorth():\n    return 'BODY_ONLY saved callable'\n\ndef ordinary():\n    return 'VECTOR_NORTH BODY_ONLY'\n")?;
    let mut engine = repo.open_map(&config)?;
    engine.refresh_structure()?;
    let generation = engine.status()?["generation"].clone();
    let saved_guide = file_record(&repo, "guide.md")?;
    let saved_callable = file_record(&repo, "b.py")?;
    let before = artifact_counts(&repo)?;
    assert_eq!(before.0, 0);
    assert_eq!(mock.count(), 0);
    drop(engine);
    repo.write("guide.md", "# Changed\n\nNew live body.\n")?;
    repo.write("a.py", "def changed_caller():\n    return 0\n")?;
    repo.write("b.py", "def Changed():\n    return 'new live callable'\n")?;
    let args = [
        "--no-reindex",
        "search-symbols",
        "VECTOR_NORTH",
        "--threshold",
        "0.9",
        "-g",
        "guide.md",
        "-g",
        "b.py",
        "-e",
        "^HTTP2VectorNorth$",
        "-e",
        "^Guide\\.Vector North(\\.Vector North Empty)?$",
    ];
    let compact = repo.cli_json(&args)?;
    assert_eq!(
        symbol_names(compact.as_array().unwrap()),
        strings(&["HTTP2VectorNorth", "Vector North", "Vector North Empty"])
    );
    assert_eq!(compact.as_array().unwrap().len(), 3);
    assert!(
        compact
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("relatedCallables").is_none())
    );
    let calls = mock.count();
    assert!(
        calls > 0,
        "no-reindex can populate lazy name/query artifacts"
    );
    let expanded_args = [vec!["--detail", "expanded"], args.to_vec()].concat();
    let expanded = repo.cli_json(&expanded_args)?;
    for row in expanded.as_array().unwrap() {
        let original = compact
            .as_array()
            .unwrap()
            .iter()
            .find(|original| {
                original["symbol"]["path"] == row["symbol"]["path"]
                    && original["symbol"]["id"] == row["symbol"]["id"]
            })
            .unwrap();
        assert_eq!(row["symbol"], original["symbol"]);
        if row["symbol"]["kind"] == "heading" {
            assert!(row.get("relatedCallables").is_none());
        } else {
            assert!(
                row["relatedCallables"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|related| related["path"] == "a.py" && related["node"]["name"] == "outer")
            );
        }
    }
    for detail in ["compact", "expanded"] {
        let output = repo
            .child(env!("CARGO_BIN_EXE_slopdex"))
            .arg("--root")
            .arg(&repo.root)
            .arg("--index")
            .arg(&repo.index)
            .args(["--format", "summary", "--detail", detail])
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout)?;
        for included in [
            "# Guide",
            "## Vector North",
            "### Vector North Empty",
            "def HTTP2VectorNorth()",
            "symbol score=1.00",
        ] {
            assert!(
                text.contains(included),
                "{detail}: missing {included}: {text}"
            );
        }
        assert_eq!(text.matches("# Guide").count(), 1, "{text}");
        assert_eq!(
            text.contains("Saved café 🚀 BODY_ONLY selected."),
            detail == "expanded",
            "{text}"
        );
        assert_eq!(
            text.contains("BODY_ONLY saved callable"),
            detail == "expanded",
            "{text}"
        );
        if detail == "expanded" {
            assert_eq!(
                text.matches("Saved café 🚀 BODY_ONLY selected.").count(),
                1,
                "{text}"
            );
            assert!(text.contains("def outer()"), "saved caller context: {text}");
            assert!(text.contains("[symbol 1.00]"), "{text}");
        }
        for omitted in [
            "BODY_ONLY ancestor",
            "BODY_ONLY sibling",
            "## Other",
            "Changed",
            "New live",
            "new live",
            "changed_caller",
            "def ordinary()",
        ] {
            assert!(
                !text.contains(omitted),
                "{detail}: unexpected {omitted}: {text}"
            );
        }
    }
    assert_eq!(repo.cli_json(&args)?, compact);
    assert_eq!(
        mock.count(),
        calls,
        "presentation and cached CLI reopen require no provider work"
    );
    assert_eq!(file_record(&repo, "guide.md")?, saved_guide);
    assert_eq!(file_record(&repo, "b.py")?, saved_callable);
    assert_eq!(
        repo.cli_json(&["--no-reindex", "status"])?["generation"],
        generation
    );
    let after = artifact_counts(&repo)?;
    assert!(after.0 > before.0);
    assert_eq!((after.1, after.2), (before.1, before.2));
    for kind in ["code", "markdown", "descriptions", "combined"] {
        assert!(!PathBuf::from(format!("{}.{kind}.usearch", repo.index.display())).exists());
    }
    assert_name_only_work(&repo, &mock)?;
    Ok(())
}

#[test]
fn search_symbols_cli_refreshes_only_structure_even_when_descriptions_are_enabled() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write(
        "api.ts",
        "export function VectorNorthOld() { return 'BODY_ONLY old'; }\n",
    )?;
    let mut engine = repo.open_map(&config)?;
    engine.refresh_structure()?;
    let generation = engine.status()?["generation"].as_u64().unwrap();
    drop(engine);
    repo.write("api.ts", "export const VECTOR_NORTH_NEW = 1;\nexport function Ordinary() { return 'VECTOR_NORTH BODY_ONLY new'; }\n")?;
    repo.write("guide.md", "# Vector North Empty\n")?;
    let rows = repo.cli_json(&["search-symbols", "VECTOR_NORTH", "--threshold", "0.9"])?;
    assert_eq!(
        symbol_names(rows.as_array().unwrap()),
        strings(&["VECTOR_NORTH_NEW", "Vector North Empty"])
    );
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert!(
        repo.cli_json(&["--no-reindex", "status"])?["generation"]
            .as_u64()
            .unwrap()
            > generation
    );
    let calls = mock.count();
    assert_eq!(
        repo.cli_json(&["search", "vector north", "--symbols", "--threshold", "0.9"])?,
        rows
    );
    assert_eq!(mock.count(), calls);
    for kind in ["code", "markdown", "descriptions", "combined"] {
        assert!(!PathBuf::from(format!("{}.{kind}.usearch", repo.index.display())).exists());
    }
    assert_name_only_work(&repo, &mock)?;
    Ok(())
}

#[test]
fn search_symbols_cli_missing_index_fails_before_providers_or_filesystem_writes() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    for key in ["embeddingApiKey", "descriptionApiKey", "rerankerApiKey"] {
        config.as_object_mut().unwrap().remove(key);
    }
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("api.ts", "export function VectorNorth() {}\n")?;
    let before = filesystem_snapshot(repo._temp.path())?;
    for args in [
        vec!["search-symbols", "VECTOR_NORTH"],
        vec!["search", "VECTOR_NORTH", "--symbols"],
        vec!["--no-reindex", "search-symbols", "VECTOR_NORTH"],
        vec!["--no-reindex", "search", "VECTOR_NORTH", "--symbols"],
    ] {
        assert_missing_index(&repo.cli(&args)?);
        assert_eq!(filesystem_snapshot(repo._temp.path())?, before, "{args:?}");
        assert_eq!(mock.count(), 0);
    }
    Ok(())
}

fn symbol_manifest(repo: &Repo) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(format!(
        "{}.symbols.usearch.manifest.json",
        repo.index.display()
    ))?)?)
}

#[test]
fn symbol_map_refresh_reconciles_new_deleted_and_renamed_names_without_reembedding_unchanged_names()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write("keep.ts", "export function VectorNorthKeep() { return 'old body'; }\nexport function VectorNorthRename() {}\n")?;
    repo.write("deleted.ts", "export const VECTOR_WEST_DELETE = 1;\n")?;
    let mut engine = repo.open(&config)?;
    engine.refresh_structure()?;
    let options = json!({"symbolQuery":"VECTOR_NORTH", "symbolThreshold":0.7});
    assert_eq!(
        map_names(&engine.map(&options)?),
        strings(&["VectorNorthKeep", "VectorNorthRename"])
    );
    let manifest = symbol_manifest(&repo)?;
    assert_eq!(manifest["vectors"].as_object().unwrap().len(), 3);
    let inputs = mock.embedding_inputs().len();
    repo.write("keep.ts", "export function VectorNorthKeep() { return 'changed VECTOR_WEST body'; }\nexport function VectorMidRenamed() {}\nexport const VECTOR_WEST_NEW = 2;\n")?;
    fs::remove_file(repo.root.join("deleted.ts"))?;
    let refreshed = engine.refresh_structure()?;
    assert_eq!(refreshed["filesUpdated"], 1);
    assert_eq!(refreshed["filesDeleted"], 1);
    assert_eq!(
        mock.embedding_inputs().len(),
        inputs,
        "structure refresh must remain provider-free"
    );
    assert_eq!(
        map_names(&engine.map(&options)?),
        strings(&["VectorNorthKeep"]),
        "old selection cache must be invalidated"
    );
    let delta = mock.embedding_inputs()[inputs..].to_vec();
    assert_eq!(delta.len(), 2, "only new vocabulary names: {delta:?}");
    assert_eq!(
        delta.into_iter().collect::<BTreeSet<_>>(),
        strings(&["vector mid renamed", "vector west new"])
    );
    let reconciled = symbol_manifest(&repo)?;
    assert_eq!(reconciled["generation"], refreshed["generation"]);
    assert_ne!(reconciled["fingerprint"], manifest["fingerprint"]);
    assert_eq!(reconciled["vectors"].as_object().unwrap().len(), 3);
    assert_eq!(
        map_names(&engine.map(&json!({"symbolQuery":"VECTOR_WEST", "symbolThreshold":0.7}))?),
        strings(&["VECTOR_WEST_NEW"])
    );
    // Returning to a previously saved name reuses its durable embedding.
    let calls = mock.count();
    repo.write("restored.ts", "export function VectorNorthRename() {}\n")?;
    engine.refresh_structure()?;
    assert_eq!(
        map_names(&engine.map(&options)?),
        strings(&["VectorNorthKeep", "VectorNorthRename"])
    );
    assert_eq!(mock.count(), calls);
    assert_eq!(
        symbol_manifest(&repo)?["vectors"]
            .as_object()
            .unwrap()
            .len(),
        4
    );
    drop(engine);
    assert_eq!(
        map_names(&repo.open(&config)?.map(&options)?),
        strings(&["VectorNorthKeep", "VectorNorthRename"])
    );
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn symbol_selection_cache_is_cleared_when_live_snapshot_is_reset() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    let options = json!({"symbolQuery":"VECTOR_NORTH"});
    repo.write("api.ts", "export function VectorNorthOld() {}\n")?;
    let mut engine = repo.open_map(&config)?;
    engine.refresh_structure()?;
    let generation = engine.status()?["generation"].clone();
    // Open a configured engine to prepare the name-only vectors.
    drop(engine);
    assert_eq!(
        map_names(&repo.open(&config)?.map(&options)?),
        strings(&["VectorNorthOld"])
    );
    // Match the canonical root identity stored by Engine, including Windows' verbatim prefix.
    let db = slopdex::storage::Database::open(
        &repo.index,
        &repo.root.canonicalize()?,
        &Value::Null,
        false,
    )?;
    db.reset()?;
    drop(db);
    repo.write("api.ts", "export function VectorNorthNew() {}\n")?;
    let mut engine = repo.open(&config)?;
    engine.refresh_structure()?;
    assert_eq!(engine.status()?["generation"], generation);
    assert_eq!(
        map_names(&engine.map(&options)?),
        strings(&["VectorNorthNew"])
    );
    Ok(())
}

#[test]
fn symbol_native_dimensions_profiles_and_shared_cache_are_isolated_and_reusable() -> Result<()> {
    let mock = Mock::start()?;
    let first = Repo::new()?;
    let second = Repo::new()?;
    let mut small = mock.config();
    small["symbolDimensions"] = json!(2);
    let mut default = small.clone();
    default.as_object_mut().unwrap().remove("symbolDimensions");
    for repo in [&first, &second] {
        repo.write(
            "api.ts",
            "export function VectorNorth() { return 'VECTOR_NORTH'; }\n",
        )?;
    }
    let mut engine = first.open(&small)?;
    engine.refresh()?;
    let content = engine.search("vector north", "search-code", &all())?;
    near(&content[0]["similarity"], 1.0);
    let content_calls = mock.count();
    let options = json!({"symbolQuery":"VectorNorth"});
    let selected = engine.map(&options)?;
    assert_eq!(map_names(&selected), strings(&["VectorNorth"]));
    let requests = mock.requests("/embeddings");
    let symbol_requests = &requests[content_calls..];
    assert_eq!(
        symbol_requests.len(),
        2,
        "one document and one query in the separate symbol profile"
    );
    for request in symbol_requests {
        assert_eq!(
            request.body["dimensions"], 2,
            "native dimension request, not client truncation"
        );
        assert_eq!(request.body["input"], json!(["vector north"]));
    }
    let manifest = symbol_manifest(&first)?;
    assert_eq!(manifest["dimensions"], 2);
    let content_profile = slopdex::providers::Providers::new(&small)?.embedding_profile();
    assert_eq!(content_profile["dimensions"], 4);
    assert!(content_profile.get("symbolNormalizationVersion").is_none());
    let mut symbol_profile = content_profile.clone();
    symbol_profile["dimensions"] = json!(2);
    symbol_profile["symbolNormalizationVersion"] = json!("symbols-v1");
    let bytes_for = |repo: &Repo, profile: &Value, query| -> Result<i64> {
        let key = slopdex::storage::Database::embedding_key(profile, query, "vector north");
        Ok(repo.db()?.query_row(
            "SELECT length(vector) FROM embeddings WHERE key=?",
            [key],
            |r| r.get(0),
        )?)
    };
    assert_eq!(bytes_for(&first, &content_profile, true)?, 16);
    for query in [false, true] {
        assert_eq!(bytes_for(&first, &symbol_profile, query)?, 8);
    }
    assert_eq!(
        engine.search("vector north", "search-code", &all())?,
        content
    );
    let small_calls = mock.count();
    drop(engine);
    let mut other = second.open(&small)?;
    other.refresh_structure()?;
    assert_eq!(other.map(&options)?, selected);
    assert_eq!(symbol_manifest(&second)?["dimensions"], 2);
    assert_eq!(
        mock.count(),
        small_calls,
        "another workspace hydrates symbol document/query artifacts"
    );
    drop(other);

    let engine = first.open(&default)?;
    assert_eq!(engine.map(&options)?, selected);
    assert_eq!(
        symbol_manifest(&first)?["dimensions"],
        4,
        "default symbol dimensions are min(content, 256)"
    );
    assert_eq!(
        mock.count(),
        small_calls + 2,
        "neither two-dimensional symbols nor same-text content query may satisfy a new symbol profile"
    );
    symbol_profile["dimensions"] = json!(4);
    for query in [false, true] {
        assert_eq!(bytes_for(&first, &symbol_profile, query)?, 16);
    }
    assert_eq!(
        engine.search("vector north", "search-code", &all())?,
        content,
        "symbol profile switch must preserve content scores/readiness"
    );
    let calls = mock.count();
    drop(engine);
    for config in [&default, &small, &default, &small] {
        for repo in [&first, &second] {
            let engine = repo.open(config)?;
            assert_eq!(engine.map(&options)?, selected);
            assert_eq!(
                mock.count(),
                calls,
                "warm profiles reuse local/shared artifacts"
            );
        }
    }
    let mut isolated = small.clone();
    isolated["embeddingModel"] = json!("isolated-symbol-model");
    let mut engine = second.open(&isolated)?;
    engine.refresh_structure()?;
    assert_eq!(engine.map(&options)?, selected);
    assert_eq!(
        mock.count(),
        calls + 2,
        "shared cache must isolate embedding model profiles"
    );
    for request in &mock.requests("/embeddings")[calls..] {
        assert_eq!(request.body["model"], "isolated-symbol-model");
        assert_eq!(request.body["dimensions"], 2);
    }
    drop(engine);
    assert_eq!(second.open(&small)?.map(&options)?, selected);
    assert_eq!(mock.count(), calls + 2);
    Ok(())
}

#[test]
fn symbol_map_without_index_warns_on_stdout_and_ignores_queries_without_provider() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    // The local fallback must work without provider credentials or cache writes.
    for key in ["embeddingApiKey", "descriptionApiKey", "rerankerApiKey"] {
        config.as_object_mut().unwrap().remove(key);
    }
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("api.ts", "export function VectorNorth() {}\n")?;
    let warning = "slopdex: warning: no active index; -q is ignored.\n";
    let before = filesystem_snapshot(repo._temp.path())?;
    for (prefix, format) in [
        (vec![], "json"),
        (vec!["--no-reindex"], "json"),
        (vec![], "summary"),
    ] {
        let ordinary = repo.cli_format(&[prefix.clone(), vec!["map"]].concat(), format)?;
        assert!(ordinary.status.success());
        for queries in [
            vec!["-q", "VECTOR_NORTH"],
            vec!["--symbol-query", "absent"],
            vec![
                "-q",
                "absent",
                "-q",
                "also absent",
                "--symbol-threshold",
                "1",
            ],
        ] {
            let output =
                repo.cli_format(&[prefix.clone(), vec!["map"], queries].concat(), format)?;
            assert!(output.status.success(), "{:?}", output.stderr);
            assert_eq!(
                output.stdout.strip_prefix(warning.as_bytes()),
                Some(ordinary.stdout.as_slice())
            );
            assert!(!String::from_utf8_lossy(&output.stderr).contains("no active index"));
        }
        assert!(!repo.index.exists());
        assert_eq!(mock.count(), 0);
        assert_eq!(filesystem_snapshot(repo._temp.path())?, before);
    }
    assert_eq!(
        map_names(repo.cli_json(&["map"])?.as_array().unwrap()),
        strings(&["VectorNorth"])
    );
    assert!(!repo.index.exists());
    let mut engine = repo.open_map(&config)?;
    engine.refresh_structure()?;
    assert_eq!(
        map_names(&engine.map(&json!({}))?),
        strings(&["VectorNorth"])
    );
    drop(engine);
    assert_eq!(
        map_names(repo.cli_json(&["map"])?.as_array().unwrap()),
        strings(&["VectorNorth"])
    );
    assert_eq!(mock.count(), 0);
    assert_eq!(artifact_counts(&repo)?.0, 0);
    assert!(!PathBuf::from(format!("{}.symbols.usearch", repo.index.display())).exists());
    Ok(())
}

#[test]
fn symbol_map_without_index_preserves_other_selectors_and_expanded_heading_bodies() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write(
        "docs/guide.md",
        "# Guide\n\nAncestor body.\n\n## Setup\n\nSelected body.\n\n## Other\n\nSibling body.\n",
    )?;
    repo.write("excluded.md", "# Setup\n\nExcluded body.\n")?;
    let args = [
        "--no-reindex",
        "--detail",
        "expanded",
        "map",
        "docs",
        "-g",
        "*.md",
        "-k",
        "headings",
        "-e",
        "^guide\\.setup$",
        "-i",
        "--private",
    ];
    let warning = "slopdex: warning: no active index; -q is ignored.\n";
    for format in ["json", "summary"] {
        let ordinary = repo.cli_format(&args, format)?;
        assert!(ordinary.status.success(), "{:?}", ordinary.stderr);
        let output = repo.cli_format(
            &[
                args.to_vec(),
                vec!["-q", "unrelated query", "--symbol-threshold", "1"],
            ]
            .concat(),
            format,
        )?;
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(
            output.stdout.strip_prefix(warning.as_bytes()),
            Some(ordinary.stdout.as_slice())
        );
        if format == "summary" {
            let text = String::from_utf8(output.stdout)?;
            assert!(
                text.contains("# Guide")
                    && text.contains("## Setup")
                    && text.contains("Selected body."),
                "{text}"
            );
            for omitted in [
                "Ancestor body.",
                "Sibling body.",
                "Excluded body.",
                "## Other",
            ] {
                assert!(!text.contains(omitted), "{text}");
            }
        }
    }
    let missing = repo.cli(&["map", "missing", "-q", "absent"])?;
    assert!(missing.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(missing.stdout.strip_prefix(warning.as_bytes()).unwrap())?,
        json!([])
    );
    assert!(!repo.index.exists());
    assert_eq!(mock.count(), 0);
    Ok(())
}

#[test]
fn shared_search_globs_precede_limit_and_markdown_regexes_match_heading_paths() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(".slopdex/config.json", &config.to_string())?;
    let distractors: String = (0..48)
        .map(|i| function(&format!("distractor_{i}"), "VECTOR_EAST"))
        .collect();
    repo.write("other/nearest.rs", &distractors)?;
    repo.write("src/selected.rs", &function("Selected", "VECTOR_NORTH"))?;
    repo.write("src/excluded.rs", &function("Excluded", "VECTOR_EAST"))?;
    repo.write("docs/guide.md", "# Guide\n\nOverview VECTOR_EAST.\n\n## Install\n\nVECTOR_NORTH.\n\n## Usage\n\nVECTOR_MID.\n")?;
    repo.write("other/guide.md", "# Guide\n\n## Install\n\nVECTOR_EAST.\n")?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let options = json!({"glob":["src/**", "!src/excluded.rs"], "regexp":["^selected$", "^absent$"], "ignoreCase":true, "limit":1, "minSimilarity":-1});
    let selected = engine.search("east", "search-code", &options)?;
    assert_eq!(names(&selected), strings(&["Selected"]));
    near(&selected[0]["similarity"], 0.0);
    let restored = engine.search("east", "search-code", &json!({"glob":["src/**", "!src/excluded.rs", "src/excluded.rs"], "limit":1, "minSimilarity":-1}))?;
    assert_eq!(
        names(&restored),
        strings(&["Excluded"]),
        "the last matching glob wins"
    );
    let md_options = json!({"glob":["docs/**"], "regexp":["^guide\\.install$", "^absent$"], "ignoreCase":true, "limit":1, "minSimilarity":-1});
    let md = engine.search("east", "search-md", &md_options)?;
    assert_eq!(md.len(), 1);
    assert_eq!(md[0]["chunk"]["path"], "docs/guide.md");
    assert_eq!(md[0]["chunk"]["headingPath"], json!(["Guide", "Install"]));
    near(&md[0]["similarity"], 0.0);
    let calls = mock.count();
    drop(engine);
    assert_eq!(
        repo.cli_json(&[
            "--no-reindex",
            "search-code",
            "east",
            "-g",
            "src/**",
            "-g",
            "!src/excluded.rs",
            "-e",
            "^selected$",
            "-e",
            "^absent$",
            "-i",
            "--limit",
            "1",
            "--threshold",
            "-1"
        ])?,
        json!(selected)
    );
    assert_eq!(
        repo.cli_json(&[
            "--no-reindex",
            "search-md",
            "east",
            "-g",
            "**/*.md",
            "-g",
            "!other/**",
            "-e",
            "^guide\\.install$",
            "-e",
            "^absent$",
            "-i",
            "--limit",
            "1",
            "--threshold",
            "-1"
        ])?,
        json!(md)
    );
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn configuration_markup_and_shell_are_indexed_end_to_end() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(
        "config/app.json",
        "{\"service\": {\"port\": 8080}, \"marker\": \"VECTOR_NORTH\"}\n",
    )?;
    repo.write("web/index.html", "<main><h1>VECTOR_EAST</h1></main>\n")?;
    repo.write("scripts/deploy.sh", "deploy() { echo VECTOR_MID; }\n")?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let map = engine.map(&json!({}))?;
    assert!(map.iter().any(|file| {
        file["path"] == "config/app.json"
            && file["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["qualifiedName"] == "service.port")
    }));
    assert!(map.iter().any(|file| {
        file["path"] == "web/index.html"
            && file["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["qualifiedName"] == "main.h1")
    }));
    let results = engine.search(
        "north",
        "search",
        &json!({"minSimilarity":-1,"glob":["config/**"]}),
    )?;
    assert!(
        results
            .iter()
            .any(|row| row["type"] == "document" && row["chunk"]["path"] == "config/app.json")
    );
    assert!(
        engine
            .search("north", "search-md", &json!({"minSimilarity":-1}))?
            .is_empty()
    );
    let code = engine.search("mid", "search-code", &json!({"minSimilarity":-1}))?;
    assert_eq!(names(&code), strings(&["deploy"]));
    Ok(())
}

#[test]
fn shared_cross_filters_select_sources_without_filtering_targets() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("src/source.rs", &function("Source", "VECTOR_EAST"))?;
    repo.write("src/excluded.rs", &function("Excluded", "VECTOR_NORTH"))?;
    repo.write("targets/peer.rs", &function("Peer", "VECTOR_MID"))?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let calls = mock.count();
    let rows = engine.cross_search(None, &json!({"glob":["src/**", "!src/excluded.rs"], "regexp":["^source$", "^absent$"], "ignoreCase":true, "matches":1, "minSimilarity":-1}))?;
    assert_eq!(sources(&rows), strings(&["Source"]));
    assert_eq!(
        names(rows[0]["matches"].as_array().unwrap()),
        strings(&["Peer"])
    );
    near(&rows[0]["matches"][0]["similarity"], 0.8);
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn rerank_and_task_description_caches_survive_restart_and_unrelated_generation_changes()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["rerankingEnabled"] = json!(true);
    repo.write("selected.rs", &function("selected", "VECTOR_EAST"))?;
    repo.write("unrelated.rs", &function("other", "VECTOR_NORTH"))?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let options = json!({"glob":"selected.rs", "minSimilarity":-1});
    let ranked = engine.search("east", "search-code", &options)?;
    let answer = engine.describe("east", &options)?;
    assert_eq!(mock.requests("/responses").len(), 1);
    let calls = mock.count();
    assert_eq!(engine.describe("east", &options)?, answer);
    assert_eq!(
        mock.count(),
        calls,
        "repeated task descriptions reuse their paid answer"
    );
    let generation = engine.status()?["generation"].as_u64().unwrap();
    // This only shifts an excluded callable's position; all paid inputs are identical.
    repo.write(
        "unrelated.rs",
        &(String::from("\n\n") + &function("other", "VECTOR_NORTH")),
    )?;
    engine.refresh()?;
    assert!(engine.status()?["generation"].as_u64().unwrap() > generation);
    assert_eq!(engine.search("east", "search-code", &options)?, ranked);
    assert_eq!(engine.describe("east", &options)?, answer);
    assert_eq!(
        mock.count(),
        calls,
        "identical query, rerank documents and explanation prompt survive generation changes"
    );
    drop(engine);
    // Force query-result misses so persistence of paid artifacts is independently tested.
    repo.db()?.execute("DELETE FROM search_cache", [])?;
    let engine = repo.open(&config)?;
    assert_eq!(engine.search("east", "search-code", &options)?, ranked);
    assert_eq!(engine.describe("east", &options)?, answer);
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn schema3_file_columns_are_authoritative_for_saved_description_context() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    let source = function("saved", "VECTOR_EAST");
    repo.write("code.rs", &source)?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let expected = engine.search("east", "search-code", &all())?;
    drop(engine);
    repo.db()?.execute("UPDATE files SET data='{}'", [])?;
    fs::remove_file(repo.root.join("code.rs"))?;
    let engine = repo.open(&config)?;
    assert_eq!(engine.search("east", "search-code", &all())?, expected);
    engine.describe("east", &json!({"minSimilarity":0.9}))?;
    let requests = mock.requests("/responses");
    let prompt = requests.last().unwrap().body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        prompt.contains(&format!("@ code:\n{}", source.trim_end())),
        "{prompt}"
    );
    assert!(!prompt.contains("Full source code for best matching files"));
    Ok(())
}

#[test]
fn shared_artifacts_reuse_paid_work_across_independent_workspace_indexes() -> Result<()> {
    let mock = Mock::start()?;
    let temp = tempfile::tempdir()?;
    let cache = temp.path().join("shared.sqlite");
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    config["artifactCachePath"] = json!(cache);
    let source = function("shared", "VECTOR_EAST");
    let roots: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|name| temp.path().join(name))
        .collect();
    for root in &roots {
        fs::create_dir(root)?;
        fs::write(root.join("api.rs"), &source)?;
    }
    let first = roots[0].join("index.sqlite");
    let second = roots[1].join("index.sqlite");
    let mut engine = Engine::open(&roots[0], &first, config.clone())?;
    engine.refresh()?;
    let calls = mock.count();
    assert!(
        calls >= 3,
        "file and callable descriptions plus code vectors"
    );
    drop(engine);

    let mut engine = Engine::open(&roots[1], &second, config.clone())?;
    engine.refresh()?;
    assert_eq!(
        mock.count(),
        calls,
        "second workspace must reuse all paid provider output"
    );
    assert_eq!(engine.status()?["fileDescriptionCount"], 1);
    assert_eq!(engine.status()?["descriptionCount"], 1);
    assert_ne!(first, second);
    let results = engine.search("east", "search-code", &all())?;
    assert!(!results.is_empty());
    assert_eq!(
        mock.count(),
        calls + 1,
        "new query still needs its own embedding"
    );
    drop(engine);

    let other = Mock::start()?;
    let mut other_config = other.config();
    other_config["artifactCachePath"] = json!(cache);
    let third = roots[1].join("other-index.sqlite");
    let mut engine = Engine::open(&roots[1], &third, other_config)?;
    engine.refresh()?;
    assert!(
        !other.embedding_inputs().is_empty(),
        "different endpoint must not reuse vectors"
    );
    Ok(())
}

/// Run with SLOPDEX_MINIO_ENDPOINT=http://127.0.0.1:9000 against MinIO.
#[test]
fn minio_shares_artifacts_across_machine_local_caches_and_survives_outage() -> Result<()> {
    let Ok(endpoint) = std::env::var("SLOPDEX_MINIO_ENDPOINT") else {
        return Ok(());
    };
    let region = s3::Region::Custom {
        region: "us-east-1".into(),
        endpoint: endpoint.clone(),
    };
    let credentials =
        s3::creds::Credentials::new(Some("minioadmin"), Some("minioadmin"), None, None, None)?;
    let bucket = format!("slopdex-test-{}", std::process::id());
    let created = s3::Bucket::create_with_path_style(
        &bucket,
        region,
        credentials,
        s3::BucketConfiguration::default(),
    )?;
    ensure!(
        (200..300).contains(&created.response_code),
        "MinIO bucket creation failed"
    );
    let mock = Mock::start()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    config.as_object_mut().unwrap().remove("artifactCachePath");
    config["artifactS3"] =
        json!({"bucket": bucket, "endpoint": endpoint, "region": "us-east-1", "prefix":"tests"});
    let mut repos = Vec::new();
    for ordinal in 0..3 {
        let repo = Repo::new()?;
        repo.write(
            if ordinal == 1 { "renamed.rs" } else { "api.rs" },
            &function("shared", "VECTOR_EAST"),
        )?;
        repo.write(".slopdex/config.json", &config.to_string())?;
        repos.push(repo);
    }
    let run = |repo: &Repo, unavailable: bool| -> Result<()> {
        let mut command = repo.child(env!("CARGO_BIN_EXE_slopdex"));
        command
            .arg("--root")
            .arg(&repo.root)
            .arg("--format")
            .arg("json")
            .arg("update")
            .env("XDG_CACHE_HOME", repo.home.join("cache"))
            .env("AWS_ACCESS_KEY_ID", "minioadmin")
            .env("AWS_SECRET_ACCESS_KEY", "minioadmin");
        if unavailable {
            let mut offline = config.clone();
            offline["artifactS3"]["endpoint"] = json!("http://127.0.0.1:1");
            fs::write(repo.root.join(".slopdex/config.json"), offline.to_string())?;
        }
        let output = command.output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    run(&repos[0], false)?;
    let paid = mock.count();
    ensure!(paid >= 3, "expected provider requests on cold cache");
    run(&repos[1], false)?;
    assert_eq!(mock.count(), paid, "second machine must reuse S3 artifacts");
    let first_index = repos[0]
        .home
        .join("cache/slopdex/workspaces")
        .join(slopdex::hash(
            repos[0].root.canonicalize()?.as_os_str().as_encoded_bytes(),
        ))
        .join("index.sqlite");
    let index = Connection::open(first_index)?;
    let (file_key, record): (String, String) = index.query_row(
        "SELECT key,value FROM cache WHERE kind='description' AND json_extract(value,'$.generation.scope')='file'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let metadata: Value = serde_json::from_str(&record)?;
    let system_hash = metadata["generation"]["system_hash"].as_str().unwrap();
    let description_object = format!("/tests/v3/description/{}/{file_key}", &file_key[..2]);
    let remote_record = created.bucket.get_object(description_object)?;
    assert_eq!(remote_record.status_code(), 200);
    assert_eq!(&remote_record.as_slice()[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
    let decoded = zstd::stream::decode_all(remote_record.as_slice())?;
    assert_eq!(&decoded[65..], record.as_bytes());
    assert!(!record.contains("Describe existing source code accurately"));
    let content_object = format!("/tests/v3/content/{}/{system_hash}", &system_hash[..2]);
    let remote_content = created.bucket.get_object(&content_object)?;
    assert_eq!(remote_content.status_code(), 200);
    let stored_system: String = index.query_row(
        "SELECT content FROM description_content WHERE hash=?",
        [system_hash],
        |row| row.get(0),
    )?;
    let decoded = zstd::stream::decode_all(remote_content.as_slice())?;
    assert_eq!(&decoded[65..], stored_system.as_bytes());
    let removed = created.bucket.delete_object(content_object)?;
    ensure!(
        (200..300).contains(&removed.status_code()),
        "could not remove a remote context object"
    );
    let missing = Repo::new()?;
    missing.write("moved.rs", &function("shared", "VECTOR_EAST"))?;
    missing.write(".slopdex/config.json", &config.to_string())?;
    run(&missing, false)?;
    assert!(
        mock.count() > paid,
        "missing context must trigger provider fallback"
    );
    let paid_after_missing = mock.count();
    let profile = slopdex::providers::Providers::new(&config)?.embedding_profile();
    let key = slopdex::storage::Database::embedding_key(&profile, true, "uncached-query");
    let zero = [0_u8; 16];
    let mut corrupt = format!("{}\n", slopdex::hash(zero)).into_bytes();
    corrupt.extend_from_slice(&zero);
    let object = format!("/tests/v3/embedding/{}/{key}", &key[..2]);
    let uploaded = created
        .bucket
        .put_object(object, &zstd::stream::encode_all(corrupt.as_slice(), 3)?)?;
    ensure!(
        (200..300).contains(&uploaded.status_code()),
        "could not stage invalid vector"
    );
    let searched = repos[1]
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repos[1].root)
        .args([
            "--no-reindex",
            "--format",
            "json",
            "search-code",
            "uncached-query",
        ])
        .env("XDG_CACHE_HOME", repos[1].home.join("cache"))
        .env("AWS_ACCESS_KEY_ID", "minioadmin")
        .env("AWS_SECRET_ACCESS_KEY", "minioadmin")
        .output()?;
    ensure!(
        searched.status.success(),
        "{}",
        String::from_utf8_lossy(&searched.stderr)
    );
    assert_eq!(
        mock.count(),
        paid_after_missing + 1,
        "invalid remote vector must fall back to provider"
    );
    run(&repos[2], true)?;
    assert!(
        mock.count() > paid_after_missing + 1,
        "outage must fall back to provider without failing"
    );
    let existing = Repo::new()?;
    let other = Repo::new()?;
    let mut backfill = config.clone();
    backfill.as_object_mut().unwrap().remove("artifactS3");
    existing.write("api.rs", &function("backfill", "VECTOR_WEST"))?;
    existing.write(".slopdex/config.json", &backfill.to_string())?;
    run(&existing, false)?;
    let already_paid = mock.count();
    backfill["artifactS3"] = config["artifactS3"].clone();
    backfill["artifactS3"]["prefix"] = json!("backfill");
    existing.write(".slopdex/config.json", &backfill.to_string())?;
    run(&existing, false)?;
    assert_eq!(
        mock.count(),
        already_paid,
        "enabling S3 must export existing artifacts without provider calls"
    );
    other.write("api.rs", &function("backfill", "VECTOR_WEST"))?;
    other.write(".slopdex/config.json", &backfill.to_string())?;
    run(&other, false)?;
    assert_eq!(
        mock.count(),
        already_paid,
        "a second machine must reuse exported artifacts"
    );
    Ok(())
}

#[test]
fn default_xdg_index_migrates_legacy_sqlite_without_discarding_wal_data() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("api.rs", &function("saved", "VECTOR_EAST"))?;
    let legacy = repo.root.join(".slopdex/index.sqlite");
    let mut engine = Engine::open(&repo.root, &legacy, config)?;
    engine.refresh()?;
    let expected_rows = engine.search("east", "search-code", &all())?;
    let generation = engine.status()?["generation"].clone();
    drop(engine);
    let calls = mock.count();
    let xdg = repo.home.join("xdg-cache");
    let searched = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .args([
            "--no-reindex",
            "--format",
            "json",
            "search-code",
            "east",
            "--threshold",
            "-1",
        ])
        .env("XDG_CACHE_HOME", &xdg)
        .output()?;
    ensure!(
        searched.status.success(),
        "{}",
        String::from_utf8_lossy(&searched.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&searched.stdout)?,
        json!(expected_rows)
    );
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&repo.root)
        .arg("--no-reindex")
        .args(["--format", "json", "status"])
        .env("XDG_CACHE_HOME", &xdg)
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout)?;
    let expected = xdg
        .join("slopdex/workspaces")
        .join(slopdex::hash(
            repo.root.canonicalize()?.as_os_str().as_encoded_bytes(),
        ))
        .join("index.sqlite")
        .canonicalize()?;
    assert_eq!(status["indexPath"], json!(expected));
    assert_eq!(status["generation"], generation);
    assert_eq!(status["fileCount"], 1);
    assert!(legacy.exists() && expected.exists());
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn existing_workspace_vectors_seed_new_shared_cache_without_provider_calls() -> Result<()> {
    let mock = Mock::start()?;
    let temp = tempfile::tempdir()?;
    let source = function("saved", "VECTOR_EAST");
    let roots: Vec<_> = ["original", "other"]
        .into_iter()
        .map(|name| temp.path().join(name))
        .collect();
    for root in &roots {
        fs::create_dir(root)?;
        fs::write(root.join("api.rs"), &source)?;
    }
    let mut config = mock.config();
    let original = roots[0].join("index.sqlite");
    // Like Repo::open, tolerate brief flock inheritance by concurrent test children.
    let open = |root: &PathBuf, index: &PathBuf, config: Value| {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match Engine::open(root, index, config.clone()) {
                Err(error)
                    if error.to_string().starts_with("Index is in use")
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10))
                }
                result => break result,
            }
        }
    };
    let mut engine = open(&roots[0], &original, config.clone())?;
    engine.refresh()?;
    drop(engine);
    let calls = mock.count();
    config["artifactCachePath"] = json!(temp.path().join("fresh-shared.sqlite"));
    drop(open(&roots[0], &original, config.clone())?);
    let mut other = open(&roots[1], &roots[1].join("index.sqlite"), config)?;
    other.refresh()?;
    assert_eq!(
        mock.count(),
        calls,
        "existing paid vectors must seed the shared cache"
    );
    Ok(())
}

#[test]
fn description_keys_reuse_renames_and_models_while_retaining_generation_context() -> Result<()> {
    let mock = Mock::start()?;
    let first = Repo::new()?;
    let second = Repo::new()?;
    let source = function("shared", "VECTOR_EAST");
    let shared = first.home.join("artifacts.sqlite");
    let mut config = mock.config();
    config["artifactCachePath"] = json!(shared);
    config["descriptionsEnabled"] = json!(true);
    first.write("src/old.rs", &source)?;
    let mut engine = first.open(&config)?;
    engine.refresh()?;
    let description = file_record(&first, "src/old.rs")?["description"].clone();
    let callable: String = first.db()?.query_row(
        "SELECT text FROM descriptions WHERE scope='callable' AND path='src/old.rs'",
        [],
        |row| row.get(0),
    )?;
    let rows: Vec<(String, Value)> = first
        .db()?
        .prepare("SELECT key,value FROM cache WHERE kind='description'")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (key, text) = row?;
            Ok((key, serde_json::from_str(&text)?))
        })
        .collect::<Result<_>>()?;
    assert_eq!(rows.len(), 2);
    let db = first.db()?;
    for (key, artifact) in &rows {
        let generation = &artifact["generation"];
        let content = |field: &str| -> Result<String> {
            let content_hash = generation[field].as_str().context("missing content hash")?;
            let text: String = db.query_row(
                "SELECT content FROM description_content WHERE hash=?",
                [content_hash],
                |row| row.get(0),
            )?;
            assert_eq!(slopdex::hash(&text), content_hash);
            Ok(text)
        };
        let system = content("system_hash")?;
        let expected = if generation["scope"] == "file" {
            slopdex::hash(json!([slopdex::hash(&source), system]).to_string())
        } else {
            slopdex::hash(
                json!([
                    "shared",
                    generation["source_hash"],
                    description.as_str().unwrap(),
                    system
                ])
                .to_string(),
            )
        };
        assert_eq!(*key, expected);
        assert_eq!(generation["path"], "src/old.rs");
        let profile: Value = serde_json::from_str(&content("profile_hash")?)?;
        assert_eq!(profile["model"], "integration-description");
        let prompt = content("prompt_hash")?;
        let first_message = &generation["messages"][0];
        let context: String = db.query_row(
            "SELECT content FROM description_content WHERE hash=?",
            [first_message["content_hash"].as_str().unwrap()],
            |row| row.get(0),
        )?;
        assert!(context.contains("File: src/old.rs"));
        assert!(context.contains("VECTOR_EAST"));
        assert!(prompt.starts_with("Describe"));
        assert!(artifact["generation"].get("prompt").is_none());
        assert!(artifact["generation"].get("system").is_none());
        assert!(
            !artifact.to_string().contains(TEST_KEY),
            "credentials must not be stored"
        );
    }
    drop(db);
    let calls = mock.requests("/responses").len();
    fs::rename(first.root.join("src/old.rs"), first.root.join("src/new.rs"))?;
    engine.refresh()?;
    assert_eq!(mock.requests("/responses").len(), calls);
    assert_eq!(
        file_record(&first, "src/new.rs")?["description"],
        description
    );
    assert_eq!(
        first.db()?.query_row::<String, _, _>(
            "SELECT text FROM descriptions WHERE scope='callable' AND path='src/new.rs'",
            [],
            |row| row.get(0)
        )?,
        callable
    );
    drop(engine);

    second.write("src/another.rs", &source)?;
    let mut different_model = config.clone();
    different_model["descriptionModel"] = json!("alternate-description-model");
    different_model["descriptionFallbackModel"] = json!("fallback-model");
    let mut engine = second.open(&different_model)?;
    engine.refresh()?;
    assert_eq!(mock.requests("/responses").len(), calls);
    assert_eq!(
        file_record(&second, "src/another.rs")?["description"],
        description
    );
    assert_eq!(
        second.db()?.query_row::<String, _, _>(
            "SELECT text FROM descriptions WHERE scope='callable' AND path='src/another.rs'",
            [],
            |row| row.get(0)
        )?,
        callable
    );
    Ok(())
}

#[test]
fn description_context_is_stored_once_by_hash_across_callable_artifacts() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let mut config = mock.config();
    config["descriptionsEnabled"] = json!(true);
    let source: String = (0..8)
        .map(|i| function(&format!("method_{i}"), "VECTOR_EAST"))
        .collect();
    repo.write("api.rs", &source)?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let db = repo.db()?;
    let rows: Vec<Value> = db
        .prepare("SELECT value FROM cache WHERE kind='description'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<_>>()?;
    assert_eq!(rows.len(), 9);
    let refs: Vec<_> = rows.iter().map(|row| &row["generation"]).collect();
    let unique = |field: &str| -> BTreeSet<String> {
        refs.iter()
            .filter_map(|row| row[field].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(unique("system_hash").len(), 1);
    assert_eq!(unique("prompt_hash").len(), 9);
    assert_eq!(unique("profile_hash").len(), 1);
    assert_eq!(unique("settings_hash").len(), 1);
    assert_eq!(unique("file_description_hash").len(), 1);
    let all_hashes: BTreeSet<_> = [
        "system_hash",
        "prompt_hash",
        "profile_hash",
        "settings_hash",
        "file_description_hash",
    ]
    .into_iter()
    .flat_map(unique)
    .chain(refs.iter().flat_map(|row| {
        row["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["content_hash"].as_str().unwrap().to_owned())
    }))
    .collect();
    let source_messages: BTreeSet<_> = refs
        .iter()
        .map(|row| row["messages"][0]["content_hash"].as_str().unwrap())
        .collect();
    assert_eq!(
        source_messages.len(),
        1,
        "full-file context is stored once across every turn"
    );
    let count: i64 = db.query_row("SELECT count(*) FROM description_content", [], |row| {
        row.get(0)
    })?;
    assert_eq!(count as usize, all_hashes.len());
    for content_hash in &all_hashes {
        let text: String = db.query_row(
            "SELECT content FROM description_content WHERE hash=?",
            [content_hash],
            |row| row.get(0),
        )?;
        assert_eq!(slopdex::hash(&text), *content_hash);
    }
    assert!(rows.iter().all(|row| {
        !row.to_string().contains("VECTOR_EAST")
            && !row
                .to_string()
                .contains("Describe existing source code accurately")
    }));
    let local = Connection::open(mock.cache.path().join("artifacts.sqlite"))?;
    let local_count: i64 = local.query_row(
        "SELECT count(*) FROM artifacts WHERE kind='content'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(local_count, count);
    Ok(())
}
