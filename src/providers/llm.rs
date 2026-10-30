//! Text generation through a user-configured agent harness, never a model API.

use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use super::{CONFIG_ORIGIN_KEY, CONFIG_ORIGIN_REPOSITORY, Context, positive, string};
use crate::models::{Llm, Message};

const MAX_OUTPUT: u64 = 32 * 1024 * 1024;
pub const TRUST_COMMANDS_KEY: &str = "trustRepositoryCommands";

pub(super) fn create(context: Arc<Context>) -> Result<Box<dyn Llm>> {
    Ok(Box::new(Harness::new(context, "description")?))
}

/// Constructing a harness is offline. A missing command only fails on use, so
/// structural indexing and embedding-only searches need no installed harness.
pub(super) struct Harness {
    context: Arc<Context>,
    command: Vec<String>,
    model: Option<String>,
    kind: &'static str,
    timeout: Duration,
}

impl Harness {
    pub(super) fn new(context: Arc<Context>, kind: &'static str) -> Result<Self> {
        let key = format!("{kind}Command");
        let command = match context.config.get(&key).filter(|v| !v.is_null()) {
            None => Vec::new(),
            Some(value) => {
                let args = value
                    .as_array()
                    .filter(|args| !args.is_empty())
                    .ok_or_else(|| anyhow!("{key} must be a non-empty array of strings"))?;
                let args = args
                    .iter()
                    .map(|arg| {
                        arg.as_str()
                            .filter(|arg| !arg.contains('\0'))
                            .map(str::to_owned)
                            .ok_or_else(|| anyhow!("{key} must contain strings without NUL bytes"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                ensure!(
                    !args[0].trim().is_empty(),
                    "{key} executable must not be empty"
                );
                args
            }
        };
        let model = string(&context.config, &[&format!("{kind}Model")])?.map(str::to_owned);
        let timeout =
            Duration::from_millis(positive(&context.config, &["agentTimeoutMs"], 300_000)? as u64);
        Ok(Self {
            context,
            command,
            model,
            kind,
            timeout,
        })
    }

    fn invocation(&self) -> Vec<String> {
        let mut args = self.command.clone();
        if let Some(model) = &self.model
            && known_harness(&args[0])
        {
            // Insert before an explicit `--`, not into positional prompt data.
            let at = args
                .iter()
                .position(|arg| arg == "--")
                .unwrap_or(args.len());
            args.splice(at..at, ["--model".into(), model.clone()]);
        }
        args
    }
}

fn known_harness(executable: &str) -> bool {
    let name = Path::new(executable)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let name = name.strip_suffix(".exe").unwrap_or(name);
    matches!(name, "opencode" | "pi" | "claude" | "codex")
}

impl Llm for Harness {
    fn profile(&self) -> Value {
        // Command arguments can contain wrapper credentials. Identify the whole
        // invocation without publishing it in status or shared artifacts.
        json!({"provider": "command", "commandHash": crate::hash(json!(self.command).to_string()),
            "configured": !self.command.is_empty(), "model": self.model,
            "strategyVersion": "harness-transcript-v1"})
    }

    fn describe_conversation(
        &self,
        system: &str,
        messages: &[Message],
        _session: &str,
    ) -> Result<String> {
        ensure!(
            !self.command.is_empty(),
            "{}Command is required for text generation; configure a non-interactive CLI, for example [\"opencode\",\"run\"]",
            self.kind
        );
        ensure!(
            self.context.config[CONFIG_ORIGIN_KEY] != CONFIG_ORIGIN_REPOSITORY
                || self.context.config[TRUST_COMMANDS_KEY] == true,
            "Repository-configured agent commands require explicit trust: use --trust-repo-commands or select a trusted file with --config"
        );
        let args = self.invocation();
        // Every invocation receives the full transcript. Do not resume a harness
        // session: cache hits can skip turns, and retries must not duplicate them.
        let input = serde_json::to_vec(&json!({
            "instructions": system,
            "task": "Complete the final user message using earlier messages only as context. Return only the answer. Do not use tools or modify files.",
            "messages": messages,
        }))?;
        self.context.report_call(
            self.kind,
            "command",
            self.model.as_deref().unwrap_or("harness default"),
        );
        run(&args, input, self.timeout)
    }
}

fn run(args: &[String], input: Vec<u8>, timeout: Duration) -> Result<String> {
    // Use files instead of pipes: a child may write before consuming stdin, or
    // leave inherited stdout open in a background process. Neither may deadlock
    // slopdex. Files are private and removed on every exit path.
    let mut stdin = tempfile::tempfile()?;
    stdin.write_all(&input)?;
    use std::io::{Seek, SeekFrom};
    stdin.seek(SeekFrom::Start(0))?;
    let mut stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?))
        .spawn()
        .map_err(|_| {
            anyhow!("Could not start agent command; check the executable and arguments")
        })?;
    let start = Instant::now();
    let status = loop {
        if stdout.metadata().map_or(true, |m| m.len() > MAX_OUTPUT)
            || stderr.metadata().map_or(true, |m| m.len() > MAX_OUTPUT)
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Agent output exceeds size limit");
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                if result.is_err() {
                    bail!("Could not wait for agent command");
                }
                bail!("Agent command timed out");
            }
        }
    };
    // Never echo argv, stdout, stderr, or the OS error chain: wrappers can put
    // credentials there, and harnesses can echo source code or prompts.
    ensure!(
        status.success(),
        "Agent command failed ({status}); run it directly to inspect its diagnostics"
    );
    stdout.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    stdout.take(MAX_OUTPUT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_OUTPUT,
        "Agent output exceeds size limit"
    );
    let output = String::from_utf8(bytes).map_err(|_| anyhow!("Agent output is not UTF-8"))?;
    let output = output.trim();
    ensure!(!output.is_empty(), "Agent returned an empty answer");
    Ok(output.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent as agent;

    fn harness(config: Value) -> Harness {
        Harness::new(
            Arc::new(Context {
                http: super::super::Http::new(&config).unwrap(),
                config,
                ollama_verified: Arc::new(std::sync::Mutex::new(false)),
            }),
            "description",
        )
        .unwrap()
    }

    #[test]
    fn known_harnesses_receive_models_verbatim_and_unknown_commands_do_not() {
        for name in [
            "opencode",
            "pi",
            "claude",
            "codex",
            "/opt/bin/opencode",
            "codex.exe",
        ] {
            let h = harness(
                json!({"descriptionCommand":[name,"run","--","literal"],"descriptionModel":"provider/model#variant"}),
            );
            assert_eq!(
                h.invocation(),
                vec![
                    name,
                    "run",
                    "--model",
                    "provider/model#variant",
                    "--",
                    "literal"
                ]
            );
        }
        let h = harness(
            json!({"descriptionCommand":["wrapper","--custom"],"descriptionModel":"ignored"}),
        );
        assert_eq!(h.invocation(), vec!["wrapper", "--custom"]);
        assert_eq!(
            harness(json!({"descriptionCommand":["opencode","run"]})).invocation(),
            vec!["opencode", "run"]
        );
    }

    #[test]
    fn full_transcript_is_sent_over_stdin_without_shell_interpolation() {
        let h = harness(json!({"descriptionCommand":[agent::executable(),"echo"]}));
        let messages = [
            Message::user("source with `code`, $(not-a-command), \"quotes\" and λ".into()),
            Message::assistant("earlier answer".into()),
            Message::user("final task".into()),
        ];
        let result = h
            .describe_conversation("system instructions", &messages, "file-session")
            .unwrap();
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["messages"], json!(messages));
        assert!(
            value["instructions"]
                .as_str()
                .unwrap()
                .starts_with("system instructions")
        );
        assert!(
            value.get("session").is_none(),
            "random correlation IDs must not change the effective model input"
        );
    }

