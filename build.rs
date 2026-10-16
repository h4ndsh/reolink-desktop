//! Embeds the application icon into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/reolink-desktop.rc");
    println!("cargo:rerun-if-changed=packaging/windows/reolink-desktop.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("packaging/windows/reolink-desktop.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("compiling the Windows resources");
    }
}
