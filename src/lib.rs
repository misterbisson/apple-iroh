//! # iroh behind a C ABI, for an app that has to ask one question
//!
//! An iroh endpoint that a Swift app can start, dial, and — the part this
//! exists for — **ask which path it actually got**. A QUIC connection between
//! two endpoints behind NAT either hole-punched its way to a direct path or is
//! being carried by a relay, and the difference decides whether video over it
//! is a product or a disappointment.
//!
//! The surface is deliberately small. It is not a general iroh binding: it is
//! the smallest thing that can answer direct-versus-relay from a phone on a
//! café network, and it will grow only as something needs it to.
//!
//! ## Conventions across the boundary
//!
//! - Every function returns `i32`. **Zero or positive is success**; negative is
//!   one of the `ERR_*` constants below. Nothing panics across the boundary —
//!   the crate is built with `panic = "abort"`, so a panic would take the app
//!   down rather than unwind into Swift, and every fallible path returns an
//!   error code instead.
//! - Endpoint ids cross as **64 lowercase hex characters**, written here rather
//!   than using iroh's own `Display`. A caller that reads an id out of one
//!   version and passes it back into another should not be at the mercy of a
//!   formatting change upstream.
//! - String-out functions take a buffer and its capacity, write UTF-8 with no
//!   trailing NUL, and return the number of bytes written. They return
//!   `ERR_BUFFER` rather than truncating, because a truncated endpoint id is a
//!   different endpoint id.

use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The protocol name two endpoints have to agree on before QUIC will talk.
/// Versioned from the start: a v0 endpoint meeting a v1 endpoint should fail to
/// negotiate rather than half-work.
///
/// `/1` since v0.4.0, which added the pull stream: a v0.3.0 endpoint accepts a
/// connection and then never answers a pull, and "the other side is too old"
/// should not read as "the network carries nothing".
const ALPN: &[u8] = b"apple-iroh/probe/1";

pub const ERR_NOT_STARTED: i32 = -1;
pub const ERR_ALREADY_STARTED: i32 = -2;
pub const ERR_BIND: i32 = -3;
pub const ERR_BAD_ID: i32 = -4;
pub const ERR_CONNECT: i32 = -5;
pub const ERR_BUFFER: i32 = -6;
pub const ERR_NO_REMOTE: i32 = -7;
/// A secret key that is not exactly 32 bytes.
pub const ERR_BAD_KEY: i32 = -8;
/// The held connection to this remote has closed — refused by the other side,
/// timed out, or dropped.
pub const ERR_CLOSED: i32 = -9;
/// A pull could not open its stream, or the stream failed before a byte came.
pub const ERR_STREAM: i32 = -10;
/// A port outside 0–65535, or a loopback listener that could not be bound.
pub const ERR_PORT: i32 = -11;

/// What a dialler writes on a new bidirectional stream to ask for bytes.
const PULL_REQUEST: &[u8] = b"pull";
/// What a forwarder writes first on a stream it wants piped to the other side's
/// exposed port (v0.7.0). The same length as `pull`, so one read of four bytes
/// tells the two apart — and a v0.6.0 endpoint, which reads to the end and
/// compares with `pull`, drops the stream rather than answering it wrongly.
const FORWARD_REQUEST: &[u8] = b"fwd1";
/// **How long one pull is served, at most.** The reader stops the stream when
/// it has timed enough; this is only what ends a stream whose reader went
/// away without saying so.
const SERVE_LIMIT: Duration = Duration::from_secs(120);
const CHUNK: usize = 64 * 1024;

/// Length of a secret key, in bytes.
pub const SECRET_KEY_LEN: i32 = 32;

/// Path flags, returned by [`apple_iroh_path`] as a bitmask.
///
/// A bitmask rather than an enum because **both can be true at once**, and that
/// state is the interesting one. These stacks normally come up on the relay and
/// upgrade once hole punching lands, so a sample showing relay and direct
/// together is the upgrade in progress — which a three-way enum would have to
/// flatten into one answer or the other.
pub const PATH_RELAY: i32 = 1;
pub const PATH_DIRECT: i32 = 2;

static RT: OnceLock<Runtime> = OnceLock::new();
static ENDPOINT: Mutex<Option<Endpoint>> = Mutex::new(None);
/// Dialled connections, held open so there is a live path to report on. A
/// connection that has been dropped tells you nothing about how it was carried.
static DIALLED: Mutex<Vec<(EndpointId, Connection)>> = Mutex::new(Vec::new());

