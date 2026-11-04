//! Both protos declare `package valkvork.v1`. They are compiled separately
//! because the agent RPC is named `Connect` (spec §11.1), which collides with
//! the `connect` constructor tonic generates on clients. The agent proto is
//! therefore built server-only and its client is hand-written in `lib.rs`.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/agent.v1.proto");
    println!("cargo:rerun-if-changed=src/admin.v1.proto");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    for (proto, sub, client) in [
        ("src/agent.v1.proto", "agent", false),
        ("src/admin.v1.proto", "admin", true),
    ] {
        let dir = out.join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        tonic_build::configure()
            .build_server(true)
            .build_client(client)
            // JSON output in the CLI (`--output json`).
            .type_attribute(".", "#[derive(serde::Serialize)]")
            .out_dir(&dir)
            .compile(&[proto], &["src/"])
            .unwrap_or_else(|e| panic!("protobuf compilation of {proto} failed: {e}"));
    }
}
