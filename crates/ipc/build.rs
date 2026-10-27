fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        // kinfo_proc vive en <sys/sysctl.h>; libc no lo exporta en Apple.
        cc::Build::new()
            .file("src/macos_proc_stat.c")
            .compile("hostig_macos_proc_stat");
    }
}
