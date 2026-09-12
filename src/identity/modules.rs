//! `MODULE <path> [-f <file>] [TARGET <name>] [AS <name>]` — a target's base
//! is another basil build (its own context + Basilfile), consumed as a
//! dependency artifact.
//!
//! The module is a separate closure domain: its deliverable's `tagHash` is
//! folded into the importer's identity and the importer's Containerfile renders
//! `FROM <module-push-ref>:<module-tagHash>` (use-or-build: that content tag is
//! probed in the module's registry; missing → the module is built and pushed
//! first). Importer identity never reads the module's raw bytes — only its
//! derived artifact (D3: no auto-exclusion of module dirs from the importer's
//! derivedData; a module inside the importer context folds both ways unless
//! dockerignored — the operator's call).
//!
//! Discovery is a pure file walk (parse + canonicalize), recursive with cycle
//! detection; closure digests are computed separately (offline keeper, or with
//! a registry resolver), in dependency order.

use crate::dsl::ast::{Base, File, Target};
use crate::dsl::parser;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[cfg(test)]
use super::closure;
/// The unique key of a module import: `<canonical-context>/<file>`. This is
/// what importer records and build orchestration key on, so the same context
/// with a different `-f` name is a *different* module.
pub fn module_key(file_dir: &Path, m: &crate::dsl::ast::ModuleRef) -> Result<String, String> {
    let ctx = if Path::new(&m.path).is_absolute() {
        PathBuf::from(&m.path)
    } else {
        file_dir.join(&m.path)
    };
    let canonical = ctx
        .canonicalize()
        .map_err(|e| format!("MODULE `{}` (line {}): {e}", m.path, m.line))?;
    let file = m.file.clone().unwrap_or_else(|| "Basilfile".to_string());
    Ok(format!("{}/{}", canonical.display(), file))
}

/// One discovered module: its resolved files, parsed AST, and chosen
/// deliverable (its registry ref + stage name).
#[derive(Debug)]
pub struct ModuleEntry {
    pub key: String,
    /// Absolute module context directory.
    pub context: PathBuf,
    /// Absolute module Basilfile path.
    pub file: PathBuf,
    pub parsed: File,
    /// Deliverable's stage name in the module file (TARGET selector, else the
    /// single qualified deliverable).
    pub deliverable: String,
    /// The deliverable's registry ref (`REGISTER`, else qualified name).
    pub push_ref: String,
}

/// All discovered modules under an importer file, `order` listing keys
/// dependency-first (a module always precedes any module that imports it), so
/// digest computation can iterate it bottom-up.
#[derive(Debug)]
pub struct ModuleIndex {
    pub entries: BTreeMap<String, ModuleEntry>,
    pub order: Vec<String>,
}

/// `MODULE <path>` — the module file must declare exactly one qualified
/// deliverable when no `TARGET` selector is given; two or zero is an error
/// naming the file.
fn select_deliverable(
    parsed: &File,
    module_ctx: &Path,
    module_file: &str,
    selector: Option<&str>,
) -> Result<(String, String), String> {
    let runs: Vec<&Target> = parsed
        .targets
        .iter()
        .filter(|t| t.register.is_some() || t.name.split('/').count() >= 2)
        .collect();
    let chosen: &Target = match selector {
        Some(name) => parsed
            .targets
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| {
                format!(
                    "MODULE `{}`: target `{name}` not in module file `{}`",
                    module_ctx.display(),
                    module_file
                )
            })?,
        None => match runs.as_slice() {
            [t] => t,
            [] => {
                return Err(format!(
                    "MODULE `{}` (file `{}`): module has no deliverable — give a target a \
                     qualified name or a `REGISTER` ref (or use `MODULE ... TARGET <name>`)",
                    module_ctx.display(),
                    module_file
                ))
            }
            many => {
                return Err(format!(
                    "MODULE `{}` (file `{}`): module declares {} qualified deliverables ({}) — \
                     select one with `MODULE ... TARGET <name>`",
                    module_ctx.display(),
                    module_file,
                    many.len(),
                    many.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
                ))
            }
        },
    };
    if selector.is_some() && chosen.register.is_none() && chosen.name.split('/').count() < 2 {
        return Err(format!(
            "MODULE target `{}` (file `{}`) is not a deliverable — give it a qualified name or a `REGISTER` ref",
            chosen.name, module_file
        ));
    }
    Ok((chosen.name.clone(), chosen.push_ref().to_string()))
}

