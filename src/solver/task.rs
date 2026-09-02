//! `DO`/`TASK` execution: run a task target's `TASK` command as a one-shot
//! container on its resolved base image, outside the LLB solve.
//!
//! Per the design: a task is the command itself, run once per matched event on
//! the target's base image (an external image or a solved stage). It is not a
//! buildkit ExecOp, never a layer, never apart of the image. Env: `BASIL_EVENT`
//! (raw event JSON), `BASIL_IMAGE` (resolved digest), `BASIL_REF_<TARGET>`.

use crate::gateway_proto::moby::buildkit::v1::frontend::llb_bridge_client::LlbBridgeClient;
use crate::gateway_proto::moby::buildkit::v1::frontend as gw;
use crate::gateway_proto::pb as gpb;

/// Run a task: start a container on `rootfs_ref` (a solved result) and run
/// `command`, streaming output, returning success on exit 0.
pub async fn run_task(
    bridge: &mut LlbBridgeClient<tonic::transport::Channel>,
    buildid: &str,
    container_id: &str,
    rootfs_ref: &str,
    command: &crate::dsl::ast::Command,
    env: Vec<(String, String)>,
) -> anyhow::Result<i32> {
    // Start the container with the task rootfs mounted at `/`.
    let root_mount = gpb::Mount {
        input: -1,
        dest: String::new(), // empty dest = the container's root filesystem
        output: -1,
        readonly: false,
        mount_type: 0,
        result_id: rootfs_ref.to_string(),
        ..Default::default()
    };
    let new_req = gw::NewContainerRequest {
        container_id: container_id.to_string(),
        mounts: vec![root_mount],
        network: 0,
        platform: None,
        constraints: None,
        extra_hosts: vec![],
        hostname: String::new(),
    };
    bridge
        .new_container(with_buildid(tonic::Request::new(new_req), buildid))
        .await
        .map_err(|e| anyhow::anyhow!("NewContainer failed: {e}"))?;

    // Build the command args.
    let args = match command {
        crate::dsl::ast::Command::Shell(s) => vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            s.clone(),
        ],
        crate::dsl::ast::Command::Exec(a) => a.clone(),
    };
    let env_arr: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();

    let meta = gpb::Meta {
        args,
        env: env_arr,
        ..Default::default()
    };
    let init = gw::InitMessage {
        container_id: container_id.to_string(),
        meta: Some(meta),
        fds: vec![0, 1, 2],
        tty: false,
        security: 0,
        secretenv: vec![],
    };
    let init_msg = gw::ExecMessage {
        process_id: "p1".to_string(),
        input: Some(gw::exec_message::Input::Init(init)),
    };

    // The exec stream: send Init, then read outgoing messages (stdout/stderr
    // and the exit). The outbound EOFs after Init; buildkit runs the process.
    let mut resp = bridge
        .exec_process(with_buildid(
            tonic::Request::new(tokio_stream::iter(vec![init_msg])),
            buildid,
        ))
        .await
        .map_err(|e| anyhow::anyhow!("ExecProcess failed: {e}"))?;
    let stream = resp.get_mut();
    let mut exit_code: Option<i32> = None;
    use futures_util::StreamExt;
    while let Some(msg) = stream.next().await {
        let msg = msg.map_err(|e| anyhow::anyhow!("exec stream error: {e}"))?;
        // The streamed item is the ExecMessage itself.
        match msg.input {
            Some(gw::exec_message::Input::File(fd)) => {
                let data = String::from_utf8_lossy(&fd.data).to_string();
                if fd.fd == 1 {
                    print!("{data}");
                } else {
                    eprint!("{data}");
                }
            }
            Some(gw::exec_message::Input::Exit(exit)) => {
                exit_code = Some(if exit.error.is_some() { 1 } else { exit.code as i32 });
            }
            Some(gw::exec_message::Input::Done(_)) => {
                break;
            }
            Some(gw::exec_message::Input::Started(_)) => {}
            _ => {}
        }
    }
    Ok(exit_code.unwrap_or(1))
}

fn with_buildid<T>(mut req: tonic::Request<T>, id: &str) -> tonic::Request<T> {
    use std::str::FromStr;
    req.metadata_mut().insert(
        "buildkit-controlapi-buildid",
        tonic::metadata::MetadataValue::from_str(id).expect("ascii buildid"),
    );
    req
}