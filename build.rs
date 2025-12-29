fn main() {
    // gRPC proto compilation disabled - tonic-build 0.14 API requires investigation
    // Future work: Determine correct tonic-build 0.14 API and enable compilation
    println!("cargo:rerun-if-changed=proto/time_service.proto");
}
