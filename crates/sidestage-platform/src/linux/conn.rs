//! One native-protocol connection to PulseAudio or pipewire-pulse.
//!
//! The `pulseaudio` crate supplies the wire format (tagstructs, commands, replies). Framing and
//! I/O are done here instead of through `pulseaudio::Client`, because that client silently drops
//! stream lifecycle events (`*StreamKilled`, `*Moved`, `Underflow`), treats a short playback
//! callback as end-of-stream, allocates per read, and sends an empty cookie, which real
//! PulseAudio rejects. Every packet is read whole by its descriptor length, so a reply the crate
//! parses only partly can never desynchronise the stream.
//!
//! Each connection belongs to one thread. Reads use a short socket timeout so loops can check
//! their stop flag; a packet that has started arriving is always read to the end.

use std::ffi::{CStr, CString};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use pulseaudio::protocol::{self, Command, CommandReply, CommandTag, Prop, Props, PulseError, TagStructReader};

use super::env;

/// Descriptor channel of control packets; any other channel carries stream data.
pub const CONTROL: u32 = u32::MAX;

const HEADER_LEN: usize = protocol::DESCRIPTOR_SIZE;
/// Offset of a reply's body in a control packet: two tagged u32s (command tag and sequence).
const REPLY_BODY: usize = 10;
/// Socket read timeout: how often idle loops wake up to check their stop flag.
const POLL: Duration = Duration::from_millis(50);
/// A packet that has started arriving must finish within this time.
const PACKET_DEADLINE: Duration = Duration::from_secs(5);
/// Largest control packet accepted (sink-input lists of a busy desktop are tens of KB).
const MAX_CONTROL: usize = 16 << 20;
/// Default wait for a reply to a request.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a request failed.
#[derive(Debug)]
pub enum CallError {
    /// The server answered with an error code.
    Server(PulseError),
    /// The connection broke or timed out.
    Connection(io::Error),
    /// The reply arrived but could not be decoded (a protocol mismatch, not a lost server).
    Decode(protocol::ProtocolError),
}

impl CallError {
    pub fn is(&self, code: PulseError) -> bool {
        matches!(self, Self::Server(c) if *c == code)
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Server(e) => write!(f, "the sound server refused: {e}"),
            Self::Connection(e) => write!(f, "sound server connection failed: {e}"),
            Self::Decode(e) => write!(f, "could not understand the sound server's reply: {e}"),
        }
    }
}

impl std::error::Error for CallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Server(_) => None,
            Self::Connection(e) => Some(e),
            Self::Decode(e) => Some(e),
        }
    }
}

impl From<io::Error> for CallError {
    fn from(e: io::Error) -> Self {
        Self::Connection(e)
    }
}

/// Whether the sound server connection itself failed (as opposed to a refused request or a
/// local error such as an unwritable journal).
pub fn is_connection_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| matches!(c.downcast_ref::<CallError>(), Some(CallError::Connection(_))))
}

/// Whether the connection is dead (closed or reset, typically because the server restarted),
/// so a fresh connection is worth trying at once. A timeout is not: a hung server would only
/// make the caller wait twice.
pub fn is_dead_connection(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<CallError>(),
            Some(CallError::Connection(io)) if io.kind() != io::ErrorKind::TimedOut
        )
    })
}

/// Start of a packet.
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub length: u32,
    pub channel: u32,
}

/// A decoded control packet. Not boxed despite the size difference: boxing would allocate on
/// the audio thread for every playback `Request`.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Control {
    /// Reply to request `seq`; the body is [`Conn::reply_body`].
    Reply {
        seq: u32,
    },
    Error {
        seq: u32,
        code: PulseError,
    },
    Command(Command),
}

pub struct Conn {
    reader: BufReader<UnixStream>,
    version: u16,
    seq: u32,
    path: PathBuf,
    reply_timeout: Duration,
    /// Last control packet's payload (reused).
    payload: Vec<u8>,
    /// Outgoing packet being encoded (reused).
    out: Vec<u8>,
}

impl Conn {
    /// Connects, authenticates and names the client. `role` ends up in the client's properties
    /// so `pactl list clients` shows which Sidestage connection is which.
    pub fn connect(role: &str) -> anyhow::Result<Self> {
        Self::connect_with(role, REPLY_TIMEOUT)
    }

    pub fn connect_with(role: &str, reply_timeout: Duration) -> anyhow::Result<Self> {
        let var = |k: &str| std::env::var(k).ok();
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let candidates = env::socket_candidates(&var, uid).map_err(anyhow::Error::msg)?;
        let mut tried = Vec::new();
        for path in candidates {
            match UnixStream::connect(&path) {
                Ok(sock) => {
                    return Self::handshake(sock, path.clone(), role, reply_timeout)
                        .with_context(|| format!("connecting to the sound server at {}", path.display()));
                }
                Err(e) => tried.push(format!("{}: {e}", path.display())),
            }
        }
        bail!(
            "no PulseAudio or PipeWire (pipewire-pulse) sound server is running for this user (tried {})",
            tried.join("; ")
        )
    }

