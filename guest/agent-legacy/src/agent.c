/* The legacy agent's core: one polling loop that decodes frames off the
 * port, dispatches control messages, and pumps every live exec's pipes —
 * no threads (a Win32 stdin writer aside, hidden behind plat.h), so the
 * same loop runs under a DOS extender.
 *
 * It speaks guest/agent-proto version 2 and advertises one feature, `exec`.
 * Everything else the wire can ask for is answered by name with an error,
 * which is what lets the host's feature ladder (PRD §19.4) degrade rather
 * than guess: a terminal, a file session or a tunnel refuses; readiness,
 * exec and the stop ladder work.
 */
#include "agent.h"

#include <stdlib.h>
#include <string.h>

#include "json.h"
#include "plat.h"
#include "wire.h"

#define MAX_SESSIONS 8
#define MAX_TOKENS 256
#define MAX_ARGV 64
#define MAX_ENV 64
#define STR_CAP 4096
/* The decoder must hold one maximal frame. */
#define IN_CAP (WIRE_HEADER_LEN + WIRE_MAX_PAYLOAD)
/* What one pump reads at a time; small keeps a slow serial line fair
 * between sessions. */
#define CHUNK 4096

struct session {
    int used;
    unsigned long id;
    struct plat_proc *proc;
    unsigned long credit;        /* guest->host bytes we may still send */
    unsigned long recv_consumed; /* host->guest bytes since the last grant */
    int out_closed;
    int err_closed;
};

static struct session sessions[MAX_SESSIONS];
static unsigned char inbuf[IN_CAP];
static struct wire_decoder dec;
static struct json_tok toks[MAX_TOKENS];
static char ctrl_buf[STR_CAP];
static unsigned char chunk[CHUNK];

/* ---- sending -------------------------------------------------------- */

static int send_frame(int kind, unsigned long channel, const void *payload,
                      unsigned long len)
{
    unsigned char hdr[WIRE_HEADER_LEN];
    wire_header(hdr, kind, channel, len);
    if (port_write(hdr, WIRE_HEADER_LEN) < 0)
        return -1;
    if (len && port_write(payload, (long)len) < 0)
        return -1;
    return 0;
}

static int send_ctrl(const struct json_out *o)
{
    if (o->overflow) {
        plat_log("agent: control message too large, dropped");
        return 0;
    }
    return send_frame(WIRE_KIND_CTRL, 0, o->buf, (unsigned long)o->len);
}

static void begin(struct json_out *o, const char *event)
{
    jo_init(o, ctrl_buf, STR_CAP);
    jo_raw(o, "{");
    jo_key(o, "event");
    jo_str(o, event);
}

static int send_simple(const char *event, long id)
{
    struct json_out o;
    begin(&o, event);
    if (id >= 0) {
        jo_raw(&o, ",");
        jo_key(&o, "id");
        jo_ulong(&o, (unsigned long)id);
    }
    jo_raw(&o, "}");
    return send_ctrl(&o);
}

static int send_error(long id, const char *msg)
{
    struct json_out o;
    begin(&o, "error");
    if (id >= 0) {
        jo_raw(&o, ",");
        jo_key(&o, "id");
        jo_ulong(&o, (unsigned long)id);
    }
    jo_raw(&o, ",");
    jo_key(&o, "msg");
    jo_str(&o, msg);
    jo_raw(&o, "}");
    return send_ctrl(&o);
}

static int send_exited(unsigned long id, long code)
{
    struct json_out o;
    begin(&o, "exited");
    jo_raw(&o, ",");
    jo_key(&o, "id");
    jo_ulong(&o, id);
    jo_raw(&o, ",");
    jo_key(&o, "code");
    jo_long(&o, code);
    jo_raw(&o, "}");
    return send_ctrl(&o);
}

static int send_window_adjust(unsigned long id, unsigned long bytes)
{
    struct json_out o;
    begin(&o, "window_adjust");
    jo_raw(&o, ",");
    jo_key(&o, "id");
    jo_ulong(&o, id);
    jo_raw(&o, ",");
    jo_key(&o, "bytes");
    jo_ulong(&o, bytes);
    jo_raw(&o, "}");
    return send_ctrl(&o);
}

static int send_hello(const char *token)
{
    struct json_out o;
    begin(&o, "hello");
    jo_raw(&o, ",");
    jo_key(&o, "proto_version");
    jo_ulong(&o, WIRE_PROTO_VERSION);
    jo_raw(&o, ",");
    jo_key(&o, "agent_version");
    jo_str(&o, AGENT_VERSION);
    jo_raw(&o, ",");
    jo_key(&o, "os");
    jo_str(&o, plat_os_tag());
    jo_raw(&o, ",");
    jo_key(&o, "features");
    jo_raw(&o, "[\"exec\"]");
    jo_raw(&o, ",");
    jo_key(&o, "token");
    jo_str(&o, token);
    jo_raw(&o, "}");
    return send_ctrl(&o);
}

