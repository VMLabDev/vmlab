/* See json.h. */
#include "json.h"

#include <string.h>

struct parser {
    const char *js;
    int len;
    int pos;
    struct json_tok *toks;
    int cap;
    int ntok;
    int parent;
};

static int alloc_tok(struct parser *p, enum json_type type, int start)
{
    struct json_tok *t;
    if (p->ntok >= p->cap)
        return -1;
    t = &p->toks[p->ntok];
    t->type = type;
    t->start = start;
    t->end = -1;
    t->size = 0;
    t->parent = p->parent;
    if (p->parent >= 0)
        p->toks[p->parent].size++;
    return p->ntok++;
}

static int is_ws(char c)
{
    return c == ' ' || c == '\t' || c == '\n' || c == '\r';
}

static int parse_string(struct parser *p)
{
    int start = p->pos;
    int i;
    p->pos++; /* opening quote */
    for (; p->pos < p->len; p->pos++) {
        char c = p->js[p->pos];
        if (c == '"') {
            i = alloc_tok(p, JSON_STRING, start + 1);
            if (i < 0)
                return -1;
            p->toks[i].end = p->pos;
            p->pos++;
            return 0;
        }
        if (c == '\\') {
            p->pos++;
            if (p->pos >= p->len)
                return -1;
            if (p->js[p->pos] == 'u') {
                if (p->pos + 4 >= p->len)
                    return -1;
                p->pos += 4;
            }
        }
    }
    return -1;
}

static int parse_primitive(struct parser *p)
{
    int start = p->pos;
    int i;
    for (; p->pos < p->len; p->pos++) {
        char c = p->js[p->pos];
        if (is_ws(c) || c == ',' || c == ']' || c == '}')
            break;
    }
    i = alloc_tok(p, JSON_PRIMITIVE, start);
    if (i < 0)
        return -1;
    p->toks[i].end = p->pos;
    return 0;
}

/* Recursive descent over one value. Depth is bounded by the message shapes
 * on this wire (three levels), so recursion is safe on every target. */
static int parse_value(struct parser *p)
{
    while (p->pos < p->len && is_ws(p->js[p->pos]))
        p->pos++;
    if (p->pos >= p->len)
        return -1;
    switch (p->js[p->pos]) {
    case '"':
        return parse_string(p);
    case '{':
    case '[': {
        int is_obj = p->js[p->pos] == '{';
        int me = alloc_tok(p, is_obj ? JSON_OBJECT : JSON_ARRAY, p->pos);
        int saved = p->parent;
        if (me < 0)
            return -1;
        p->pos++;
        p->parent = me;
        for (;;) {
            while (p->pos < p->len && is_ws(p->js[p->pos]))
                p->pos++;
            if (p->pos >= p->len)
                return -1;
            if (p->js[p->pos] == (is_obj ? '}' : ']')) {
                p->pos++;
                break;
            }
            if (p->toks[me].size > 0) {
                if (p->js[p->pos] != ',')
                    return -1;
                p->pos++;
                while (p->pos < p->len && is_ws(p->js[p->pos]))
                    p->pos++;
            }
            if (is_obj) {
                int key;
                if (p->pos >= p->len || p->js[p->pos] != '"')
                    return -1;
                if (parse_string(p) < 0)
                    return -1;
                key = p->ntok - 1;
                while (p->pos < p->len && is_ws(p->js[p->pos]))
                    p->pos++;
                if (p->pos >= p->len || p->js[p->pos] != ':')
                    return -1;
                p->pos++;
                /* The value hangs off the key, jsmn-style: a key has size 1. */
                p->parent = key;
                if (parse_value(p) < 0)
                    return -1;
                p->parent = me;
            } else {
                if (parse_value(p) < 0)
                    return -1;
            }
        }
        p->toks[me].end = p->pos;
        p->parent = saved;
        return 0;
    }
    default:
        return parse_primitive(p);
    }
}

int json_parse(const char *js, int len, struct json_tok *toks, int cap)
{
    struct parser p;
    p.js = js;
    p.len = len;
    p.pos = 0;
    p.toks = toks;
    p.cap = cap;
    p.ntok = 0;
    p.parent = -1;
    if (parse_value(&p) < 0)
        return -1;
    while (p.pos < p.len && is_ws(p.js[p.pos]))
        p.pos++;
    if (p.pos != p.len)
        return -1;
    return p.ntok;
}

int json_skip(const struct json_tok *toks, int ntok, int i)
{
    int end;
    if (i < 0 || i >= ntok)
        return ntok;
    end = toks[i].end;
    i++;
    while (i < ntok && toks[i].start < end)
        i++;
    return i;
}

int json_get(const char *js, const struct json_tok *toks, int ntok, int obj,
             const char *key)
{
    int i;
    if (obj < 0 || obj >= ntok || toks[obj].type != JSON_OBJECT)
        return -1;
    i = obj + 1;
    while (i < ntok && toks[i].parent == obj) {
        /* i is a key; its value is i+1 */
        if (json_streq(js, toks, i, key) && i + 1 < ntok)
            return i + 1;
        i = json_skip(toks, ntok, i + 1);
    }
    return -1;
}

int json_streq(const char *js, const struct json_tok *toks, int i, const char *s)
{
    int n = (int)strlen(s);
    if (toks[i].type != JSON_STRING)
        return 0;
    return toks[i].end - toks[i].start == n && memcmp(js + toks[i].start, s, n) == 0;
}