    fn handshake(sock: UnixStream, path: PathBuf, role: &str, reply_timeout: Duration) -> anyhow::Result<Self> {
        sock.set_read_timeout(Some(POLL))?;
        sock.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut conn = Self {
            reader: BufReader::with_capacity(64 * 1024, sock),
            version: protocol::MAX_VERSION,
            seq: 0,
            path,
            reply_timeout,
            payload: Vec::with_capacity(4096),
            out: Vec::with_capacity(4096),
        };
        let auth = protocol::AuthParams {
            version: protocol::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie: read_cookie(),
        };
        let reply: protocol::AuthReply =
            conn.request(&Command::Auth(auth)).context("authentication failed (is the PulseAudio cookie readable?)")?;
        if reply.version < protocol::MIN_VERSION {
            bail!("the sound server speaks protocol version {}, which is too old", reply.version);
        }
        conn.version = protocol::MAX_VERSION.min(reply.version);
        let named: protocol::SetClientNameReply =
            conn.request(&Command::SetClientName(client_props(role))).context("registering the client failed")?;
        log::debug!("pulse: {role} connection is client #{} (protocol {})", named.client_id, conn.version);
        Ok(conn)
    }

    pub fn version(&self) -> u16 {
        self.version
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Sends `cmd` and waits for its reply. Unrelated packets that arrive meanwhile are
    /// dropped, so this is for connections without streams or subscriptions (or for setup).
    pub fn request<T: CommandReply>(&mut self, cmd: &Command) -> Result<T, CallError> {
        let seq = self.send(cmd)?;
        self.await_reply(seq)?;
        let mut body = self.reply_body();
        TagStructReader::new(&mut body, self.version).read::<T>().map_err(CallError::Decode)
    }

    /// Sends `cmd` and waits for an empty reply.
    pub fn ack(&mut self, cmd: &Command) -> Result<(), CallError> {
        let seq = self.send(cmd)?;
        self.await_reply(seq)
    }

    /// Sends `cmd` without waiting; returns its sequence number.
    pub fn send(&mut self, cmd: &Command) -> io::Result<u32> {
        // u32::MAX is the server's "no sequence" (events), never use it.
        self.seq = if self.seq >= u32::MAX - 1 { 1 } else { self.seq + 1 };
        self.out.clear();
        let n =
            protocol::encode_command_message(&mut self.out, self.seq, cmd, self.version).map_err(io::Error::other)?;
        self.reader.get_mut().write_all(&self.out[..n])?;
        Ok(self.seq)
    }

    fn await_reply(&mut self, seq: u32) -> Result<(), CallError> {
        let deadline = Instant::now() + self.reply_timeout;
        loop {
            if Instant::now() > deadline {
                return Err(CallError::Connection(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the sound server did not answer",
                )));
            }
            if !self.wait_readable()? {
                continue;
            }
            let header = self.read_header()?;
            if header.channel != CONTROL {
                self.skip(header.length)?;
                continue;
            }
            self.read_control(header.length)?;
            match self.decode() {
                Ok(Control::Reply { seq: s }) if s == seq => return Ok(()),
                Ok(Control::Error { seq: s, code }) if s == seq => {
                    return Err(CallError::Server(code));
                }
                Ok(other) => log::trace!("pulse: ignoring {other:?} while waiting for reply {seq}"),
                Err(e) => log::debug!("pulse: ignoring an undecodable message: {e}"),
            }
        }
    }

