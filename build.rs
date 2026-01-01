fn main() {
    // Only run on Windows targets
    if cfg!(target_os = "windows") {
        // Use winres to embed the icon into the final PE binary
        let mut res = winres::WindowsResource::new();
        res.set_icon("icon.ico");
        res.compile().expect("Failed to embed icon.ico into the executable");
    }
}
