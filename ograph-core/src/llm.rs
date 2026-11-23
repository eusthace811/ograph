//! Model calls through a configured command (default: headless `pi`). ograph holds no
//! credentials and no provider code: the prompt goes to the command's standard input and JSON
//! comes back on standard output. Answers are cached by content hash, so a re-harvest makes no
//! new calls and gives the same triples.

use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use anyhow::Context;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Mutex;

#[derive(Clone, Debug, Deserialize)]
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
    pub calls: std::sync::atomic::AtomicUsize,
    pub hits: std::sync::atomic::AtomicUsize,
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
                if let Ok(e) = serde_json::from_str::<Entry>(line) {
                    cache.insert(e.key, e.answer);
                }
            }
        }
        Self { config, cache_path, cache: Mutex::new(cache), calls: Default::default(), hits: Default::default() }
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
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(a);
        }
        // One retry: a timeout is usually the transport, not the question.
        let answer = self.run(prompt, input).or_else(|_| self.run(prompt, input))?;
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        let timed_out = |child: &mut std::process::Child| {
            // A negative pid signals the whole group.
            let _ = Command::new("kill").args(["-KILL", &format!("-{}", child.id())]).status();
            let _ = child.kill();
            let _ = child.wait();
            anyhow::anyhow!("model command timed out after {} s", self.config.timeout_s)
        };
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if std::time::Instant::now() > deadline {
                return Err(timed_out(&mut child));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        for _ in 0..2 {
            // A process the command left behind can hold the pipes open past its exit.
            let Ok((is_out, buf)) = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) else {
                return Err(timed_out(&mut child));
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
    fn timeout_holds_when_a_leftover_process_keeps_the_pipe_open() {
        let config = LlmConfig { command: vec!["sh".into(), "-c".into(), "cat >/dev/null; sleep 30 & echo hi".into()], model: "t".into(), parallel: 1, timeout_s: 1 };
        let llm = Llm::new(config, std::env::temp_dir().join("ograph-llm-test-none/llm.jsonl"));
        let t = std::time::Instant::now();
        assert!(llm.run("p", "x").unwrap_err().to_string().contains("timed out"));
        assert!(t.elapsed().as_secs() < 5);
    }
}