/// **Who may connect in.** Empty means nobody.
///
/// Deny by default, deliberately. The endpoint id is a public key and QUIC's
/// handshake proves the other side holds its secret, so *who* is connecting is
/// already settled cryptographically — what iroh does not decide is whether
/// that someone is welcome. An open accept loop is how a listener ships open,
/// and "the caller forgot to configure it" should fail closed.
///
/// Outgoing dials are not checked against this: a dial names its remote, and
/// the handshake refuses anyone who is not that remote.
static ALLOWED: Mutex<Vec<EndpointId>> = Mutex::new(Vec::new());

/// Incoming connections closed because their id was not allowed. Counted so a
/// side that sees no connection can tell "nobody arrived" from "somebody
/// arrived and was turned away" — two very different things to debug.
static REFUSED: AtomicI32 = AtomicI32::new(0);

/// The loopback port forwarded streams are piped to; zero exposes nothing.
static EXPOSED: AtomicI32 = AtomicI32::new(0);

/// Loopback listeners this side opened with `apple_iroh_forward`, one per
/// remote, so asking again hands back the same port.
static FORWARDS: Mutex<Vec<(EndpointId, u16, JoinHandle<()>)>> = Mutex::new(Vec::new());

/// **What forwarding has carried**, at the `CARRIED_*` indices. Counted in the
/// library because an instrument has to show that it was in the path: a player
/// that plays from somewhere else looks exactly like one playing over iroh.
pub const CARRIED_FORWARD_STREAMS: usize = 0;
pub const CARRIED_FORWARD_FAILED: usize = 1;
pub const CARRIED_FORWARD_SENT: usize = 2;
pub const CARRIED_FORWARD_RECEIVED: usize = 3;
pub const CARRIED_EXPOSE_STREAMS: usize = 4;
pub const CARRIED_EXPOSE_FAILED: usize = 5;
pub const CARRIED_EXPOSE_SENT: usize = 6;
pub const CARRIED_EXPOSE_RECEIVED: usize = 7;
pub const CARRIED_FIELDS: usize = 8;
static CARRIED: [AtomicU64; CARRIED_FIELDS] = [const { AtomicU64::new(0) }; CARRIED_FIELDS];

fn count(slot: usize, by: u64) {
    CARRIED[slot].fetch_add(by, Ordering::Relaxed);
}

fn runtime() -> &'static Runtime {
    RT.get_or_init(|| {
        Runtime::new().expect("apple-iroh: could not build a tokio runtime")
    })
}

/// Binds an endpoint and starts accepting.
///
/// Accepting matters even for the side that only dials: hole punching is
/// two-sided, and an endpoint that will not accept cannot be punched to.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_start() -> i32 {
    start(None)
}

/// Binds an endpoint **under a key the caller kept**, and starts accepting.
///
/// The endpoint id is the public half of the secret key, so an endpoint started
/// with [`apple_iroh_start`] has a new id every time. That is fine for two
/// endpoints on one desk and useless for the case this exists for: a Mac left
/// at home and an iPad in a café, where the id the iPad carried out of the door
/// has to still be the Mac's id when it dials. So the app keeps the key — in
/// the keychain, on that device only — and hands it back here on every launch.
///
/// `len` must be exactly [`SECRET_KEY_LEN`]; anything else is `ERR_BAD_KEY`
/// rather than a key silently derived from the wrong bytes.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_start_with_secret(key: *const u8, len: i32) -> i32 {
    if key.is_null() || len != SECRET_KEY_LEN {
        return ERR_BAD_KEY;
    }
    let mut bytes = [0u8; 32];
    // SAFETY: non-null and exactly 32 bytes, checked above.
    bytes.copy_from_slice(unsafe { std::slice::from_raw_parts(key, 32) });
    start(Some(SecretKey::from_bytes(&bytes)))
}

/// Writes this endpoint's 32-byte secret key, so a caller that started with
/// [`apple_iroh_start`] can keep the key it was given and reuse it.
///
/// Returns 32. **The caller now holds the thing that is this endpoint's
/// identity**: whoever has these bytes can answer as it.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_secret_key(buf: *mut u8, cap: i32) -> i32 {
    let Some(endpoint) = endpoint() else {
        return ERR_NOT_STARTED;
    };
    if buf.is_null() || cap < SECRET_KEY_LEN {
        return ERR_BUFFER;
    }
    let bytes = endpoint.secret_key().to_bytes();
    // SAFETY: non-null and at least 32 bytes of capacity, checked above.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, 32) };
    SECRET_KEY_LEN
}