    /// True when a packet is ready to be read, false after an idle poll interval.
    pub fn wait_readable(&mut self) -> io::Result<bool> {
        match self.reader.fill_buf() {
            Ok([]) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the sound server closed the connection")),
            Ok(_) => Ok(true),
            Err(e) if is_retry(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn read_header(&mut self) -> io::Result<Header> {
        let mut b = [0u8; HEADER_LEN];
        read_full(&mut self.reader, &mut b)?;
        Ok(Header {
            length: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            channel: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        })
    }

    /// Reads a control packet's payload into the reusable buffer.
    pub fn read_control(&mut self, length: u32) -> io::Result<()> {
        let len = length as usize;
        if len > MAX_CONTROL {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("control packet of {len} bytes")));
        }
        self.payload.clear();
        self.payload.resize(len, 0);
        read_full(&mut self.reader, &mut self.payload)
    }

    /// Decodes the last control packet.
    pub fn decode(&self) -> Result<Control, protocol::ProtocolError> {
        decode_control(&self.payload, self.version)
    }

    /// Command tag of the last control packet, read without decoding the rest. Lets the audio
    /// thread skip messages whose decoding would allocate (e.g. `PlaybackStreamMoved` carries
    /// a sink name).
    pub fn tag(&self) -> Option<u32> {
        match self.payload.get(..5)? {
            // A tagged u32: 'L' then big-endian.
            &[b'L', a, b, c, d] => Some(u32::from_be_bytes([a, b, c, d])),
            _ => None,
        }
    }

    /// Body of the last control packet, when it was a reply.
    pub fn reply_body(&self) -> &[u8] {
        self.payload.get(REPLY_BODY..).unwrap_or_default()
    }

    /// The last control packet's payload, undecoded.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Streams a data packet's payload to `f` in pieces, straight out of the read buffer
    /// (no copy, no allocation).
    pub fn read_data(&mut self, length: u32, mut f: impl FnMut(&[u8])) -> io::Result<()> {
        let mut left = length as usize;
        let deadline = Instant::now() + PACKET_DEADLINE;
        while left > 0 {
            let available = match self.reader.fill_buf() {
                Ok([]) => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-packet"));
                }
                Ok(b) => b,
                Err(e) if is_retry(&e) => {
                    check_deadline(deadline)?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            let n = available.len().min(left);
            f(&available[..n]);
            self.reader.consume(n);
            left -= n;
        }
        Ok(())
    }

    pub fn skip(&mut self, length: u32) -> io::Result<()> {
        self.read_data(length, |_| {})
    }

    /// Makes sure stream packets of up to `samples` f32s encode without allocating.
    pub fn reserve_stream(&mut self, samples: usize) {
        let want = HEADER_LEN + samples * 4;
        self.out.reserve(want.saturating_sub(self.out.len()));
    }

    /// Writes f32 samples as one float32le data packet on `channel` (relative seek, offset 0).
    pub fn write_stream(&mut self, channel: u32, samples: &[f32]) -> io::Result<()> {
        self.out.clear();
        self.out.extend_from_slice(&[0; HEADER_LEN]);
        for s in samples {
            self.out.extend_from_slice(&s.to_le_bytes());
        }
        let len = (samples.len() * 4) as u32;
        self.out[0..4].copy_from_slice(&len.to_be_bytes());
        self.out[4..8].copy_from_slice(&channel.to_be_bytes());
        // Offset (bytes 8..16) and flags (16..20, seek mode PA_SEEK_RELATIVE = 0) stay zero.
        self.reader.get_mut().write_all(&self.out)
    }
}

/// Decodes a control packet. Replies and errors are recognised from their first two fields, so
/// an error code the crate does not know still ends the wait for its reply.
fn decode_control(payload: &[u8], version: u16) -> Result<Control, protocol::ProtocolError> {
    let mut cursor = payload;
    let mut ts = TagStructReader::new(&mut cursor, version);
    let tag = ts.read_u32()?;
    let seq = ts.read_u32()?;
    if tag == CommandTag::Reply as u32 {
        return Ok(Control::Reply { seq });
    }
    if tag == CommandTag::Error as u32 {
        let code = ts.read_enum::<PulseError>().unwrap_or(PulseError::Unknown);
        return Ok(Control::Error { seq, code });
    }
    let mut cursor = payload;
    let (_, cmd) = Command::read_tag_prefixed(&mut cursor, version)?;
    Ok(Control::Command(cmd))
}

/// Fills `buf` completely, tolerating read timeouts until the packet deadline.
fn read_full(reader: &mut BufReader<UnixStream>, buf: &mut [u8]) -> io::Result<()> {
    let deadline = Instant::now() + PACKET_DEADLINE;
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the sound server closed the connection"));
            }
            Ok(n) => filled += n,
            Err(e) if is_retry(&e) => check_deadline(deadline)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() > deadline {
        Err(io::Error::new(io::ErrorKind::TimedOut, "the sound server stalled in the middle of a message"))
    } else {
        Ok(())
    }
}

fn is_retry(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted)
}

fn read_cookie() -> Vec<u8> {
    let var = |k: &str| std::env::var(k).ok();
    let file = env::cookie_candidates(&var).into_iter().find_map(|p| std::fs::read(p).ok());
    env::auth_cookie(file.as_deref())
}

fn client_props(role: &str) -> Props {
    let mut props = Props::new();
    props.set(Prop::ApplicationName, c"Sidestage");
    if let Ok(pid) = CString::new(std::process::id().to_string()) {
        props.set(Prop::ApplicationProcessId, pid);
    }
    if let Some(binary) = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .and_then(|n| CString::new(n).ok())
    {
        props.set(Prop::ApplicationProcessBinary, binary);
    }
    set_str(&mut props, c"sidestage.connection", role);
    props
}

/// Sets a free-form string property (values are NUL-terminated on the wire).
pub fn set_str(props: &mut Props, key: &CStr, value: &str) {
    if let Ok(v) = CString::new(value) {
        props.set_bytes(key, v.as_bytes_with_nul());
    }
}

/// A string property, trimmed, or `None` when missing or empty.
pub fn get_str(props: &Props, key: &CStr) -> Option<String> {
    let bytes = props.get_bytes(key)?;
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let s = String::from_utf8_lossy(bytes).trim().to_owned();
    (!s.is_empty()).then_some(s)
}
