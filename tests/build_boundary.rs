use std::{path::Path, process::Command};

#[test]
fn build_revision_is_short_and_safe_without_a_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("revision-check");
    let compilation = Command::new("rustc")
        .args(["--edition=2024"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(compilation.status.success());
    for (revision, expected) in [
        (None, "unknown"),
        (Some(""), "unknown"),
        (Some("unknown"), "unknown"),
        (Some("abcdef"), "unknown"),
        (Some("abcdefg"), "unknown"),
        (Some("abcdef0\ncargo:rustc-env=INJECTED=yes"), "unknown"),
        (Some("6929bbc16a15839fd1ed154aa67dd7801f8d0508"), "6929bbc"),
        (Some(" ABCDEF0 "), "abcdef0"),
    ] {
        let mut command = Command::new(&binary);
        command
            .current_dir(temp.path())
            .env("PROFILE", "debug")
            .env_remove("CARGO_FEATURE_EMBEDDED_UI")
            .env_remove("LIBRARY_BUILD_COMMIT");
        if let Some(revision) = revision {
            command.env("LIBRARY_BUILD_COMMIT", revision);
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout
                .lines()
                .any(|line| { line == format!("cargo:rustc-env=LIBRARY_BUILD_COMMIT={expected}") })
        );
        assert!(stdout.contains("cargo:rerun-if-env-changed=LIBRARY_BUILD_COMMIT"));
        assert!(!stdout.contains("INJECTED"));
    }
}

#[test]
fn embedded_build_rejects_missing_assets_and_emits_url_paths() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp
        .path()
        .join(format!("build-check{}", std::env::consts::EXE_SUFFIX));
    let compilation = Command::new("rustc")
        .args(["--edition=2024"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        compilation.status.success(),
        "{}",
        String::from_utf8_lossy(&compilation.stderr)
    );
    let invoke = |enabled: bool| {
        let mut command = Command::new(&binary);
        command
            .current_dir(temp.path())
            .env("PROFILE", "release")
            .env("OUT_DIR", temp.path())
            .env_remove("CARGO_FEATURE_EMBEDDED_UI");
        if enabled {
            command.env("CARGO_FEATURE_EMBEDDED_UI", "1");
        }
        command.output().unwrap()
    };
    let output = invoke(false);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("release requires"));
    let output = invoke(true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing web/dist/index.html"));
    let dist = temp.path().join("web/dist");
    std::fs::create_dir_all(dist.join("assets")).unwrap();
    std::fs::write(
        dist.join("index.html"),
        "<script src=\"/assets/app.js\"></script>",
    )
    .unwrap();
    let output = invoke(true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing referenced frontend asset"));
    std::fs::write(dist.join("assets/app.js"), "document.title = 'Library';").unwrap();
    let output = invoke(true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = std::fs::read_to_string(temp.path().join("assets.rs")).unwrap();
    assert!(generated.contains("\"/assets/app.js\""));
    std::fs::remove_dir_all(&dist).unwrap();
    let source = temp.path().join("verify.rs");
    std::fs::write(&source, format!("{generated}\nfn main() {{ assert!(ASSETS.iter().any(|(route,bytes)| *route == \"/assets/app.js\" && *bytes == b\"document.title = 'Library';\")); }}")).unwrap();
    let verify = temp.path().join("verify");
    let compiled = Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&verify)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    assert!(Command::new(verify).status().unwrap().success());
}
