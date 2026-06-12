// Embed Windows VERSIONINFO resource into tgrep.exe.
//
// Without this, the signed binary has no ProductName, FileVersion, etc.
// in the Windows "Properties → Details" tab.
//
// Reference implementations:
//   - microsoft/vscode cli/build.rs (winresource)
//   - microsoft/coreutils build.rs (winresource + manifest)
//   - microsoft/python-environment-tools crates/pet/build.rs (winresource)

fn main() {
    // Use CARGO_CFG_TARGET_OS instead of #[cfg(target_os = "windows")] because
    // build scripts run on the host OS, not the target. When cross-compiling
    // (e.g. x64 host -> arm64 target in OneBranch), #[cfg] would check the host
    // and skip VERSIONINFO embedding even though the output is a Windows .exe.
    // Reference: microsoft/vscode cli/build.rs uses the same pattern.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    compile_windows_resources();
}

#[cfg(not(windows))]
fn compile_windows_resources() {
    println!("cargo:warning=Skipping Windows VERSIONINFO resource: build host is not Windows");
}

#[cfg(windows)]
fn compile_windows_resources() {
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "tgrep");
    res.set(
        "FileDescription",
        "tgrep - trigram-indexed grep for large codebases",
    );
    res.set("CompanyName", "Microsoft Corporation");
    res.set(
        "LegalCopyright",
        "Copyright (c) Microsoft Corporation. All rights reserved.",
    );
    res.set("InternalName", "tgrep");
    res.set("OriginalFilename", "tgrep.exe");
    // FileVersion and ProductVersion are auto-populated from Cargo.toml
    // by winresource when not explicitly set.
    res.compile().expect("Failed to compile Windows resources");
}