static int send_os_info(void)
{
    struct os_info info;
    struct json_out o;
    memset(&info, 0, sizeof info);
    plat_os_info(&info);
    begin(&o, "os_info");
    jo_raw(&o, ",");
    jo_key(&o, "info");
    jo_raw(&o, "{");
    jo_key(&o, "id");
    jo_str(&o, info.id);
    jo_raw(&o, ",");
    jo_key(&o, "name");
    jo_str(&o, info.name);
    jo_raw(&o, ",");
    jo_key(&o, "version");
    jo_str(&o, info.version);
    jo_raw(&o, ",");
    jo_key(&o, "kernel");
    jo_str(&o, info.kernel);
    jo_raw(&o, ",");
    jo_key(&o, "arch");
    jo_str(&o, info.arch);
    jo_raw(&o, ",");
    jo_key(&o, "hostname");
    jo_str(&o, info.hostname);
    jo_raw(&o, "}}");
    return send_ctrl(&o);
}

static int send_net_info(void)
{
    struct json_out o;
    begin(&o, "net_info");
    jo_raw(&o, ",");
    jo_key(&o, "interfaces");
    jo_raw(&o, "[]}");
    return send_ctrl(&o);
}

static int send_shutting_down(const char *mode)
{
    struct json_out o;
    begin(&o, "shutting_down");
    jo_raw(&o, ",");
    jo_key(&o, "mode");
    jo_str(&o, mode);
    jo_raw(&o, "}");
    return send_ctrl(&o);
}

/* ---- sessions ------------------------------------------------------- */

static struct session *find_session(unsigned long id)
{
    int i;
    for (i = 0; i < MAX_SESSIONS; i++)
        if (sessions[i].used && sessions[i].id == id)
            return &sessions[i];
    return NULL;
}

static void drop_session(struct session *s)
{
    if (s->proc) {
        proc_kill(s->proc);
        proc_free(s->proc);
    }
    memset(s, 0, sizeof *s);
}

static void drop_all_sessions(void)
{
    int i;
    for (i = 0; i < MAX_SESSIONS; i++)
        if (sessions[i].used)
            drop_session(&sessions[i]);
}

/* ---- open_exec ------------------------------------------------------ */

/* argv / env storage for one open. Strings are decoded into `pool`. */
struct exec_args {
    char pool[STR_CAP * 4];
    int used;
    char *argv[MAX_ARGV];
    int argc;
    char *env[MAX_ENV];
    int nenv;
    char *cwd;
};

static char *pool_str(struct exec_args *a, const char *js, int tok)
{
    char *dst = a->pool + a->used;
    int cap = (int)sizeof a->pool - a->used;
    int n = json_str(js, toks, tok, dst, cap);
    if (n < 0)
        return NULL;
    a->used += n + 1;
    return dst;
}

