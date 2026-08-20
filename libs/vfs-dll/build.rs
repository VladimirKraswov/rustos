fn main() {
    // Major ABI входит и в имя файла, и в DT_SONAME. Поэтому loader сможет
    // одновременно держать несовместимые vfs-1.dll и vfs-2.dll.
    println!("cargo:rustc-link-arg=-soname=vfs-1.dll");
}
