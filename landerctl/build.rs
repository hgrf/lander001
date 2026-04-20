fn main() {
    println!("cargo:rerun-if-changed=../proto/robot.proto");

    let protoc = protoc_bin_vendored::protoc_bin_path().expect("failed to find protoc binary");
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    prost_build::Config::new()
        .compile_protos(&["../proto/robot.proto"], &["../proto"])
        .expect("failed to compile protobuf schema");
}
