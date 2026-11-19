fn main() {
    // image_dds decodes BCn through intel_tex_2, which is C++.
    println!("cargo:rustc-link-lib=dylib=stdc++");
}
