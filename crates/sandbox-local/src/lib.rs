//! Local process sandbox: SBX-1 via bubblewrap plus SBX-4.
//!
//! `LocalSandbox::new` runs each call under `bwrap` (SBX-1): only a read-only system layer and the
//! call's own workspace (`<root>/ws/<workspace>`, also `HOME`) are visible, in fresh pid, ipc, uts,
//! user and net namespaces. With empty `egress` there is no network; otherwise traffic goes only
//! through a per-call allowlist proxy (Unix socket bridged by `sf-netshim`), and every attempt is
//! returned in `ToolOutput.egress`. If `bwrap` or the shim is unavailable, `run` fails closed with
//! `Error::Forbidden`. `LocalSandbox::unconfined` skips isolation, does NOT meet SBX-1 and refuses
//! any call with egress rules.
//!
//! Also enforced: a scrubbed environment with no inherited parent secrets (SBX-2), and wall-clock,
//! output-size and kill-switch limits that kill the child's whole process group (SBX-4).

mod proxy;

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sf_core::{Error, KillSwitch, Limits, Result, Sandbox, ToolCall, ToolOutput};

const POLL: Duration = Duration::from_millis(10);
const SHIM_IN_SANDBOX: &str = "/run/sf-netshim";
const SOCK_DIR_IN_SANDBOX: &str = "/run/sf";

/// Runs tool calls as child processes confined to `root` (and, unless unconfined, to a bwrap sandbox).
#[derive(Debug, Clone)]
pub struct LocalSandbox {
    root: PathBuf,
    isolate: bool,
    shim: Option<PathBuf>,
}

impl LocalSandbox {
    /// Creates `root` if needed. Calls run under bwrap with `HOME` set to their workspace (SBX-1).
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| Error::Invalid(format!("sandbox root: {e}")))?;
        let root = root.canonicalize().map_err(|e| Error::Invalid(format!("sandbox root: {e}")))?;
        Ok(LocalSandbox { root, isolate: true, shim: None })
    }

    /// No isolation, for hosts without bwrap. Does NOT meet SBX-1: only cwd is scoped, the network
    /// is open, and any call with non-empty `egress` is `Forbidden` (no allowlist can be enforced).
    pub fn unconfined(root: impl Into<PathBuf>) -> Result<Self> {
        Ok(LocalSandbox { isolate: false, ..Self::new(root)? })
    }

    /// Sets the `sf-netshim` binary used for calls with egress rules.
    pub fn with_shim(mut self, path: impl Into<PathBuf>) -> Self {
        self.shim = Some(path.into());
        self
    }

    /// Shim lookup: explicit, `SF_NETSHIM`, next to the current exe, then its parent dir.
    fn find_shim(&self) -> Option<PathBuf> {
        if let Some(p) = &self.shim {
            return p.is_file().then(|| p.clone());
        }
        if let Some(p) = std::env::var_os("SF_NETSHIM") {
            let p = PathBuf::from(p);
            return p.is_file().then_some(p);
        }
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        [dir.to_path_buf(), dir.parent()?.to_path_buf()].into_iter().map(|d| d.join("sf-netshim")).find(|p| p.is_file())
    }

    /// Maps `ToolCall.workspace` to its directory, creating it on demand (SBX-1).
    fn workspace(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty() {
            return Ok(self.root.clone());
        }
        let ok = name != "." && name != ".." && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        if !ok {
            return Err(Error::Invalid(format!("bad workspace name: {name}")));
        }
        let dir = self.root.join("ws").join(name);
        std::fs::create_dir_all(&dir).map_err(|e| Error::Invalid(format!("workspace {name}: {e}")))?;
        dir.canonicalize().map_err(|e| Error::Invalid(format!("workspace {name}: {e}")))
    }

    /// Resolves `ToolCall.cwd` under the workspace `base`; `..`, absolute paths and symlink escapes are rejected (SBX-1).
    fn resolve_cwd(base: &Path, cwd: Option<&str>) -> Result<PathBuf> {
        let Some(rel) = cwd else { return Ok(base.to_path_buf()) };
        let rel = Path::new(rel);
        let escapes = rel.is_absolute()
            || rel.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)));
        if escapes {
            return Err(Error::Invalid(format!("cwd escapes sandbox root: {}", rel.display())));
        }
        let full = base
            .join(rel)
            .canonicalize()
            .map_err(|e| Error::Invalid(format!("cwd {}: {e}", rel.display())))?;
        if !full.starts_with(base) {
            return Err(Error::Invalid(format!("cwd escapes sandbox root: {}", rel.display())));
        }
        Ok(full)
    }

    /// The bwrap invocation: read-only system layer, the workspace at its own path, no other host files.
    fn bwrap(ws: &Path, cwd: &Path, bridge: Option<(&Path, &Path)>) -> Command {
        let mut c = Command::new("bwrap");
        c.args(["--unshare-all", "--die-with-parent", "--ro-bind", "/usr", "/usr"]);
        for d in ["/bin", "/lib", "/lib64", "/sbin"] {
            match std::fs::read_link(d) {
                Ok(t) => c.arg("--symlink").arg(t).arg(d),
                Err(_) => c.args(["--ro-bind-try", d, d]),
            };
        }
        for d in ["/etc/ssl", "/etc/ca-certificates", "/etc/alternatives", "/etc/ld.so.cache"] {
            c.args(["--ro-bind-try", d, d]);
        }
        c.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        c.arg("--bind").arg(ws).arg(ws);
        if let Some((sock_dir, shim)) = bridge {
            c.arg("--bind").arg(sock_dir).arg(SOCK_DIR_IN_SANDBOX);
            c.arg("--ro-bind").arg(shim).arg(SHIM_IN_SANDBOX);
        }
        c.arg("--chdir").arg(cwd);
        c.arg("--");
        c
    }

    /// Builds the command for `call`: bwrap-wrapped (optionally via the shim) or direct when unconfined.
    fn command(&self, call: &ToolCall, ws: &Path, cwd: &Path, bridge: Option<(&Path, &Path)>) -> Command {
        let mut c = if self.isolate {
            let mut c = Self::bwrap(ws, cwd, bridge);
            if bridge.is_some() {
                c.arg(SHIM_IN_SANDBOX).arg(format!("{SOCK_DIR_IN_SANDBOX}/p.sock")).arg("--");
            }
            c.arg(&call.program);
            c
        } else {
            let mut c = Command::new(&call.program);
            c.current_dir(cwd);
            c
        };
        c.args(&call.args)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", ws)
            .envs(call.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        c
    }
}

