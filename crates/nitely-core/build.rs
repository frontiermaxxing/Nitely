//! Lists the schema migrations in `migrations/` (one `.sql` file each) for `db.rs`, in file-name
//! order, so adding one is adding a file: two branches never edit the same lines for it.

use std::path::Path;

fn main() {
    // Source installs supply HEAD explicitly, invalidating the generated migration manifest even
    // when a checkout/ref update preserved file timestamps.
    println!("cargo:rerun-if-env-changed=NITELY_SOURCE_COMMIT");
    // The machine this build runs on, for cargo's per-target test runner setting (test_budget.rs).
    println!("cargo:rustc-env=NITELY_HOST_TARGET={}", std::env::var("TARGET").expect("cargo sets TARGET"));
    // Read when the script runs, not when it was compiled: cargo reuses a compiled build script
    // for another checkout of the same crate (a release build into a shared target folder), and
    // the folder it was compiled in may be gone by then (NIT-469).
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let dir = Path::new(&manifest).join("migrations");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| n.ends_with(".sql"))
                .collect()
        })
        .expect("reading the source migration directory");
    names.sort();
    let mut out = String::from("&[\n");
    for name in &names {
        let stem = name.trim_end_matches(".sql");
        let valid = !stem.is_empty() && stem.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        assert!(valid, "migration file names are lower-case letters, digits and dashes: {name}");
        let path = dir.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        out.push_str(&format!("    ({stem:?}, include_str!({:?})),\n", path.display().to_string()));
    }
    out.push(']');
    let target = Path::new(&std::env::var("OUT_DIR").unwrap()).join("migrations.rs");
    std::fs::write(target, out).unwrap();
}
