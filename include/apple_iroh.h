/* apple-iroh — iroh as a static library for Apple platforms.
 *
 * Every function returns int32_t. Zero or positive is success; negative is one
 * of the APPLE_IROH_ERR_* values. Nothing panics across this boundary: the Rust
 * side is built with panic = "abort", so failures come back as codes.
 *
 * Endpoint ids cross as 64 lowercase hex characters. String-out functions write
 * UTF-8 with NO trailing NUL, return the number of bytes written, and return
 * APPLE_IROH_ERR_BUFFER rather than truncating.
 */
#ifndef APPLE_IROH_H
#define APPLE_IROH_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define APPLE_IROH_ERR_NOT_STARTED     (-1)
#define APPLE_IROH_ERR_ALREADY_STARTED (-2)
#define APPLE_IROH_ERR_BIND            (-3)
#define APPLE_IROH_ERR_BAD_ID          (-4)
#define APPLE_IROH_ERR_CONNECT         (-5)
#define APPLE_IROH_ERR_BUFFER          (-6)
#define APPLE_IROH_ERR_NO_REMOTE       (-7)
#define APPLE_IROH_ERR_BAD_KEY         (-8)
#define APPLE_IROH_ERR_CLOSED          (-9)
#define APPLE_IROH_ERR_STREAM          (-10)

/* A secret key is exactly this many bytes. */
#define APPLE_IROH_SECRET_KEY_LEN      (32)

/* Path flags. A bitmask, not an enum: both can be true at once, and that state
 * is a relay connection in the middle of upgrading to a direct one. */
#define APPLE_IROH_PATH_RELAY  (1)
#define APPLE_IROH_PATH_DIRECT (2)

/* Binds an endpoint and starts accepting — from ids on the allow list only.
 * Accepting matters even on a side that only dials, because hole punching is
 * two-sided. Generates a new key, and so a new id, every call. */
int32_t apple_iroh_start(void);

/* Binds under a caller-kept secret key, so the endpoint id is the same on every
 * launch. The id is the public half of the key; apple_iroh_start generates a
 * new key, and so a new id, every time. len must be APPLE_IROH_SECRET_KEY_LEN. */
int32_t apple_iroh_start_with_secret(const uint8_t *key, int32_t len);

/* Writes the running endpoint's 32-byte secret key, and returns 32. Whoever
 * holds these bytes can answer as this endpoint: keep them where only this
 * device can read them. */
int32_t apple_iroh_secret_key(uint8_t *buf, int32_t cap);

/* Writes this endpoint's id, 64 hex characters. */
int32_t apple_iroh_endpoint_id(char *buf, int32_t cap);

/* Dials a remote and holds the connection open. BLOCKS — call it off the main
 * thread. A dial that reaches a relay, exchanges addresses and attempts a hole
 * punch is not fast, and on a bad network it is slow. */
int32_t apple_iroh_connect(const char *id_hex);

/* How the connection is currently carried: APPLE_IROH_PATH_RELAY |
 * APPLE_IROH_PATH_DIRECT. Zero means the remote is known with no address in
 * active use; APPLE_IROH_ERR_NO_REMOTE means it has never been heard of, which
 * is a different thing while a dial is in flight. APPLE_IROH_ERR_CLOSED means
 * the held connection has closed — including refused by the other side's allow
 * list, which a successful connect does not rule out. */
int32_t apple_iroh_path(const char *id_hex);

/* Which path the held connection is SENDING on: APPLE_IROH_PATH_RELAY or
 * APPLE_IROH_PATH_DIRECT, or 0 when none is selected. apple_iroh_path reports
 * open addresses, and relay and direct are usually both open; this is where the
 * bytes go. rtt_us (nullable) gets that path's round-trip estimate in
 * microseconds. APPLE_IROH_ERR_NO_REMOTE when no connection is held. (v0.5.0) */
int32_t apple_iroh_selected(const char *id_hex, int32_t *rtt_us);

/* The relay carrying this remote, or zero bytes written when none is active. */
int32_t apple_iroh_relay(const char *id_hex, char *buf, int32_t cap);

/* Who may connect in. The list starts EMPTY, and empty means nobody: an
 * endpoint that was never told whom to accept refuses everyone. Dials out are
 * not checked against it — a dial names its remote and the handshake proves
 * the remote is that one. */
int32_t apple_iroh_allow(const char *id_hex);
void apple_iroh_allow_none(void);

/* Incoming connections refused because their id was not allowed. Tells "nobody
 * arrived" apart from "somebody arrived and was turned away". */
int32_t apple_iroh_refused(void);

/* Times bytes arriving from a held connection's remote for duration_ms, and
 * writes how many arrived in each interval_ms into samples, which needs room
 * for ceil(duration_ms / interval_ms). Returns the intervals covered: fewer
 * than that means the stream ended early, and the rest were NOT measured.
 * first_byte_ms (nullable) gets ms from asking to the first byte, or -1.
 *
 * The remote answers with zeros until the reader stops, so the reading is the
 * path and not a disk. Both ends answer pulls. BLOCKS for the duration — sample
 * apple_iroh_path from another thread meanwhile, because a figure without the
 * path that carried it means nothing. Needs v0.4.0 at both ends; the protocol
 * name changed so an older endpoint fails to connect rather than never answer. */
int32_t apple_iroh_pull(const char *id_hex, int32_t duration_ms, int32_t interval_ms,
                        uint64_t *samples, int32_t cap, int32_t *first_byte_ms);

/* Drops every held connection and the endpoint. */
void apple_iroh_stop(void);

#ifdef __cplusplus
}
#endif

#endif /* APPLE_IROH_H */
