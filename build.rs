// Embeds the app icon (and version info) into Ferrocap.exe so File Explorer and the taskbar show it.
fn main() {
    println!("cargo:rerun-if-changed=assets/ferrocap.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/ferrocap.ico");
        res.set("ProductName", "Ferrocap");
        res.set("FileDescription", "Ferrocap - clip it after it happens");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed icon: {e}");
        }
    }
}
