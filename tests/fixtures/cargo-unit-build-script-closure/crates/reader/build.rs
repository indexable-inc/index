use std::{env, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Declare the source to the planner, then exercise its runtime location.
    let declared = include_str!("../../shared/message.txt");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing manifest dir")?);
    let message = fs::read_to_string(manifest.join("../../shared/message.txt"))?;
    assert_eq!(message, declared);
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing output dir")?);
    fs::write(out.join("message.rs"), format!("pub const MESSAGE: &str = {message:?};\n"))?;
    println!("cargo:rerun-if-changed=../../shared/message.txt");
    Ok(())
}
