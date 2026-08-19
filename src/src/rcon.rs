use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Client -> server: authenticate.
const TYPE_AUTH: i32 = 3;
/// Client -> server: run a command. Also the type the server uses to
/// acknowledge a successful auth.
const TYPE_COMMAND: i32 = 2;
/// Server -> client: command output.
const TYPE_RESPONSE: i32 = 0;

/// The server signals a rejected password by echoing this request id.
const AUTH_FAILED: i32 = -1;

/// Guards against a malicious or malfunctioning peer announcing a huge frame.
const MAX_PACKET: i32 = 4096;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for further packets once a response has been received.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub enum RconError {
    NotConfigured,
    Connect(String),
    Auth,
    Protocol(String),
    Io(String),
}

impl std::fmt::Display for RconError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RconError::NotConfigured => write!(f, "RCON is not configured"),
            RconError::Connect(e) => write!(f, "could not reach the server: {e}"),
            RconError::Auth => write!(f, "RCON authentication failed"),
            RconError::Protocol(e) => write!(f, "unexpected RCON response: {e}"),
            RconError::Io(e) => write!(f, "RCON I/O error: {e}"),
        }
    }
}

impl std::error::Error for RconError {}

/// Encode one RCON packet.
pub fn encode_packet(request_id: i32, packet_type: i32, body: &str) -> Vec<u8> {
    // request_id + type + body + two trailing NULs.
    let length = 4 + 4 + body.len() as i32 + 2;
    let mut out = Vec::with_capacity(length as usize + 4);
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&packet_type.to_le_bytes());
    out.extend_from_slice(body.as_bytes());
    out.push(0);
    out.push(0);
    out
}

/// Decode the `request_id`, `type` and body of a packet payload — that is, the
/// bytes following the length prefix.
pub fn decode_payload(payload: &[u8]) -> Result<(i32, i32, String), RconError> {
    if payload.len() < 10 {
        return Err(RconError::Protocol(format!(
            "packet of {} bytes is shorter than the 10-byte minimum",
            payload.len()
        )));
    }
    let request_id = i32::from_le_bytes(payload[0..4].try_into().unwrap());
    let packet_type = i32::from_le_bytes(payload[4..8].try_into().unwrap());

    // Body runs to the first NUL; the final byte is padding.
    let body_bytes = &payload[8..payload.len() - 1];
    let end = body_bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(body_bytes.len());
    let body = String::from_utf8_lossy(&body_bytes[..end]).into_owned();

    Ok((request_id, packet_type, body))
}

/// Connect, authenticate, run one command and return its output.
pub async fn execute(address: &str, password: &str, command: &str) -> Result<String, RconError> {
    if password.is_empty() {
        return Err(RconError::NotConfigured);
    }

    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| RconError::Connect("timed out".to_string()))?
        .map_err(|e| RconError::Connect(e.to_string()))?;

    // Auth and command use distinct ids so a stray packet cannot be mistaken
    // for the reply we are waiting on.
    const AUTH_ID: i32 = 1;
    const COMMAND_ID: i32 = 2;

    write_packet(&mut stream, AUTH_ID, TYPE_AUTH, password).await?;

    // Some servers emit an empty RESPONSE_VALUE before the auth result; the
    // auth outcome is the first packet of type COMMAND.
    loop {
        let (request_id, packet_type, _) = read_packet(&mut stream).await?;
        if packet_type != TYPE_COMMAND {
            continue;
        }
        if request_id == AUTH_FAILED {
            return Err(RconError::Auth);
        }
        break;
    }

    write_packet(&mut stream, COMMAND_ID, TYPE_COMMAND, command).await?;

    let (_, _, mut output) = read_packet(&mut stream).await?;

    // Responses over ~4KB arrive split with no end marker, so collect whatever
    // follows within a short window and treat silence as the end.
    while let Ok(Ok((_, packet_type, chunk))) =
        tokio::time::timeout(DRAIN_TIMEOUT, read_packet(&mut stream)).await
    {
        if packet_type != TYPE_RESPONSE {
            break;
        }
        output.push_str(&chunk);
    }

    Ok(output)
}

async fn write_packet(
    stream: &mut TcpStream,
    request_id: i32,
    packet_type: i32,
    body: &str,
) -> Result<(), RconError> {
    let packet = encode_packet(request_id, packet_type, body);
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(&packet))
        .await
        .map_err(|_| RconError::Io("write timed out".to_string()))?
        .map_err(|e| RconError::Io(e.to_string()))
}

async fn read_packet(stream: &mut TcpStream) -> Result<(i32, i32, String), RconError> {
    let mut length_bytes = [0u8; 4];
    read_exact(stream, &mut length_bytes).await?;
    let length = i32::from_le_bytes(length_bytes);

    if !(10..=MAX_PACKET).contains(&length) {
        return Err(RconError::Protocol(format!(
            "declared packet length {length} is out of range"
        )));
    }

    let mut payload = vec![0u8; length as usize];
    read_exact(stream, &mut payload).await?;
    decode_payload(&payload)
}

async fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), RconError> {
    match tokio::time::timeout(IO_TIMEOUT, stream.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
            Err(RconError::Protocol("connection closed early".to_string()))
        }
        Ok(Err(e)) => Err(RconError::Io(e.to_string())),
        Err(_) => Err(RconError::Io("read timed out".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_documented_frame_layout() {
        let packet = encode_packet(7, TYPE_COMMAND, "list");

        // 4 (id) + 4 (type) + 4 (body) + 2 (NULs) = 14, plus the length prefix.
        assert_eq!(&packet[0..4], &14i32.to_le_bytes());
        assert_eq!(&packet[4..8], &7i32.to_le_bytes());
        assert_eq!(&packet[8..12], &2i32.to_le_bytes());
        assert_eq!(&packet[12..16], b"list");
        assert_eq!(&packet[16..18], &[0, 0]);
        assert_eq!(packet.len(), 18);
    }

    #[test]
    fn length_prefix_excludes_itself() {
        let packet = encode_packet(1, TYPE_AUTH, "hunter2");
        let declared = i32::from_le_bytes(packet[0..4].try_into().unwrap());
        assert_eq!(declared as usize, packet.len() - 4);
    }

    #[test]
    fn encode_decode_round_trips() {
        let packet = encode_packet(42, TYPE_RESPONSE, "There are 3 players online");
        let (id, kind, body) = decode_payload(&packet[4..]).unwrap();
        assert_eq!(id, 42);
        assert_eq!(kind, TYPE_RESPONSE);
        assert_eq!(body, "There are 3 players online");
    }

    #[test]
    fn decodes_an_empty_body() {
        let packet = encode_packet(1, TYPE_COMMAND, "");
        let (id, kind, body) = decode_payload(&packet[4..]).unwrap();
        assert_eq!((id, kind, body.as_str()), (1, TYPE_COMMAND, ""));
    }

    #[test]
    fn rejects_a_truncated_payload() {
        assert!(matches!(
            decode_payload(&[0, 0, 0]),
            Err(RconError::Protocol(_))
        ));
    }

    #[test]
    fn auth_failure_is_signalled_by_request_id() {
        // What the server sends when the password is wrong.
        let packet = encode_packet(AUTH_FAILED, TYPE_COMMAND, "");
        let (id, kind, _) = decode_payload(&packet[4..]).unwrap();
        assert_eq!(id, AUTH_FAILED);
        assert_eq!(kind, TYPE_COMMAND);
    }

    #[tokio::test]
    async fn empty_password_is_refused_without_dialing() {
        // Port 1 would refuse instantly anyway, but the point is that the
        // configuration check happens before any network access.
        let err = execute("127.0.0.1:1", "", "list").await.unwrap_err();
        assert!(matches!(err, RconError::NotConfigured));
    }

    /// Drives the real client against a scripted server, which is the only way
    /// to cover the auth handshake and multi-packet drain end to end.
    #[tokio::test]
    async fn executes_a_command_against_a_scripted_server() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // Auth request.
            let (id, kind, body) = read_one(&mut socket).await;
            assert_eq!(kind, TYPE_AUTH);
            assert_eq!(body, "s3cret");
            socket
                .write_all(&encode_packet(id, TYPE_COMMAND, ""))
                .await
                .unwrap();

            // Command request, answered in two chunks to exercise the drain.
            let (id, kind, body) = read_one(&mut socket).await;
            assert_eq!(kind, TYPE_COMMAND);
            assert_eq!(body, "list");
            socket
                .write_all(&encode_packet(id, TYPE_RESPONSE, "There are 2 "))
                .await
                .unwrap();
            socket
                .write_all(&encode_packet(id, TYPE_RESPONSE, "players online"))
                .await
                .unwrap();
        });

        let output = execute(&addr, "s3cret", "list").await.unwrap();
        assert_eq!(output, "There are 2 players online");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn surfaces_a_rejected_password() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_one(&mut socket).await;
            socket
                .write_all(&encode_packet(AUTH_FAILED, TYPE_COMMAND, ""))
                .await
                .unwrap();
        });

        assert!(matches!(
            execute(&addr, "wrong", "list").await,
            Err(RconError::Auth)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refuses_an_absurd_declared_length() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_one(&mut socket).await;
            // A hostile peer claiming a 1GB frame must not cause a 1GB alloc.
            socket.write_all(&1_000_000_000i32.to_le_bytes()).await.ok();
            // Hold the connection open so the client fails on the length check
            // rather than on the socket closing.
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        assert!(matches!(
            execute(&addr, "pw", "list").await,
            Err(RconError::Protocol(_))
        ));
        server.abort();
    }

    async fn read_one(socket: &mut TcpStream) -> (i32, i32, String) {
        let mut len = [0u8; 4];
        socket.read_exact(&mut len).await.unwrap();
        let mut payload = vec![0u8; i32::from_le_bytes(len) as usize];
        socket.read_exact(&mut payload).await.unwrap();
        decode_payload(&payload).unwrap()
    }
}