/// Maps a failed bwrap/shim start (reported on stderr, exit 1/127) to the right error; `None` = the program ran.
fn launch_failure(call: &ToolCall, code: Option<i32>, stderr: &str) -> Option<Error> {
    if let Some(m) = stderr.strip_prefix("bwrap: execvp ").filter(|_| code == Some(1)) {
        return Some(Error::Invalid(format!("failed to spawn {}: {}", call.program, m.trim())));
    }
    if stderr.starts_with("sf-netshim: spawn ") && code == Some(127) {
        return Some(Error::Invalid(format!("failed to spawn {}: {}", call.program, stderr.trim())));
    }
    (code == Some(1) && stderr.starts_with("bwrap: "))
        .then(|| Error::Forbidden(format!("isolation unavailable: {}", stderr.trim())))
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
        let shim = if call.egress.is_empty() {
            None
        } else if !self.isolate {
            return Err(Error::Forbidden("unconfined sandbox cannot enforce an egress allowlist".into()));
        } else {
            Some(self.find_shim().ok_or_else(|| Error::Forbidden("isolation unavailable: sf-netshim not found".into()))?)
        };
        let ws = self.workspace(&call.workspace)?;
        let cwd = Self::resolve_cwd(&ws, call.cwd.as_deref())?;
        let proxy = match &shim {
            Some(_) => Some(
                proxy::Proxy::start(call.egress.clone())
                    .map_err(|e| Error::Forbidden(format!("isolation unavailable: proxy: {e}")))?,
            ),
            None => None,
        };
        let bridge = proxy.as_ref().zip(shim.as_deref()).map(|(p, s)| (p.dir(), s));
        let start = Instant::now();
        let spawned = self.command(call, &ws, &cwd, bridge).spawn();
        let result = match spawned {
            Ok(child) => Self::supervise(call, child, limits, kill, start),
            Err(e) if self.isolate && e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::Forbidden("isolation unavailable: bwrap not found".into()))
            }
            Err(e) => Err(Error::Invalid(format!("failed to spawn {}: {e}", call.program))),
        };
        let egress = proxy.map(proxy::Proxy::finish).unwrap_or_default();
        result.map(|o| ToolOutput { egress, ..o })
    }
}

