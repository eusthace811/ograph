//! Model calls through a configured command (default: headless `pi`). ograph holds no
//! credentials and no provider code: the prompt goes to the command's standard input and JSON
//! comes back on standard output. Answers that hold JSON are cached by content hash, so a
//! re-harvest makes no new calls and gives the same triples; any other answer (a rate-limit
//! notice, a refusal) is asked again next time.

use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use anyhow::Context;
use std::process::{Command, Stdio};
use std::sync::Mutex;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    pub command: Vec<String>,
    /// Shown in provenance and part of the cache key, e.g. `pi:openai/gpt-6-luna`.
    pub model: String,
    #[serde(default = "default_parallel")]
    pub parallel: usize,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
}

fn default_parallel() -> usize {
    4
}
fn default_timeout() -> u64 {
    90
}

pub struct Llm {
    pub config: LlmConfig,
    cache_path: PathBuf,
    cache: Mutex<HashMap<String, String>>,
}

#[derive(serde::Serialize, Deserialize)]
struct Entry {
    key: String,
    answer: String,
}

pub fn hash(s: &str) -> String {
    hex::encode(crate::gateway::sha2_hash(s))[..16].to_owned()
}

impl Llm {
    pub fn new(config: LlmConfig, cache_path: PathBuf) -> Self {
        let mut cache = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&cache_path) {
            for line in text.lines() {
                if let Ok(e) = serde_json::from_str::<Entry>(line)
                    && first_json(&e.answer).is_some()
                {
                    cache.insert(e.key, e.answer);
                }
            }
        }
        Self { config, cache_path, cache: Mutex::new(cache) }
    }

    fn key(&self, prompt: &str, input: &str) -> String {
        hash(&format!("{}\u{0}{}\u{0}{}", self.config.model, hash(prompt), input))
    }

    fn cached(&self, key: &str) -> Option<String> {
        self.cache.lock().unwrap().get(key).cloned()
    }

    /// Asks the model, or answers from the cache. Returns the raw answer text.
    pub fn ask(&self, prompt: &str, input: &str) -> anyhow::Result<String> {
        let key = self.key(prompt, input);
        if let Some(a) = self.cached(&key) {
            return Ok(a);
        }
        // One retry: a timeout is usually the transport, not the question.
        let answer = self.run(prompt, input).or_else(|_| self.run(prompt, input))?;
        if first_json(&answer).is_none() {
            return Ok(answer);
        }
        let mut cache = self.cache.lock().unwrap();
        cache.insert(key.clone(), answer.clone());
        if let Some(dir) = self.cache_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.cache_path)?;
        writeln!(f, "{}", serde_json::to_string(&Entry { key, answer: answer.clone() })?)?;
        Ok(answer)
    }

    fn run(&self, prompt: &str, input: &str) -> anyhow::Result<String> {
        let (cmd, args) = self.config.command.split_first().ok_or_else(|| anyhow::anyhow!("empty llm command"))?;
        let mut command = Command::new(cmd);
        // Less a second: Windows stamps process creation from a clock that lags by up to ~16 ms.
        #[cfg(windows)]
        let spawned_ms = std::time::UNIX_EPOCH.elapsed().map_or(0, |d| d.as_millis()).saturating_sub(1000);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {cmd}"))?;
        // Feed and drain the pipes on their own threads, so a large prompt or answer cannot
        // fill a pipe buffer and stall the child until the timeout.
        let mut stdin = child.stdin.take().expect("piped");
        let text = format!("{prompt}\n\n{input}");
        std::thread::spawn(move || stdin.write_all(text.as_bytes()));
        let (tx, rx) = std::sync::mpsc::channel();
        for (is_out, r) in [(true, child.stdout.take().map(|r| Box::new(r) as Box<dyn std::io::Read + Send>)), (false, child.stderr.take().map(|r| Box::new(r) as _))] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                r.map(|mut r| r.read_to_end(&mut buf));
                let _ = tx.send((is_out, buf));
            });
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(self.config.timeout_s);
        // Stop the whole tree, including whatever the command left behind: on Unix a negative
        // pid signals the process group. On Windows taskkill /T follows child processes, but not
        // from a command that already exited, so its leftovers are found by their parent pid.
        // `child` keeps that pid from being reused, and the start time rules out processes
        // orphaned earlier by an exited process that had the same pid.
        let kill_group = |child: &mut std::process::Child| {
            #[cfg(unix)]
            let _ = Command::new("kill").args(["-KILL", &format!("-{}", child.id())]).status();
            #[cfg(windows)]
            {
                let pid = child.id();
                let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).status();
                let leftovers = format!(
                    "$t = [DateTimeOffset]::FromUnixTimeMilliseconds({spawned_ms}).LocalDateTime; \
                     Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}' | \
                     Where-Object {{ $_.CreationDate -ge $t }} | ForEach-Object {{ taskkill /T /F /PID $_.ProcessId }}"
                );
                let _ = Command::new("powershell").args(["-NoProfile", "-Command", &leftovers]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
            let _ = child.kill();
            let _ = child.wait();
        };
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if std::time::Instant::now() > deadline {
                kill_group(&mut child);
                anyhow::bail!("model command timed out after {} s", self.config.timeout_s);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        // A process the command left behind can hold the pipes open past its exit. After a
        // short grace, stopping it closes them and the answer already written is kept.
        let grace = std::time::Duration::from_secs(1);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        for _ in 0..2 {
            let (is_out, buf) = match rx.recv_timeout(grace) {
                Ok(m) => m,
                Err(_) => {
                    kill_group(&mut child);
                    rx.recv_timeout(grace).map_err(|_| anyhow::anyhow!("model command left its output open"))?
                }
            };
            *(if is_out { &mut out } else { &mut err }) = buf;
        }
        if !status.success() {
            anyhow::bail!("model command failed: {}", String::from_utf8_lossy(&err).trim());
        }
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }

    /// Runs `ask` over many inputs with `parallel` workers, keeping input order.
    pub fn ask_all(&self, prompt: &str, inputs: &[String]) -> Vec<anyhow::Result<String>> {
        crate::ordered_map(inputs, self.config.parallel, |input| self.ask(prompt, input))
    }
}