fn start(secret: Option<SecretKey>) -> i32 {
    let mut slot = match ENDPOINT.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    if slot.is_some() {
        return ERR_ALREADY_STARTED;
    }
    let built = runtime().block_on(async {
        let builder = Endpoint::builder(presets::N0).alpns(vec![ALPN.to_vec()]);
        let builder = match secret {
            Some(secret) => builder.secret_key(secret),
            None => builder,
        };
        builder.bind().await
    });
    let endpoint = match built {
        Ok(endpoint) => endpoint,
        Err(_) => return ERR_BIND,
    };
    for slot in &CARRIED {
        slot.store(0, Ordering::Relaxed);
    }

    // Accept in the background and hold whatever arrives. The probe has no
    // protocol yet; what it needs is that a connection exists and stays up so
    // its path can be sampled.
    let accepting = endpoint.clone();
    runtime().spawn(async move {
        while let Some(incoming) = accepting.accept().await {
            tokio::spawn(async move {
                if let Ok(conn) = incoming.await {
                    let id = conn.remote_id();
                    if is_allowed(&id) {
                        remember(id, conn);
                    } else {
                        REFUSED.fetch_add(1, Ordering::Relaxed);
                        conn.close(1u32.into(), b"not allowed");
                    }
                }
            });
        }
    });

    *slot = Some(endpoint);
    0
}

fn allowed() -> std::sync::MutexGuard<'static, Vec<EndpointId>> {
    match ALLOWED.lock() {
        Ok(list) => list,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn is_allowed(id: &EndpointId) -> bool {
    allowed().contains(id)
}

/// Lets one remote connect in. Idempotent.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_allow(id_hex: *const c_char) -> i32 {
    let Some(id) = parse_id(id_hex) else {
        return ERR_BAD_ID;
    };
    let mut list = allowed();
    if !list.contains(&id) {
        list.push(id);
    }
    0
}

/// Empties the allow list, so nobody new can connect in. Connections already
/// accepted are left alone; call `apple_iroh_stop` to drop those.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_allow_none() {
    allowed().clear();
}

/// How many incoming connections have been refused since the library loaded.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_refused() -> i32 {
    REFUSED.load(Ordering::Relaxed)
}

fn held_closed(id: &EndpointId) -> bool {
    let held = match DIALLED.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.iter()
        .find(|(known, _)| known == id)
        .is_some_and(|(_, conn)| conn.close_reason().is_some())
}

fn remember(id: EndpointId, conn: Connection) {
    serve(conn.clone());
    let mut held = match DIALLED.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.retain(|(known, _)| *known != id);
    held.push((id, conn));
}

fn held(id: &EndpointId) -> Option<Connection> {
    let held = match DIALLED.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.iter().find(|(known, _)| known == id).map(|(_, conn)| conn.clone())
}

/// **Answers pulls and forwards on a held connection**, from either end.
///
/// A pull is a bidirectional stream carrying `pull`; the answer is zeros, in
/// 64 KiB writes, until the reader stops the stream. Zeros because what is
/// being timed is the path and nothing behind it: a file would put this Mac's
/// disk in the reading, and QUIC encrypts every byte, so nothing on the way
/// can compress them.
///
/// A forward is a stream opening with `fwd1`, piped both ways to the loopback
/// port this side exposed, or dropped when it exposed none.
///
/// Both ends serve, so either can time the other direction. Only connections
/// that got past the allow list, or that this side dialled, are ever held.
fn serve(conn: Connection) {
    runtime().spawn(async move {
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            tokio::spawn(async move {
                let mut head = [0u8; 4];
                if recv.read_exact(&mut head).await.is_err() {
                    return;
                }
                if head == FORWARD_REQUEST {
                    expose(send, recv).await;
                    return;
                }
                if head != PULL_REQUEST {
                    return;
                }
                let chunk = vec![0u8; CHUNK];
                let _ = tokio::time::timeout(SERVE_LIMIT, async {
                    while send.write_all(&chunk).await.is_ok() {}
                })
                .await;
                let _ = send.finish();
            });
        }
    });
}

/// Pipes one forwarded stream to the exposed loopback port.
async fn expose(mut send: SendStream, mut recv: RecvStream) {
    let port = EXPOSED.load(Ordering::Relaxed);
    let local = match port {
        1..=65535 => TcpStream::connect(("127.0.0.1", port as u16)).await.ok(),
        _ => None,
    };
    let Some(local) = local else {
        count(CARRIED_EXPOSE_FAILED, 1);
        let _ = send.reset(1u32.into());
        let _ = recv.stop(1u32.into());
        return;
    };
    count(CARRIED_EXPOSE_STREAMS, 1);
    pipe(local, send, recv, CARRIED_EXPOSE_SENT, CARRIED_EXPOSE_RECEIVED).await;
}

