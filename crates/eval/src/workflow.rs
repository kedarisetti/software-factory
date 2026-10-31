//! Workflow suite (APR-*, DUR-3, AUD-*): drives the real `sf` binary over the built-in workflows in a tempdir.
//! Each gated step must park, `approve --as golden` must resume it, every effect must run exactly once
//! (and a gated effect not before its approval), and `audit-verify` must pass. Metric: pass rate, must be 1.0.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

use crate::metrics::Metrics;
use crate::{ctx, read_jsonl, Result};

#[derive(Debug, Deserialize)]
pub struct Case {
    pub workflow: String,
    /// Env var naming the file the workflow appends effect lines to.
    pub effect_env: String,
    /// Action kinds that park for approval, in order; their effect must not exist before `approve`.
    #[serde(default)]
    pub gated: Vec<String>,
    /// Plain approval steps that park without an action.
    #[serde(default)]
    pub approval_steps: usize,
    /// Effect label to the number of times it must appear.
    pub effects: BTreeMap<String, usize>,
}

/// `$SF_BIN`, else `sf` next to this executable.
pub fn sf_bin() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("SF_BIN") {
        return Ok(p.into());
    }
    let exe = ctx(std::env::current_exe(), "current_exe")?;
    let p = exe.with_file_name("sf");
    if p.exists() {
        Ok(p)
    } else {
        Err(format!("sf binary not found at {} (cargo build -p sf-cli, or set SF_BIN)", p.display()))
    }
}

struct Sandbox {
    bin: PathBuf,
    db: PathBuf,
    sandbox: PathBuf,
    effects: PathBuf,
    effect_env: String,
}

impl Sandbox {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.arg("--db").arg(&self.db).arg("--sandbox").arg(&self.sandbox);
        if std::env::var("SF_EVAL_UNCONFINED").as_deref() == Ok("1") {
            c.arg("--unconfined");
        }
        c.args(args).env(&self.effect_env, &self.effects);
        for k in ["SF_SHIP_CRASH_AFTER_WRITE", "SF_DEMO_CRASH_AFTER_WRITE", "SF_DEMO_CRASH_AFTER_EFFECT", "SF_DEMO_SLEEP_MS"] {
            c.env_remove(k);
        }
        c
    }

    fn json(&self, args: &[&str]) -> Result<Value> {
        let out = ctx(self.cmd(args).output(), format!("sf {args:?}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            return Err(format!("sf {args:?} failed: {text} {}", String::from_utf8_lossy(&out.stderr)));
        }
        ctx(serde_json::from_str(&text), format!("sf {args:?}: {text}"))
    }

    fn effect_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.effects).map(|s| s.lines().map(String::from).collect()).unwrap_or_default()
    }

    fn count(&self, label: &str) -> usize {
        self.effect_lines().iter().filter(|l| l.split(' ').next() == Some(label)).count()
    }
}

/// Run one case; `Err` names the first violated check.
pub fn check(bin: &Path, case: &Case) -> Result<()> {
    let dir = ctx(tempfile::tempdir(), "tempdir")?;
    let sb = Sandbox {
        bin: bin.to_path_buf(),
        db: dir.path().join("sf.db"),
        sandbox: dir.path().join("sbx"),
        effects: dir.path().join("effects.txt"),
        effect_env: case.effect_env.clone(),
    };
    let id = sb.json(&["start", &case.workflow])?["run_id"].as_str().ok_or("no run_id")?.to_string();
    sb.json(&["run"])?;
    let want_parks = case.gated.len() + case.approval_steps;
    let mut parks = 0;
    loop {
        let s = sb.json(&["status", &id])?;
        if s["state"] != "AwaitingApproval" {
            break;
        }
        if parks == want_parks {
            return Err(format!("parked more than the {want_parks} expected times: {s}"));
        }
        if let Some(kind) = s["pending_action"]["kind"].as_str() {
            if !case.gated.iter().any(|g| g == kind) {
                return Err(format!("unexpected gated action {kind}"));
            }
            if sb.count(kind) != 0 {
                return Err(format!("{kind} ran before approval"));
            }
        }
        sb.json(&["approve", &id, "--as", "golden"])?;
        sb.json(&["run"])?;
        parks += 1;
    }
    let s = sb.json(&["status", &id])?;
    if s["state"] != "Succeeded" {
        return Err(format!("run ended {} not Succeeded: {s}", s["state"]));
    }
    if parks != want_parks {
        return Err(format!("parked {parks} times, expected {want_parks}"));
    }
    for (label, want) in &case.effects {
        let got = sb.count(label);
        if got != *want {
            return Err(format!("effect {label} ran {got} times, expected {want}: {:?}", sb.effect_lines()));
        }
    }
    let total: usize = case.effects.values().sum();
    if sb.effect_lines().len() != total {
        return Err(format!("{} effect lines, expected {total}: {:?}", sb.effect_lines().len(), sb.effect_lines()));
    }
    let v = sb.json(&["audit-verify"])?;
    if v != serde_json::json!({"ok": true}) {
        return Err(format!("audit-verify: {v}"));
    }
    Ok(())
}

pub fn run(golden: &Path) -> Result<Metrics> {
    let bin = sf_bin()?;
    let cases: Vec<Case> = read_jsonl(&golden.join("cases/workflow/workflows.jsonl"))?
        .into_iter()
        .map(|v| ctx(serde_json::from_value(v), "workflows.jsonl"))
        .collect::<Result<_>>()?;
    let mut out = Metrics::new();
    let mut passed = 0;
    for case in &cases {
        let ok = match check(&bin, case) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("workflow {} FAILED: {e}", case.workflow);
                false
            }
        };
        passed += ok as usize;
        out.insert(format!("workflow.{}.pass", case.workflow), ok as u8 as f64);
    }
    out.insert("workflow.pass_rate".into(), passed as f64 / cases.len().max(1) as f64);
    Ok(out)
}