static int hexval(char c)
{
    if (c >= '0' && c <= '9')
        return c - '0';
    if (c >= 'a' && c <= 'f')
        return c - 'a' + 10;
    if (c >= 'A' && c <= 'F')
        return c - 'A' + 10;
    return -1;
}

static int put(char *out, int cap, int *n, char c)
{
    if (*n + 1 >= cap)
        return -1;
    out[(*n)++] = c;
    return 0;
}

static int put_utf8(char *out, int cap, int *n, unsigned long cp)
{
    if (cp < 0x80)
        return put(out, cap, n, (char)cp);
    if (cp < 0x800)
        return put(out, cap, n, (char)(0xC0 | (cp >> 6))) ||
               put(out, cap, n, (char)(0x80 | (cp & 0x3F)));
    if (cp < 0x10000)
        return put(out, cap, n, (char)(0xE0 | (cp >> 12))) ||
               put(out, cap, n, (char)(0x80 | ((cp >> 6) & 0x3F))) ||
               put(out, cap, n, (char)(0x80 | (cp & 0x3F)));
    return put(out, cap, n, (char)(0xF0 | (cp >> 18))) ||
           put(out, cap, n, (char)(0x80 | ((cp >> 12) & 0x3F))) ||
           put(out, cap, n, (char)(0x80 | ((cp >> 6) & 0x3F))) ||
           put(out, cap, n, (char)(0x80 | (cp & 0x3F)));
}

int json_str(const char *js, const struct json_tok *toks, int i, char *out, int cap)
{
    int p, n = 0;
    if (cap <= 0)
        return -1;
    if (toks[i].type != JSON_STRING) {
        out[0] = 0;
        return -1;
    }
    for (p = toks[i].start; p < toks[i].end; p++) {
        char c = js[p];
        if (c != '\\') {
            if (put(out, cap, &n, c))
                goto full;
            continue;
        }
        p++;
        switch (js[p]) {
        case 'n': c = '\n'; break;
        case 't': c = '\t'; break;
        case 'r': c = '\r'; break;
        case 'b': c = '\b'; break;
        case 'f': c = '\f'; break;
        case 'u': {
            unsigned long cp = 0;
            int k;
            for (k = 1; k <= 4; k++) {
                int h = hexval(js[p + k]);
                if (h < 0)
                    goto full;
                cp = (cp << 4) | (unsigned long)h;
            }
            p += 4;
            /* A surrogate pair arrives as two escapes. */
            if (cp >= 0xD800 && cp <= 0xDBFF && js[p + 1] == '\\' && js[p + 2] == 'u') {
                unsigned long lo = 0;
                for (k = 3; k <= 6; k++) {
                    int h = hexval(js[p + k]);
                    if (h < 0)
                        goto full;
                    lo = (lo << 4) | (unsigned long)h;
                }
                if (lo >= 0xDC00 && lo <= 0xDFFF) {
                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                    p += 6;
                }
            }
            if (put_utf8(out, cap, &n, cp))
                goto full;
            continue;
        }
        default: c = js[p]; break; /* \" \\ \/ */
        }
        if (put(out, cap, &n, c))
            goto full;
    }
    out[n] = 0;
    return n;
full:
    out[cap - 1] = 0;
    return -1;
}

int json_ulong(const char *js, const struct json_tok *toks, int i, unsigned long *out)
{
    int p;
    unsigned long v = 0;
    if (toks[i].type != JSON_PRIMITIVE || toks[i].end == toks[i].start)
        return -1;
    for (p = toks[i].start; p < toks[i].end; p++) {
        char c = js[p];
        if (c < '0' || c > '9')
            return -1;
        v = v * 10 + (unsigned long)(c - '0');
    }
    *out = v;
    return 0;
}

void jo_init(struct json_out *o, char *buf, int cap)
{
    o->buf = buf;
    o->cap = cap;
    o->len = 0;
    o->overflow = 0;
    if (cap > 0)
        buf[0] = 0;
}

static void jo_char(struct json_out *o, char c)
{
    if (o->len + 1 >= o->cap) {
        o->overflow = 1;
        return;
    }
    o->buf[o->len++] = c;
    o->buf[o->len] = 0;
}

void jo_raw(struct json_out *o, const char *s)
{
    while (*s)
        jo_char(o, *s++);
}

void jo_str(struct json_out *o, const char *s)
{
    static const char hex[] = "0123456789abcdef";
    jo_char(o, '"');
    for (; *s; s++) {
        unsigned char c = (unsigned char)*s;
        switch (c) {
        case '"': jo_raw(o, "\\\""); break;
        case '\\': jo_raw(o, "\\\\"); break;
        case '\n': jo_raw(o, "\\n"); break;
        case '\r': jo_raw(o, "\\r"); break;
        case '\t': jo_raw(o, "\\t"); break;
        default:
            if (c < 0x20) {
                jo_raw(o, "\\u00");
                jo_char(o, hex[c >> 4]);
                jo_char(o, hex[c & 0xF]);
            } else {
                jo_char(o, (char)c);
            }
        }
    }
    jo_char(o, '"');
}

void jo_ulong(struct json_out *o, unsigned long v)
{
    char tmp[24];
    int n = 0;
    do {
        tmp[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    while (n)
        jo_char(o, tmp[--n]);
}

void jo_long(struct json_out *o, long v)
{
    if (v < 0) {
        jo_char(o, '-');
        jo_ulong(o, (unsigned long)(-(v + 1)) + 1UL);
    } else {
        jo_ulong(o, (unsigned long)v);
    }
}

void jo_key(struct json_out *o, const char *key)
{
    jo_str(o, key);
    jo_char(o, ':');
}
