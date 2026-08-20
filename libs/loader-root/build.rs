use std::{env, path::PathBuf};

fn main() {
    let directory = env::var("RUSTOS_DLL_DIR").unwrap_or_else(|_| {
        // Clippy/check не выполняет target linker. Реальная сборка обязана
        // передать точный каталог уже собранной dependency.
        "target/x86_64-unknown-rustos/debug".into()
    });
    println!("cargo:rerun-if-env-changed=RUSTOS_DLL_DIR");
    let dependency = PathBuf::from(directory).join("fixture_1.dll");
    println!("cargo:rustc-link-arg={}", dependency.display());
    println!("cargo:rustc-link-arg=-soname=loader-test-root.dll");
    // Fixture одновременно является PIE root: loader проверяет e_entry и
    // передаёт его приложению после линковки зависимостей.
    println!("cargo:rustc-link-arg=-e");
    println!("cargo:rustc-link-arg=linked_answer");
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=relro");
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=now");
}