/// **Carries one TCP connection over one QUIC stream**, both ways, counting
/// bytes into `sent` (TCP → QUIC) and `received` (QUIC → TCP).
///
/// A clean end in one direction is passed on as a half-close and the other
/// direction runs to its own end, which is how an HTTP request and its answer
/// finish. A failure in either direction ends both: the player abandoning a
/// range closes its socket, and the far side has to stop sending rather than
/// push the rest of a clip into a stream nobody reads.
async fn pipe(tcp: TcpStream, mut send: SendStream, mut recv: RecvStream, sent: usize, received: usize) {
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let up = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) => {
                    let _ = send.finish();
                    return true;
                }
                Ok(n) => {
                    if send.write_all(&buf[..n]).await.is_err() {
                        return false;
                    }
                    count(sent, n as u64);
                }
                Err(_) => {
                    let _ = send.reset(0u32.into());
                    return false;
                }
            }
        }
    };
    let down = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) => {
                    if tcp_write.write_all(&buf[..n]).await.is_err() {
                        let _ = recv.stop(0u32.into());
                        return false;
                    }
                    count(received, n as u64);
                }
                Ok(None) => {
                    let _ = tcp_write.shutdown().await;
                    return true;
                }
                Err(_) => return false,
            }
        }
    };
    tokio::pin!(up, down);
    tokio::select! {
        clean = &mut up => if clean { down.await; },
        clean = &mut down => if clean { up.await; },
    }
}

/// **Pipes one loopback port on this side to `id_hex`'s exposed port**, and
/// writes the port into `port_out`.
///
/// Binds `127.0.0.1` on a port the system picks — loopback only, so nothing on
/// the local network can use it — and carries each connection made to it over
/// its own stream on the held connection. The connection is looked up at each
/// accept, so a redial is picked up without asking for a new port. Asking again
/// for the same remote returns the same port.
///
/// It forwards bytes, not requests: whatever speaks to the port is speaking to
/// whatever the other side exposed. (v0.7.0)
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_forward(id_hex: *const c_char, port_out: *mut i32) -> i32 {
    if endpoint().is_none() {
        return ERR_NOT_STARTED;
    }
    let Some(id) = parse_id(id_hex) else {
        return ERR_BAD_ID;
    };
    if port_out.is_null() {
        return ERR_BUFFER;
    }
    if held(&id).is_none() {
        return ERR_NO_REMOTE;
    }
    let mut forwards = match FORWARDS.lock() {
        Ok(list) => list,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some((_, port, _)) = forwards.iter().find(|(known, _, task)| *known == id && !task.is_finished()) {
        // SAFETY: non-null, checked above.
        unsafe { *port_out = *port as i32 };
        return 0;
    }
    let Ok(listener) = runtime().block_on(TcpListener::bind(("127.0.0.1", 0))) else {
        return ERR_PORT;
    };
    let Ok(port) = listener.local_addr().map(|addr| addr.port()) else {
        return ERR_PORT;
    };
    let task = runtime().spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Some(conn) = held(&id).filter(|conn| conn.close_reason().is_none()) else {
                    count(CARRIED_FORWARD_FAILED, 1);
                    return;
                };
                let Ok((mut send, recv)) = conn.open_bi().await else {
                    count(CARRIED_FORWARD_FAILED, 1);
                    return;
                };
                if send.write_all(FORWARD_REQUEST).await.is_err() {
                    count(CARRIED_FORWARD_FAILED, 1);
                    return;
                }
                count(CARRIED_FORWARD_STREAMS, 1);
                pipe(tcp, send, recv, CARRIED_FORWARD_SENT, CARRIED_FORWARD_RECEIVED).await;
            });
        }
    });
    forwards.retain(|(known, _, _)| *known != id);
    forwards.push((id, port, task));
    // SAFETY: non-null, checked above.
    unsafe { *port_out = port as i32 };
    0
}

/// **Lets remotes reach one loopback port on this side**, through their
/// `apple_iroh_forward`. Zero exposes nothing, which is where it starts.
///
/// Only connections that are held — allowed in, or dialled from here — can
/// open a stream at all, and only to `127.0.0.1` on this one port: the remote
/// cannot name a host or choose a port. (v0.7.0)
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_expose(port: i32) -> i32 {
    if !(0..=65535).contains(&port) {
        return ERR_PORT;
    }
    EXPOSED.store(port, Ordering::Relaxed);
    0
}

/// Writes the `CARRIED_*` counters since the endpoint started, and returns
/// `CARRIED_FIELDS`. (v0.7.0)
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_carried(fields: *mut u64, cap: i32) -> i32 {
    if fields.is_null() || cap < CARRIED_FIELDS as i32 {
        return ERR_BUFFER;
    }
    for (index, slot) in CARRIED.iter().enumerate() {
        // SAFETY: non-null with at least CARRIED_FIELDS of room, checked above.
        unsafe { *fields.add(index) = slot.load(Ordering::Relaxed) };
    }
    CARRIED_FIELDS as i32
}

