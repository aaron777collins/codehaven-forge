//! `sqlx::migrate!` reads the migrations directory at compile time but does not tell cargo
//! about it, so a new migration file would not rebuild this crate, and the build cache (sccache)
//! would hand back a MIGRATOR without it. This makes cargo watch the directory and passes a
//! digest of every file in it to the compiler, which lib.rs reads with `env!`, so both see the
//! change.

use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=migrations");
    let mut files: Vec<_> = fs::read_dir("migrations")
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    files.sort();
    // FNV-1a over each name and its contents: stable, no dependency.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for f in &files {
        eat(f.to_string_lossy().as_bytes());
        eat(&[0]);
        eat(&fs::read(f).unwrap_or_default());
        eat(&[0]);
    }
    println!("cargo:rustc-env=CH_DB_MIGRATIONS_DIGEST={}-{h:016x}", files.len());
}
