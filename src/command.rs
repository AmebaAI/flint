use std::io;

use async_stream::stream;
use aws_smithy_eventstream::frame::write_message_to;
use aws_smithy_types::{
    event_stream::{Header, HeaderValue, Message},
    str_bytes::StrBytes,
};
use axum::body::{Body, Bytes};
use serde_json::json;

use crate::session::{CommandEvent, SessionCommandExecution};

pub(crate) fn event_stream_body(mut execution: SessionCommandExecution) -> Body {
    let events = stream! {
        let mut stdout = Utf8Stream::default();
        let mut stderr = Utf8Stream::default();
        yield encode_chunk(json!({"contentStart": {}}));
        loop {
            match execution.recv().await {
                Some(Ok(CommandEvent::Stdout(output))) => {
                    if let Some(output) = stdout.push(&output) {
                        yield encode_chunk(json!({"contentDelta": {"stdout": output}}));
                    }
                }
                Some(Ok(CommandEvent::Stderr(output))) => {
                    if let Some(output) = stderr.push(&output) {
                        yield encode_chunk(json!({"contentDelta": {"stderr": output}}));
                    }
                }
                Some(Ok(CommandEvent::Exited(exit_code))) => {
                    if let Some(output) = stdout.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stdout": output}}));
                    }
                    if let Some(output) = stderr.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stderr": output}}));
                    }
                    let exit_code = i32::try_from(exit_code).unwrap_or(-1);
                    yield encode_chunk(json!({
                        "contentStop": {"exitCode": exit_code, "status": "COMPLETED"}
                    }));
                    break;
                }
                Some(Ok(CommandEvent::TimedOut)) => {
                    if let Some(output) = stdout.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stdout": output}}));
                    }
                    if let Some(output) = stderr.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stderr": output}}));
                    }
                    yield encode_chunk(json!({
                        "contentStop": {"exitCode": -1, "status": "TIMED_OUT"}
                    }));
                    break;
                }
                Some(Ok(CommandEvent::Cancelled)) => {
                    if let Some(output) = stdout.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stdout": output}}));
                    }
                    if let Some(output) = stderr.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stderr": output}}));
                    }
                    yield encode_exception("runtimeClientError", "runtime command was cancelled");
                    break;
                }
                Some(Err(_)) => {
                    if let Some(output) = stdout.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stdout": output}}));
                    }
                    if let Some(output) = stderr.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stderr": output}}));
                    }
                    yield encode_exception("runtimeClientError", "runtime command execution failed");
                    break;
                }
                None => {
                    if let Some(output) = stdout.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stdout": output}}));
                    }
                    if let Some(output) = stderr.finish() {
                        yield encode_chunk(json!({"contentDelta": {"stderr": output}}));
                    }
                    yield encode_exception(
                        "internalServerException",
                        "runtime command ended without a completion event",
                    );
                    break;
                }
            }
        }
    };
    Body::from_stream(events)
}

#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(_) => self.take_valid(),
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                if valid == 0 {
                    None
                } else {
                    Some(
                        String::from_utf8(self.pending.drain(..valid).collect())
                            .expect("validated UTF-8 prefix"),
                    )
                }
            }
            Err(_) => self.finish(),
        }
    }

    fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned())
        }
    }

    fn take_valid(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            None
        } else {
            Some(String::from_utf8(std::mem::take(&mut self.pending)).expect("validated UTF-8"))
        }
    }
}

fn encode_chunk(payload: serde_json::Value) -> Result<Bytes, io::Error> {
    encode_message("event", ":event-type", "chunk", payload)
}

fn encode_exception(exception_type: &str, message: &str) -> Result<Bytes, io::Error> {
    encode_message(
        "exception",
        ":exception-type",
        exception_type,
        json!({"message": message}),
    )
}

fn encode_message(
    message_type: &str,
    smithy_header: &str,
    smithy_type: &str,
    payload: serde_json::Value,
) -> Result<Bytes, io::Error> {
    let payload = serde_json::to_vec(&payload).map_err(io::Error::other)?;
    let headers = vec![
        Header::new(
            ":message-type",
            HeaderValue::String(StrBytes::copy_from_str(message_type)),
        ),
        Header::new(
            StrBytes::copy_from_str(smithy_header),
            HeaderValue::String(StrBytes::copy_from_str(smithy_type)),
        ),
        Header::new(
            ":content-type",
            HeaderValue::String(StrBytes::copy_from_str("application/json")),
        ),
    ];
    let message = Message::new_from_parts(headers, payload);
    let mut frame = Vec::new();
    write_message_to(&message, &mut frame).map_err(io::Error::other)?;
    Ok(Bytes::from(frame))
}

#[cfg(test)]
mod tests {
    use aws_smithy_eventstream::{frame::read_message_from, smithy::parse_response_headers};
    use serde_json::Value;

    use super::{Utf8Stream, encode_chunk, encode_exception};

    #[test]
    fn command_chunks_use_aws_event_stream_framing() {
        let encoded =
            encode_chunk(serde_json::json!({"contentStart": {}})).expect("encode command event");
        let message = read_message_from(encoded).expect("decode event-stream frame");
        let headers = parse_response_headers(&message).expect("parse Smithy headers");
        assert_eq!(headers.message_type.as_str(), "event");
        assert_eq!(headers.smithy_type.as_str(), "chunk");
        assert_eq!(
            headers.content_type.map(|value| value.as_str()),
            Some("application/json")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(message.payload()).expect("event payload"),
            serde_json::json!({"contentStart": {}}),
        );
    }

    #[test]
    fn command_exceptions_are_valid_aws_event_stream_frames() {
        let encoded = encode_exception("runtimeClientError", "runtime command execution failed")
            .expect("encode command exception");
        let message = read_message_from(encoded).expect("decode exception frame");
        let headers = parse_response_headers(&message).expect("parse exception headers");
        assert_eq!(headers.message_type.as_str(), "exception");
        assert_eq!(headers.smithy_type.as_str(), "runtimeClientError");
        assert_eq!(
            serde_json::from_slice::<Value>(message.payload()).expect("exception payload"),
            serde_json::json!({"message": "runtime command execution failed"}),
        );
    }

    #[test]
    fn utf8_decoder_preserves_characters_split_across_docker_frames() {
        let mut decoder = Utf8Stream::default();
        assert_eq!(decoder.push(&[0xe2, 0x82]), None);
        assert_eq!(decoder.push(&[0xac, b'!']), Some("€!".to_owned()));
        assert_eq!(decoder.finish(), None);

        assert_eq!(decoder.push(&[0xe2, 0x82]), None);
        assert_eq!(decoder.finish(), Some("�".to_owned()));
    }
}
