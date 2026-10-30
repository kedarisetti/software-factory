//! `sf-kb`: CLI and MCP front doors for the knowledge service (KIF-3).
//! Output is always one JSON document on stdout: `{"ok":true,"result":..}` or
//! `{"ok":false,"error":{"code":..,"message":..}}`. Exit code 0, 1 (operation failed) or 2 (usage).

mod cli;
mod dispatch;
mod mcp;

use std::io::{self, Read};
use std::path::Path;
use std::process::ExitCode;

use serde_json::{json, Value};
use sf_kb::{Caller, Kb};

use cli::{Cmd, Globals};
use dispatch::{dispatch, OpError};

/// The only place the concrete store and index are constructed (index lives in `<root>/.kb/index`).
fn open_kb(root: &Path) -> sf_kb::Result<Kb> {
    let dir = root.join(".kb").join("index");
    std::fs::create_dir_all(&dir)?;
    Kb::open(root, Box::new(sf_retrieval_tantivy::TantivyIndex::open(&dir)?))
}

fn fail(code: &str, message: &str) -> Value {
    json!({ "ok": false, "error": { "code": code, "message": message } })
}

fn sf_err(e: sf_kb::Error) -> OpError {
    (e.code().to_string(), e.to_string())
}

fn exec(g: &Globals, cmd: Cmd) -> Result<Value, OpError> {
    if cmd == Cmd::Version {
        return Ok(json!({ "contract": sf_kb::CONTRACT_VERSION, "version": env!("CARGO_PKG_VERSION") }));
    }
    Kb::check_version(g.contract).map_err(sf_err)?;
    let kb = open_kb(&g.root).map_err(sf_err)?;
    match cmd {
        Cmd::Op(op, args) => {
            let caller = Caller { agent: g.agent.clone().unwrap_or_else(|| "human".into()), session: g.session.clone() };
            dispatch(&kb, &caller, op, args)
        }
        _ => kb.rebuild_index().map(|n| json!({ "indexed": n })).map_err(sf_err),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let read_stdin = || {
        let mut s = String::new();
        io::stdin().read_to_string(&mut s).map(|_| s)
    };
    let (g, cmd) = match cli::parse(&args, &|k| std::env::var(k).ok(), &mut { read_stdin }) {
        Ok(p) => p,
        Err(m) => {
            println!("{}", fail("usage", &m));
            return ExitCode::from(2);
        }
    };
    if cmd == Cmd::Mcp {
        // Stdout carries protocol only; problems go to stderr.
        if let Err(e) = Kb::check_version(g.contract) {
            eprintln!("sf-kb: {e}");
            return ExitCode::from(1);
        }
        let root = g.root.clone();
        let served = mcp::serve(io::stdin().lock(), io::stdout().lock(), g.agent, g.session, move || open_kb(&root));
        return match served {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("sf-kb: {e}");
                ExitCode::from(1)
            }
        };
    }
    match exec(&g, cmd) {
        Ok(result) => {
            println!("{}", json!({ "ok": true, "result": result }));
            ExitCode::SUCCESS
        }
        Err((code, message)) => {
            println!("{}", fail(&code, &message));
            ExitCode::from(1)
        }
    }
}
