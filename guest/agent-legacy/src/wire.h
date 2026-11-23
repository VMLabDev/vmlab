/* The vmlab agent frame layer (guest/agent-proto/src/lib.rs):
 *
 *   magic "VMLB" | len u32 LE | kind u8 | channel u32 LE | payload
 *
 * Channel 0 carries JSON control messages; every other channel is a byte
 * stream. The decoder resynchronises by scanning to the next magic, which
 * is what lets a serial line that dropped or replayed bytes (an online
 * snapshot restore) recover without a reset.
 *
 * A frame the host was cut off in is the case a magic scan alone does not
 * cover: the header arrived, the rest never will, and a serial line gives
 * no sign that the host hung up. The next connection's bytes would be taken
 * as the missing payload. So the decoder also looks inside a frame for a
 * whole control frame that the caller's `restart` test accepts (the agent
 * accepts a hello, which starts every connection) and, on one, drops what
 * came before it. The price: payload bytes that hold a complete, valid
 * hello frame are taken for a new connection.
 */
#ifndef VMLAB_WIRE_H
#define VMLAB_WIRE_H

#define WIRE_MAGIC "VMLB"
#define WIRE_HEADER_LEN 13
#define WIRE_PROTO_VERSION 2UL
/* The protocol's hard cap on one payload. The agent sends much smaller
 * frames than this; it only has to *accept* one this big. */
#define WIRE_MAX_PAYLOAD (64UL * 1024UL)
/* Credit both sides start every channel with, and the point at which a
 * receiver tops the sender back up. */
#define WIRE_INITIAL_WINDOW (256UL * 1024UL)
#define WIRE_WINDOW_REPLENISH (WIRE_INITIAL_WINDOW / 2UL)

#define WIRE_KIND_CTRL 0
#define WIRE_KIND_DATA 1
#define WIRE_KIND_DATA_ERR 2

/* Write a 13-byte header for `len` payload bytes into `hdr`. */
void wire_header(unsigned char *hdr, int kind, unsigned long channel, unsigned long len);

/* The longest control payload looked at as a restart. A hello is under
 * 100 bytes plus its token. */
#define WIRE_RESTART_MAX 1024UL

/* Nonzero when a control payload starts a new connection. */
typedef int (*wire_restart_fn)(const unsigned char *payload, unsigned long len);

/* Incremental decoder over a caller-owned buffer. */
struct wire_decoder {
    unsigned char *buf;
    unsigned long cap;
    unsigned long len;
    /* Bytes before this offset hold no restart; zero after the buffer
     * moved. */
    unsigned long scan;
    wire_restart_fn restart;
};

/* `restart` may be null: then a frame cut short is waited for in full. */
void wire_decoder_init(struct wire_decoder *d, unsigned char *buf, unsigned long cap,
                       wire_restart_fn restart);

/* Room left for `wire_push`; zero means a frame is pending and must be
 * drained first. */
unsigned long wire_room(const struct wire_decoder *d);

/* Append received bytes (at most `wire_room`). */
void wire_push(struct wire_decoder *d, const unsigned char *bytes, unsigned long n);

/* Pop the next complete frame. On success the payload pointer aliases the
 * decoder's buffer and stays valid until the next push or pop. Returns 1
 * for a frame, 0 for none yet. Garbage between frames is skipped. */
int wire_next(struct wire_decoder *d, int *kind, unsigned long *channel,
              const unsigned char **payload, unsigned long *len);

/* Consume the frame `wire_next` returned. */
void wire_consume(struct wire_decoder *d);

/* The frame `wire_next` returned was not a message: step past its magic
 * only, so a frame that starts inside it is still found. */
void wire_reject(struct wire_decoder *d);

#endif
