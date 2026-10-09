fn main() {
    println!("cargo:rerun-if-changed=assets/app.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "VhdxDock");
        res.set("FileDescription", "VhdxDock — VHDX image builder and differencing disk manager");
        res.set_manifest_file("assets/app.manifest");
        res.compile().expect("compile Windows application resources");
    }
}
