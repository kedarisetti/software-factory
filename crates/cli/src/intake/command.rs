//! `command` sources: run an argv (no shell) and read JSON from its stdout. The child gets a cleared
//! environment (PATH and HOME from the host plus the configured vars), its own process group, no
//! stdin, and is killed, group and all, on timeout. Everything it prints is untrusted data.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const MAX_STDOUT: u64 = 8 << 20;
const STDERR_SHOWN: usize = 500;

pub struct Run<'a> {
    pub command: &'a [String],
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

/// Replaces `{cursor}` in each argv element.
pub fn substitute(argv: &[String], cursor: &str) -> Vec<String> {
    argv.iter().map(|a| a.replace("{cursor}", cursor)).collect()
}

fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Env values never appear in errors: any that show up in stderr are masked.
fn tidy(stderr: &[u8], env: &[(String, String)]) -> String {
    let mut t = String::from_utf8_lossy(stderr).trim().to_string();
    for (_, v) in env {
        if !v.is_empty() {
            t = t.replace(v.as_str(), "***");
        }
    }
    t.replace('\n', " | ").chars().take(STDERR_SHOWN).collect()
}

/// Runs the command to completion and returns its stdout parsed as JSON. Always reaps the process.
pub fn run(r: &Run) -> Result<Value, String> {
    let mut cmd = Command::new(&r.command[0]);
    cmd.args(&r.command[1..]).env_clear();
    for k in ["PATH", "HOME"] {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    let mut child = cmd
        .envs(r.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start command {:?}: {e}", r.command[0]))?;
    let pid = child.id();
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let out_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(o) = out {
            let _ = o.take(MAX_STDOUT + 1).read_to_end(&mut b);
        }
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(e) = err {
            let _ = e.take(4096).read_to_end(&mut b);
        }
        b
    });
    let deadline = Instant::now() + r.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                kill_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "command timed out after {} ms; process group killed",
                    r.timeout.as_millis()
                ));
            }
            Err(e) => break Err(format!("wait failed: {e}")),
        }
    };
    kill_group(pid); // stragglers holding the pipes open
    let (stdout, stderr) = (
        out_t.join().unwrap_or_default(),
        err_t.join().unwrap_or_default(),
    );
    let status = status?;
    if !status.success() {
        let why = tidy(&stderr, &r.env);
        let code = status
            .code()
            .map_or("a signal".to_string(), |c| format!("code {c}"));
        return Err(format!(
            "command exited with {code}{}",
            if why.is_empty() {
                String::new()
            } else {
                format!(": {why}")
            }
        ));
    }
    if stdout.len() as u64 > MAX_STDOUT {
        return Err("command output is over 8 MiB".into());
    }
    serde_json::from_slice(&stdout).map_err(|e| {
        format!(
            "command output is not JSON ({e}); the first 80 chars: {:?}",
            String::from_utf8_lossy(&stdout)
                .chars()
                .take(80)
                .collect::<String>()
        )
    })
}