    #[test]
    fn command_errors_are_bounded_and_do_not_echo_diagnostics() {
        for (mode, arg, expected) in [
            ("fail", "", "failed"),
            ("emit", "  ", "empty answer"),
            ("sleep", "5000", "timed out"),
            ("flood", "33554433", "size limit"),
        ] {
            let timeout = if mode == "sleep" { 100 } else { 30_000 };
            let h = harness(
                json!({"descriptionCommand":[agent::executable(),mode,arg],"agentTimeoutMs":timeout}),
            );
            let start = Instant::now();
            let error = h.describe("instructions", "prompt").unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!format!("{error:#?}").contains("secret-bearing"));
            assert!(start.elapsed() < Duration::from_secs(30));
        }
        assert!(
            harness(json!({}))
                .describe("s", "p")
                .unwrap_err()
                .to_string()
                .contains("descriptionCommand")
        );
        assert!(
            harness(json!({"descriptionCommand":["missing-agent-executable"]}))
                .describe("s", "p")
                .unwrap_err()
                .to_string()
                .contains("Could not start")
        );
    }

    #[test]
    fn repository_commands_require_explicit_trust_before_spawning() {
        let mut config = json!({"descriptionCommand":[agent::executable(),"emit","answer"],CONFIG_ORIGIN_KEY:CONFIG_ORIGIN_REPOSITORY});
        assert!(
            harness(config.clone())
                .describe("s", "p")
                .unwrap_err()
                .to_string()
                .contains("--trust-repo-commands")
        );
        config[TRUST_COMMANDS_KEY] = json!(true);
        assert_eq!(harness(config).describe("s", "p").unwrap(), "answer");
    }

    #[test]
    fn profiles_include_command_arguments_and_model_and_invalid_commands_fail_offline() {
        let a = harness(json!({"descriptionCommand":["opencode","run"],"descriptionModel":"a"}));
        let b = harness(
            json!({"descriptionCommand":["opencode","run","--agent","custom"],"descriptionModel":"a"}),
        );
        let c = harness(json!({"descriptionCommand":["opencode","run"],"descriptionModel":"b"}));
        assert_ne!(a.profile(), b.profile());
        assert_ne!(a.profile(), c.profile());
        let secret = harness(json!({"descriptionCommand":["wrapper","--token","private-value"]}));
        assert!(!secret.profile().to_string().contains("private-value"));
        for command in [
            json!("sh -c code"),
            json!([]),
            json!([""]),
            json!(["ok", 4]),
            json!(["ok", "\0"]),
        ] {
            assert!(super::super::Providers::new(&json!({"descriptionCommand":command})).is_err());
        }
    }
}
