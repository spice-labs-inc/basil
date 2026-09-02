//! Serialize the composed `ImageConfig` to an OCI/Docker image config JSON, the
//! value basil attaches as `containerimage.config` in the gateway bridge
//! result metadata. Only the fields basil can emit are set; the exporter fills
//! history/diff_ids/rootfs from the layers.

use crate::emitter::ImageConfig;
use serde_json::{json, Map, Value};

/// The OCI image config JSON for a target's composed config + platform.
/// `rootfs`/`history` are left minimal; buildkit's exporter patches them from
/// the solved layers.
pub fn render(config: &ImageConfig, architecture: &str, os: &str) -> Value {
    let env: Vec<Value> = config.env.iter().cloned().map(Value::String).collect();
    let cmd = config.cmd.as_ref().map(|c| json!(c));
    let entrypoint = config.entrypoint.as_ref().map(|c| json!(c));
    let user = config.user.as_ref().map(|s| Value::String(s.clone()));
    let working_dir = if config.working_dir.is_empty() {
        "/".to_string()
    } else {
        config.working_dir.clone()
    };

    let mut labels = Map::new();
    for (k, v) in &config.labels {
        labels.insert(k.clone(), Value::String(v.clone()));
    }
    let labels = if labels.is_empty() {
        Value::Null
    } else {
        Value::Object(labels)
    };

    let mut exposed = Map::new();
    for p in &config.exposed_ports {
        exposed.insert(p.clone(), Value::Object(Map::new()));
    }
    let exposed = if exposed.is_empty() {
        Value::Null
    } else {
        Value::Object(exposed)
    };

    let mut volumes = Map::new();
    for v in &config.volumes {
        volumes.insert(v.clone(), Value::Object(Map::new()));
    }
    let volumes = if volumes.is_empty() {
        Value::Null
    } else {
        Value::Object(volumes)
    };

    let mut cfg = Map::new();
    cfg.insert("Env".to_string(), Value::Array(env));
    if let Some(c) = cmd {
        cfg.insert("Cmd".to_string(), c);
    }
    if let Some(e) = entrypoint {
        cfg.insert("Entrypoint".to_string(), e);
    }
    if let Some(u) = user {
        cfg.insert("User".to_string(), u);
    }
    cfg.insert("WorkingDir".to_string(), Value::String(working_dir));
    cfg.insert("Labels".to_string(), labels);
    cfg.insert("ExposedPorts".to_string(), exposed);
    cfg.insert("Volumes".to_string(), volumes);

    json!({
        "architecture": architecture,
        "os": os,
        "config": cfg,
        "rootfs": { "type": "layers", "diff_ids": [] },
        "history": []
    })
}

/// Serialize to compact JSON bytes (as stored in result metadata).
pub fn render_bytes(config: &ImageConfig, architecture: &str, os: &str) -> Vec<u8> {
    serde_json::to_vec(&render(config, architecture, os)).expect("config JSON is serializable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_core_fields() {
        let c = ImageConfig {
            env: vec![
                "PATH=/usr/bin".to_string(),
                "FOO=bar".to_string(),
            ],
            cmd: Some(vec!["/bin/app".to_string()]),
            entrypoint: None,
            user: Some("1000".to_string()),
            working_dir: "/app".to_string(),
            labels: vec![("a".to_string(), "b".to_string())],
            exposed_ports: vec!["8080/tcp".to_string()],
            volumes: vec!["/data".to_string()],
        };
        let v = render(&c, "amd64", "linux");
        assert_eq!(v["architecture"], "amd64");
        assert_eq!(v["os"], "linux");
        assert_eq!(v["config"]["WorkingDir"], "/app");
        assert_eq!(v["config"]["User"], "1000");
        assert_eq!(v["config"]["Env"][1], "FOO=bar");
        assert_eq!(v["config"]["Labels"]["a"], "b");
        assert_eq!(v["config"]["ExposedPorts"]["8080/tcp"], json!({}));
        assert_eq!(v["config"]["Volumes"]["/data"], json!({}));
        assert_eq!(v["config"]["Cmd"], json!(["/bin/app"]));
    }

    #[test]
    fn empty_optional_fields_are_null() {
        let c = ImageConfig::default();
        let v = render(&c, "arm64", "linux");
        assert!(v["config"]["Cmd"].is_null());
        assert!(v["config"]["Entrypoint"].is_null());
        assert!(v["config"]["User"].is_null());
        assert!(v["config"]["Labels"].is_null());
        // working dir defaults to "/"
        assert_eq!(v["config"]["WorkingDir"], "/");
    }
}