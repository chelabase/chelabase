//! Bundling a function: esbuild (the project's, else PATH's), `.wasm` imports kept as `wasm`
//! modules (a WASI component transpiled with jco first), and the runtime's manifest with the
//! SHA-256 of its exact bytes.
//!
//! A `.wasm` import is named in the bundle by its path as written, so import it with a `./`
//! path; esbuild keeps external paths verbatim, and the runtime resolves them from the bundle's
//! main module (`index.js`).

use super::config::Function;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The bundle's main module.
pub const MAIN: &str = "index.js";
pub const ESBUILD_INSTALL: &str = "esbuild isn't installed: run `npm install --save-dev esbuild` in the project (or put esbuild on PATH)";
pub const JCO_INSTALL: &str = "a WASI component needs jco, which isn't installed: run `npm install --save-dev \
     @bytecodealliance/jco @bytecodealliance/preview2-shim` in the project (or put jco on PATH)";

/// One module of the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub name: String,
    /// `js`, `wasm`, `text` or `data`.
    pub kind: &'static str,
    pub bytes: Vec<u8>,
}

/// What a deploy uploads: the manifest's exact bytes and their hash, and the files the bundle
/// was built from (what a watch reacts to).
pub struct Built {
    pub manifest: Vec<u8>,
    pub sha256: String,
    pub inputs: BTreeSet<PathBuf>,
}

/// The runtime's manifest (compact JSON, keys sorted), as uploaded byte for byte.
pub fn manifest(main: &str, modules: &[Module], date: &str, flags: &[String]) -> Vec<u8> {
    let modules: Vec<_> = modules
        .iter()
        .map(|m| serde_json::json!({ "name": m.name, "type": m.kind, "base64": STANDARD.encode(&m.bytes) }))
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "compatibilityDate": date,
        "compatibilityFlags": flags,
        "mainModule": main,
        "modules": modules,
    }))
    .expect("a JSON value serialises")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug, PartialEq, Eq)]
pub enum WasmKind {
    /// A core module (version 1).
    Core,
    /// A component (the component model's layer 1).
    Component,
    NotWasm,
}

/// What a `.wasm` file's 8-byte header says it is.
pub fn wasm_kind(bytes: &[u8]) -> WasmKind {
    match bytes {
        [0, b'a', b's', b'm', 1, 0, 0, 0, ..] => WasmKind::Core,
        [0, b'a', b's', b'm', _, _, 1, 0, ..] => WasmKind::Component,
        _ => WasmKind::NotWasm,
    }
}

/// `name` in `node_modules/.bin` of `project` or a directory above it, else on `path`.
pub fn find_tool(project: &Path, name: &str, path: Option<OsString>) -> Option<PathBuf> {
    project
        .ancestors()
        .map(|d| d.join("node_modules/.bin").join(name))
        .chain(
            path.iter()
                .flat_map(std::env::split_paths)
                .map(|d| d.join(name)),
        )
        .find(|p| p.is_file())
}

fn tool(project: &Path, name: &str, install: &str) -> Result<PathBuf> {
    find_tool(project, name, std::env::var_os("PATH")).with_context(|| install.to_string())
}

#[derive(Deserialize)]
struct Meta {
    inputs: BTreeMap<String, MetaInput>,
}

#[derive(Deserialize)]
struct MetaInput {
    #[serde(default)]
    imports: Vec<MetaImport>,
}

#[derive(Deserialize)]
struct MetaImport {
    path: String,
    #[serde(default)]
    external: bool,
}