fn discover_into(
    targets: &[Target],
    file_dir: &Path,
    index: &mut ModuleIndex,
    stack: &mut Vec<String>,
) -> Result<(), String> {
    for t in targets {
        let Base::Module(m) = &t.base else { continue };
        let key = module_key(file_dir, m)?;
        if stack.iter().any(|k| k == &key) {
            let mut chain: Vec<String> = stack.clone();
            chain.push(key.clone());
            return Err(format!(
                "MODULE cycle: {}",
                chain.join(" -> ")
            ));
        }
        if index.entries.contains_key(&key) {
            continue; // already discovered (shared dependency)
        }
        let ctx = if Path::new(&m.path).is_absolute() {
            PathBuf::from(&m.path)
        } else {
            file_dir.join(&m.path)
        };
        let canonical = ctx.canonicalize().map_err(|e| format!("MODULE `{}`: {e}", m.path))?;
        let file_name = m.file.clone().unwrap_or_else(|| "Basilfile".to_string());
        let file_abs = canonical.join(&file_name);
        let src = crate::fsctx::fs_err(std::fs::read_to_string(&file_abs), &file_abs, "read")
            .map_err(|e| format!("MODULE `{}`: {e:#}", m.path))?;
        let parsed = parser::parse(&src).map_err(|e| {
            format!("MODULE `{}` (file `{}`): {e}", m.path, file_abs.display())
        })?;
        let (deliverable, push_ref) =
            select_deliverable(&parsed, &canonical, &file_name, m.target.as_deref())?;
        stack.push(key.clone());
        discover_into(&parsed.targets, &canonical, index, stack)?;
        stack.pop();
        // Insert AFTER the module's own deps, so `order` is dependency-first.
        index.entries.insert(
            key.clone(),
            ModuleEntry {
                key: key.clone(),
                context: canonical,
                file: file_abs,
                parsed,
                deliverable,
                push_ref,
            },
        );
        index.order.push(key);
    }
    Ok(())
}

/// Discover every module reachable from `targets` (the importer's in-scope
/// set), recursively, with cycle detection. Pure file walk — no registries, no
/// digests.
pub fn discover_module_index(targets: &[Target], file_dir: &Path) -> Result<ModuleIndex, String> {
    let mut index = ModuleIndex { entries: BTreeMap::new(), order: Vec::new() };
    let mut stack = Vec::new();
    discover_into(targets, file_dir, &mut index, &mut stack)?;
    Ok(index)
}

/// Module closure tables: per module key, the deliverable `tagHash` plus the
/// module's full closure table (every in-scope target, so its intermediate
/// stages can be cached). `order` is dependency-first, so every module's deps
/// resolve before it.
#[derive(Debug)]
pub struct ModuleClosures {
    /// Module key -> deliverable `tagHash` (folds into the importer and names
    /// the module's content tag).
    pub deliverable: BTreeMap<String, [u8; 32]>,
    /// Module key -> full closure table of that module's targets.
    pub tables: BTreeMap<String, std::collections::HashMap<String, [u8; 32]>>,
}

