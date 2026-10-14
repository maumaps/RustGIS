//! A loopback relay: an MCP client drives the session of the desktop app that is already open.
//!
//! The app starts a listener with its `ui.mcpRelay` command (off until then), and
//! `rustgis-cli mcp --attach` connects to it. The relay is the same [`crate::mcp`] protocol as
//! `rustgis-cli mcp`, one JSON-RPC message per line, so there is no second command protocol:
//! every action reaches the open [`Session`] through [`Session::run`] (`tools::run` maps
//! `run_command` to its command id and JSON parameters), one undo step per command, exactly as
//! the private-session server does.
//!
//! Safety: the listener binds 127.0.0.1 only — [`Relay::bind`] refuses any other address before
//! opening a socket — and it is off until the user starts it in the app. Each run generates a
//! random token and writes it to a user-only file ([`token_path`]); a connection is dropped
//! unless its first message is the relay's `rustgis/attach` carrying that token, so no other
//! process — and no web page reaching the loopback port — can drive the app. The attach's
//! `--yes` claim then decides what that connection allows (the `--yes` of `rustgis-cli mcp`);
//! the existing [`Server::refusal`] policy still runs per call on that basis, so attaching
//! bypasses no refusal.
use std::collections::VecDeque;
use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

use rustgis_engine::Session;
use serde_json::Value;

use crate::mcp::{Server, expects_reply};

/// The port `rustgis-cli mcp --attach` connects to unless it names another. (GeoLibre's runner
/// is 8766; the relay takes the next one.)
pub const DEFAULT_PORT: u16 = 8767;

/// The most bytes in one message: the same limit as `rustgis-cli mcp` (a project's JSON can be
/// large).
const MAX_LINE: u64 = 64 * 1024 * 1024;

/// The connection message: `{"jsonrpc":"2.0","method":"rustgis/attach","params":{"access":bool,
/// "token":"…"}}`. It carries no commands; it proves the client is this user's own (`token`,
/// read from [`token_path`]) and says what its `--yes` allows, before its first call.
const ATTACH: &str = "rustgis/attach";

/// A queued message: its line, what its connection allows, and where its reply goes.
type Request = (String, bool, SyncSender<Option<String>>);

struct Shared {
    requests: Mutex<VecDeque<Request>>,
    /// The run's attach token: only a connection that sends it in its first message stays open.
    token: String,
    /// What to run when a message is queued, so the UI thread draws a frame to run it.
    wake: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

/// A running listener. Messages queue for [`Relay::poll`], which runs them on the thread that
/// owns the [`Session`] — the app's UI thread — so the session never leaves its owner.
#[derive(Clone)]
pub struct Relay {
    shared: Arc<Shared>,
    port: u16,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay").field("port", &self.port).finish()
    }
}

/// The token file for `port`: in the system's temporary folder, one per relay. The relay writes
/// it when it starts (readable by this user only); `rustgis-cli mcp --attach` reads it, so no
/// token ever travels through a command line, an argument or the clipboard, and the agent never
/// pastes one.
pub fn token_path(port: u16) -> PathBuf {
    std::env::temp_dir().join(format!("rustgis-mcp-{port}.token"))
}

