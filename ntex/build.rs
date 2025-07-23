use version_check as rustc;

fn main() {
    if rustc::is_min_version("1.81.0").unwrap_or(false) {
        println!("cargo:rustc-cfg=feature=\"rust-181\"");
    }
}
