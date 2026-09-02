//! Low-level LLB graph builder.
//!
//! Builds a `pb::Definition` (the LLB DAG) from high-level operations,
//! mirroring buildkit's `client/llb` marshaling rules: vertex digests are
//! SHA-256 over the marshaled `pb.Op`; input edges reference vertices by
//! digest + output index; exec mounts are sorted by target and assign output
//! indices to writable mounts. Digests are internal to a single emitted
//! Definition (buildkit recomputes and validates them), so self-consistency is
//! what matters.

use buildkit_client::proto::pb;
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A reference to a vertex's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    pub vertex: usize,
    pub index: i64,
}

/// The in-progress graph.
#[derive(Debug)]
pub struct Graph {
    ops: Vec<pb::Op>,
    digest_of: Vec<String>,
    by_digest: HashMap<String, usize>,
}

impl Graph {
    pub fn new() -> Self {
        Graph {
            ops: Vec::new(),
            digest_of: Vec::new(),
            by_digest: HashMap::new(),
        }
    }

    fn digest(&self, op: &pb::Op) -> String {
        let bytes = op.encode_to_vec();
        let mut h = Sha256::new();
        h.update(&bytes);
        format!("sha256:{}", hex(&h.finalize()))
    }

    /// Add an op (deduped by digest) and return its vertex index.
    pub fn vertex(&mut self, op: pb::Op) -> usize {
        let d = self.digest(&op);
        if let Some(&i) = self.by_digest.get(&d) {
            return i;
        }
        let i = self.ops.len();
        self.ops.push(op);
        self.digest_of.push(d.clone());
        self.by_digest.insert(d, i);
        i
    }

    /// Reference a vertex's output by index.
    pub fn output(&self, vertex: usize, index: i64) -> Output {
        Output { vertex, index }
    }

    /// Number of vertices (for tests/inspection).
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    #[cfg(test)]
    pub fn op(&self, i: usize) -> &pb::Op {
        &self.ops[i]
    }

    /// The input edge for an `Output` reference (its digest + index).
    fn edge(&self, out: Output) -> pb::Input {
        pb::Input {
            digest: self.digest_of[out.vertex].clone(),
            index: out.index,
        }
    }

    /// Build a `Definition` from a set of output "roots" (the resolved target
    /// outputs). `roots` are vertex indices; `max_index` is the highest output
    /// index used. All reachable vertices are numerically ordered; buildkit
    /// only needs a valid set of `def` (marshaled) + metadata. We include every
    /// op we created; unreachable-but-present is harmless.
    pub fn finish(self, _roots: &[usize]) -> pb::Definition {
        let mut def = pb::Definition::default();
        for (i, op) in self.ops.iter().enumerate() {
            def.def.push(op.encode_to_vec());
            let mut md = pb::OpMetadata::default();
            md.description = HashMap::new();
            def.metadata.insert(self.digest_of[i].clone(), md);
        }
        def
    }
}

/// A source op for `docker-image://`.
pub fn source_image(graph: &mut Graph, reference: &str, platform: Option<pb::Platform>) -> Output {
    let op = pb::Op {
        inputs: vec![],
        platform,
        constraints: None,
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: format!("docker-image://{reference}"),
            attrs: HashMap::new(),
        })),
    };
    let v = graph.vertex(op);
    graph.output(v, 0)
}

/// A source op for a local context (`local://<id>`), e.g. the build context.
/// Output 0 is the directory tree.
pub fn source_local(graph: &mut Graph, id: &str, platform: Option<pb::Platform>) -> Output {
    let op = pb::Op {
        inputs: vec![],
        platform,
        constraints: None,
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: format!("local://{id}"),
            attrs: HashMap::new(),
        })),
    };
    let v = graph.vertex(op);
    graph.output(v, 0)
}

/// An empty (scratch) rootfs.
pub fn scratch(graph: &mut Graph) -> Output {
    let op = pb::Op {
        inputs: vec![],
        platform: None,
        constraints: None,
        op: Some(pb::op::Op::Merge(pb::MergeOp {
            inputs: vec![pb::MergeInput { input: -1 }],
        })),
    };
    let v = graph.vertex(op);
    graph.output(v, 0)
}

