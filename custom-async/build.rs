fn main() {
    let musl = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "musl";
    let musl_v1_2_3 = std::env::var("RUST_LIBC_UNSTABLE_MUSL_V1_2_3").is_ok();
    println!("cargo:rerun-if-env-changed=RUST_LIBC_UNSTABLE_MUSL_V1_2_3");
    println!("cargo:rustc-check-cfg=cfg(musl_v1_2_3)");
    if musl && musl_v1_2_3 {
        println!("cargo:rustc-cfg=musl_v1_2_3");
    }
}
