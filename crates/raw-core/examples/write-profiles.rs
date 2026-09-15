//! Regenerate the ICC profiles this project ships.
//!
//!     cargo run -p raw-core --example write-profiles
//!
//! Writes `profiles/monostar.icc` from `raw_core::icc`. Run it after changing anything
//! the profile is built from; `the_generator_reproduces_the_shipped_profile` fails
//! until you do, which is the point — the checked-in binary cannot drift away from the
//! code that claims to produce it.
//!
//! Publishing this alongside the profile is what makes its CC0 dedication legible: a
//! reproducible build from published constants is the evidence that nothing was
//! derived from somebody else's binary.

fn main() {
    let path = std::path::Path::new("profiles/monostar.icc");
    let bytes = raw_core::icc::monostar();
    let before = std::fs::read(path).ok();
    std::fs::write(path, &bytes).expect("write profiles/monostar.icc");

    let id: String = bytes[84..100].iter().map(|b| format!("{b:02x}")).collect();
    println!(
        "{} — {} bytes, profile ID {id}",
        path.display(),
        bytes.len()
    );
    match before {
        Some(old) if old == bytes => println!("  unchanged"),
        Some(old) => println!("  changed: was {} bytes", old.len()),
        None => println!("  created"),
    }
}
