fn main() {
    println!("cargo:rustc-link-arg=-soname=fixture-1.dll");
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=relro");
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=now");
}