/// The first JSON object in a model answer (models sometimes wrap it in prose or fences). A brace
/// in the prose (`{category}`, `{ is`) is not taken for one, and a balanced span that is not
/// JSON is skipped whole, so an object inside a malformed one is never taken for the answer.
pub fn first_json(answer: &str) -> Option<serde_json::Value> {
    let mut rest = answer;
    loop {
        // An object opens with `{"` or `{}`; a brace in the prose (`{ is`, `{category}`) does not.
        let start = rest.match_indices('{').map(|(i, _)| i).find(|&i| rest[i + 1..].trim_start().starts_with(['"', '}']))?;
        rest = &rest[start..];
        let span = balanced(rest)?;
        if let Ok(v) = serde_json::from_str(span) {
            return Some(v);
        }
        rest = &rest[span.len()..];
    }
}

/// The prefix of `s`, which starts with `{`, up to its matching `}`.
fn balanced(s: &str) -> Option<&str> {
    let (mut depth, mut in_str, mut esc) = (0, false, false);
    for (i, c) in s.char_indices() {
        match c {
            _ if esc => esc = false,
            '\\' if in_str => esc = true,
            '"' => in_str = !in_str,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// True when `quote` appears in `text` word for word, ignoring case and runs of whitespace.
/// Model answers that cite their source are checked with this before any fact is proposed.
pub fn quoted(text: &str, quote: &str) -> bool {
    let q = squash_quote(quote);
    q.len() >= 3 && squash(text).contains(&q)
}

/// Lower case, with each run of whitespace one space.
pub(crate) fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A quote as [`quoted`] compares it: squashed, without the quote marks around it.
pub(crate) fn squash_quote(quote: &str) -> String {
    squash(quote.trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace()))
}

// These tests use shell scripts as stand-ins for the model command.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn the_first_json_object_skips_braces_in_the_prose_but_not_into_a_bad_object() {
        let v = |s: &str| first_json(s).map(|v| v.to_string());
        assert_eq!(v(r#"use {category}: {"category":"db"}"#).as_deref(), Some(r#"{"category":"db"}"#));
        assert_eq!(v("```json\n{\"a\": \"}\"}\n```").as_deref(), Some(r#"{"a":"}"}"#));
        assert_eq!(v(r#"{"a": {"b": 1}, oops}"#), None);
        assert_eq!(v("no object {"), None);
        assert_eq!(v(r#"{"a": {"b": 1}"#), None);
        assert_eq!(v(r#"Note: a brace { is unmatched. {"category":"db"}"#).as_deref(), Some(r#"{"category":"db"}"#));
        assert_eq!(v(r#"It said "hi. {"category":"db"}"#).as_deref(), Some(r#"{"category":"db"}"#));
    }

    #[test]
    fn answer_larger_than_a_pipe_buffer_arrives() {
        let config = LlmConfig { command: vec!["sh".into(), "-c".into(), "cat >/dev/null; head -c 200000 /dev/zero | tr '\\0' a".into()], model: "t".into(), parallel: 1, timeout_s: 5 };
        let llm = Llm::new(config, std::env::temp_dir().join("ograph-llm-test-none/llm.jsonl"));
        assert_eq!(llm.run("p", &"x".repeat(200_000)).unwrap().len(), 200_000);
    }

    #[test]
    fn the_answer_survives_a_leftover_process_holding_the_pipe() {
        let config = LlmConfig { command: vec!["sh".into(), "-c".into(), "cat >/dev/null; sleep 30 & echo hi".into()], model: "t".into(), parallel: 1, timeout_s: 20 };
        let llm = Llm::new(config, std::env::temp_dir().join("ograph-llm-test-none/llm.jsonl"));
        let t = std::time::Instant::now();
        assert_eq!(llm.run("p", "x").unwrap(), "hi");
        assert!(t.elapsed().as_secs() < 5);
    }

    #[test]
    fn an_answer_that_is_not_json_is_asked_again() {
        let dir = std::env::temp_dir().join(format!("ograph-llm-test-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("calls");
        let script = format!("cat >/dev/null; echo x >> '{}'; echo 'Rate limit reached'", calls.display());
        let config = LlmConfig { command: vec!["sh".into(), "-c".into(), script], model: "t".into(), parallel: 1, timeout_s: 5 };
        for _ in 0..2 {
            let llm = Llm::new(config.clone(), dir.join("llm.jsonl"));
            assert_eq!(llm.ask("p", "x").unwrap(), "Rate limit reached");
        }
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);
    }

    #[test]
    fn a_hung_command_times_out() {
        let config = LlmConfig { command: vec!["sh".into(), "-c".into(), "sleep 30".into()], model: "t".into(), parallel: 1, timeout_s: 1 };
        let llm = Llm::new(config, std::env::temp_dir().join("ograph-llm-test-none/llm.jsonl"));
        let t = std::time::Instant::now();
        assert!(llm.run("p", "x").unwrap_err().to_string().contains("timed out"));
        assert!(t.elapsed().as_secs() < 5);
    }
}
