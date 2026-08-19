//! Records, at compile time, which machine this build runs on.
//!
//! Provisioning another endpoint has to decide whether the executable it is
//! running is one the far end could run, and the only honest answer comes from
//! the compiler: the target triple this build was produced for. Cargo tells a
//! build script and tells nothing else, so it is passed on as an environment
//! variable the crate reads with `env!`.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").expect("cargo did not say what this build targets");
    println!("cargo::rustc-env=AGENTBUS_TARGET={target}");
}