/// A fresh token for one relay run: 32 hex characters, keyed by the operating system's entropy
/// ([`std::collections::hash_map::RandomState`] seeds its hashers from the OS) and mixed with
/// the process id and the clock.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut out = String::with_capacity(32);
    for salt in 0..2u8 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        (std::process::id(), salt).hash(&mut h);
        if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            d.as_nanos().hash(&mut h);
        }
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Writes the run's token for `port` where [`token_path`] points, readable by this user only.
fn write_token_file(port: u16, token: &str) -> Result<PathBuf, String> {
    use std::io::Write as _;
    let path = token_path(port);
    // `create_new` never follows an existing file or symlink into a write. What a previous run
    // left is this user's own and is replaced; what this user cannot remove is refused.
    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!("{}: {e} (a token file this user cannot remove; remove it to start the MCP relay)", path.display()));
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(token.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(b"\n").map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

impl Relay {
    /// Binds `addr`. Anything but a loopback address is refused before a socket is opened, so a
    /// relay can never be reached from another computer.
    pub fn bind(addr: SocketAddr) -> Result<Self, String> {
        if !addr.ip().is_loopback() {
            return Err(format!("{addr}: the MCP relay listens on the loopback interface only"));
        }
        let listener = TcpListener::bind(addr).map_err(|e| format!("{addr}: {e}"))?;
        let port = listener.local_addr().map_err(|e| format!("{addr}: {e}"))?.port();
        // The attach token, written before any connection can arrive (the listener below is the
        // only way in, and nothing has the token until `rustgis-cli mcp --attach` reads it).
        let token = random_token();
        write_token_file(port, &token)?;
        let shared = Arc::new(Shared { requests: Mutex::new(VecDeque::new()), token, wake: Mutex::new(None) });
        let queued = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("rustgis-mcp-relay".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let queued = Arc::clone(&queued);
                            let _ = std::thread::Builder::new()
                                .name("rustgis-mcp-relay-connection".into())
                                .spawn(move || connection(stream, queued));
                        }
                        Err(e) => log::warn!("MCP relay: {e}"),
                    }
                }
            })
            .map_err(|e| format!("the MCP relay could not start: {e}"))?;
        Ok(Self { shared, port })
    }

    /// Binds 127.0.0.1:`port`; 0 asks the system for a free port.
    pub fn start(port: u16) -> Result<Self, String> {
        Self::bind(SocketAddr::from(([127, 0, 0, 1], port)))
    }

    /// The port the listener is on (what `start(0)` chose).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// What runs when a message is queued: [`Relay::poll`] only runs messages on a frame the UI
    /// thread draws, and in reactive mode egui draws none while the window is idle — so the app
    /// passes its `request_repaint` here, and a queued message wakes the frame it waits on.
    pub fn set_wake(&self, wake: Box<dyn Fn() + Send + Sync>) {
        *self.shared.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(wake);
    }

    /// Runs the messages that arrived since the last call, on `s`. Returns how many ran. The
    /// calls go through [`Session::run`], so each is one undo step as everywhere else.
    pub fn poll(&self, s: &mut Session) -> usize {
        let pending: Vec<Request> = {
            let mut queue = self.shared.requests.lock().unwrap_or_else(|e| e.into_inner());
            queue.drain(..).collect()
        };
        let mut ran = 0;
        for (line, allow_access, reply) in pending {
            let server = Server::new(allow_access);
            // A last resort: a panic answers that message with an error instead of breaking the
            // app (Session::run catches panics inside commands; this covers the tools around it).
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| server.handle(s, &line)))
                .unwrap_or_else(|_| Some(internal_error(&line)));
            let _ = reply.send(out); // the connection may have hung up
            ran += 1;
        }
        ran
    }
}

/// A line that starts like an HTTP request (`POST / HTTP/1.1`) or response is not the JSON line
/// protocol: a web page can reach a loopback port with `fetch`, and it must get nowhere.
fn looks_like_http(line: &str) -> bool {
    const METHODS: &[&str] = &["GET ", "POST ", "PUT ", "HEAD ", "DELETE ", "OPTIONS ", "PATCH ", "CONNECT ", "TRACE "];
    line.starts_with("HTTP/") || METHODS.iter().any(|m| line.starts_with(m))
}