/// **Times bytes arriving from `id_hex`** for `duration_ms`, and writes how
/// many arrived in each `interval_ms` into `samples`.
///
/// Returns the number of intervals the pull covered. Fewer than
/// `ceil(duration_ms / interval_ms)` means the stream ended early — the other
/// side stopped serving, or the connection went — and the intervals after it
/// were not measured, which is not the same as nothing arriving in them.
///
/// `first_byte_ms`, when not null, gets the milliseconds from asking to the
/// first byte, or -1 if none came.
///
/// **Blocks** for the duration. The path can be sampled from another thread
/// while it runs, and should be: a throughput figure means nothing without
/// the path that carried it.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_pull(
    id_hex: *const c_char,
    duration_ms: i32,
    interval_ms: i32,
    samples: *mut u64,
    cap: i32,
    first_byte_ms: *mut i32,
) -> i32 {
    if endpoint().is_none() {
        return ERR_NOT_STARTED;
    }
    let Some(id) = parse_id(id_hex) else {
        return ERR_BAD_ID;
    };
    if duration_ms <= 0 || interval_ms <= 0 || samples.is_null() {
        return ERR_BUFFER;
    }
    let slots = ((duration_ms + interval_ms - 1) / interval_ms) as usize;
    if cap < 0 || (cap as usize) < slots {
        return ERR_BUFFER;
    }
    let Some(conn) = held(&id) else {
        return ERR_NO_REMOTE;
    };
    if conn.close_reason().is_some() {
        return ERR_CLOSED;
    }
    let interval = interval_ms as u128;
    let pulled = runtime().block_on(async move {
        let (mut send, mut recv) = conn.open_bi().await.map_err(|_| ERR_STREAM)?;
        send.write_all(PULL_REQUEST).await.map_err(|_| ERR_STREAM)?;
        send.finish().map_err(|_| ERR_STREAM)?;
        let began = Instant::now();
        let deadline = began + Duration::from_millis(duration_ms as u64);
        let mut buckets = vec![0u64; slots];
        let mut first = -1i32;
        let mut buf = vec![0u8; CHUNK];
        let mut covered = slots;
        loop {
            match tokio::time::timeout_at(deadline, recv.read(&mut buf)).await {
                Err(_) => break,
                Ok(Ok(Some(n))) => {
                    let at = began.elapsed().as_millis();
                    if first < 0 {
                        first = at as i32;
                    }
                    let slot = ((at / interval) as usize).min(slots - 1);
                    buckets[slot] += n as u64;
                }
                Ok(Ok(None)) | Ok(Err(_)) => {
                    if first < 0 {
                        return Err(ERR_STREAM);
                    }
                    let at = began.elapsed().as_millis();
                    covered = (at.div_ceil(interval) as usize).min(slots);
                    break;
                }
            }
        }
        let _ = recv.stop(0u32.into());
        Ok((buckets, first, covered))
    });
    match pulled {
        Ok((buckets, first, covered)) => {
            // SAFETY: non-null with at least `slots` of capacity, checked above.
            unsafe { std::ptr::copy_nonoverlapping(buckets.as_ptr(), samples, slots) };
            if !first_byte_ms.is_null() {
                // SAFETY: non-null, checked here.
                unsafe { *first_byte_ms = first };
            }
            covered as i32
        }
        Err(code) => code,
    }
}

fn endpoint() -> Option<Endpoint> {
    match ENDPOINT.lock() {
        Ok(slot) => slot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Writes this endpoint's id as 64 lowercase hex characters.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_endpoint_id(buf: *mut c_char, cap: i32) -> i32 {
    let Some(endpoint) = endpoint() else {
        return ERR_NOT_STARTED;
    };
    write_out(&hex(endpoint.id().as_bytes()), buf, cap)
}

/// Dials a remote by its hex id and holds the connection open.
///
/// **Blocks.** Call it off the main thread: a dial that has to reach a relay,
/// exchange addresses and attempt a hole punch is not a fast operation, and on
/// a bad network it is a slow one.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_connect(id_hex: *const c_char) -> i32 {
    let Some(endpoint) = endpoint() else {
        return ERR_NOT_STARTED;
    };
    let Some(id) = parse_id(id_hex) else {
        return ERR_BAD_ID;
    };
    let dialled = runtime().block_on(async move {
        endpoint.connect(EndpointAddr::new(id), ALPN).await
    });
    match dialled {
        Ok(conn) => {
            remember(id, conn);
            0
        }
        Err(_) => ERR_CONNECT,
    }
}

