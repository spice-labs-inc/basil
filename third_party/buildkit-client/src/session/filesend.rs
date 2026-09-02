//! FileSend export handling: receive files the server (buildkit) pushes back
//! to the client over the `FileSend` service (buildkit `session/filesync`).
//!
//! Buildkit's `tar` exporter streams the resulting archive to the client as
//! gRPC-framed `BytesMessage` messages over `FileSend/DiffCopy`. This handler
//! decodes each message's `data` bytes and writes them to the destination file.

use crate::error::{Error, Result};
use crate::proto::moby::filesync::v1::BytesMessage;
use bytes::Bytes;
use h2::server::SendResponse;
use http::Response;
use prost::Message as _;
use std::io::Write;
use std::path::PathBuf;

/// Accumulates received export bytes to a destination file.
#[derive(Debug, Clone, Default)]
pub struct FileSendSink {
    dest: PathBuf,
}

impl FileSendSink {
    pub fn new(dest: impl Into<PathBuf>) -> Self {
        FileSendSink { dest: dest.into() }
    }
}

/// Handle a FileSend/DiffCopy stream: read gRPC-framed `BytesMessage` payloads
/// from buildkit and write their concatenated `data` to the sink destination.
pub(super) async fn handle_file_send_stream(
    sink: &FileSendSink,
    mut request_stream: h2::RecvStream,
    mut respond: SendResponse<Bytes>,
) -> Result<()> {
    // Respond with gRPC headers, trailers-only (no response body expected).
    let response = Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .body(())
        .unwrap();
    respond
        .send_response(response, true)
        .map_err(|e| Error::Http2Stream { source: e })?;

    // gRPC server streams carry length-prefixed messages; each is a
    // `BytesMessage`. Concatenate the `data` payloads.
    let mut out = std::fs::File::create(&sink.dest).map_err(Error::Io)?;
    let mut buf = Vec::new();
    while let Some(chunk) = request_stream.data().await {
        let chunk = chunk.map_err(|e| Error::Http2Stream { source: e })?;
        buf.extend_from_slice(&chunk);
        // gRPC frames may split a message across flows; parse any complete
        // length-prefixed messages from the buffer.
        while buf.len() >= 5 {
            let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
            if buf.len() < 5 + len {
                break;
            }
            let msg_bytes = &buf[5..5 + len];
            match BytesMessage::decode(msg_bytes) {
                Ok(msg) => out.write_all(&msg.data).map_err(Error::Io)?,
                Err(_) => break, // partial; wait for more
            }
            buf.drain(..5 + len);
        }
    }

    // Unparsed remainder (malformed trailing bytes): append as-is so a valid
    // tar still lands on disk.
    out.write_all(&buf).map_err(Error::Io)?;
    out.sync_all().map_err(Error::Io)?;
    Ok(())
}