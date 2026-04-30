fn main() {
    let project_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    
    // 1. 寻找库文件的路径
    println!("cargo:rustc-link-search=native={}/libs", project_dir);

    // 2. 链接库名 (不带 lib 前缀和 .dll/.so 后缀)
    println!("cargo:rustc-link-lib=dylib=wsjtx_bridge");

    // 3. 只有在 Linux 下才设置 RPATH
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/libs");
    }

    // 4. 如果你在 Windows 上使用 MSVC (link.exe)，屏蔽掉 Linux 的链接参数
    // Windows 找 DLL 的逻辑是搜索 .exe 同级目录，所以不需要 RPATH
}
