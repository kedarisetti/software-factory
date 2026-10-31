//! Local process sandbox: a local approximation of SBX-1 plus SBX-4.
//!
//! **Not enforced locally yet: the network egress allowlist (SBX-1).** Commands run as ordinary
//! child processes with full network access; egress control is milestone 3.
//!
//! Enforced here: filesystem scoping of the working directory to a root (SBX-1), a scrubbed
//! environment with no inherited parent secrets (SBX-2), and wall-clock, output-size and
//! kill-switch limits that kill the child's whole process group (SBX-4).

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sf_core::{Error, KillSwitch, Limits, Result, Sandbox, ToolCall, ToolOutput};

const POLL: Duration = Duration::from_millis(10);

/// Runs tool calls as child processes confined (by cwd) to `root`.
#[derive(Debug, Clone)]
pub struct LocalSandbox {
    root: PathBuf,
}

impl LocalSandbox {
    /// Creates `root` if needed. Commands run with cwd inside it and `HOME` set to it.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| Error::Invalid(format!("sandbox root: {e}")))?;
        let root = root.canonicalize().map_err(|e| Error::Invalid(format!("sandbox root: {e}")))?;
        Ok(LocalSandbox { root })
    }

    /// Resolves `ToolCall.cwd` under the root; `..`, absolute paths and symlink escapes are rejected (SBX-1).
    fn resolve_cwd(&self, cwd: Option<&str>) -> Result<PathBuf> {
        let Some(rel) = cwd else { return Ok(self.root.clone()) };
        let rel = Path::new(rel);
        let escapes = rel.is_absolute()
            || rel.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)));
        if escapes {
            return Err(Error::Invalid(format!("cwd escapes sandbox root: {}", rel.display())));
        }
        let full = self
            .root
            .join(rel)
            .canonicalize()
            .map_err(|e| Error::Invalid(format!("cwd {}: {e}", rel.display())))?;
        if !full.starts_with(&self.root) {
            return Err(Error::Invalid(format!("cwd escapes sandbox root: {}", rel.display())));
        }
        Ok(full)
    }
}

/// SIGKILLs the whole process group `pgid`, best effort.
fn kill_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(["-s", "KILL", "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Drains `r` to EOF, keeping at most `cap` bytes; returns (text, truncated).
fn drain(mut r: impl Read + Send + 'static, cap: usize) -> JoinHandle<(String, bool)> {
    thread::spawn(move || {
        let (mut buf, mut truncated, mut chunk) = (Vec::new(), false, [0u8; 8192]);
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let room = cap.saturating_sub(buf.len());
            if n > room {
                truncated = true;
            }
            buf.extend_from_slice(&chunk[..n.min(room)]);
        }
        (String::from_utf8_lossy(&buf).into_owned(), truncated)
    })
}

/// Waits for exit, a timeout or the kill switch; kills the group in the latter two cases.
fn wait(child: &mut Child, limits: &Limits, kill: &KillSwitch, start: Instant) -> Result<std::process::ExitStatus> {
    let pgid = child.id();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(Error::Invalid(format!("wait failed: {e}"))),
        }
        if kill.is_tripped() {
            break Err(Error::Killed);
        }
        if start.elapsed() >= limits.wall_clock {
            break Err(Error::Timeout(limits.wall_clock));
        }
        thread::sleep(POLL);
    };
    // Also reaps stragglers (e.g. background grandchildren holding the pipes open).
    kill_group(pgid);
    if outcome.is_err() {
        let _ = child.wait();
    }
    outcome
}

