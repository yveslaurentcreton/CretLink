//! Framing tests over `tokio::io::duplex` (ST-M001-003).

use std::time::Duration;

use cretlink_protocol::{Message, PROTOCOL_V1, ProtocolError, Role, limits};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

use super::{Framed, TransportError};

fn sample() -> Message {
    Message::Hello {
        protocol: PROTOCOL_V1,
        role: Role::Console,
        capabilities: vec!["exec".to_string()],
    }
}

#[tokio::test]
async fn roundtrip_preserves_the_message() {
    let (client, server) = duplex(4096);
    let mut sender = Framed::new(client);
    let mut receiver = Framed::new(server);

    sender.send(&sample()).await.unwrap();
    let received = receiver.recv().await.unwrap();

    assert_eq!(received, sample());
}

#[tokio::test]
async fn back_to_back_frames_are_received_in_order() {
    let (client, server) = duplex(4096);
    let mut sender = Framed::new(client);
    let mut receiver = Framed::new(server);

    let first = sample();
    let second = Message::Hello {
        protocol: PROTOCOL_V1,
        role: Role::Agent,
        capabilities: vec![],
    };
    sender.send(&first).await.unwrap();
    sender.send(&second).await.unwrap();

    assert_eq!(receiver.recv().await.unwrap(), first);
    assert_eq!(receiver.recv().await.unwrap(), second);
}

#[tokio::test]
async fn wire_format_is_u32_le_prefix_plus_json_payload() {
    let (client, mut server) = duplex(4096);
    let mut sender = Framed::new(client);
    sender.send(&sample()).await.unwrap();
    drop(sender);

    let mut raw = Vec::new();
    server.read_to_end(&mut raw).await.unwrap();

    let prefix: [u8; 4] = raw[..4].try_into().unwrap();
    let declared = u32::from_le_bytes(prefix) as usize;
    assert_eq!(declared, raw.len() - 4);
    assert_eq!(Message::decode(&raw[4..]).unwrap(), sample());
}

#[tokio::test]
async fn oversized_prefix_is_rejected_before_reading_a_payload() {
    let (mut client, server) = duplex(64);
    let mut receiver = Framed::new(server);

    let declared = u32::try_from(limits::MAX_FRAME_BYTES + 1).unwrap();
    client.write_all(&declared.to_le_bytes()).await.unwrap();
    client.flush().await.unwrap();

    // The writer stays open and never sends a payload: recv must reject on
    // the prefix alone. If it allocated and waited for the declared payload
    // it would block here forever.
    let error = receiver.recv().await.unwrap_err();
    assert!(matches!(
        error,
        TransportError::Oversized { len, max }
            if len == limits::MAX_FRAME_BYTES + 1 && max == limits::MAX_FRAME_BYTES
    ));
    drop(client);
}

#[tokio::test]
async fn truncated_frame_surfaces_as_eof() {
    let (mut client, server) = duplex(64);
    client.write_all(&8u32.to_le_bytes()).await.unwrap();
    client.write_all(b"abc").await.unwrap();
    drop(client);

    let mut receiver = Framed::new(server);
    assert!(matches!(
        receiver.recv().await.unwrap_err(),
        TransportError::Eof
    ));
}

#[tokio::test]
async fn clean_close_surfaces_as_eof() {
    let (client, server) = duplex(64);
    drop(client);

    let mut receiver = Framed::new(server);
    assert!(matches!(
        receiver.recv().await.unwrap_err(),
        TransportError::Eof
    ));
}

#[tokio::test]
async fn garbage_payload_surfaces_as_decode_error() {
    let (mut client, server) = duplex(256);
    let payload = b"this is not json";
    let declared = u32::try_from(payload.len()).unwrap();
    client.write_all(&declared.to_le_bytes()).await.unwrap();
    client.write_all(payload).await.unwrap();
    client.flush().await.unwrap();

    let mut receiver = Framed::new(server);
    assert!(matches!(
        receiver.recv().await.unwrap_err(),
        TransportError::Decode(ProtocolError::Malformed(_))
    ));
}

#[tokio::test]
async fn split_writes_still_parse() {
    let (mut client, server) = duplex(4096);
    let message = sample();
    let payload = message.encode().unwrap();
    let mut raw = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
    raw.extend_from_slice(&payload);

    let writer = tokio::spawn(async move {
        for byte in raw {
            client.write_all(&[byte]).await.unwrap();
            client.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
        client
    });

    let mut receiver = Framed::new(server);
    assert_eq!(receiver.recv().await.unwrap(), message);
    drop(writer.await.unwrap());
}

#[tokio::test]
async fn send_and_recv_work_on_split_halves() {
    // A read half only implements AsyncRead and a write half only
    // AsyncWrite: framing must work over each so callers can run a
    // dedicated reader and writer task on one stream.
    let (client, server) = duplex(4096);
    let (server_read, _server_write) = tokio::io::split(server);
    let (_client_read, client_write) = tokio::io::split(client);

    let mut sender = Framed::new(client_write);
    let mut receiver = Framed::new(server_read);

    sender.send(&sample()).await.unwrap();
    assert_eq!(receiver.recv().await.unwrap(), sample());
}

#[tokio::test]
async fn send_encoded_is_wire_identical_to_send() {
    let (client, server) = duplex(4096);
    let mut sender = Framed::new(client);
    let mut receiver = Framed::new(server);

    let payload = sample().encode().unwrap();
    sender.send_encoded(&payload).await.unwrap();
    assert_eq!(receiver.recv().await.unwrap(), sample());
}

#[tokio::test]
async fn send_encoded_rejects_an_oversized_payload_before_writing() {
    let (client, server) = duplex(64);
    let mut sender = Framed::new(client);

    let payload = vec![0u8; limits::MAX_FRAME_BYTES + 1];
    let error = sender.send_encoded(&payload).await.unwrap_err();
    assert!(matches!(error, TransportError::Oversized { .. }));

    // Nothing reached the stream: the peer sees a clean close, not a
    // partial frame.
    drop(sender);
    let mut receiver = Framed::new(server);
    assert!(matches!(
        receiver.recv().await.unwrap_err(),
        TransportError::Eof
    ));
}

#[tokio::test(start_paused = true)]
async fn recv_is_cancel_safe_across_dropped_futures() {
    let (mut client, server) = duplex(4096);
    let mut receiver = Framed::new(server);
    let message = sample();
    let payload = message.encode().unwrap();

    // Deliver the prefix plus half the payload, then cancel a recv
    // mid-frame via a timeout.
    let half = payload.len() / 2;
    client
        .write_all(&u32::try_from(payload.len()).unwrap().to_le_bytes())
        .await
        .unwrap();
    client.write_all(&payload[..half]).await.unwrap();
    client.flush().await.unwrap();

    let cancelled = tokio::time::timeout(Duration::from_millis(10), receiver.recv()).await;
    assert!(cancelled.is_err(), "recv must still be waiting mid-frame");

    // The rest of the frame arrives; a fresh recv must resume and parse.
    client.write_all(&payload[half..]).await.unwrap();
    client.flush().await.unwrap();
    assert_eq!(receiver.recv().await.unwrap(), message);
}
