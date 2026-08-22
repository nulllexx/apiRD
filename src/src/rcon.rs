//! Minimal RCON client for the Minecraft server.
//!
//! Replaces shelling out to `docker exec … rcon-cli`, which required handing
//! the API container the Docker socket — root-equivalent access to the host.
//! Talking RCON directly scopes command execution to the game server itself.
//!
//! Protocol (Source RCON): every packet is
//! `i32 length | i32 request_id | i32 type | body NUL | NUL`, all little-endian,
//! where `length` counts everything after itself.

use std::io;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

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

/// How long to wait for a command that is expected to block the server thread.
///
/// [`IO_TIMEOUT`] is sized for commands that answer immediately, which is
/// almost all of them. `save-all flush` is the exception: it writes every
/// loaded chunk and every player synchronously, and on a large modded world
/// that is comfortably longer than five seconds. Giving up early would not stop
/// the save — it runs to completion server-side either way — but it would leave
/// the caller unable to tell when it had finished, which is the one thing the
/// caller actually needs to know before shutting the server down.
pub const SLOW_COMMAND_TIMEOUT: Duration = Duration::from_secs(180);
/// How long to wait for further packets once a response has been received.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(300);

/// Minecraft splits an RCON response into 4096-byte chunks, so a first packet
/// comfortably under that cannot have been split. Checking this lets the common
/// case return immediately instead of always paying the drain window.
const SPLIT_THRESHOLD: usize = 4000;

/// Upper bound on packets discarded while looking for the reply to the current
/// request, so a confused peer cannot keep us reading forever.
const MAX_SKIPPED_PACKETS: u32 = 16;

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

impl RconError {
    /// Message for an operator looking at the console.
    ///
    /// Kept next to the error rather than at each call site so the two places
    /// that surface RCON failures — the command box and the stats probe —
    /// cannot drift into describing the same failure differently.
    pub fn user_message(&self) -> String {
        match self {
            // Distinguished from a transient failure so an operator can tell
            // "nobody set RCON_PASSWORD" from "the server is down".
            RconError::NotConfigured => "RCON is not configured on this server",
            RconError::Auth => "RCON password is incorrect",
            RconError::Connect(_) => "The server is not reachable — is it running?",
            _ => "The server did not respond",
        }
        .to_string()
    }
}

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

/// A long-lived, authenticated RCON connection.
///
/// The connection is held open rather than dialled per command. Minecraft logs
/// a client thread starting and shutting down for *every* connection, and those
/// lines land in `latest.log` — which the admin console streams — so a
/// connect-per-command design has the console spamming itself two lines deep
/// each time an operator runs anything.
///
/// The trade is state that can go stale: the server restarts, the socket dies
/// while idle, and the next command inherits a dead connection. That is what
/// the single retry in [`RconClient::execute`] exists for.
pub struct RconClient {
    address: String,
    password: String,
    /// RCON is one request/response at a time per socket, so concurrent
    /// commands have to serialize rather than interleave on the same stream.
    conn: Mutex<Option<TcpStream>>,
    next_id: AtomicI32,
}

impl RconClient {
    pub fn new(address: String, password: String) -> Arc<Self> {
        Arc::new(Self {
            address,
            password,
            conn: Mutex::new(None),
            next_id: AtomicI32::new(10),
        })
    }

    /// Whether a password is configured at all. Without one the console still
    /// streams logs; only the command box is unusable.
    pub fn is_configured(&self) -> bool {
        !self.password.is_empty()
    }

    /// Run one command, reusing the open connection when there is one.
    ///
    /// Attempts at most twice. The first attempt can fail because a connection
    /// that was healthy when stored died while idle — a server restart being
    /// the obvious way — and reconnecting resolves that transparently. A second
    /// failure is reported rather than retried, so a genuinely broken server
    /// does not turn one command into an unbounded reconnect loop.
    pub async fn execute(&self, command: &str) -> Result<String, RconError> {
        self.execute_within(command, IO_TIMEOUT).await
    }