impl Sandbox for LocalSandbox {
    fn run(&self, call: &ToolCall, limits: &Limits, kill: &KillSwitch) -> Result<ToolOutput> {
        if kill.is_tripped() {
            return Err(Error::Killed);
        }
        let cwd = self.resolve_cwd(call.cwd.as_deref())?;
        let start = Instant::now();
        let mut child = Command::new(&call.program)
            .args(&call.args)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", &self.root)
            .envs(call.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| Error::Invalid(format!("failed to spawn {}: {e}", call.program)))?;

        let cap = limits.max_output_bytes;
        let out = drain(child.stdout.take().expect("piped"), cap);
        let err = drain(child.stderr.take().expect("piped"), cap);
        let mut stdin = child.stdin.take().expect("piped");
        let input = call.stdin.clone().unwrap_or_default();
        // Own thread so a large stdin cannot deadlock against a child that is not reading.
        let feeder = thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });

        let status = wait(&mut child, limits, kill, start);
        let _ = feeder.join();
        let ((stdout, t1), (stderr, t2)) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
        let status = status?;
        Ok(ToolOutput {
            exit_code: status.code(),
            stdout,
            stderr,
            duration_ms: start.elapsed().as_millis() as u64,
            truncated: t1 || t2,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sb() -> (tempfile::TempDir, LocalSandbox) {
        let d = tempfile::tempdir().unwrap();
        let s = LocalSandbox::new(d.path().join("root")).unwrap();
        (d, s)
    }
    fn sh(script: &str) -> ToolCall {
        ToolCall { program: "sh".into(), args: vec!["-c".into(), script.into()], ..Default::default() }
    }
    fn run(s: &LocalSandbox, c: &ToolCall) -> Result<ToolOutput> {
        s.run(c, &Limits::default(), &KillSwitch::new())
    }
    fn pgrep_count(pattern: &str) -> usize {
        let o = Command::new("pgrep").args(["-f", pattern]).output().unwrap();
        String::from_utf8_lossy(&o.stdout).lines().count()
    }

    #[test]
    fn captures_stdout_stderr_and_exit_code() {
        let (_d, s) = sb();
        let o = run(&s, &sh("echo hi; echo oops >&2; exit 3")).unwrap();
        assert_eq!((o.stdout.as_str(), o.stderr.as_str(), o.exit_code), ("hi\n", "oops\n", Some(3)));
        assert!(!o.truncated);
    }

    #[test]
    fn stdin_is_delivered_and_closed() {
        let (_d, s) = sb();
        let c = ToolCall { stdin: Some("abc".into()), ..sh("cat") };
        assert_eq!(run(&s, &c).unwrap().stdout, "abc");
    }

    #[test]
    fn env_is_scrubbed_and_explicit_env_passes() {
        let (_d, s) = sb();
        std::env::set_var("SF_SANDBOX_SECRET", "s3cret");
        let c = ToolCall { env: vec![("FOO".into(), "bar".into())], ..sh("echo \"[$SF_SANDBOX_SECRET][$FOO][$HOME]\"") };
        let o = run(&s, &c).unwrap();
        assert_eq!(o.stdout.trim(), format!("[][bar][{}]", s.root.display()));
    }

    #[test]
    fn cwd_inside_ok_and_escapes_rejected() {
        let (d, s) = sb();
        std::fs::create_dir(s.root.join("sub")).unwrap();
        let ok = ToolCall { cwd: Some("sub".into()), ..sh("pwd") };
        assert!(run(&s, &ok).unwrap().stdout.trim().ends_with("/sub"));
        std::os::unix::fs::symlink(d.path(), s.root.join("link")).unwrap();
        for bad in ["..", "sub/../..", "/tmp", "link", "missing"] {
            let c = ToolCall { cwd: Some(bad.into()), ..sh("pwd") };
            assert!(matches!(run(&s, &c), Err(Error::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn spawn_failure_names_program() {
        let (_d, s) = sb();
        let c = ToolCall { program: "no-such-prog-xyz".into(), ..Default::default() };
        let Err(Error::Invalid(m)) = run(&s, &c) else { panic!() };
        assert!(m.contains("no-such-prog-xyz"));
    }

    #[test]
    fn timeout_kills_process_group_including_grandchild() {
        let (_d, s) = sb();
        let l = Limits { wall_clock: Duration::from_millis(300), ..Limits::default() };
        let t = Instant::now();
        let r = s.run(&sh("sleep 31 & sleep 31"), &l, &KillSwitch::new());
        assert!(matches!(r, Err(Error::Timeout(_))));
        assert!(t.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(pgrep_count("sleep 31"), 0);
    }

    #[test]
    fn kill_switch_before_and_during_run() {
        let (_d, s) = sb();
        let k = KillSwitch::new();
        let k2 = k.clone();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            k2.trip();
        });
        let t = Instant::now();
        let r = s.run(&sh("sleep 32 & sleep 32"), &Limits::default(), &k);
        h.join().unwrap();
        assert!(matches!(r, Err(Error::Killed)));
        assert!(t.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(pgrep_count("sleep 32"), 0);
        assert!(matches!(s.run(&sh("true"), &Limits::default(), &k), Err(Error::Killed)));
    }

    #[test]
    fn output_is_truncated_without_deadlock() {
        let (_d, s) = sb();
        let l = Limits { max_output_bytes: 1000, ..Limits::default() };
        let o = s.run(&sh("head -c 5000000 /dev/zero | tr '\\0' x"), &l, &KillSwitch::new()).unwrap();
        assert_eq!(o.stdout.len(), 1000);
        assert!(o.truncated);
        assert_eq!(o.exit_code, Some(0));
    }

    #[test]
    fn sandbox_is_shareable() {
        let (_d, s) = sb();
        let s: Arc<dyn Sandbox> = Arc::new(s);
        assert_eq!(s.run(&sh("echo x"), &Limits::default(), &KillSwitch::new()).unwrap().stdout, "x\n");
    }
}
