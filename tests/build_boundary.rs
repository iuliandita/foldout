use std::{path::Path, process::Command};

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
            .env("CARGO_PKG_VERSION", "0.1.0")
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

#[test]
fn source_only_build_captures_commit_or_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("build-version");
    let compilation = Command::new("rustc")
        .args(["--edition=2024"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(compilation.status.success());
    for (commit, expected) in [
        (Some("ABCDEF0123456789abcdef0123456789abcdef01"), "abcdef0"),
        (Some("1234567"), "1234567"),
        (Some("unknown"), "unknown"),
        (Some(""), "unknown"),
        (Some("123456"), "unknown"),
        (Some("123456g"), "unknown"),
        (Some("1234567\ncargo:rustc-env=INJECTED=yes"), "unknown"),
        (None, "unknown"),
    ] {
        let mut command = Command::new(&binary);
        command
            .current_dir(temp.path())
            .env_clear()
            .env("PROFILE", "debug")
            .env("PATH", "")
            .env("CARGO_PKG_VERSION", "0.1.0");
        if let Some(commit) = commit {
            command.env("FOLDOUT_BUILD_COMMIT", commit);
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains(&format!(
                "cargo:rustc-env=FOLDOUT_BUILD_VERSION=0.1.0 ({expected})\n"
            )),
            "{stdout}"
        );
        assert!(stdout.contains("cargo:rerun-if-env-changed=FOLDOUT_BUILD_COMMIT"));
        assert!(!stdout.contains("INJECTED=yes"));
    }
}

#[test]
fn cli_version_uses_compile_time_identity_without_starting_service() {
    let output = Command::new(env!("CARGO_BIN_EXE_libraryd"))
        .arg("--version")
        .env_clear()
        .env("FOLDOUT_BUILD_COMMIT", "7654321")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("libraryd {}\n", libraryd::BUILD_VERSION)
    );
}
