use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=migrations");
    let enabled = env::var_os("CARGO_FEATURE_EMBEDDED_UI").is_some();
    if env::var("PROFILE").as_deref() == Ok("release") && !enabled {
        panic!("release requires --features embedded-ui; run bun run --cwd web build first");
    }
    if !enabled {
        return;
    }
    let root = Path::new("web/dist");
    assert!(
        root.join("index.html").is_file(),
        "missing web/dist/index.html; run bun run --cwd web build first"
    );
    let mut assets = Vec::new();
    collect(root, root, &mut assets);
    assets.sort();
    let assets: Vec<_> = assets
        .into_iter()
        .map(|(route, file)| {
            (
                route,
                fs::read(file).expect("frontend output changed while taking build snapshot"),
            )
        })
        .collect();
    let html = std::str::from_utf8(
        &assets
            .iter()
            .find(|(route, _)| route == "/index.html")
            .expect("missing frontend entrypoint")
            .1,
    )
    .expect("frontend entrypoint is not UTF-8");
    let mut has_script = false;
    // Vite emits quoted, root-relative URLs for the configured assets directory.
    for url in html
        .split(['\"', '\''])
        .filter(|value| value.starts_with("/assets/"))
    {
        assert!(
            assets.iter().any(|(route, _)| route == url),
            "missing referenced frontend asset: {url}"
        );
        has_script |= url.ends_with(".js");
    }
    assert!(has_script, "frontend entrypoint has no compiled JavaScript");
    let output = Path::new(&env::var_os("OUT_DIR").unwrap()).join("embedded-assets");
    fs::create_dir_all(&output).expect("cannot create frontend snapshot directory");
    let mut generated = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for (index, (route, bytes)) in assets.into_iter().enumerate() {
        let snapshot = output.join(index.to_string());
        fs::write(&snapshot, bytes).expect("cannot snapshot frontend asset");
        let file = snapshot
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        generated.push_str(&format!("({route:?}, include_bytes!({file:?})),\n"));
    }
    generated.push_str("];\n");
    fs::write(
        Path::new(&env::var_os("OUT_DIR").unwrap()).join("assets.rs"),
        generated,
    )
    .unwrap();
}

fn collect(root: &Path, directory: &Path, assets: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(directory).expect("cannot read frontend output") {
        let path = entry.unwrap().path();
        assert!(
            !path.is_symlink(),
            "frontend output must not contain symlinks"
        );
        if path.is_dir() {
            collect(root, &path, assets);
        } else {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .components()
                .map(|part| part.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>()
                .join("/");
            let route = format!("/{relative}");
            let absolute = path.canonicalize().unwrap().to_str().unwrap().to_owned();
            assets.push((route, absolute));
        }
    }
}
