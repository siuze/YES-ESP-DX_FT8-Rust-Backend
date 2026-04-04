fn main() {
    // 获取项目根目录
    let project_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    
    // 1. 告诉 Rust 到哪里找你的 libwsjtx_bridge.so
    println!("cargo:rustc-link-search=native={}/libs", project_dir);

    // 2. 只需要链接这一个库
    println!("cargo:rustc-link-lib=dylib=wsjtx_bridge");

    // 3. 设置运行时路径 RPATH，让编译出来的程序在同级目录找 libs
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/libs");
}
