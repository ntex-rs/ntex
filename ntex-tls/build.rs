use std::env;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(ntex_openssl_mem)");

    // openssl-sys describes the linked library through DEP_OPENSSL_* variables.
    // CRYPTO_set_mem_functions with file/line callbacks exists in OpenSSL 1.1.0+,
    // LibreSSL, BoringSSL and aws-lc do not support it.
    let forks = [
        "DEP_OPENSSL_LIBRESSL",
        "DEP_OPENSSL_BORINGSSL",
        "DEP_OPENSSL_AWSLC",
    ];
    let openssl = forks.iter().all(|var| env::var_os(var).is_none());
    let version = env::var("DEP_OPENSSL_VERSION_NUMBER")
        .ok()
        .and_then(|v| u64::from_str_radix(&v, 16).ok());
    if openssl && version.is_some_and(|v| v >= 0x1010_0000) {
        println!("cargo::rustc-cfg=ntex_openssl_mem");
    }
}
