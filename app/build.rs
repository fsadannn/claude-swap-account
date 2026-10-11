fn main() {
    // Embed the icon into the Windows exe (Explorer, taskbar, shortcuts).
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("failed to embed icon");
    }
}
