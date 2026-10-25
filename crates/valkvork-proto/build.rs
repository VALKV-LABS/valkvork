fn main() {
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .out_dir("src")
        .compile(
            &["src/agent.v1.proto", "src/admin.v1.proto"],
            &["src/"]
        )
        .unwrap_or_else(|e| panic!("protobuf compilation failed: {}", e));
}