/// Runs esbuild on `entry` (relative to `root`) into `out`, with a metafile when `meta`.
fn esbuild(root: &Path, entry: &Path, out: &Path, meta: Option<&Path>) -> Result<()> {
    let esbuild = tool(root, "esbuild", ESBUILD_INSTALL)?;
    let mut cmd = Command::new(&esbuild);
    cmd.current_dir(root)
        .arg(entry)
        .args([
            "--bundle",
            "--format=esm",
            "--platform=neutral",
            "--conditions=workerd,worker,browser",
            "--main-fields=module,main",
            "--external:*.wasm",
            "--log-level=error",
        ])
        .arg(format!("--outfile={}", out.display()));
    if let Some(meta) = meta {
        cmd.arg(format!("--metafile={}", meta.display()));
    }
    let output = cmd
        .output()
        .with_context(|| format!("running {}", esbuild.display()))?;
    if !output.status.success() {
        bail!(
            "esbuild failed on {}:\n{}",
            entry.display(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    Ok(())
}

/// Bundles the function `name` of the project at `root`.
pub fn build(root: &Path, name: &str, f: &Function) -> Result<Built> {
    let out = root.join(".chelabase/build").join(name);
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let (js, meta) = (out.join(MAIN), out.join("meta.json"));
    let mut inputs = BTreeSet::from([root.join(&f.entry)]);
    if !root.join(&f.entry).is_file() {
        bail!("{name}: entry {} doesn't exist", f.entry.display());
    }
    esbuild(root, &f.entry, &js, Some(&meta))?;
    let meta: Meta =
        serde_json::from_slice(&std::fs::read(&meta)?).context("esbuild's metafile")?;
    let mut modules = vec![Module {
        name: MAIN.into(),
        kind: "js",
        bytes: std::fs::read(&js)?,
    }];
    let mut wasm: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (input, i) in &meta.inputs {
        let input = root.join(input);
        for imp in i
            .imports
            .iter()
            .filter(|i| i.external && i.path.ends_with(".wasm"))
        {
            let Some(module) = imp
                .path
                .strip_prefix("./")
                .filter(|p| !p.split('/').any(|s| s == ".."))
            else {
                bail!(
                    "{}: import `{}` with a ./ path (no ..)",
                    input.display(),
                    imp.path
                );
            };
            let file = input.parent().unwrap_or(root).join(module);
            match wasm.insert(module.to_string(), file.clone()) {
                Some(other) if other != file => bail!(
                    "two different files are imported as `{}` ({} and {}): rename one",
                    imp.path,
                    other.display(),
                    file.display()
                ),
                _ => {}
            }
        }
        inputs.insert(input);
    }
    for (module, file) in wasm {
        let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        match wasm_kind(&bytes) {
            WasmKind::Core => modules.push(Module {
                name: module,
                kind: "wasm",
                bytes,
            }),
            WasmKind::Component => modules.extend(component(root, &out, &module, &file)?),
            WasmKind::NotWasm => bail!("{} isn't a WebAssembly file", file.display()),
        }
        inputs.insert(file);
    }
    let manifest = manifest(
        MAIN,
        &modules,
        f.compatibility_date(),
        &f.compatibility_flags,
    );
    let sha256 = sha256_hex(&manifest);
    Ok(Built {
        manifest,
        sha256,
        inputs,
    })
}

/// A WASI component imported as `module`: `jco transpile` (instantiation mode), then a JS module
/// of that name whose default export is the instantiated component, its WASI imports from
/// `@bytecodealliance/preview2-shim`, and its core modules as `wasm` modules beside it.
fn component(root: &Path, out: &Path, module: &str, file: &Path) -> Result<Vec<Module>> {
    let jco = tool(root, "jco", JCO_INSTALL)?;
    let stem = Path::new(module)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("component");
    let dir = out.join("jco").join(module.replace('/', "__"));
    let _ = std::fs::remove_dir_all(&dir);
    let status = Command::new(&jco)
        .current_dir(root)
        .arg("transpile")
        .arg(file)
        .args(["--name", stem, "--instantiation", "async", "--quiet", "-o"])
        .arg(&dir)
        .status()
        .with_context(|| format!("running {}", jco.display()))?;
    if !status.success() {
        bail!("jco transpile failed on {}", file.display());
    }
    let mut cores = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "wasm") {
            cores.push(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    cores.sort();
    let imports: String = cores
        .iter()
        .enumerate()
        .map(|(i, c)| format!("import c{i} from \"./{c}\";\n"))
        .collect();
    let table: String = cores
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c:?}: c{i}, "))
        .collect();
    let wrapper = format!(
        "import {{ instantiate }} from \"./{stem}.js\";\nimport * as shim from \"@bytecodealliance/preview2-shim\";\n\
         {imports}const cores = {{ {table}}};\n\
         const camel = (s) => s.replace(/-(\\w)/g, (_, c) => c.toUpperCase());\n\
         const imports = new Proxy({{}}, {{ get(_, k) {{ const m = /^wasi:([^/]+)\\/([^@]+)/.exec(String(k)); \
         return m ? shim[m[1]][camel(m[2])] : undefined; }} }});\n\
         export default await instantiate((p) => cores[p], imports);\n"
    );
    std::fs::write(dir.join("chelabase-component.js"), wrapper)?;
    let js = dir.join("chelabase-component.bundle.js");
    esbuild(root, &dir.join("chelabase-component.js"), &js, None).context(
        "bundling the transpiled component (is @bytecodealliance/preview2-shim installed?)",
    )?;
    let prefix = module
        .rsplit_once('/')
        .map(|(d, _)| format!("{d}/"))
        .unwrap_or_default();
    let mut modules = vec![Module {
        name: module.into(),
        kind: "js",
        bytes: std::fs::read(&js)?,
    }];
    for c in cores {
        modules.push(Module {
            name: format!("{prefix}{c}"),
            kind: "wasm",
            bytes: std::fs::read(dir.join(&c))?,
        });
    }
    Ok(modules)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The functions e2e's manifest (services/functions/tests/e2e/main.rs builds the same
    /// bytes with `serde_json::json!`): the CLI must produce them byte for byte, and the hash
    /// is of exactly those bytes.
    #[test]
    fn the_manifest_matches_the_shared_vector() {
        let source = "export default { fetch() { return new Response(\"open\") } }";
        let bytes = manifest(
            "main.js",
            &[Module {
                name: "main.js".into(),
                kind: "js",
                bytes: source.as_bytes().to_vec(),
            }],
            "2026-10-01",
            &[],
        );
        let e2e = serde_json::to_string(&serde_json::json!({
            "compatibilityDate": "2026-10-01",
            "compatibilityFlags": [],
            "mainModule": "main.js",
            "modules": [{"name": "main.js", "type": "js", "base64": STANDARD.encode(source)}],
        }))
        .unwrap();
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), e2e);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            "{\"compatibilityDate\":\"2026-10-01\",\"compatibilityFlags\":[],\"mainModule\":\"main.js\",\
             \"modules\":[{\"base64\":\"ZXhwb3J0IGRlZmF1bHQgeyBmZXRjaCgpIHsgcmV0dXJuIG5ldyBSZXNwb25zZSgib3BlbiIpIH0gfQ==\",\
             \"name\":\"main.js\",\"type\":\"js\"}]}"
        );
        assert_eq!(
            sha256_hex(&bytes),
            hex::encode(Sha256::digest(e2e.as_bytes()))
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn wasm_headers_tell_core_from_component() {
        assert_eq!(wasm_kind(b"\0asm\x01\0\0\0rest"), WasmKind::Core);
        assert_eq!(wasm_kind(b"\0asm\x0d\0\x01\0rest"), WasmKind::Component);
        assert_eq!(
            wasm_kind(b"\0asm\x0e\0\x01\0"),
            WasmKind::Component,
            "a later component version"
        );
        assert_eq!(wasm_kind(b"\0asm\x01\0"), WasmKind::NotWasm, "too short");
        assert_eq!(
            wasm_kind(b"\0asm\x02\0\0\0"),
            WasmKind::NotWasm,
            "an unknown core version"
        );
        assert_eq!(wasm_kind(b"PK\x03\x04\0\0\0\0"), WasmKind::NotWasm);
    }

    #[test]
    fn tools_come_from_node_modules_then_path() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let project = t.path().join("app/functions");
        std::fs::create_dir_all(&project).unwrap();
        let on_path = t.path().join("bin");
        std::fs::create_dir_all(&on_path).unwrap();
        let exe = |p: &Path| {
            std::fs::write(p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        exe(&on_path.join("esbuild"));
        let path = Some(on_path.clone().into_os_string());
        assert_eq!(
            find_tool(&project, "esbuild", path.clone()),
            Some(on_path.join("esbuild"))
        );
        let bin = t.path().join("app/node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        exe(&bin.join("esbuild"));
        assert_eq!(
            find_tool(&project, "esbuild", path.clone()),
            Some(bin.join("esbuild"))
        );
        assert_eq!(find_tool(&project, "jco", path), None);
        assert!(ESBUILD_INSTALL.contains("npm install --save-dev esbuild"));
        assert!(JCO_INSTALL.contains("npm install --save-dev @bytecodealliance/jco"));
    }

    #[test]
    fn a_missing_esbuild_fails_the_build_with_the_install_command() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a.js"), "export default {}").unwrap();
        let f = Function {
            entry: "a.js".into(),
            auth: None,
            compatibility_date: None,
            compatibility_flags: vec![],
        };
        // SAFETY: no other test reads PATH.
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", t.path()) };
        let err = build(t.path(), "a", &f).err().map(|e| e.to_string());
        if let Some(p) = old {
            unsafe { std::env::set_var("PATH", p) };
        }
        assert_eq!(err.as_deref(), Some(ESBUILD_INSTALL));
    }
}
