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
use std::os::unix::process::CommandExt;
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

    pub fn key(&self, prompt: &str, input: &str) -> String {
        hash(&format!("{}\u{0}{}\u{0}{}", self.config.model, hash(prompt), input))
    }

    pub fn cached(&self, key: &str) -> Option<String> {
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
        // Its own process group, so a timeout also stops whatever the command started.
        let mut child = Command::new(cmd)
            .args(args)
            .process_group(0)
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
        // A negative pid signals the whole group, including whatever the command left behind.
        let kill_group = |child: &mut std::process::Child| {
            let _ = Command::new("kill").args(["-KILL", &format!("-{}", child.id())]).status();
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
        let next = std::sync::atomic::AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<anyhow::Result<String>>>> = inputs.iter().map(|_| Mutex::new(None)).collect();
        std::thread::scope(|s| {
            for _ in 0..self.config.parallel.max(1) {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= inputs.len() {
                        break;
                    }
                    *slots[i].lock().unwrap() = Some(self.ask(prompt, &inputs[i]));
                });
            }
        });
        slots.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect()
    }
}

/// The first JSON object in a model answer (models sometimes wrap it in prose or fences).
pub fn first_json(answer: &str) -> Option<serde_json::Value> {
    let start = answer.find('{')?;
    let mut depth = 0;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in answer[start..].char_indices() {
        match c {
            _ if esc => esc = false,
            '\\' if in_str => esc = true,
            '"' => in_str = !in_str,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&answer[start..start + i + 1]).ok();
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
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let q = squash(quote.trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace()));
    q.len() >= 3 && squash(text).contains(&q)
}

#[cfg(test)]
mod tests {
    use super::*;

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
