//! Conformance fixture: deterministic identity.
//!
//! The design pins determinism with a checked-in golden fixture whose expected
//! digest the test suite recomputes. A digest is a *real computed value*, not
//! fabricated: run with `REGENERATE=1` to refresh `expected.json` from the
//! current computation, then the committed file is the source of truth that
//! catches any accidental drift (a change in serialization, dockerignore
//! handling, or instruction rendering fails the test).

use basil::dsl::parser::parse;
use basil::identity::closure::{BaseResolver, compute_closure, hex};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

struct PinnedResolver;
impl BaseResolver for PinnedResolver {
    fn resolve(&mut self, raw: &str, _tag: Option<&str>, digest: Option<&str>) -> Result<String, String> {
        // `scratch` is the empty image: a fixed sentinel, never a registry pull.
        if raw == "scratch" {
            return Ok("scratch-empty".to_string());
        }
        // Fixture only uses pinned bases; if one is missing, fail loudly.
        digest
            .map(|d| d.to_string())
            .ok_or_else(|| "conformance fixture base must be pinned".to_string())
    }
}

const FIXTURE: &str = "tests/conformance";

fn compute() -> BTreeMap<String, String> {
    let dsl = std::fs::read_to_string(format!("{FIXTURE}/Basilfile")).unwrap();
    let file = parse(&dsl).unwrap();
    let ctx = Path::new(FIXTURE).join("ctx");
    let overrides = HashMap::new();
    let tags = compute_closure(&file.targets, &file.default_platforms, &ctx, &overrides, &mut PinnedResolver)
        .unwrap();
    tags.iter()
        .map(|(k, v)| (k.clone(), hex(v)))
        .collect::<std::collections::BTreeMap<_, _>>()
}

#[test]
fn conformance_digests_stable() {
    let computed = compute();
    let expected_path = format!("{FIXTURE}/expected.json");

    if std::env::var("REGENERATE").as_deref() == Ok("1") {
        let json = serde_json::to_string_pretty(&computed).unwrap();
        std::fs::write(&expected_path, format!("{json}\n")).unwrap();
        eprintln!("regenerated {expected_path}");
    }

    let expected_str = std::fs::read_to_string(&expected_path).unwrap();
    let expected: BTreeMap<String, String> = serde_json::from_str(&expected_str).unwrap();
    assert_eq!(computed, expected, "identity drift — REGENERATE=1 to refresh golden");
}