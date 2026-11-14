// Embeds the Material Icon Theme's SVGs, sorted by name for a binary search.
use std::path::Path;
use std::process::Command;
use std::{env, fs};

fn main() {
    // Windows: the app's icon in the .exe, resource 1, which GPUI loads for its windows.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=../../packaging/windows/sparkcommander.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("../../packaging/windows/sparkcommander.ico");
        resource.compile().expect("the Windows icon resource");
    }
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    style_helpers(&manifest);
    let dir = Path::new(&manifest).join("../../vendor/material-icon-theme/icons");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = fs::read_dir(&dir)
        .expect("vendor/material-icon-theme/icons")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".svg"))
        .collect();
    names.sort();
    let mut code = String::from("pub const MATERIAL_ICONS: &[(&str, &[u8])] = &[\n");
    for name in names {
        let path = dir.join(&name);
        code += &format!(
            "    ({name:?}, include_bytes!({:?})),\n",
            path.to_str().unwrap()
        );
    }
    code += "];\n";
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("material_icons.rs"),
        code,
    )
    .unwrap();
}

/// Linux: sc-qtstyle and sc-gtkstyle beside the app, where it looks for them,
/// when their toolkit's development files are here; not when cross-building.
fn style_helpers(manifest: &str) {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !["linux", "freebsd"].contains(&os.as_str()) || env::var("TARGET").ok() != env::var("HOST").ok() {
        return;
    }
    // OUT_DIR is <profile>/build/sc-gui-<hash>/out.
    let out = env::var("OUT_DIR").unwrap();
    let Some(beside) = Path::new(&out).ancestors().nth(3) else {
        return;
    };
    for (name, library) in [("sc-qtstyle", "Qt6Widgets"), ("sc-gtkstyle", "gtk+-3.0")] {
        let dir = Path::new(manifest).join("..").join(name);
        println!("cargo:rerun-if-changed={}", dir.display());
        let found = Command::new("pkg-config").args(["--exists", library]).status().is_ok_and(|status| status.success());
        if !found {
            continue;
        }
        let built = Command::new("sh").arg(dir.join("build.sh")).arg(beside).output();
        match built {
            Ok(output) if output.status.success() => {}
            Ok(output) => println!("cargo:warning={name} did not build: {}", String::from_utf8_lossy(&output.stderr).lines().next().unwrap_or("")),
            Err(err) => println!("cargo:warning={name} did not build: {err}"),
        }
    }
}