/// How the connection to `id_hex` is currently being carried, as
/// `PATH_RELAY | PATH_DIRECT`.
///
/// Zero means iroh knows the remote but has no address in active use for it.
/// `ERR_NO_REMOTE` means it has never heard of it, which is a different thing
/// and is worth telling apart when a dial is still in flight.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_path(id_hex: *const c_char) -> i32 {
    let Some(endpoint) = endpoint() else {
        return ERR_NOT_STARTED;
    };
    let Some(id) = parse_id(id_hex) else {
        return ERR_BAD_ID;
    };
    // **A closed connection first, before the addresses.** Measured: a dialler
    // refused by the other side's allow list got `connect -> 0`, and four
    // seconds later `remote_info` still listed an active relay address, so this
    // reported "relayed" for a connection that no longer existed. Addresses say
    // how a remote *could* be reached; only the connection says whether it is.
    if held_closed(&id) {
        return ERR_CLOSED;
    }
    let Some(info) = runtime().block_on(endpoint.remote_info(id)) else {
        return ERR_NO_REMOTE;
    };
    let mut flags = 0;
    for addr in info.addrs() {
        if !matches!(addr.usage(), iroh::endpoint::TransportAddrUsage::Active) {
            continue;
        }
        if addr.addr().is_relay() {
            flags |= PATH_RELAY;
        } else {
            flags |= PATH_DIRECT;
        }
    }
    flags
}

/// **Which path the held connection is sending on**: `PATH_RELAY`,
/// `PATH_DIRECT`, or zero when none is selected.
///
/// `apple_iroh_path` reports which addresses are *open*, and both are open for
/// most of a healthy session: iroh keeps the relay path up after a direct one
/// lands. Measured on a Mac dialling an iPhone on one network: 305 s of
/// "relay and direct" and never direct alone, so the address view cannot say
/// where the bytes went. The connection's selected path can.
///
/// `rtt_us`, when not null, gets that path's round-trip estimate in
/// microseconds — microseconds because a LAN's is under a millisecond.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_selected(id_hex: *const c_char, rtt_us: *mut i32) -> i32 {
    if endpoint().is_none() {
        return ERR_NOT_STARTED;
    }
    let Some(id) = parse_id(id_hex) else {
        return ERR_BAD_ID;
    };
    let Some(conn) = held(&id) else {
        return ERR_NO_REMOTE;
    };
    if conn.close_reason().is_some() {
        return ERR_CLOSED;
    }
    let paths = conn.paths();
    for path in paths.iter() {
        if !path.is_selected() {
            continue;
        }
        if !rtt_us.is_null() {
            let micros = path.rtt().as_micros().min(i32::MAX as u128) as i32;
            // SAFETY: non-null, checked here.
            unsafe { *rtt_us = micros };
        }
        return if path.is_relay() { PATH_RELAY } else { PATH_DIRECT };
    }
    0
}

/// Address kinds, from [`apple_iroh_selected_addr`]. A kind, never the
/// address: a reading is pasted into a ticket, and the finding is what sort of
/// address carried the bytes — a LAN, a tunnel's shared range, the public
/// internet — not which one.
pub const ADDR_UNKNOWN: i32 = 0;
pub const ADDR_RELAY: i32 = 1;
pub const ADDR_LOOPBACK: i32 = 2;
/// RFC 1918 IPv4, or an IPv6 unique local address (fc00::/7).
pub const ADDR_PRIVATE: i32 = 3;
/// 169.254/16 or fe80::/10.
pub const ADDR_LINK_LOCAL: i32 = 4;
/// 100.64/10: carrier-grade NAT, and also the range Tailscale assigns.
pub const ADDR_SHARED: i32 = 5;
pub const ADDR_PUBLIC: i32 = 6;
/// Added to a kind when the address is IPv6.
pub const ADDR_V6: i32 = 16;

fn ip_kind(ip: std::net::IpAddr) -> i32 {
    use std::net::IpAddr;
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            if v4.is_loopback() {
                ADDR_LOOPBACK
            } else if v4.is_private() {
                ADDR_PRIVATE
            } else if v4.is_link_local() {
                ADDR_LINK_LOCAL
            } else if a == 100 && (64..128).contains(&b) {
                ADDR_SHARED
            } else if v4.is_unspecified() {
                ADDR_UNKNOWN
            } else {
                ADDR_PUBLIC
            }
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            let kind = if v6.is_loopback() {
                ADDR_LOOPBACK
            } else if first & 0xfe00 == 0xfc00 {
                ADDR_PRIVATE
            } else if first & 0xffc0 == 0xfe80 {
                ADDR_LINK_LOCAL
            } else if v6.is_unspecified() {
                ADDR_UNKNOWN
            } else {
                ADDR_PUBLIC
            };
            kind + ADDR_V6
        }
    }
}

