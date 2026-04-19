fn main() {
    embuild::espidf::sysenv::output();

    println!("cargo:rerun-if-changed=proto/robot.proto");
    generate_proto_bindings();

    println!("cargo:rerun-if-changed=assets/cats/cat1.png");
    println!("cargo:rerun-if-changed=assets/cats/cat2.png");
    println!("cargo:rerun-if-changed=assets/cats/cat3.png");

    generate_rgb565("assets/cats/cat1.png", "cats/cat1_rgb565_le.bin");
    generate_rgb565("assets/cats/cat2.png", "cats/cat2_rgb565_le.bin");
    generate_rgb565("assets/cats/cat3.png", "cats/cat3_rgb565_le.bin");
}

fn generate_proto_bindings() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("failed to find protoc binary");
    // Safe for build scripts in this toolchain and keeps setup zero-config.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    prost_build::Config::new()
        .compile_protos(&["proto/robot.proto"], &["proto"])
        .expect("failed to compile protobuf schema");
}

fn generate_rgb565(input: &str, output: &str) {
    let img = image::open(input)
        .unwrap_or_else(|_| panic!("failed to open {}", input))
        .resize_exact(240, 240, image::imageops::FilterType::Lanczos3)
        .to_rgb8();

    let mut out = Vec::with_capacity(240 * 240 * 2);
    for px in img.pixels() {
        let r = px[0] as u16;
        let g = px[1] as u16;
        let b = px[2] as u16;

        let rgb565 = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3);

        // ImageRawLE expects little-endian bytes.
        out.push((rgb565 & 0xFF) as u8);
        out.push((rgb565 >> 8) as u8);
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let out_path = std::path::Path::new(&out_dir).join(output);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|_| panic!("failed to create output directory for {}", output));
    }
    std::fs::write(out_path, out)
        .unwrap_or_else(|_| panic!("failed to write generated image {}", output));
}