    /// Run one command, allowing longer than usual for the reply.
    ///
    /// Only the wait for the *response* is extended; connecting and writing
    /// keep the ordinary timeouts, because neither gets slower just because the
    /// command will. See [`SLOW_COMMAND_TIMEOUT`].
    pub async fn execute_within(
        &self,
        command: &str,
        read_timeout: Duration,
    ) -> Result<String, RconError> {
        if !self.is_configured() {
            return Err(RconError::NotConfigured);
        }

        let mut held = self.conn.lock().await;
        let mut last_err: Option<RconError> = None;

        for _attempt in 0..2 {
            if held.is_none() {
                match connect_and_auth(&self.address, &self.password).await {
                    Ok(stream) => *held = Some(stream),
                    // Neither a rejected password nor a refused connection
                    // becomes true on an immediate retry.
                    Err(e) => return Err(e),
                }
            }

            let stream = held.as_mut().expect("connected just above");
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);

            match run_command(stream, id, command, read_timeout).await {
                Ok(output) => return Ok(output),
                Err(e) => {
                    // However this failed, the socket now sits at an unknown
                    // point in the protocol. Drop it rather than leave a
                    // half-consumed response for the next command to read.
                    *held = None;
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| RconError::Io("command failed".to_string())))
    }
}

async fn connect_and_auth(address: &str, password: &str) -> Result<TcpStream, RconError> {
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| RconError::Connect("timed out".to_string()))?
        .map_err(|e| RconError::Connect(e.to_string()))?;

    // Small, latency-sensitive packets; Nagle would sit on them.
    let _ = stream.set_nodelay(true);

    const AUTH_ID: i32 = 1;
    write_packet(&mut stream, AUTH_ID, TYPE_AUTH, password).await?;

    // Some servers emit an empty RESPONSE_VALUE before the auth result; the
    // auth outcome is the first packet of type COMMAND.
    loop {
        let (request_id, packet_type, _) = read_packet(&mut stream, IO_TIMEOUT).await?;
        if packet_type != TYPE_COMMAND {
            continue;
        }
        if request_id == AUTH_FAILED {
            return Err(RconError::Auth);
        }
        break;
    }

    Ok(stream)
}

