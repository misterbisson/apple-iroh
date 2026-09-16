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

/* Path flags. A bitmask, not an enum: both can be true at once, and that state
 * is a relay connection in the middle of upgrading to a direct one. */
#define APPLE_IROH_PATH_RELAY  (1)
#define APPLE_IROH_PATH_DIRECT (2)

/* Binds an endpoint and starts accepting. Accepting matters even on a side that
 * only dials, because hole punching is two-sided. */
int32_t apple_iroh_start(void);

/* Writes this endpoint's id, 64 hex characters. */
int32_t apple_iroh_endpoint_id(char *buf, int32_t cap);

/* Dials a remote and holds the connection open. BLOCKS — call it off the main
 * thread. A dial that reaches a relay, exchanges addresses and attempts a hole
 * punch is not fast, and on a bad network it is slow. */
int32_t apple_iroh_connect(const char *id_hex);

/* How the connection is currently carried: APPLE_IROH_PATH_RELAY |
 * APPLE_IROH_PATH_DIRECT. Zero means the remote is known with no address in
 * active use; APPLE_IROH_ERR_NO_REMOTE means it has never been heard of, which
 * is a different thing while a dial is in flight. */
int32_t apple_iroh_path(const char *id_hex);

/* The relay carrying this remote, or zero bytes written when none is active. */
int32_t apple_iroh_relay(const char *id_hex, char *buf, int32_t cap);

/* Drops every held connection and the endpoint. */
void apple_iroh_stop(void);

#ifdef __cplusplus
}
#endif

#endif /* APPLE_IROH_H */
