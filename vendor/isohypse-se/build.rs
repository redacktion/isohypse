fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let obj = format!("{out}/enclave.o");
    let lib = format!("{out}/libisohypse_se_native.a");
    let swiftc = std::process::Command::new("swiftc")
        .args(["-O", "-parse-as-library", "-emit-object", "-o", &obj, "swift/enclave.swift"])
        .status()
        .expect("run swiftc");
    assert!(swiftc.success(), "swiftc failed to build swift/enclave.swift");
    let archive = std::process::Command::new("ar")
        .args(["crus", &lib, &obj])
        .status()
        .expect("run ar");
    assert!(archive.success(), "ar failed to archive the enclave object");
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=isohypse_se_native");
    println!("cargo:rustc-link-lib=framework=CryptoKit");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-lib=dylib=swiftCore");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rerun-if-changed=swift/enclave.swift");
    println!("cargo:rerun-if-changed=build.rs");
}