/// **What kind of address the selected path uses, at each end** (v0.8.0).
///
/// `remote_kind` gets the kind of the address bytes are sent to, and
/// `local_kind` the kind of this device's address on that path; either may be
/// null. Returns the same as [`apple_iroh_selected`]: `PATH_RELAY`,
/// `PATH_DIRECT`, or zero when nothing is selected, with both kinds left at
/// `ADDR_UNKNOWN`.
///
/// Built because a direct path on one Wi-Fi read a 30 ms round trip, and
/// nothing could say whether it crossed the LAN, a VPN tunnel, or went out to
/// the public address and back.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_selected_addr(
    id_hex: *const c_char,
    remote_kind: *mut i32,
    local_kind: *mut i32,
) -> i32 {
    if endpoint().is_none() {
        return ERR_NOT_STARTED;
    }
    let Some(id) = parse_id(id_hex) else {
        return ERR_BAD_ID;
    };
    let Some(conn) = held(&id) else {
        return ERR_NO_REMOTE;
    };
    if conn.close_reason().is_some() {
        return ERR_CLOSED;
    }
    let mut remote = ADDR_UNKNOWN;
    let mut local = ADDR_UNKNOWN;
    let mut selected = 0;
    for path in conn.paths().iter() {
        if !path.is_selected() {
            continue;
        }
        if path.is_relay() {
            selected = PATH_RELAY;
            remote = ADDR_RELAY;
            local = ADDR_RELAY;
        } else {
            selected = PATH_DIRECT;
            if let iroh::TransportAddr::Ip(addr) = path.remote_addr() {
                remote = ip_kind(addr.ip());
            }
            if let iroh::endpoint::LocalTransportAddr::Ip(Some(ip)) = path.local_addr() {
                local = ip_kind(*ip);
            }
        }
        break;
    }
    if !remote_kind.is_null() {
        // SAFETY: non-null, checked here.
        unsafe { *remote_kind = remote };
    }
    if !local_kind.is_null() {
        // SAFETY: non-null, checked here.
        unsafe { *local_kind = local };
    }
    selected
}

/// The relay carrying this remote, or zero bytes written when none is active.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_relay(id_hex: *const c_char, buf: *mut c_char, cap: i32) -> i32 {
    let Some(endpoint) = endpoint() else {
        return ERR_NOT_STARTED;
    };
    let Some(id) = parse_id(id_hex) else {
        return ERR_BAD_ID;
    };
    let Some(info) = runtime().block_on(endpoint.remote_info(id)) else {
        return ERR_NO_REMOTE;
    };
    for addr in info.addrs() {
        if !matches!(addr.usage(), iroh::endpoint::TransportAddrUsage::Active) {
            continue;
        }
        if let iroh::TransportAddr::Relay(url) = addr.addr() {
            return write_out(&url.to_string(), buf, cap);
        }
    }
    0
}

/// Slots `apple_iroh_net_report` writes, in order. Tri-states are `-1`
/// unknown, `0` no, `1` yes.
pub const NET_UDP_V4: usize = 0;
pub const NET_UDP_V6: usize = 1;
pub const NET_VARIES_V4: usize = 2;
pub const NET_VARIES_V6: usize = 3;
pub const NET_PUBLIC_V4: usize = 4;
pub const NET_PUBLIC_V6: usize = 5;
pub const NET_CAPTIVE: usize = 6;
pub const NET_RELAY_US: usize = 7;
pub const NET_FIELDS: i32 = 8;

