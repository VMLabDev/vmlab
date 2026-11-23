/* See wire.h. */
#include "wire.h"

#include <string.h>

static void put_u32(unsigned char *p, unsigned long v)
{
    p[0] = (unsigned char)(v & 0xFF);
    p[1] = (unsigned char)((v >> 8) & 0xFF);
    p[2] = (unsigned char)((v >> 16) & 0xFF);
    p[3] = (unsigned char)((v >> 24) & 0xFF);
}

static unsigned long get_u32(const unsigned char *p)
{
    return (unsigned long)p[0] | ((unsigned long)p[1] << 8) |
           ((unsigned long)p[2] << 16) | ((unsigned long)p[3] << 24);
}

void wire_header(unsigned char *hdr, int kind, unsigned long channel, unsigned long len)
{
    memcpy(hdr, WIRE_MAGIC, 4);
    put_u32(hdr + 4, len);
    hdr[8] = (unsigned char)kind;
    put_u32(hdr + 9, channel);
}

void wire_decoder_init(struct wire_decoder *d, unsigned char *buf, unsigned long cap,
                       wire_restart_fn restart)
{
    d->buf = buf;
    d->cap = cap;
    d->len = 0;
    d->scan = 0;
    d->restart = restart;
}

unsigned long wire_room(const struct wire_decoder *d)
{
    return d->cap - d->len;
}

void wire_push(struct wire_decoder *d, const unsigned char *bytes, unsigned long n)
{
    if (n > wire_room(d))
        n = wire_room(d);
    memcpy(d->buf + d->len, bytes, (size_t)n);
    d->len += n;
}

/* Drop `n` leading bytes. */
static void drop(struct wire_decoder *d, unsigned long n)
{
    d->scan = 0;
    if (n >= d->len) {
        d->len = 0;
        return;
    }
    memmove(d->buf, d->buf + n, (size_t)(d->len - n));
    d->len -= n;
}

/* Skip to the next magic, or to the last three bytes when none is in view
 * (a magic may be split across pushes). */
static void resync(struct wire_decoder *d)
{
    unsigned long i;
    for (i = 0; i + 4 <= d->len; i++) {
        if (memcmp(d->buf + i, WIRE_MAGIC, 4) == 0) {
            drop(d, i);
            return;
        }
    }
    drop(d, d->len > 3 ? d->len - 3 : 0);
}

/* The frame at the head of the buffer ends at `end`, whole or not. Look
 * inside it for a whole control frame the caller takes as a restart, and
 * drop everything before one. A host that was cut off in a frame leaves its
 * header behind, and the next connection's bytes would otherwise be read as
 * that frame's payload (wire.h). Each byte is looked at once: `scan`
 * remembers how far the search got. Returns 1 when the buffer now starts at
 * the restart. */
static int restart_inside(struct wire_decoder *d, unsigned long end)
{
    unsigned long i, clen, resume = 0;
    if (end > d->len)
        end = d->len;
    for (i = d->scan ? d->scan : 1; i < end && i + 4 <= d->len; i++) {
        if (d->buf[i] != (unsigned char)WIRE_MAGIC[0] ||
            memcmp(d->buf + i, WIRE_MAGIC, 4) != 0)
            continue;
        if (i + WIRE_HEADER_LEN <= d->len) {
            clen = get_u32(d->buf + i + 4);
            if (d->buf[i + 8] != WIRE_KIND_CTRL || get_u32(d->buf + i + 9) != 0 ||
                clen > WIRE_RESTART_MAX)
                continue;
            if (i + WIRE_HEADER_LEN + clen <= d->len) {
                if (d->restart(d->buf + i + WIRE_HEADER_LEN, clen)) {
                    drop(d, i);
                    return 1;
                }
                continue;
            }
        }
        /* Not all here yet: look at it again when more has arrived. */
        if (!resume)
            resume = i;
    }
    d->scan = resume ? resume : i;
    return 0;
}

int wire_next(struct wire_decoder *d, int *kind, unsigned long *channel,
              const unsigned char **payload, unsigned long *len)
{
    for (;;) {
        unsigned long plen;
        if (d->len < 4)
            return 0;
        if (memcmp(d->buf, WIRE_MAGIC, 4) != 0) {
            resync(d);
            continue;
        }
        if (d->len < WIRE_HEADER_LEN)
            return 0;
        plen = get_u32(d->buf + 4);
        if (plen > WIRE_MAX_PAYLOAD || d->buf[8] > WIRE_KIND_DATA_ERR) {
            /* Not a real header: step past this magic and rescan. */
            drop(d, 1);
            resync(d);
            continue;
        }
        if (d->restart && restart_inside(d, WIRE_HEADER_LEN + plen))
            continue;
        if (d->len < WIRE_HEADER_LEN + plen)
            return 0;
        *kind = d->buf[8];
        *channel = get_u32(d->buf + 9);
        *payload = d->buf + WIRE_HEADER_LEN;
        *len = plen;
        return 1;
    }
}

void wire_consume(struct wire_decoder *d)
{
    unsigned long plen;
    if (d->len < WIRE_HEADER_LEN)
        return;
    plen = get_u32(d->buf + 4);
    drop(d, WIRE_HEADER_LEN + plen);
}

void wire_reject(struct wire_decoder *d)
{
    drop(d, 1);
}
