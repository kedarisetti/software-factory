//! Thin wrapper: the shim logic is `sf_sandbox_local::netshim::main`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(sf_sandbox_local::netshim::main(&args));
}
