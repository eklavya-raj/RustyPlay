fn main() {
    println!("cargo:rerun-if-changed=uxplay/lib/playfair");

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

    // Dynamically detect and link Fraunhofer fdk-aac library
    if let Ok(output) = std::process::Command::new("pkg-config")
        .args(&["--libs", "--cflags", "fdk-aac"])
        .output()
    {
        if output.status.success() {
            let flags_str = String::from_utf8_lossy(&output.stdout);
            for arg in flags_str.split_whitespace() {
                if arg.starts_with("-L") {
                    println!("cargo:rustc-link-search=native={}", &arg[2..]);
                } else if arg.starts_with("-l") {
                    println!("cargo:rustc-link-lib={}", &arg[2..]);
                }
            }
        } else {
            // Fallback if pkg-config returns error
            println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
            println!("cargo:rustc-link-search=native=/usr/local/lib");
            println!("cargo:rustc-link-lib=fdk-aac");
        }
    } else {
        // Fallback if pkg-config is not installed
        println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
        println!("cargo:rustc-link-search=native=/usr/local/lib");
        println!("cargo:rustc-link-lib=fdk-aac");
    }
}
