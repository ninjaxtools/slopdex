//! Local-only Ollama lifecycle tests: no service, credentials, or downloads.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use slopdex::{
    engine::Engine,
    providers::{Providers, prepare_embeddings},
};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const MODEL: &str = "custom:test";
const DIGEST: &str = "sha256:original-local-model";

#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    body: Value,
    digest: String,
}

struct State {
    installed: bool,
    digest: String,
    cloud_tags: bool,
    cloud_show: bool,
    fail_embed: bool,
    requests: Vec<Request>,
}

struct Ollama {
    base: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    finished: mpsc::Receiver<Result<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Ollama {
    fn start(installed: bool) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base = format!("http://{}/api", listener.local_addr()?);
        let state = Arc::new(Mutex::new(State {
            installed,
            digest: DIGEST.into(),
            cloud_tags: false,
            cloud_show: false,
            fail_embed: false,
            requests: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, finished) = mpsc::channel();
        let shared = state.clone();
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let result = (|| -> Result<()> {
                let deadline = Instant::now() + Duration::from_secs(90);
                while !stopping.load(Ordering::Acquire) {
                    ensure!(
                        Instant::now() < deadline,
                        "Ollama mock exceeded its lifetime"
                    );
                    match listener.accept() {
                        Ok((stream, _)) => serve(stream, &shared)?,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(())
            })();
            let _ = tx.send(result);
        });
        Ok(Self {
            base,
            state,
            stop,
            finished,
            worker: Some(worker),
        })
    }

    fn requests(&self, path: &str) -> Vec<Request> {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|request| request.path == path)
            .cloned()
            .collect()
    }

    fn count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    fn digest(&self, digest: &str) {
        self.state.lock().unwrap().digest = digest.into();
    }
}

impl Drop for Ollama {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.finished.recv_timeout(Duration::from_secs(5));
        // Each accepted socket has a two-second timeout. Join even when the
        // worker reports a failure, before propagating it into the test.
        let joined = self.worker.take().unwrap().join();
        if !thread::panicking() {
            assert!(joined.is_ok(), "Ollama mock worker panicked");
            result
                .expect("Ollama mock did not stop in time")
                .expect("Ollama mock request failed");
        }
    }
}