impl LocalSandbox {
    /// Feeds stdin, captures capped output and enforces the SBX-4 limits on a spawned child.
    fn supervise(call: &ToolCall, mut child: Child, limits: &Limits, kill: &KillSwitch, start: Instant) -> Result<ToolOutput> {
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
        if let Some(e) = launch_failure(call, status.code(), &stderr) {
            return Err(e);
        }
        Ok(ToolOutput {
            exit_code: status.code(),
            stdout,
            stderr,
            duration_ms: start.elapsed().as_millis() as u64,
            truncated: t1 || t2,
            egress: Vec::new(),
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

    fn ws(name: &str, script: &str) -> ToolCall {
        ToolCall { workspace: name.into(), ..sh(script) }
    }

    #[test]
    fn workspace_is_home_cwd_and_created_on_demand() {
        let (_d, s) = sb();
        let o = run(&s, &ws("run1", "echo $HOME; pwd; touch f")).unwrap();
        let w = s.root.join("ws/run1");
        assert_eq!(o.stdout, format!("{0}\n{0}\n", w.display()));
        assert!(w.join("f").exists());
    }

    #[test]
    fn bad_workspace_names_rejected() {
        let (_d, s) = sb();
        for bad in [".", "..", "a/b", "../x", "a b", "/etc"] {
            assert!(matches!(run(&s, &ws(bad, "true")), Err(Error::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn host_files_outside_workspace_are_invisible() {
        let (d, s) = sb();
        let canary = d.path().join("canary");
        std::fs::write(&canary, "secret").unwrap();
        let o = run(&s, &ws("w", &format!("cat {} 2>&1; ls /home 2>&1; ls {} 2>&1", canary.display(), d.path().display()))).unwrap();
        assert!(!o.stdout.contains("secret"), "{}", o.stdout);
        assert!(o.stdout.contains("No such file"), "{}", o.stdout);
    }

    #[test]
    fn writes_outside_workspace_do_not_reach_host() {
        let (d, s) = sb();
        let name = format!("sf-host-probe-{}", std::process::id());
        let o = run(&s, &ws("w", &format!("echo x > /tmp/{name}; cat /tmp/{name}"))).unwrap();
        assert_eq!(o.stdout, "x\n");
        assert!(!Path::new("/tmp").join(&name).exists());
        drop(d);
    }

    #[test]
    fn workspaces_cannot_see_each_other() {
        let (_d, s) = sb();
        run(&s, &ws("a", "echo mine > f")).unwrap();
        let o = run(&s, &ws("b", &format!("cat {}/ws/a/f 2>&1; ls {}/ws 2>&1", s.root.display(), s.root.display()))).unwrap();
        assert!(!o.stdout.contains("mine") && o.stdout.ends_with("\nb\n"), "{}", o.stdout);
    }

    #[test]
    fn no_network_without_egress() {
        let (_d, s) = sb();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let p = l.local_addr().unwrap().port();
        let o = run(&s, &ws("w", &format!("curl -s -m 3 http://127.0.0.1:{p}/"))).unwrap();
        assert_ne!(o.exit_code, Some(0));
        assert!(l.accept().is_err());
        assert!(o.egress.is_empty());
    }

    #[test]
    fn missing_bwrap_fails_closed() {
        assert!(matches!(
            launch_failure(&sh("true"), Some(1), "bwrap: No permissions to create new namespace\n"),
            Some(Error::Forbidden(m)) if m.starts_with("isolation unavailable")
        ));
        assert!(launch_failure(&sh("true"), Some(1), "plain failure").is_none());
    }

    #[test]
    fn unconfined_rejects_egress_and_runs_otherwise() {
        let d = tempfile::tempdir().unwrap();
        let s = LocalSandbox::unconfined(d.path().join("root")).unwrap();
        let c = ToolCall { egress: vec![sf_core::HostRule { host: "example.com".into(), port: None }], ..sh("true") };
        assert!(matches!(run(&s, &c), Err(Error::Forbidden(_))));
        assert_eq!(run(&s, &sh("echo ok")).unwrap().stdout, "ok\n");
    }

    #[test]
    fn missing_shim_with_egress_is_forbidden() {
        let (_d, s) = sb();
        let s = s.with_shim("/nonexistent/sf-netshim");
        let c = ToolCall { egress: vec![sf_core::HostRule { host: "example.com".into(), port: None }], ..sh("true") };
        assert!(matches!(run(&s, &c), Err(Error::Forbidden(_))));
    }
}
