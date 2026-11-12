//! Windows: embed icons/baselynx.ico as icon resource 1, which GPUI loads for its windows and
//! Explorer, the taskbar and the Start menu show for the executable.

fn main() {
    println!("cargo:rerun-if-changed=baselynx.rc");
    println!("cargo:rerun-if-changed=icons/baselynx.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("baselynx.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embedding the Windows icon");
    }
}