fn serve(mut stream: TcpStream, shared: &Mutex<State>) -> Result<()> {
    // Accepted sockets inherit nonblocking mode on Windows.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(&mut stream);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut parts = first.split_whitespace();
    let method = parts.next().context("missing HTTP method")?.to_owned();
    let path = parts.next().context("missing HTTP path")?.to_owned();
    let mut length = 0;
    let mut header_bytes = first.len();
    loop {
        let mut line = String::new();
        ensure!(reader.read_line(&mut line)? > 0, "incomplete HTTP headers");
        header_bytes += line.len();
        ensure!(header_bytes <= 16_384, "oversized HTTP headers");
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>()?;
        }
    }
    ensure!(length <= 1_048_576, "oversized mock request");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)?
    };
    let mut state = shared.lock().unwrap();
    ensure!(state.requests.len() < 128, "too many Ollama mock requests");
    let digest = state.digest.clone();
    state.requests.push(Request {
        method: method.clone(),
        path: path.clone(),
        body: body.clone(),
        digest,
    });
    let (status, response) = match (method.as_str(), path.as_str()) {
        ("GET", "/api/tags") => {
            let models = if state.installed {
                let mut entry = json!({"name": MODEL, "model": MODEL, "digest": state.digest});
                if state.cloud_tags {
                    entry["remote_host"] = json!("https://ollama.com");
                }
                vec![entry]
            } else {
                Vec::new()
            };
            (200, json!({"models": models}).to_string())
        }
        ("POST", "/api/show") => {
            ensure!(body["model"] == MODEL, "unexpected show model: {body}");
            let mut details = json!({"capabilities": ["embedding"],
                "model_info": {"general.architecture": "bert", "bert.context_length": 4096}});
            if state.cloud_show {
                details["remote_model"] = json!("remote:cloud");
            }
            (200, details.to_string())
        }
        ("POST", "/api/embed") => {
            ensure!(body["model"] == MODEL, "unexpected embedding model: {body}");
            ensure!(
                body.get("dimensions").is_none(),
                "generic models must use native dimensions"
            );
            let inputs = body["input"]
                .as_array()
                .context("embed input must be an array")?;
            if state.fail_embed {
                (
                    400,
                    json!({"error": "mock embedding validation failed"}).to_string(),
                )
            } else {
                let vectors: Vec<_> = inputs
                    .iter()
                    .map(|input| {
                        if input.as_str().unwrap_or_default().contains("NORTH") {
                            json!([0.0, 1.0, 0.0, 0.0])
                        } else {
                            json!([1.0, 0.0, 0.0, 0.0])
                        }
                    })
                    .collect();
                (200, json!({"embeddings": vectors}).to_string())
            }
        }
        ("POST", "/api/pull") => {
            ensure!(
                body["model"] == MODEL && body["stream"] == true,
                "invalid pull: {body}"
            );
            state.installed = true;
            (
                200,
                concat!(
                    "{\"status\":\"pulling manifest\"}\n",
                    "{\"status\":\"downloading\",\"completed\":4,\"total\":4}\n",
                    "{\"status\":\"success\"}\n"
                )
                .to_owned(),
            )
        }
        _ => anyhow::bail!("unexpected Ollama route: {method} {path}"),
    };
    drop(state);
    write!(
        stream,
        "HTTP/1.1 {status} OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    // Send pull events separately so the streaming parser crosses read boundaries.
    for line in response.split_inclusive('\n') {
        stream.write_all(line.as_bytes())?;
        stream.flush()?;
    }
    Ok(())
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
    index: PathBuf,
    cache: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        // Subprocess cwd resolves symlinked temporary ancestors on macOS.
        let base = temp.path().canonicalize()?;
        let root = base.join("repo");
        let home = base.join("home");
        fs::create_dir(&root)?;
        fs::create_dir_all(home.join("slopdex"))?;
        fs::write(
            root.join("code.rs"),
            "pub fn east() -> &'static str { \"EAST\" }\npub fn north() -> &'static str { \"NORTH\" }\n",
        )?;
        Ok(Self {
            root,
            index: base.join("index.sqlite"),
            cache: base.join("artifacts.sqlite"),
            config: home.join("slopdex/config.json"),
            home,
            _temp: temp,
        })
    }

    fn settings(&self, mock: &Ollama) -> Value {
        json!({"provider": "ollama", "model": MODEL, "embeddingBaseUrl": mock.base,
            "artifactCachePath": self.cache, "embeddingBatchSize": 1,
            "embeddingParallelism": 1, "providerTimeoutMs": 1000, "providerMaxRetries": 0,
            "ollamaPullTimeoutMs": 1000})
    }

    fn save(&self, settings: &Value) -> Result<()> {
        fs::write(&self.config, serde_json::to_vec_pretty(settings)?)?;
        Ok(())
    }

    fn saved(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&fs::read(&self.config)?)?)
    }

    fn cli(&self, args: &[&str]) -> Result<Output> {
        let stdout = tempfile::NamedTempFile::new_in(&self.home)?;
        let stderr = tempfile::NamedTempFile::new_in(&self.home)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_slopdex"));
        command
            .current_dir(&self.root)
            .args(["--root"])
            .arg(&self.root)
            .arg("--index")
            .arg(&self.index)
            .args(["--output", "json"])
            .args(args)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .stdin(Stdio::null())
            .stdout(stdout.reopen()?)
            .stderr(stderr.reopen()?);
        for key in [
            "OPENAI_API_KEY",
            "JINA_API_KEY",
            "OLLAMA_API_KEY",
            "OPENCODE_API_KEY",
            "OPENCODE_GO_API_KEY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(key);
        }
        let mut child = command.spawn()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                anyhow::bail!("CLI {args:?} exceeded timeout");
            }
            thread::sleep(Duration::from_millis(5));
        };
        Ok(Output {
            status,
            stdout: fs::read(stdout.path())?,
            stderr: fs::read(stderr.path())?,
        })
    }

    fn success(&self, args: &[&str]) -> Result<Value> {
        let output = self.cli(args)?;
        ensure!(
            output.status.success(),
            "CLI {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

fn assert_config_path(result: &Value, expected: &Path) -> Result<()> {
    let reported = Path::new(
        result["configPath"]
            .as_str()
            .context("missing configPath")?,
    );
    assert_eq!(reported.canonicalize()?, expected.canonicalize()?);
    Ok(())
}

fn failure(output: &Output, message: &str) {
    assert!(!output.status.success(), "unexpected CLI success");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "expected {message:?}: {stderr}");
}

#[test]
fn preparation_pulls_pins_identity_and_engine_verifies_once() -> Result<()> {
    let fixture = Fixture::new()?;
    let mock = Ollama::start(false)?;
    let mut config = fixture.settings(&mock);
    prepare_embeddings(&mut config, true)?;
    assert_eq!(config["dimensions"], 4);
    assert_eq!(config["ollamaNativeDimensions"], 4);
    assert_eq!(config["ollamaModelDigest"], DIGEST);
    assert_eq!(config["ollamaModelFamily"], "generic");
    assert_eq!(config["ollamaContextLength"], 4096);
    assert_eq!(config["ollamaAutoPull"], true);
    assert_eq!(mock.requests("/api/pull").len(), 1);
    assert_eq!(mock.requests("/api/tags").len(), 2);
    assert_eq!(mock.requests("/api/show").len(), 1);
    let smoke = mock.requests("/api/embed");
    assert_eq!(smoke.len(), 1);
    assert_eq!(smoke[0].body["truncate"], false);

    let calls = mock.count();
    let providers = Providers::new(&config)?;
    assert_eq!(providers.dimensions(), 4);
    let mut engine = Engine::open(&fixture.root, &fixture.index, config.clone())?;
    assert_eq!(mock.count(), calls, "saved identity must construct offline");
    engine.refresh()?;
    let options = json!({"threshold": 0, "limit": 20});
    engine.search("EAST query one", "search-code", &options)?;
    engine.search("NORTH query two", "search-code", &options)?;
    assert_eq!(mock.requests("/api/tags").len(), 3);
    assert_eq!(mock.requests("/api/show").len(), 2);
    for request in mock.requests("/api/embed").into_iter().skip(1) {
        assert_eq!(request.method, "POST");
        assert_eq!(request.body["truncate"], true);
        assert_eq!(request.body["options"]["num_ctx"], 4096);
    }
    drop(engine);
    // A saved pin also permits construction when no server can be reached.
    config["embeddingBaseUrl"] = json!("http://127.0.0.1:1/api");
    let offline = Engine::open(&fixture.root, &fixture.index, config)?;
    assert_eq!(mock.requests("/api/tags").len(), 3);
    drop(offline);
    Ok(())
}

#[test]
fn minimal_configuration_recovers_model_identity_without_warm_model_calls() -> Result<()> {
    let fixture = Fixture::new()?;
    let mock = Ollama::start(true)?;
    let config = fixture.settings(&mock);
    let mut engine = Engine::open(&fixture.root, &fixture.index, config.clone())?;
    engine.refresh()?;
    drop(engine);
    let calls = mock.count();
    let mut engine = Engine::open(&fixture.root, &fixture.index, config)?;
    engine.refresh()?;
    assert_eq!(
        mock.count(),
        calls,
        "warm updates recover identity from the index without loading the model"
    );
    Ok(())
}

#[test]
fn noninteractive_config_setup_update_search_and_warm_reuse() -> Result<()> {
    let fixture = Fixture::new()?;
    let mock = Ollama::start(false)?;
    fixture.save(&fixture.settings(&mock))?;
    let result = fixture.success(&[
        "config",
        "embeddings",
        "--provider",
        "ollama",
        "--model",
        MODEL,
        "--pull",
    ])?;
    assert_config_path(&result, &fixture.config)?;
    assert_eq!(result["ollamaModelDigest"], DIGEST);
    assert!(
        !fixture.index.exists(),
        "config embeddings must not create an index"
    );
    assert!(
        !fixture.cache.exists(),
        "setup must not open an artifact store"
    );
    let saved = fs::read(&fixture.config)?;
    fixture.success(&["update"])?;
    assert!(fixture.index.exists());
    let embeds = mock.requests("/api/embed").len();
    let warm = fixture.success(&["update"])?;
    assert_eq!(warm["filesUpdated"], 0);
    assert_eq!(
        mock.requests("/api/embed").len(),
        embeds,
        "warm update must reuse vectors"
    );
    let result = fixture.success(&["search-code", "EAST", "--threshold", "0", "--no-reindex"])?;
    assert!(result.as_array().is_some_and(|rows| !rows.is_empty()));
    let calls = mock.count();
    assert_eq!(
        fixture.success(&["search-code", "EAST", "--threshold", "0", "--no-reindex"])?,
        result
    );
    assert_eq!(
        mock.count(),
        calls,
        "warm query must reuse its cached embedding"
    );
    assert_eq!(
        fs::read(&fixture.config)?,
        saved,
        "ordinary commands must not rewrite configuration"
    );
    Ok(())
}

#[test]
fn update_setup_saves_global_settings_and_bare_model_flags_are_temporary() -> Result<()> {
    let fixture = Fixture::new()?;
    let mock = Ollama::start(false)?;
    let mut config = fixture.settings(&mock);
    config["model"] = json!("stale-previous-selection");
    config["dimensions"] = json!(3072);
    config["ollamaNativeDimensions"] = json!(3072);
    config["ollamaModelDigest"] = json!("stale-digest");
    fixture.save(&config)?;
    fixture.success(&[
        "update",
        "--setup",
        "--provider",
        "ollama",
        "--model",
        MODEL,
        "--pull",
    ])?;
    let saved = fixture.saved()?;
    assert_eq!(saved["model"], MODEL);
    assert_eq!(
        saved["dimensions"], 4,
        "model switch must discard stale dimensions"
    );
    assert_eq!(saved["ollamaNativeDimensions"], 4);
    assert_eq!(saved["ollamaModelDigest"], DIGEST);
    assert_eq!(saved["ollamaAutoPull"], true);
    assert!(fixture.index.exists());
    assert!(
        !fixture.root.join(".slopdex/config.json").exists(),
        "noninteractive setup uses selected global file"
    );

    // Start with an unprepared selection; bare overrides resolve only in memory.
    fixture.save(&config)?;
    let original = fs::read(&fixture.config)?;
    fixture.success(&["update", "--provider", "ollama", "--model", MODEL])?;
    assert_eq!(fs::read(&fixture.config)?, original);
    Ok(())
}

#[test]
fn changed_digest_rejects_inference_and_profiles_recover_without_force() -> Result<()> {
    let fixture = Fixture::new()?;
    let mock = Ollama::start(true)?;
    fixture.save(&fixture.settings(&mock))?;
    fixture.success(&[
        "update",
        "--setup",
        "--provider",
        "ollama",
        "--model",
        MODEL,
    ])?;
    let original = fixture.saved()?;
    // Restoring source after a failed update may allocate new row IDs; the
    // reusable profile contract concerns source content and ranking instead.
    let without_ids = |mut rows: Value| {
        for row in rows.as_array_mut().unwrap() {
            row["function"].as_object_mut().unwrap().remove("id");
        }
        rows
    };
    let expected = without_ids(fixture.success(&[
        "search-code",
        "EAST",
        "--threshold",
        "0",
        "--no-reindex",
    ])?);
    let before = mock.requests("/api/embed").len();
    mock.digest("sha256:replacement-local-model");
    failure(
        &fixture.cli(&["search-code", "uncached query", "--no-reindex"])?,
        "config embeddings",
    );
    assert_eq!(
        mock.requests("/api/embed").len(),
        before,
        "digest mismatch must fail before embedding/cache writes"
    );
    assert_eq!(
        fixture.saved()?,
        original,
        "failed inference must not repin a model"
    );
    let source = fs::read(fixture.root.join("code.rs"))?;
    fs::write(
        fixture.root.join("code.rs"),
        "pub fn changed() -> &'static str { \"new uncached code\" }\n",
    )?;
    failure(&fixture.cli(&["update"])?, "config embeddings");
    assert_eq!(mock.requests("/api/embed").len(), before);
    fs::write(fixture.root.join("code.rs"), source)?;

    let result = fixture.success(&[
        "config",
        "embeddings",
        "--provider",
        "ollama",
        "--model",
        MODEL,
    ])?;
    assert_eq!(
        result["ollamaModelDigest"],
        "sha256:replacement-local-model"
    );
    let prepared = mock.requests("/api/embed").len();
    fixture.success(&["update"])?;
    assert!(
        mock.requests("/api/embed").len() > prepared,
        "new digest needs a separate embedding profile"
    );
    assert!(
        mock.requests("/api/embed")
            .iter()
            .skip(prepared)
            .all(|request| request.digest == "sha256:replacement-local-model")
    );

    mock.digest(DIGEST);
    fixture.success(&[
        "config",
        "embeddings",
        "--provider",
        "ollama",
        "--model",
        MODEL,
    ])?;
    let restored = mock.requests("/api/embed").len();
    fixture.success(&["update"])?;
    assert_eq!(
        without_ids(fixture.success(&[
            "search-code",
            "EAST",
            "--threshold",
            "0",
            "--no-reindex",
        ])?),
        expected
    );
    assert_eq!(
        mock.requests("/api/embed").len(),
        restored,
        "restoring the original pin reuses the old profile and query"
    );
    Ok(())
}

#[test]
fn failed_setup_is_atomic_and_cloud_models_are_rejected() -> Result<()> {
    let fixture = Fixture::new()?;
    let mock = Ollama::start(false)?;
    fixture.save(&fixture.settings(&mock))?;
    let original = fs::read(&fixture.config)?;
    let args = [
        "config",
        "embeddings",
        "--provider",
        "ollama",
        "--model",
        MODEL,
    ];
    failure(&fixture.cli(&args)?, "--pull");
    assert!(mock.requests("/api/pull").is_empty());
    assert_eq!(fs::read(&fixture.config)?, original);
    {
        let mut state = mock.state.lock().unwrap();
        state.installed = true;
        state.cloud_tags = true;
    }
    failure(&fixture.cli(&args)?, "cloud-backed");
    assert!(mock.requests("/api/show").is_empty());
    assert!(mock.requests("/api/embed").is_empty());
    assert_eq!(fs::read(&fixture.config)?, original);
    {
        let mut state = mock.state.lock().unwrap();
        state.cloud_tags = false;
        state.cloud_show = true;
    }
    failure(&fixture.cli(&args)?, "cloud-backed");
    assert!(mock.requests("/api/embed").is_empty());
    assert_eq!(fs::read(&fixture.config)?, original);
    {
        let mut state = mock.state.lock().unwrap();
        state.cloud_show = false;
        state.fail_embed = true;
    }
    failure(&fixture.cli(&args)?, "HTTP 400");
    assert_eq!(
        fs::read(&fixture.config)?,
        original,
        "smoke failure must leave the saved file byte-for-byte unchanged"
    );
    assert!(!fixture.index.exists());
    assert!(!fixture.cache.exists());

    let mut unavailable = fixture.settings(&mock);
    unavailable["embeddingBaseUrl"] = json!("http://127.0.0.1:1/api");
    fixture.save(&unavailable)?;
    let unchanged = fs::read(&fixture.config)?;
    failure(&fixture.cli(&args)?, "ollama serve");
    assert_eq!(fs::read(&fixture.config)?, unchanged);
    Ok(())
}

#[test]
fn missing_index_and_structural_map_never_contact_ollama() -> Result<()> {
    let fixture = Fixture::new()?;
    let mock = Ollama::start(true)?;
    fixture.save(&fixture.settings(&mock))?;
    failure(&fixture.cli(&["search-code", "EAST"])?, "slopdex update");
    assert_eq!(
        mock.count(),
        0,
        "missing-index guard must run before identity resolution"
    );
    assert!(!fixture.cache.exists());
    fixture.success(&["map"])?;
    assert_eq!(
        mock.count(),
        0,
        "unindexed structural map must not select embeddings"
    );

    let mut unavailable = fixture.settings(&mock);
    unavailable["embeddingBaseUrl"] = json!("http://127.0.0.1:1/api");
    // Structural commands do not require a valid semantic model selection.
    unavailable["dimensions"] = json!(9999);
    fixture.save(&unavailable)?;
    let mut engine = Engine::open_map(&fixture.root, &fixture.index, unavailable)?;
    engine.refresh_structure()?;
    drop(engine);
    let result = fixture.success(&["map"])?;
    assert!(result.as_array().is_some_and(|rows| !rows.is_empty()));
    assert_eq!(mock.count(), 0);
    Ok(())
}