async fn run_command(
    stream: &mut TcpStream,
    id: i32,
    command: &str,
    read_timeout: Duration,
) -> Result<String, RconError> {
    write_packet(stream, id, TYPE_COMMAND, command).await?;

    // Skip anything that is not the reply to *this* request. On a reused
    // connection an earlier command that timed out can leave its response
    // sitting in the socket, and returning that would show an operator the
    // answer to a question they did not ask.
    let mut output = String::new();
    let mut skipped = 0;
    loop {
        let (request_id, _, body) = read_packet(stream, read_timeout).await?;
        if request_id == id {
            output.push_str(&body);
            break;
        }
        skipped += 1;
        if skipped > MAX_SKIPPED_PACKETS {
            return Err(RconError::Protocol(
                "no response matching the request id".to_string(),
            ));
        }
    }

    // Only a response big enough to have been split is worth waiting on;
    // otherwise every command would pay the drain window in latency.
    if output.len() >= SPLIT_THRESHOLD {
        while let Ok(Ok((request_id, packet_type, chunk))) =
            tokio::time::timeout(DRAIN_TIMEOUT, read_packet(stream, IO_TIMEOUT)).await
        {
            if request_id != id || packet_type != TYPE_RESPONSE {
                break;
            }
            output.push_str(&chunk);
        }
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

async fn read_packet(
    stream: &mut TcpStream,
    read_timeout: Duration,
) -> Result<(i32, i32, String), RconError> {
    let mut length_bytes = [0u8; 4];
    read_exact(stream, &mut length_bytes, read_timeout).await?;
    let length = i32::from_le_bytes(length_bytes);

    if !(10..=MAX_PACKET).contains(&length) {
        return Err(RconError::Protocol(format!(
            "declared packet length {length} is out of range"
        )));
    }

    let mut payload = vec![0u8; length as usize];
    read_exact(stream, &mut payload, IO_TIMEOUT).await?;
    decode_payload(&payload)
}

async fn read_exact(
    stream: &mut TcpStream,
    buf: &mut [u8],
    read_timeout: Duration,
) -> Result<(), RconError> {
    match tokio::time::timeout(read_timeout, stream.read_exact(buf)).await {
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

    /// The slow timeout has to be longer than the ordinary one by enough to
    /// matter, and longer than the grace period Docker gives the container --
    /// otherwise the save this exists to wait for is still running when the
    /// server is killed, which is the whole failure being fixed.
    #[test]
    fn the_slow_command_timeout_outlasts_a_shutdown() {
        assert!(
            super::SLOW_COMMAND_TIMEOUT > super::IO_TIMEOUT,
            "a slow command needs longer than an ordinary one"
        );
        assert!(
            super::SLOW_COMMAND_TIMEOUT >= std::time::Duration::from_secs(180),
            "must cover the 180s stop_grace_period in docker-compose.yml"
        );
    }
    use super::*;
    use tokio::net::TcpListener;

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
        // Port 1 would refuse instantly anyway; the point is that the
        // configuration check happens before any network access.
        let client = RconClient::new("127.0.0.1:1".to_string(), String::new());
        assert!(!client.is_configured());
        assert!(matches!(
            client.execute("list").await,
            Err(RconError::NotConfigured)
        ));
    }

    async fn read_one(socket: &mut TcpStream) -> (i32, i32, String) {
        let mut len = [0u8; 4];
        socket.read_exact(&mut len).await.unwrap();
        let mut payload = vec![0u8; i32::from_le_bytes(len) as usize];
        socket.read_exact(&mut payload).await.unwrap();
        decode_payload(&payload).unwrap()
    }

    /// Accept one connection, complete the auth handshake, and return it.
    async fn accept_and_auth(listener: &TcpListener) -> TcpStream {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (id, kind, _) = read_one(&mut socket).await;
        assert_eq!(kind, TYPE_AUTH);
        socket
            .write_all(&encode_packet(id, TYPE_COMMAND, ""))
            .await
            .unwrap();
        socket
    }

    /// Read one command and answer it, echoing the client's request id.
    async fn serve_one_command(socket: &mut TcpStream, reply: &str) -> String {
        let (id, kind, body) = read_one(socket).await;
        assert_eq!(kind, TYPE_COMMAND);
        socket
            .write_all(&encode_packet(id, TYPE_RESPONSE, reply))
            .await
            .unwrap();
        body
    }

    #[tokio::test]
    async fn executes_a_command_against_a_scripted_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let mut socket = accept_and_auth(&listener).await;
            assert_eq!(serve_one_command(&mut socket, "There are 2 players").await, "list");
        });

        let client = RconClient::new(addr, "s3cret".to_string());
        assert_eq!(client.execute("list").await.unwrap(), "There are 2 players");
        server.await.unwrap();
    }

    /// The whole point of holding the connection: Minecraft logs a client
    /// thread per connection, and those lines show up in the console we stream.
    #[tokio::test]
    async fn reuses_one_connection_across_commands() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            // Exactly one accept. A client that reconnects per command would
            // hang here on the second command instead of being served.
            let mut socket = accept_and_auth(&listener).await;
            serve_one_command(&mut socket, "first").await;
            serve_one_command(&mut socket, "second").await;
            serve_one_command(&mut socket, "third").await;
        });

        let client = RconClient::new(addr, "s3cret".to_string());
        assert_eq!(client.execute("a").await.unwrap(), "first");
        assert_eq!(client.execute("b").await.unwrap(), "second");
        assert_eq!(client.execute("c").await.unwrap(), "third");
        server.await.unwrap();
    }

    /// A held connection dies whenever the server restarts, which is routine
    /// here — the console has a restart button.
    #[tokio::test]
    async fn reconnects_after_the_server_drops_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let mut socket = accept_and_auth(&listener).await;
            serve_one_command(&mut socket, "before restart").await;
            drop(socket); // the "restart"

            let mut socket = accept_and_auth(&listener).await;
            serve_one_command(&mut socket, "after restart").await;
        });

        let client = RconClient::new(addr, "s3cret".to_string());
        assert_eq!(client.execute("a").await.unwrap(), "before restart");
        // Transparent to the caller: no error surfaces for the dead socket.
        assert_eq!(client.execute("b").await.unwrap(), "after restart");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn surfaces_a_rejected_password() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_one(&mut socket).await;
            socket
                .write_all(&encode_packet(AUTH_FAILED, TYPE_COMMAND, ""))
                .await
                .unwrap();
            // A second accept would mean the client retried a bad password.
            let second = tokio::time::timeout(Duration::from_millis(400), listener.accept()).await;
            assert!(second.is_err(), "must not retry a rejected password");
        });

        let client = RconClient::new(addr, "wrong".to_string());
        assert!(matches!(client.execute("list").await, Err(RconError::Auth)));
        server.await.unwrap();
    }

    /// A leftover reply from an earlier, abandoned command must not be handed
    /// back as this command's output.
    #[tokio::test]
    async fn ignores_a_stale_reply_left_on_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let mut socket = accept_and_auth(&listener).await;
            let (id, _, _) = read_one(&mut socket).await;
            // Answer with a stale id first, then the real reply.
            socket
                .write_all(&encode_packet(id - 999, TYPE_RESPONSE, "STALE"))
                .await
                .unwrap();
            socket
                .write_all(&encode_packet(id, TYPE_RESPONSE, "fresh"))
                .await
                .unwrap();
        });

        let client = RconClient::new(addr, "s3cret".to_string());
        assert_eq!(client.execute("list").await.unwrap(), "fresh");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refuses_an_absurd_declared_length() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_one(&mut socket).await;
            // A hostile peer claiming a 1GB frame must not cause a 1GB alloc.
            socket.write_all(&1_000_000_000i32.to_le_bytes()).await.ok();
            tokio::time::sleep(Duration::from_millis(800)).await;
        });

        let client = RconClient::new(addr, "pw".to_string());
        assert!(matches!(
            client.execute("list").await,
            Err(RconError::Protocol(_))
        ));
        server.abort();
    }

    /// Short replies must not pay the drain window, or every command an
    /// operator types would sit for DRAIN_TIMEOUT before showing anything.
    #[tokio::test]
    async fn a_short_reply_returns_without_waiting_out_the_drain() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let mut socket = accept_and_auth(&listener).await;
            serve_one_command(&mut socket, "short").await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let client = RconClient::new(addr, "s3cret".to_string());
        let began = std::time::Instant::now();
        assert_eq!(client.execute("list").await.unwrap(), "short");
        assert!(
            began.elapsed() < DRAIN_TIMEOUT,
            "returned in {:?}, which means it waited out the drain",
            began.elapsed()
        );
        server.abort();
    }

    /// A reply at the split threshold still gets its continuation collected.
    #[tokio::test]
    async fn collects_a_split_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let head = "x".repeat(SPLIT_THRESHOLD);
        let expected = format!("{head}tail");

        let server = tokio::spawn(async move {
            let mut socket = accept_and_auth(&listener).await;
            let (id, _, _) = read_one(&mut socket).await;
            socket
                .write_all(&encode_packet(id, TYPE_RESPONSE, &"x".repeat(SPLIT_THRESHOLD)))
                .await
                .unwrap();
            socket
                .write_all(&encode_packet(id, TYPE_RESPONSE, "tail"))
                .await
                .unwrap();
        });

        let client = RconClient::new(addr, "s3cret".to_string());
        assert_eq!(client.execute("list").await.unwrap(), expected);
        server.await.unwrap();
    }
}
