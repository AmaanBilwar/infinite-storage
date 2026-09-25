fn main() {
    #[cfg(target_os = "windows")]
    {
        winfsp::build::winfsp_link_delayload();
    }

    #[cfg(not(target_os = "windows"))]
    {
        // No Windows-only link steps on other platforms.
    }
}
