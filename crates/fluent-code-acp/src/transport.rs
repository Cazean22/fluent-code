#![cfg_attr(not(test), allow(dead_code))]

#[cfg(test)]
use std::io;

#[cfg(test)]
use std::io::{BufRead, Write};

#[cfg(test)]
use serde::Serialize;
#[cfg(test)]
use thiserror::Error;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StdioTransport;

impl StdioTransport {
    pub const fn new() -> Self {
        Self
    }

    pub const fn kind(self) -> &'static str {
        "stdio"
    }

    #[cfg(test)]
    pub fn read_frame<R: BufRead>(
        self,
        reader: &mut R,
    ) -> Result<Option<String>, StdioTransportError> {
        let mut frame_bytes = Vec::new();
        let bytes_read = reader
            .read_until(b'\n', &mut frame_bytes)
            .map_err(StdioTransportError::Read)?;

        if bytes_read == 0 {
            return Ok(None);
        }

        if !matches!(frame_bytes.last(), Some(b'\n')) {
            return Err(StdioTransportError::UnterminatedFrame);
        }

        let frame = String::from_utf8(frame_bytes).map_err(StdioTransportError::NonUtf8)?;
        let frame = frame
            .strip_suffix('\n')
            .expect("frame bytes ending in newline always strip one newline");
        let frame = frame.strip_suffix('\r').unwrap_or(frame);

        if frame.is_empty() {
            return Err(StdioTransportError::EmptyFrame);
        }

        if frame.contains(['\n', '\r']) {
            return Err(StdioTransportError::MultilineFrame);
        }

        Ok(Some(frame.to_string()))
    }

    #[cfg(test)]
    pub fn serialize_frame<T: Serialize>(
        self,
        message: &T,
    ) -> Result<Vec<u8>, StdioTransportError> {
        let serialized_frame =
            serde_json::to_vec(message).map_err(StdioTransportError::Serialize)?;

        if serialized_frame.contains(&b'\n') || serialized_frame.contains(&b'\r') {
            return Err(StdioTransportError::MultilineFrame);
        }

        let mut framed_output = serialized_frame;
        framed_output.push(b'\n');
        Ok(framed_output)
    }

    #[cfg(test)]
    pub fn write_frame<W: Write, T: Serialize>(
        self,
        writer: &mut W,
        message: &T,
    ) -> Result<(), StdioTransportError> {
        let frame = self.serialize_frame(message)?;
        writer
            .write_all(&frame)
            .and_then(|()| writer.flush())
            .map_err(StdioTransportError::Write)
    }
}

#[cfg(test)]
#[derive(Debug, Error)]
pub enum StdioTransportError {
    #[error("failed to read stdio frame")]
    Read(#[source] io::Error),
    #[error("stdio frame must be UTF-8")]
    NonUtf8(#[source] std::string::FromUtf8Error),
    #[error("stdio frame must end with a newline delimiter")]
    UnterminatedFrame,
    #[error("stdio frame must contain exactly one JSON-RPC line")]
    MultilineFrame,
    #[error("stdio frame must not be empty")]
    EmptyFrame,
    #[error("failed to serialize JSON-RPC frame")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to write stdio frame")]
    Write(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::StdioTransport;
    use crate::protocol::{JsonRpcProtocol, JsonRpcRequest, JsonRpcResponse, Method};

    #[test]
    fn stdio_transport_reports_stdio_kind() {
        assert_eq!(StdioTransport::new().kind(), "stdio");
    }

    #[test]
    fn valid_jsonrpc_frame_round_trip() {
        let transport = StdioTransport::new();
        let request = JsonRpcRequest::new(7, Method::Initialize, json!({ "protocolVersion": 1 }));
        let serialized_frame = transport.serialize_frame(&request).unwrap();

        assert_eq!(
            serialized_frame
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1
        );
        assert!(serialized_frame.ends_with(b"\n"));

        let mut reader = Cursor::new(serialized_frame);
        let frame = transport.read_frame(&mut reader).unwrap().unwrap();
        let parsed_request = JsonRpcProtocol::new().parse_request(&frame).unwrap();

        assert_eq!(parsed_request.method, Method::Initialize);
        assert_eq!(parsed_request.params, json!({ "protocolVersion": 1 }));
    }

    #[test]
    fn non_utf8_frame_is_rejected() {
        let transport = StdioTransport::new();
        let mut reader = Cursor::new(vec![0xFF, b'\n']);

        let error = transport.read_frame(&mut reader).unwrap_err();

        assert_eq!(error.to_string(), "stdio frame must be UTF-8");
    }

    #[test]
    fn unterminated_frame_is_rejected() {
        let transport = StdioTransport::new();
        let mut reader = Cursor::new(br#"{"jsonrpc":"2.0"}"#.to_vec());

        let error = transport.read_frame(&mut reader).unwrap_err();

        assert_eq!(
            error.to_string(),
            "stdio frame must end with a newline delimiter"
        );
    }

    #[test]
    fn write_frame_emits_only_the_jsonrpc_line() {
        let transport = StdioTransport::new();
        let response = JsonRpcResponse::new(5, json!({ "ready": true }));
        let expected_output = transport.serialize_frame(&response).unwrap();
        let mut output = Vec::new();

        transport.write_frame(&mut output, &response).unwrap();

        assert_eq!(output, expected_output);
        assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
    }
}