static void handle_open_exec(const char *js, int ntok, unsigned long id)
{
    static struct exec_args a;
    struct exec_spec spec;
    struct session *s;
    struct plat_proc *proc;
    char err[256];
    int i, t, argv_tok, env_tok, cwd_tok;

    if (find_session(id)) {
        send_error((long)id, "channel id already open");
        return;
    }
    if (json_get(js, toks, ntok, 0, "logon") >= 0) {
        int lt = json_get(js, toks, ntok, 0, "logon");
        if (toks[lt].type == JSON_OBJECT) {
            send_error((long)id, "legacy agent: cannot mint a logon; execs run as the "
                                 "agent identity only");
            return;
        }
    }
    argv_tok = json_get(js, toks, ntok, 0, "argv");
    if (argv_tok < 0 || toks[argv_tok].type != JSON_ARRAY || toks[argv_tok].size == 0) {
        send_error((long)id, "exec: empty argv");
        return;
    }

    memset(&a, 0, sizeof a);
    t = argv_tok + 1;
    for (i = 0; i < toks[argv_tok].size; i++) {
        if (a.argc >= MAX_ARGV) {
            send_error((long)id, "exec: too many arguments");
            return;
        }
        a.argv[a.argc] = pool_str(&a, js, t);
        if (!a.argv[a.argc]) {
            send_error((long)id, "exec: argument too long");
            return;
        }
        a.argc++;
        t = json_skip(toks, ntok, t);
    }

    env_tok = json_get(js, toks, ntok, 0, "env");
    if (env_tok >= 0 && toks[env_tok].type == JSON_ARRAY) {
        t = env_tok + 1;
        for (i = 0; i < toks[env_tok].size; i++) {
            /* Each entry is a two-element array [key, value]. */
            int kt = t + 1, vt;
            char *k, *v;
            int klen;
            if (toks[t].type != JSON_ARRAY || toks[t].size != 2) {
                send_error((long)id, "exec: malformed env entry");
                return;
            }
            vt = json_skip(toks, ntok, kt);
            if (a.nenv >= MAX_ENV) {
                send_error((long)id, "exec: too many env entries");
                return;
            }
            k = pool_str(&a, js, kt);
            if (!k) {
                send_error((long)id, "exec: env entry too long");
                return;
            }
            klen = (int)strlen(k);
            /* Turn "KEY\0" into "KEY=VALUE\0" in place: the value is decoded
             * right after the key's terminator, which becomes the '='. */
            k[klen] = '=';
            v = pool_str(&a, js, vt);
            if (!v) {
                send_error((long)id, "exec: env entry too long");
                return;
            }
            a.env[a.nenv++] = k;
            t = json_skip(toks, ntok, t);
        }
    }

    cwd_tok = json_get(js, toks, ntok, 0, "cwd");
    if (cwd_tok >= 0 && toks[cwd_tok].type == JSON_STRING) {
        a.cwd = pool_str(&a, js, cwd_tok);
        if (!a.cwd) {
            send_error((long)id, "exec: cwd too long");
            return;
        }
    }

    s = NULL;
    for (i = 0; i < MAX_SESSIONS; i++)
        if (!sessions[i].used) {
            s = &sessions[i];
            break;
        }
    if (!s) {
        send_error((long)id, "exec: too many open channels");
        return;
    }

    spec.argv = a.argv;
    spec.argc = a.argc;
    spec.env = a.env;
    spec.nenv = a.nenv;
    spec.cwd = a.cwd;
    err[0] = 0;
    proc = proc_start(&spec, err, (int)sizeof err);
    if (!proc) {
        send_error((long)id, err[0] ? err : "exec: spawn failed");
        return;
    }
    memset(s, 0, sizeof *s);
    s->used = 1;
    s->id = id;
    s->proc = proc;
    s->credit = WIRE_INITIAL_WINDOW;
    send_simple("opened", (long)id);
}

/* ---- dispatch ------------------------------------------------------- */

static unsigned long field_ulong(const char *js, int ntok, const char *key, unsigned long dflt)
{
    unsigned long v;
    int t = json_get(js, toks, ntok, 0, key);
    if (t < 0 || json_ulong(js, toks, t, &v) < 0)
        return dflt;
    return v;
}

/* Returns -1 when the loop should stop (a shutdown was initiated). */
static int handle_ctrl(const unsigned char *payload, unsigned long len)
{
    const char *js = (const char *)payload;
    int ntok, cmd_tok;
    static char cmd[64];
    unsigned long id;

    ntok = json_parse(js, (int)len, toks, MAX_TOKENS);
    if (ntok < 0 || toks[0].type != JSON_OBJECT) {
        send_error(-1, "unparseable control message");
        return 0;
    }
    cmd_tok = json_get(js, toks, ntok, 0, "cmd");
    if (cmd_tok < 0 || json_str(js, toks, cmd_tok, cmd, (int)sizeof cmd) < 0) {
        send_error(-1, "control message without a cmd");
        return 0;
    }
    id = field_ulong(js, ntok, "id", 0);

    if (strcmp(cmd, "hello") == 0) {
        static char token[256];
        int tt = json_get(js, toks, ntok, 0, "token");
        if (tt < 0 || json_str(js, toks, tt, token, (int)sizeof token) < 0)
            token[0] = 0;
        /* Both sides discard channel state from before the exchange. */
        drop_all_sessions();
        send_hello(token);
    } else if (strcmp(cmd, "open_exec") == 0) {
        handle_open_exec(js, ntok, id);
    } else if (strcmp(cmd, "eof") == 0) {
        struct session *s = find_session(id);
        if (s)
            proc_close_in(s->proc);
    } else if (strcmp(cmd, "close") == 0) {
        struct session *s = find_session(id);
        if (s)
            drop_session(s);
    } else if (strcmp(cmd, "window_adjust") == 0) {
        struct session *s = find_session(id);
        if (s)
            s->credit += field_ulong(js, ntok, "bytes", 0);
    } else if (strcmp(cmd, "ping") == 0) {
        send_simple("pong", -1);
    } else if (strcmp(cmd, "os_info") == 0) {
        send_os_info();
    } else if (strcmp(cmd, "net_info") == 0) {
        send_net_info();
    } else if (strcmp(cmd, "shutdown") == 0) {
        static char mode[32];
        int mt = json_get(js, toks, ntok, 0, "mode");
        if (mt < 0 || json_str(js, toks, mt, mode, (int)sizeof mode) < 0)
            strcpy(mode, "powerdown");
        /* The ack may be the last bytes on the wire; send it first. */
        send_shutting_down(mode);
        if (plat_shutdown(mode) == 0)
            return -1;
        send_error(-1, "shutdown: not supported on this guest");
    } else if (strcmp(cmd, "open_terminal") == 0) {
        send_error((long)id, "legacy agent: no terminal (features: exec)");
    } else if (strcmp(cmd, "open_fileops") == 0) {
        send_error((long)id, "legacy agent: no fileops (features: exec)");
    } else if (strcmp(cmd, "open_tunnel") == 0) {
        send_error((long)id, "legacy agent: no tunnel (features: exec)");
    } else if (strcmp(cmd, "open_tail") == 0) {
        send_error((long)id, "legacy agent: no tail (features: exec)");
    } else if (strcmp(cmd, "open_watch") == 0) {
        send_error((long)id, "legacy agent: no watch (features: exec)");
    } else if (strcmp(cmd, "open_eventlog") == 0) {
        send_error((long)id, "legacy agent: no eventlog (features: exec)");
    } else if (strcmp(cmd, "resize") == 0) {
        /* No terminals, so nothing to resize; silently fine. */
    } else if (strcmp(cmd, "subscribe_metrics") == 0 ||
               strcmp(cmd, "unsubscribe_metrics") == 0 ||
               strcmp(cmd, "get_clipboard") == 0 ||
               strcmp(cmd, "set_clipboard") == 0) {
        send_error(-1, "legacy agent: not available (features: exec)");
    } else {
        send_error(-1, "legacy agent: unknown command");
    }
    return 0;
}

