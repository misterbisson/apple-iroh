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
use std::sync::{Mutex, OnceLock};

use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use tokio::runtime::Runtime;

/// The protocol name two endpoints have to agree on before QUIC will talk.
/// Versioned from the start: a v0 endpoint meeting a v1 endpoint should fail to
/// negotiate rather than half-work.
const ALPN: &[u8] = b"apple-iroh/probe/0";

pub const ERR_NOT_STARTED: i32 = -1;
pub const ERR_ALREADY_STARTED: i32 = -2;
pub const ERR_BIND: i32 = -3;
pub const ERR_BAD_ID: i32 = -4;
pub const ERR_CONNECT: i32 = -5;
pub const ERR_BUFFER: i32 = -6;
pub const ERR_NO_REMOTE: i32 = -7;

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
    let mut slot = match ENDPOINT.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    if slot.is_some() {
        return ERR_ALREADY_STARTED;
    }
    let built = runtime().block_on(async {
        Endpoint::builder(presets::N0)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
    });
    let endpoint = match built {
        Ok(endpoint) => endpoint,
        Err(_) => return ERR_BIND,
    };

    // Accept in the background and hold whatever arrives. The probe has no
    // protocol yet; what it needs is that a connection exists and stays up so
    // its path can be sampled.
    let accepting = endpoint.clone();
    runtime().spawn(async move {
        while let Some(incoming) = accepting.accept().await {
            tokio::spawn(async move {
                if let Ok(conn) = incoming.await {
                    let id = conn.remote_id();
                    remember(id, conn);
                }
            });
        }
    });

    *slot = Some(endpoint);
    0
}

fn remember(id: EndpointId, conn: Connection) {
    let mut held = match DIALLED.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.retain(|(known, _)| *known != id);
    held.push((id, conn));
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

/// Drops every held connection and the endpoint.
///
/// The tokio runtime is deliberately **not** torn down: it is a `OnceLock` for
/// the life of the process, so starting again after stopping reuses it rather
/// than leaving a thread pool behind on every cycle.
#[unsafe(no_mangle)]
pub extern "C" fn apple_iroh_stop() {
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