/// **What iroh last learned about the network this endpoint is on**, as eight
/// integers: whether UDP gets out over IPv4 and IPv6, whether the public
/// address changes with the server asked (the NAT behaviour that makes a hole
/// punch hardest), whether a public IPv4 and IPv6 address was seen, whether a
/// captive portal is suspected, and the round trip to the preferred relay in
/// microseconds (`-1` if none).
///
/// Returns `NET_FIELDS` when a report exists, `0` when iroh has not finished
/// its first one, `ERR_BUFFER` when `cap` is short. **The addresses themselves
/// never cross**: a reading gets pasted into a ticket, and whether a public
/// address exists is the finding, not which one it is.
///
/// iroh re-runs the report on its own schedule and when the network changes, so
/// polling this is how a caller sees the network move.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_net_report(fields: *mut i32, cap: i32) -> i32 {
    let Some(endpoint) = endpoint() else {
        return ERR_NOT_STARTED;
    };
    if fields.is_null() || cap < NET_FIELDS {
        return ERR_BUFFER;
    }
    let mut watcher = endpoint.net_report();
    let Some(report) = iroh::Watcher::get(&mut watcher) else {
        return 0;
    };
    let tri = |value: Option<bool>| match value {
        None => -1,
        Some(false) => 0,
        Some(true) => 1,
    };
    let relay = report.preferred_relay.as_ref().and_then(|url| {
        report
            .relay_latency
            .iter()
            .filter(|(_, seen, _)| *seen == url)
            .map(|(_, _, latency)| latency)
            .min()
    });
    let mut out = [0i32; NET_FIELDS as usize];
    out[NET_UDP_V4] = report.udp_v4 as i32;
    out[NET_UDP_V6] = report.udp_v6 as i32;
    out[NET_VARIES_V4] = tri(report.mapping_varies_by_dest_ipv4);
    out[NET_VARIES_V6] = tri(report.mapping_varies_by_dest_ipv6);
    out[NET_PUBLIC_V4] = report.global_v4.is_some() as i32;
    out[NET_PUBLIC_V6] = report.global_v6.is_some() as i32;
    out[NET_CAPTIVE] = tri(report.captive_portal);
    out[NET_RELAY_US] = relay.map_or(-1, |d| d.as_micros().min(i32::MAX as u128) as i32);
    // SAFETY: non-null with at least NET_FIELDS of capacity, checked above.
    unsafe { std::ptr::copy_nonoverlapping(out.as_ptr(), fields, NET_FIELDS as usize) };
    NET_FIELDS
}

/// Drops every held connection and the endpoint.
///
/// The tokio runtime is deliberately **not** torn down: it is a `OnceLock` for
/// the life of the process, so starting again after stopping reuses it rather
/// than leaving a thread pool behind on every cycle.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_stop() {
    if let Ok(mut forwards) = FORWARDS.lock() {
        for (_, _, task) in forwards.drain(..) {
            task.abort();
        }
    }
    EXPOSED.store(0, Ordering::Relaxed);
    if let Ok(mut held) = DIALLED.lock() {
        held.clear();
    }
    let mut slot = match ENDPOINT.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(endpoint) = slot.take() {
        runtime().block_on(endpoint.close());
    }
}

// MARK: - Crossing the boundary

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A 64-character hex id, or nil for anything else — including a null pointer,
/// invalid UTF-8, the wrong length, and 32 bytes that are not a valid key.
fn parse_id(id_hex: *const c_char) -> Option<EndpointId> {
    if id_hex.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(id_hex) }.to_str().ok()?;
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    EndpointId::from_bytes(&bytes).ok()
}

/// Writes UTF-8 with no trailing NUL and returns the byte count, or
/// `ERR_BUFFER` if it would not fit. **Never truncates**: half an endpoint id
/// is a different endpoint id, and a caller that got one would spend a long
/// time looking for the wrong bug.
fn write_out(text: &str, buf: *mut c_char, cap: i32) -> i32 {
    if buf.is_null() || cap < 0 {
        return ERR_BUFFER;
    }
    let bytes = text.as_bytes();
    if bytes.len() > cap as usize {
        return ERR_BUFFER;
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len()) };
    bytes.len() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(text: &str) -> i32 {
        ip_kind(text.parse().unwrap())
    }

    #[test]
    fn address_kinds_by_range() {
        assert_eq!(kind("127.0.0.1"), ADDR_LOOPBACK);
        assert_eq!(kind("192.168.1.20"), ADDR_PRIVATE);
        assert_eq!(kind("10.0.0.4"), ADDR_PRIVATE);
        assert_eq!(kind("172.31.255.1"), ADDR_PRIVATE);
        assert_eq!(kind("172.32.0.1"), ADDR_PUBLIC);
        assert_eq!(kind("169.254.3.3"), ADDR_LINK_LOCAL);
        assert_eq!(kind("100.64.0.1"), ADDR_SHARED);
        assert_eq!(kind("100.127.255.254"), ADDR_SHARED);
        assert_eq!(kind("100.128.0.1"), ADDR_PUBLIC);
        assert_eq!(kind("100.63.255.255"), ADDR_PUBLIC);
        assert_eq!(kind("8.8.8.8"), ADDR_PUBLIC);
        assert_eq!(kind("::1"), ADDR_LOOPBACK + ADDR_V6);
        assert_eq!(kind("fd7a:115c:a1e0::1"), ADDR_PRIVATE + ADDR_V6);
        assert_eq!(kind("fe80::1"), ADDR_LINK_LOCAL + ADDR_V6);
        assert_eq!(kind("2001:db8::1"), ADDR_PUBLIC + ADDR_V6);
        // A v4 address carried in v6 form is the v4 address.
        assert_eq!(kind("::ffff:192.168.1.20"), ADDR_PRIVATE);
    }
}