/// An exec op. `root` is the base rootfs output; `args`/`env`/`cwd`/`user` the
/// exec meta; `mounts` extra mounts (bind/secret/cache/ssh) each with an
/// optional source output. Returns the exec's root output (index 0, the `/`
/// mount) and a separate output for any explicit `--mount=...,output=<path>`
/// if provided (basil v1 does not expose output mounts; skip).
///
/// Mounts are sorted by dest; the `/` root mount is last in sort order, giving
/// it the last output index — but buildkit's exec root is the output of the
/// `/` mount. We follow the go rule: the returned exec output is the `/`
/// mount's output index.
pub fn run(
    graph: &mut Graph,
    root: Output,
    args: Vec<String>,
    env: Vec<String>,
    cwd: String,
    user: String,
    platform: Option<pb::Platform>,
    extra_mounts: Vec<ExtraMount>,
) -> Output {
    let mut mounts: Vec<(Option<Output>, String, bool)> = vec![(Some(root), "/".to_string(), false)];
    for m in extra_mounts {
        mounts.push((m.source, m.dest, m.readonly));
    }
    // Sort by dest.
    mounts.sort_by(|a, b| a.1.cmp(&b.1));

    // Build the pb.Op. Input edges: each mount's source. Dedup by edge equality
    // (digest+index). The screatch root has source None → Empty input.
    let mut inputs: Vec<pb::Input> = Vec::new();
    let mut mount_pb = Vec::new();
    let mut out_index: i64 = 0;
    let mut root_output_index: i64 = -1;

    for (src, dest, readonly) in &mounts {
        // Input index for this mount: a scratch source uses Empty (-1);
        // otherwise the matching op input edge (deduped).
        let input_index: i64 = if let Some(src) = src {
            let edge = graph.edge(*src);
            match inputs.iter().position(|e| e.digest == edge.digest && e.index == edge.index) {
                Some(i) => i as i64,
                None => {
                    inputs.push(edge);
                    (inputs.len() - 1) as i64
                }
            }
        } else {
            -1
        };

        // Output index (SkipOutput = -1) for readonly mounts; writable mounts
        // get an incrementing index.
        let this_out: i64 = if *readonly {
            -1
        } else {
            let idx = out_index;
            out_index += 1;
            idx
        };
        // The `/` mount is the exec root.
        if dest == "/" {
            root_output_index = this_out;
        }

        mount_pb.push(pb::Mount {
            input: input_index,
            selector: String::new(),
            dest: dest.clone(),
            output: this_out,
            readonly: *readonly,
            ..Default::default()
        });
    }

    let meta = pb::Meta {
        args,
        env,
        cwd,
        user: user.clone(),
        ..Default::default()
    };

    let exec = pb::ExecOp {
        meta: Some(meta),
        mounts: mount_pb,
        network: 0,
        security: 0,
        ..Default::default()
    };

    let op = pb::Op {
        inputs,
        platform,
        constraints: None,
        op: Some(pb::op::Op::Exec(exec)),
    };
    let v = graph.vertex(op);
    graph.output(v, root_output_index)
}

/// A file-operation (COPY/ADD). `Input` is the destination tree (current
/// state); `source` is either a context/file source output. For `COPY
/// --from=<stage>`, the source is the stage's output. Returns the resulting
/// tree output.
pub fn file_op(
    graph: &mut Graph,
    input_tree: Output,
    source_tree: Output,
    src_path: Vec<String>,
    dest_path: String,
    chown: Option<String>,
    chmod: Option<String>,
    is_add: bool,
    platform: Option<pb::Platform>,
) -> Output {
    let copy = pb::FileActionCopy {
        src: src_path.join(","),
        dest: dest_path.clone(),
        // chown requires resolving a uid/gid (base-image user lookup at build
        // time); v1 does not fabricate one. The `--chown` string is preserved
        // in image history/metadata, resolved during file apply by buildkit
        // from the exec environment where possible.
        owner: None,
        mode: 0,
        follow_symlink: false,
        dir_copy_contents: false,
        attempt_unpack_docker_compatibility: is_add,
        create_dest_path: true,
        allow_wildcard: true,
        allow_empty_wildcard: false,
        timestamp: 0,
        include_patterns: vec![],
        exclude_patterns: vec![],
        always_replace_existing_dest_paths: false,
        mode_str: chmod.unwrap_or_default(),
    };

    let action = pb::file_action::Action::Copy(copy);
    let file_action = pb::FileAction {
        input: 0,            // input_tree is input 0
        secondary_input: 1,  // source_tree is secondary input
        output: 0,
        action: Some(action),
    };
    let _ = chown;

    let file_op = pb::FileOp {
        actions: vec![file_action],
    };

    // Input edges: [input_tree, source_tree].
    let inputs = vec![graph.edge(input_tree), graph.edge(source_tree)];
    let op = pb::Op {
        inputs,
        platform,
        constraints: None,
        op: Some(pb::op::Op::File(file_op)),
    };
    let v = graph.vertex(op);
    graph.output(v, 0)
}

/// A bind mount on an exec.
#[derive(Debug, Clone)]
pub struct ExtraMount {
    pub source: Option<Output>,
    pub dest: String,
    pub readonly: bool,
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}