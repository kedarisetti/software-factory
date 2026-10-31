//! `sf-eval`: run the golden set, gate against a baseline, write a new baseline (ENT-1).
//!
//!   sf-eval run [--golden golden] [--out metrics.json]
//!   sf-eval gate [--golden golden] [--baseline golden/baseline.json] [--metrics metrics.json]
//!   sf-eval baseline [--golden golden] [--baseline golden/baseline.json]
//!   sf-eval gen-kb [--golden golden]
//!
//! Exit 0 ok, 1 regression, 2 usage or suite error.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use sf_eval::gate::{compare, regressions, Baseline, Direction, Row, Status};
use sf_eval::metrics::Metrics;
use sf_eval::{ctx, kbgen, run_all, Report, Result};

struct Opts {
    golden: PathBuf,
    baseline: Option<PathBuf>,
    out: Option<PathBuf>,
    metrics: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Opts> {
    let mut o = Opts { golden: "golden".into(), baseline: None, out: None, metrics: None };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().map(PathBuf::from).ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--golden" => o.golden = val()?,
            "--baseline" => o.baseline = Some(val()?),
            "--out" => o.out = Some(val()?),
            "--metrics" => o.metrics = Some(val()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(o)
}

fn print_metrics(r: &Report) {
    let w = r.metrics.keys().map(String::len).max().unwrap_or(0);
    for (k, v) in &r.metrics {
        eprintln!("{k:<w$}  {v:.4}");
    }
    let t: Vec<String> = r.timings_ms.iter().map(|(k, v)| format!("{k} {:.1}s", *v as f64 / 1000.0)).collect();
    eprintln!("time: {}", t.join(", "));
}

fn fmt_opt(x: Option<f64>) -> String {
    x.map_or("-".into(), |v| format!("{v:.4}"))
}

fn print_rows(rows: &[Row]) {
    let w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
    eprintln!("{:<w$}  {:>8}  {:>8}  {:>8}  {:>6}  status", "metric", "baseline", "current", "delta", "tol");
    for r in rows {
        let status = match r.status {
            Status::Ok => "ok",
            Status::Improved => "improved",
            Status::Regressed => "REGRESSED",
            Status::Missing => "MISSING",
            Status::New => "new (not gated)",
        };
        let dir = match r.direction {
            Some(Direction::Higher) => "^",
            Some(Direction::Lower) => "v",
            None => " ",
        };
        eprintln!(
            "{:<w$}  {:>8}  {:>8}  {:>8}  {:>6}  {dir} {status}",
            r.name,
            fmt_opt(r.baseline),
            fmt_opt(r.current),
            r.delta().map_or("-".into(), |d| format!("{d:+.4}")),
            format!("{:.3}", r.tolerance)
        );
    }
}

fn load_metrics(path: &PathBuf) -> Result<Metrics> {
    let v: serde_json::Value = ctx(serde_json::from_str(&ctx(std::fs::read_to_string(path), path.display())?), path.display())?;
    let m = v.get("metrics").unwrap_or(&v);
    let map: BTreeMap<String, f64> = ctx(serde_json::from_value(m.clone()), path.display())?;
    Ok(map)
}

fn real_main(args: Vec<String>) -> Result<ExitCode> {
    let cmd = args.first().ok_or("usage: sf-eval run|gate|baseline|gen-kb [--golden DIR] ...")?.as_str();
    let o = parse(&args[1..])?;
    let baseline_path = o.baseline.clone().unwrap_or_else(|| o.golden.join("baseline.json"));
    match cmd {
        "run" => {
            let r = run_all(&o.golden)?;
            print_metrics(&r);
            let json = ctx(serde_json::to_string_pretty(&r), "report")?;
            match &o.out {
                Some(p) => ctx(std::fs::write(p, json + "\n"), p.display())?,
                None => println!("{json}"),
            }
            Ok(ExitCode::SUCCESS)
        }
        "gate" => {
            let baseline = Baseline::load(&baseline_path)?;
            let current = match &o.metrics {
                Some(p) => load_metrics(p)?,
                None => {
                    let r = run_all(&o.golden)?;
                    eprintln!("time: {:?} ms", r.timings_ms);
                    r.metrics
                }
            };
            let rows = compare(&baseline, &current);
            print_rows(&rows);
            let bad = regressions(&rows);
            if bad.is_empty() {
                eprintln!("gate: pass ({} metrics)", rows.len());
                return Ok(ExitCode::SUCCESS);
            }
            for r in &bad {
                match r.current {
                    Some(c) => eprintln!(
                        "REGRESSION {}: {:.4} -> {:.4} ({:+.4}), tolerance {:.3}, {} is better",
                        r.name,
                        r.baseline.unwrap_or(0.0),
                        c,
                        r.delta().unwrap_or(0.0),
                        r.tolerance,
                        if r.direction == Some(Direction::Higher) { "higher" } else { "lower" }
                    ),
                    None => eprintln!("REGRESSION {}: metric missing from this run (baseline {})", r.name, fmt_opt(r.baseline)),
                }
            }
            eprintln!("gate: FAIL ({} regressions)", bad.len());
            Ok(ExitCode::from(1))
        }
        "baseline" => {
            let previous = Baseline::load(&baseline_path).ok();
            let r = run_all(&o.golden)?;
            print_metrics(&r);
            Baseline::from_metrics(&r.metrics, previous.as_ref()).save(&baseline_path)?;
            eprintln!("wrote {}; review the diff and commit it", baseline_path.display());
            Ok(ExitCode::SUCCESS)
        }
        "gen-kb" => {
            let n = kbgen::generate(&o.golden)?;
            eprintln!("wrote {n} entries to {}/fixtures/kb", o.golden.display());
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command {other}")),
    }
}

fn main() -> ExitCode {
    real_main(std::env::args().skip(1).collect()).unwrap_or_else(|e| {
        eprintln!("sf-eval: {e}");
        ExitCode::from(2)
    })
}
