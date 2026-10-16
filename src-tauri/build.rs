fn main() {
    // tauri-build embeds icons/icon.ico in the exe but asks to rerun only when the config changes,
    // so a new icon alone would leave the old one in the build.
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build()
}