/// One attached client: queue what it sends for the app's frame, write the app's replies back.
fn connection(stream: TcpStream, shared: Arc<Shared>) {
    let Ok(writer) = stream.try_clone() else { return };
    let mut input = std::io::BufReader::new(stream);
    let mut output = writer;
    let mut allow_access = false;
    let mut attached = false;
    loop {
        let line = match read_message(&mut input) {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(why) => {
                let e = serde_json::json!({"jsonrpc": "2.0", "id": Value::Null, "error": {"code": -32700, "message": why}});
                let _ = writeln!(output, "{e}").and_then(|()| output.flush());
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if !attached && looks_like_http(&line) {
            log::warn!("MCP relay: dropped a connection that spoke HTTP, not the relay protocol");
            return;
        }
        // Before the attach the connection has proven nothing, so a bad line drops it. After
        // a valid attach the line belongs to the client: a non-JSON line is queued like any
        // other, and `Server::handle` answers the parse error while the session stays open.
        let msg = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => Some(msg),
            Err(_) if attached => None,
            Err(_) => return,
        };
        if let Some(msg) = &msg {
            // The first non-empty message must be a valid attach: the run's token, or the
            // connection goes. Nothing else a line could say is believed. A repeat attach
            // still carries the token (it may update what the connection allows).
            let is_attach = msg.get("method").and_then(Value::as_str) == Some(ATTACH);
            if !attached || is_attach {
                let ok = is_attach
                    && msg.get("params").and_then(|p| p.get("token")).and_then(Value::as_str) == Some(shared.token.as_str());
                if !ok {
                    return;
                }
                allow_access = msg.get("params").and_then(|p| p.get("access")).and_then(Value::as_bool).unwrap_or(false);
                attached = true;
                continue;
            }
        }
        let (tx, rx) = sync_channel(1);
        {
            let mut queue = shared.requests.lock().unwrap_or_else(|e| e.into_inner());
            queue.push_back((line.clone(), allow_access, tx));
        }
        // The app's frame runs queued messages, and egui draws no frame the UI thread isn't
        // asked for: wake it now, or the client waits until the user moves the mouse.
        if let Some(wake) = &*shared.wake.lock().unwrap_or_else(|e| e.into_inner()) {
            wake();
        }
        if !expects_reply(&line) {
            continue;
        }
        match rx.recv() {
            Ok(Some(reply)) => {
                if writeln!(output, "{reply}").and_then(|()| output.flush()).is_err() {
                    return;
                }
            }
            // No reply to this message, or the app went away.
            _ => return,
        }
    }
}

/// Reads one message (a line), bounded like `rustgis-cli mcp`. `Ok(None)` is the end of the
/// stream; `Err(why)` is a message too long or not UTF-8 (its line is skipped).
fn read_message(input: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut buf = Vec::new();
    let n = input.by_ref().take(MAX_LINE + 1).read_until(b'\n', &mut buf).map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() as u64 > MAX_LINE && buf.last() != Some(&b'\n') {
        // Skip the rest of the line before saying why it wasn't read.
        loop {
            buf.clear();
            let m = input.by_ref().take(MAX_LINE).read_until(b'\n', &mut buf).map_err(|e| e.to_string())?;
            if m == 0 || buf.last() == Some(&b'\n') {
                break;
            }
        }
        return Err("message too long (64 MB at most)".into());
    }
    // Messages are UTF-8; a bad byte could change a path inside one, so it isn't guessed at.
    let line = std::str::from_utf8(&buf).map_err(|_| "message is not UTF-8".to_string())?.to_string();
    Ok(Some(line))
}

/// The internal-error reply to a message that panicked, with the id the client can match it to.
fn internal_error(message: &str) -> String {
    let id = serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .filter(|id| id.is_string() || id.is_number())
        .unwrap_or(Value::Null);
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "internal error"}}).to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::io::{BufRead, BufReader, Write};

    use rustgis_engine::{Project, Session};
    use serde_json::json;

    use super::*;

    fn sample() -> Session {
        let mut s = Session::new(Project::default());
        s.run("project.sample", &Value::Null).unwrap();
        s
    }

    /// Reads the one reply a request gets.
    fn reply(client: &TcpStream) -> Value {
        let mut input = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// The token the relay wrote for its port — the same file the CLI reads.
    fn token(relay: &Relay) -> String {
        std::fs::read_to_string(token_path(relay.port())).unwrap().trim().to_string()
    }

    /// Attaches a client the way `rustgis-cli mcp --attach` does.
    fn attach(client: &mut TcpStream, relay: &Relay, access: bool) {
        let hello = json!({"jsonrpc": "2.0", "method": "rustgis/attach", "params": {"access": access, "token": token(relay)}});
        writeln!(client, "{hello}").unwrap();
        client.flush().unwrap();
    }

    /// Lets the connection thread hand the request to the app's frame, which runs it.
    fn settle(relay: &Relay, s: &mut Session) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while relay.poll(s) == 0 {
            assert!(std::time::Instant::now() < deadline, "the relay never handed the request to the app");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A command sent through the relay runs on the target Session through Session::run (its
    /// undo history shows it), and a bind away from loopback is refused.
    #[test]
    fn relay_runs_a_command_on_the_target_session() {
        // The negative control: anything but loopback is refused, before any socket opens.
        for addr in ["0.0.0.0:0", "[::]:0", "192.0.2.1:8767"] {
            let a: SocketAddr = addr.parse().unwrap();
            let e = Relay::bind(a).unwrap_err();
            assert!(e.contains("loopback"), "{addr}: {e}");
        }

        let relay = Relay::start(0).unwrap();
        let mut s = sample();
        let layers = s.map().layers.len();
        let undo = s.undo_depth();
        let mut client = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        attach(&mut client, &relay, false);
        // One command id and its JSON parameters, through the command registry's Session::run.
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "run_command",
            "arguments": {"command": "layer.addSample", "params": {"name": "countries"}}
        }});
        writeln!(client, "{call}").unwrap();
        client.flush().unwrap();
        settle(&relay, &mut s);
        let reply = reply(&client);
        assert_eq!(reply["id"], 1, "{reply}");
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        // The session the relay was started for: the command ran here, one undo step.
        assert_eq!(s.map().layers.len(), layers + 1, "{reply}");
        assert_eq!(s.undo_depth(), undo + 1, "one undo step, as in the private-session server");
    }

    /// A connection that doesn't start with the run's token is dropped: a self-declared access
    /// flag proves nothing, no command runs, and a line that speaks HTTP gets nowhere.
    #[test]
    fn relay_drops_connections_without_the_token() {
        let relay = Relay::start(0).unwrap();
        let mut s = sample();
        // A wrong token: the connection is closed and nothing was queued.
        let mut client = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        let bad = json!({"jsonrpc": "2.0", "method": "rustgis/attach", "params": {"access": true, "token": "not-the-token"}});
        writeln!(client, "{bad}").unwrap();
        client.flush().unwrap();
        let mut input = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        assert_eq!(input.read_line(&mut line).unwrap(), 0, "the relay kept a client without the token: {line}");
        assert_eq!(relay.poll(&mut s), 0, "a connection without the token queued a message");
        // A command with no attach at all: dropped the same way.
        let mut client = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "list_layers", "arguments": {}}});
        writeln!(client, "{call}").unwrap();
        client.flush().unwrap();
        let mut input = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        assert_eq!(input.read_line(&mut line).unwrap(), 0, "the relay kept a client that never attached: {line}");
        assert_eq!(relay.poll(&mut s), 0, "a connection that never attached queued a message");
        // An HTTP request line — a web page's fetch at the loopback port — is a protocol
        // violation: dropped, no parse-error reply to keep the exchange going.
        let mut client = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        writeln!(client, "POST / HTTP/1.1").unwrap();
        client.flush().unwrap();
        let mut input = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        assert_eq!(input.read_line(&mut line).unwrap(), 0, "the relay kept an HTTP request open: {line}");
        assert_eq!(relay.poll(&mut s), 0, "an HTTP request queued a message");
        let _ = std::fs::remove_file(token_path(relay.port()));
    }

    /// After a valid attach a non-JSON line is not a protocol violation of its own: the server
    /// answers it with a parse error and the same connection keeps serving requests.
    #[test]
    fn relay_returns_parse_error_after_attach_and_stays_usable() {
        let relay = Relay::start(0).unwrap();
        let mut s = sample();
        let mut client = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        attach(&mut client, &relay, false);
        writeln!(client, "this is not JSON").unwrap();
        client.flush().unwrap();
        settle(&relay, &mut s);
        let parse_error = reply(&client);
        assert_eq!(parse_error["error"]["code"], -32700, "{parse_error}");
        // The same connection still answers a real request afterwards.
        let ping = json!({"jsonrpc": "2.0", "id": 7, "method": "ping"});
        writeln!(client, "{ping}").unwrap();
        client.flush().unwrap();
        settle(&relay, &mut s);
        let pong = reply(&client);
        assert_eq!(pong["id"], 7, "{pong}");
        let _ = std::fs::remove_file(token_path(relay.port()));
    }

    /// A file command still needs --yes: without the connection's access message the existing
    /// refusal policy answers it.
    #[test]
    fn relay_refuses_file_commands_without_yes() {
        let relay = Relay::start(0).unwrap();
        let mut s = sample();
        let mut client = TcpStream::connect(("127.0.0.1", relay.port())).unwrap();
        attach(&mut client, &relay, false);
        let out = std::env::temp_dir().join(format!("rustgis-relay-test-{}.png", std::process::id()));
        let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "run_command",
            "arguments": {"command": "share.exportMap", "params": {"path": out.display().to_string()}}
        }});
        writeln!(client, "{call}").unwrap();
        client.flush().unwrap();
        settle(&relay, &mut s);
        let reply = reply(&client);
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains("--yes"), "{reply}");
        assert!(!out.exists(), "the file command did not run");
        let _ = std::fs::remove_file(out);
        let _ = std::fs::remove_file(token_path(relay.port()));
    }
}