/// Compute every module's closure table bottom-up (its targets, its own module
/// deps already folded).
pub fn compute_module_digests(
    index: &ModuleIndex,
    resolver: &mut dyn super::closure::BaseResolver,
) -> Result<ModuleClosures, String> {
    let mut deliverable: BTreeMap<String, [u8; 32]> = BTreeMap::new();
    let mut tables: BTreeMap<String, std::collections::HashMap<String, [u8; 32]>> =
        BTreeMap::new();
    for key in &index.order {
        let e = &index.entries[key];
        let closures = super::closure::compute_closure(
            &e.parsed.targets,
            &e.parsed.default_platforms,
            &e.parsed.global_args,
            &e.context,
            &e.file,
            &std::collections::HashMap::new(),
            resolver,
            &deliverable,
        )?;
        let hash = closures.get(&e.deliverable).ok_or_else(|| {
            format!("module `{}`: deliverable `{}` has no closure", key, e.deliverable)
        })?;
        deliverable.insert(key.clone(), *hash);
        tables.insert(key.clone(), closures);
    }
    Ok(ModuleClosures { deliverable, tables })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("basil-modules-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, content: &str) {
        let full = dir.join(name);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
    }

    #[test]
    fn discovers_single_module_and_deliverable() {
        let dir = tmpdir("d1");
        // A sibling module: its own context + Basilfile with a REGISTER
        // deliverable; the importer references it file-relative.
        write(&dir, "mod/Basilfile", "FROM alpine AS shared\nCOPY a.txt /a\nREGISTER 127.0.0.1:5001/shared\n");
        write(&dir, "mod/a.txt", "x");
        let import = "MODULE mod -f Basilfile TARGET shared AS shared\nCOPY --from=shared /a /a\n";
        let f = parser::parse(import).unwrap();
        let index = discover_module_index(&f.targets, &dir).unwrap();
        assert_eq!(index.order.len(), 1, "{:?}", index.order);
        let e = &index.entries[&index.order[0]];
        assert_eq!(e.deliverable, "shared");
        assert_eq!(e.push_ref, "127.0.0.1:5001/shared");
        assert_eq!(e.context, dir.join("mod").canonicalize().unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn module_cycle_is_an_error() {
        let dir = tmpdir("d2");
        write(&dir, "a/Basilfile", "MODULE ../b AS b\nREGISTER 127.0.0.1:5001/a\n");
        write(&dir, "b/Basilfile", "MODULE ../a AS a\nREGISTER 127.0.0.1:5001/b\n");
        let f = parser::parse("MODULE a -f Basilfile AS a\n").unwrap();
        let err = discover_module_index(&f.targets, &dir).unwrap_err();
        assert!(err.contains("cycle"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiple_deliverables_need_target_selector() {
        let dir = tmpdir("d3");
        write(
            &dir,
            "mod/Basilfile",
            "FROM alpine AS one\nREGISTER 127.0.0.1:5001/one\nFROM alpine AS two\nREGISTER 127.0.0.1:5001/two\n",
        );
        let f = parser::parse("MODULE mod AS m\n").unwrap();
        let err = discover_module_index(&f.targets, &dir).unwrap_err();
        assert!(err.contains("TARGET"), "{err}");
        let f2 = parser::parse("MODULE mod TARGET two AS m\n").unwrap();
        let index = discover_module_index(&f2.targets, &dir).unwrap();
        let e = &index.entries[&index.order[0]];
        assert_eq!(e.deliverable, "two");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_module_discovered_once() {
        let dir = tmpdir("d4");
        write(&dir, "shared/Basilfile", "FROM alpine AS out\nREGISTER 127.0.0.1:5001/shared\n");
        write(&dir, "mod1/Basilfile", "MODULE ../shared AS s\nREGISTER 127.0.0.1:5001/mod1\n");
        write(&dir, "mod2/Basilfile", "MODULE ../shared AS s\nREGISTER 127.0.0.1:5001/mod2\n");
        let f = parser::parse("MODULE mod1 AS m1\nMODULE mod2 AS m2\n").unwrap();
        let index = discover_module_index(&f.targets, &dir).unwrap();
        // mod1, mod2 both import ../shared -> 3 entries, shared first.
        assert_eq!(index.order.len(), 3, "{:?}", index.order);
        assert!(index.order[0].ends_with("/shared/Basilfile"), "{:?}", index.order);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_module_path_is_an_error() {
        let dir = tmpdir("d5");
        let f = parser::parse("MODULE nope AS n\n").unwrap();
        let err = discover_module_index(&f.targets, &dir).unwrap_err();
        assert!(err.contains("MODULE"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_basilfile_and_named_file() {
        let dir = tmpdir("d6");
        write(&dir, "m/Basilfile.x", "FROM alpine AS x\nREGISTER 127.0.0.1:5001/x\n");
        let f = parser::parse("MODULE m -f Basilfile.x TARGET x AS x\n").unwrap();
        let index = discover_module_index(&f.targets, &dir).unwrap();
        assert_eq!(index.order.len(), 1);
        assert!(index.order[0].ends_with("/m/Basilfile.x"), "{:?}", index.order);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn module_digest_folds_into_importer_and_changes_tag_once() {
        use std::collections::HashMap;
        let dir = tmpdir("d7");
        // Module (sibling, outside the importer context) + importer that
        // FROMs it. The importer's closure folds the module deliverable hash
        // as base.module.* — a module edit must change the importer tag
        // exactly once, through the folded digest.
        write(&dir, "shared/Basilfile", "FROM alpine@sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA AS out\nCOPY a.txt /a\nREGISTER 127.0.0.1:5001/shared\n");
        write(&dir, "shared/a.txt", "x");
        write(&dir, "app/Basilfile", "MODULE ../shared AS shared\nFROM scratch AS app\nCOPY --from=shared /a /a\nREGISTER 127.0.0.1:5001/app\n");
        let src = std::fs::read_to_string(dir.join("app/Basilfile")).unwrap();
        let f = crate::dsl::parser::parse(&src).unwrap();
        let app_file = dir.join("app/Basilfile");

        let index = discover_module_index(&f.targets, dir.join("app").as_path()).unwrap();
        let module_digests =
            compute_module_digests(&index, &mut crate::identity::closure::OfflineResolver)
                .unwrap();
        let closures = super::closure::compute_closure(
            &f.targets,
            &f.default_platforms,
            &f.global_args,
            dir.join("app").as_path(),
            &app_file,
            &HashMap::new(),
            &mut crate::identity::closure::OfflineResolver,
            &module_digests.deliverable,
        )
        .unwrap();
        let app_tag1 = closures["app"];
        // The module digest is folded: editing the module source must change
        // the importer's tagHash.
        write(&dir, "shared/a.txt", "y");
        let index2 = discover_module_index(&f.targets, dir.join("app").as_path()).unwrap();
        let module_digests2 =
            compute_module_digests(&index2, &mut crate::identity::closure::OfflineResolver)
                .unwrap();
        let closures2 = super::closure::compute_closure(
            &f.targets,
            &f.default_platforms,
            &f.global_args,
            dir.join("app").as_path(),
            &app_file,
            &HashMap::new(),
            &mut crate::identity::closure::OfflineResolver,
            &module_digests2.deliverable,
        )
        .unwrap();
        assert_ne!(app_tag1, closures2["app"], "module edit must fold through to the importer");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn module_with_unpinned_base_fails_offline() {
        let dir = tmpdir("d8");
        write(&dir, "m/Basilfile", "FROM nginx AS out\nREGISTER 127.0.0.1:5001/m\n");
        write(&dir, "app/Basilfile", "MODULE ../m AS out\nCOPY --from=out /x /x\nREGISTER 127.0.0.1:5001/app\n");
        let src = std::fs::read_to_string(dir.join("app/Basilfile")).unwrap();
        let f = crate::dsl::parser::parse(&src).unwrap();
        let index = discover_module_index(&f.targets, dir.join("app").as_path()).unwrap();
        let err = compute_module_digests(&index, &mut crate::identity::closure::OfflineResolver)
            .unwrap_err();
        assert!(err.contains("nginx"), "unpinned module base must fail offline: {err}");
        let _ = fs::remove_dir_all(&dir);
    }
}