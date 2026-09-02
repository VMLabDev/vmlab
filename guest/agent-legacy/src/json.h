/* Minimal JSON tokenizer and reader for the handful of control messages the
 * legacy agent answers (guest/agent-proto, PRD §7.4). C89, no allocation:
 * the caller supplies a token array, and values are read out of the source
 * text by index. Modelled on jsmn's shape without depending on it.
 */
#ifndef VMLAB_JSON_H
#define VMLAB_JSON_H

#include <stddef.h>

enum json_type {
    JSON_UNDEFINED = 0,
    JSON_OBJECT,
    JSON_ARRAY,
    JSON_STRING,
    JSON_PRIMITIVE /* number, true, false, null */
};

struct json_tok {
    enum json_type type;
    int start;  /* byte offset of the first character */
    int end;    /* byte offset one past the last character */
    int size;   /* number of direct children (pairs for an object) */
    int parent; /* token index of the container, -1 at the root */
};

/* Tokenize `len` bytes of `js`. Returns the token count, or -1 when the text
 * is malformed or `cap` tokens do not suffice. */
int json_parse(const char *js, int len, struct json_tok *toks, int cap);

/* Index of the token following the whole subtree rooted at `i`. */
int json_skip(const struct json_tok *toks, int ntok, int i);

/* Value token of `key` in object token `obj`, or -1. */
int json_get(const char *js, const struct json_tok *toks, int ntok, int obj,
             const char *key);

/* Whether string token `i` equals `s` (no unescaping: keys and enum values
 * on this wire are plain ASCII). */
int json_streq(const char *js, const struct json_tok *toks, int i, const char *s);

/* Copy string token `i` into `out` (capacity `cap`, always NUL-terminated),
 * decoding escapes; \uXXXX becomes UTF-8. Returns the decoded length, or -1
 * when it does not fit. */
int json_str(const char *js, const struct json_tok *toks, int i, char *out, int cap);

/* Unsigned integer value of primitive token `i`. Returns 0 on success. */
int json_ulong(const char *js, const struct json_tok *toks, int i, unsigned long *out);

/* Append JSON text to a buffer. `json_out` tracks overflow so callers check
 * once, at the end. */
struct json_out {
    char *buf;
    int cap;
    int len;
    int overflow;
};

void jo_init(struct json_out *o, char *buf, int cap);
void jo_raw(struct json_out *o, const char *s);
void jo_str(struct json_out *o, const char *s); /* quoted + escaped */
void jo_ulong(struct json_out *o, unsigned long v);
void jo_long(struct json_out *o, long v);
/* `"key":` */
void jo_key(struct json_out *o, const char *key);

#endif
