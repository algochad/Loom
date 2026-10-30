// The exe's own icon (Explorer, the taskbar before the window opens); the window sets it too.
fn main() {
    println!("cargo:rerun-if-changed=loom.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("loom.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=no exe icon: {e}");
        }
    }
}