static void handle_data(unsigned long channel, const unsigned char *payload,
                        unsigned long len)
{
    struct session *s = find_session(channel);
    if (!s || len == 0)
        return;
    proc_write_in(s->proc, payload, (long)len);
}

/* ---- pumping -------------------------------------------------------- */

/* Move what a pipe holds onto the wire, within credit. Returns 1 when
 * anything moved. */
static int pump(struct session *s, int which, int *closed, int kind)
{
    long want, n;
    if (*closed || s->credit == 0)
        return 0;
    want = (long)sizeof chunk;
    if ((unsigned long)want > s->credit)
        want = (long)s->credit;
    n = proc_read(s->proc, which, chunk, want);
    if (n < 0) {
        *closed = 1;
        return 0;
    }
    if (n == 0)
        return 0;
    s->credit -= (unsigned long)n;
    send_frame(kind, s->id, chunk, (unsigned long)n);
    return 1;
}

static int service_sessions(void)
{
    int i, active = 0;
    for (i = 0; i < MAX_SESSIONS; i++) {
        struct session *s = &sessions[i];
        unsigned long consumed;
        long code;
        if (!s->used)
            continue;
        active |= pump(s, 0, &s->out_closed, WIRE_KIND_DATA);
        active |= pump(s, 1, &s->err_closed, WIRE_KIND_DATA_ERR);

        consumed = proc_consumed(s->proc);
        if (consumed) {
            s->recv_consumed += consumed;
            if (s->recv_consumed >= WIRE_WINDOW_REPLENISH) {
                send_window_adjust(s->id, s->recv_consumed);
                s->recv_consumed = 0;
            }
            active = 1;
        }

        if (s->out_closed && s->err_closed && proc_exited(s->proc, &code)) {
            /* Both pipes drained: the channel's bytes are complete. */
            send_simple("eof", (long)s->id);
            send_exited(s->id, code);
            proc_free(s->proc);
            memset(s, 0, sizeof *s);
            active = 1;
        }
    }
    return active;
}

/* ---- the loop ------------------------------------------------------- */

int agent_run(void)
{
    wire_decoder_init(&dec, inbuf, IN_CAP);
    memset(sessions, 0, sizeof sessions);

    for (;;) {
        int active = 0;
        int kind;
        unsigned long channel, len, room;
        const unsigned char *payload;
        long n;

        room = wire_room(&dec);
        if (room > sizeof chunk)
            room = sizeof chunk;
        if (room) {
            n = port_read(chunk, (long)room);
            if (n < 0) {
                drop_all_sessions();
                return -1;
            }
            if (n > 0) {
                wire_push(&dec, chunk, (unsigned long)n);
                active = 1;
            }
        }

        while (wire_next(&dec, &kind, &channel, &payload, &len)) {
            active = 1;
            if (kind == WIRE_KIND_CTRL && channel == 0) {
                if (handle_ctrl(payload, len) < 0) {
                    wire_consume(&dec);
                    drop_all_sessions();
                    return 0;
                }
            } else if (kind == WIRE_KIND_DATA) {
                handle_data(channel, payload, len);
            }
            wire_consume(&dec);
        }

        active |= service_sessions();

        if (!active)
            plat_sleep_ms(5);
    }
}
