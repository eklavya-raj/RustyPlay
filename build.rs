fn main() {
    println!("cargo:rerun-if-changed=uxplay/lib/playfair");

    let out_dir = std::env::var("OUT_DIR").unwrap();

    cc::Build::new()
        .include("uxplay/lib")
        .include("uxplay/lib/playfair")
        .file("uxplay/lib/playfair/playfair.c")
        .file("uxplay/lib/playfair/omg_hax.c")
        .file("uxplay/lib/playfair/hand_garble.c")
        .file("uxplay/lib/playfair/modified_md5.c")
        .file("uxplay/lib/playfair/sap_hash.c")
        .warnings(false)
        .compile("playfair");

    // Link playfair static library directly using linker arguments
    println!("cargo:rustc-link-arg=-L{}", out_dir);
    println!("cargo:rustc-link-arg=-lplayfair");

    // Link fdk-aac library directly using linker arguments
    println!("cargo:rustc-link-arg=-L/opt/homebrew/Cellar/fdk-aac/2.0.3/lib");
    println!("cargo:rustc-link-arg=-lfdk-aac");
}
