/*
 * vmlab-gpu-keepawake — holds one host GPU out of runtime suspend for as
 * long as it runs (PRD §5.2, ADR-0018).
 *
 *   vmlab-gpu-keepawake --pci 0000:01:00.0 --parent <pid> [--interval-ms 100]
 *
 * Opens the GPU at the PCI address through EGL's device platform
 * (EGL_EXT_device_enumeration, EGL_EXT_platform_device), makes an OpenGL ES
 * context current without a surface, and then, every interval, clears a
 * 16x16 renderbuffer and waits for it (glFinish). Under NVIDIA's runtime D3
 * an open context alone does not keep the GPU awake: the driver suspends it
 * once it has been idle for a few seconds, whoever holds it open. Small
 * regular work does. That was measured with the same loop before it was
 * shipped (vmlab-win3d, docs/zeus.md, "Runtime power management").
 *
 * It prints `ready <renderer>` once the context is current, so the
 * supervisor that spawned it knows the hold took, and runs until SIGINT or
 * SIGTERM, or until its parent (`--parent`) is no longer its parent: a
 * supervisor that dies without stopping it must not leave the GPU held. That
 * is a poll of getppid() each interval rather than PR_SET_PDEATHSIG, which
 * fires when the spawning *thread* exits and so would end the hold whenever
 * the runtime retired a worker.
 *
 * libEGL is opened with dlopen and every entry point is looked up, so
 * building vmlab needs no EGL headers or libraries, and a host without EGL
 * gets a message naming it rather than a binary that will not load. The few
 * EGL and GLES types used are declared here; they are the stable ABI of EGL
 * 1.5 and OpenGL ES 2.0.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

typedef void *EGLDisplay;
typedef void *EGLContext;
typedef void *EGLSurface;
typedef void *EGLConfig;
typedef void *EGLDeviceEXT;
typedef unsigned int EGLBoolean;
typedef unsigned int EGLenum;
typedef int32_t EGLint;
#define EGL_NONE 0x3038
#define EGL_OPENGL_ES_API 0x30A0
#define EGL_CONTEXT_MAJOR_VERSION 0x3098
#define EGL_PLATFORM_DEVICE_EXT 0x313F
#define EGL_DRM_RENDER_NODE_FILE_EXT 0x3377

typedef unsigned int GLuint;
typedef unsigned int GLenum;
typedef int GLsizei;
typedef int GLint;
typedef float GLfloat;
typedef unsigned int GLbitfield;
#define GL_FRAMEBUFFER 0x8D40
#define GL_RENDERBUFFER 0x8D41
#define GL_RGBA4 0x8056
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_COLOR_BUFFER_BIT 0x00004000
#define GL_RENDERER 0x1F01

typedef void *(*GetProcAddress)(const char *);
typedef EGLint (*GetError)(void);
typedef EGLBoolean (*QueryDevices)(EGLint, EGLDeviceEXT *, EGLint *);
typedef const char *(*QueryDeviceString)(EGLDeviceEXT, EGLint);
typedef EGLDisplay (*GetPlatformDisplay)(EGLenum, void *, const EGLint *);
typedef EGLBoolean (*Initialize)(EGLDisplay, EGLint *, EGLint *);
typedef EGLBoolean (*BindApi)(EGLenum);
typedef EGLContext (*CreateContext)(EGLDisplay, EGLConfig, EGLContext, const EGLint *);
typedef EGLBoolean (*MakeCurrent)(EGLDisplay, EGLSurface, EGLSurface, EGLContext);
typedef EGLBoolean (*DestroyContext)(EGLDisplay, EGLContext);
typedef EGLBoolean (*Terminate)(EGLDisplay);

static volatile sig_atomic_t stop;
static void on_signal(int sig) {
    (void)sig;
    stop = 1;
}

static GetError egl_error;

static void die(const char *msg) {
    fprintf(stderr, "vmlab-gpu-keepawake: %s (EGL error 0x%x)\n", msg,
            egl_error ? (unsigned)egl_error() : 0u);
    exit(1);
}

/* The render node (/dev/dri/renderDNNN) of the PCI device, from sysfs. */
static int render_node(const char *pci, char *out, size_t n) {
    char path[PATH_MAX];
    for (int i = 128; i < 256; i++) {
        snprintf(path, sizeof path, "/sys/bus/pci/devices/%s/drm/renderD%d", pci, i);
        if (access(path, F_OK) == 0) {
            snprintf(out, n, "/dev/dri/renderD%d", i);
            return 0;
        }
    }
    return -1;
}

int main(int argc, char **argv) {
    const char *pci = NULL;
    long parent = 0;
    long interval_ms = 100;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--pci") && i + 1 < argc)
            pci = argv[++i];
        else if (!strcmp(argv[i], "--parent") && i + 1 < argc)
            parent = strtol(argv[++i], NULL, 10);
        else if (!strcmp(argv[i], "--interval-ms") && i + 1 < argc)
            interval_ms = strtol(argv[++i], NULL, 10);
        else
            pci = NULL, i = argc;
    }
    if (!pci) {
        fprintf(stderr, "usage: vmlab-gpu-keepawake --pci 0000:01:00.0 [--parent PID] "
                        "[--interval-ms 100]\n");
        return 2;
    }
    if (interval_ms < 1) interval_ms = 1;

    char node[64];
    if (render_node(pci, node, sizeof node)) {
        fprintf(stderr, "vmlab-gpu-keepawake: no render node for PCI device %s\n", pci);
        return 1;
    }

    void *egl = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!egl) {
        fprintf(stderr, "vmlab-gpu-keepawake: cannot load libEGL.so.1: %s\n", dlerror());
        return 1;
    }
    GetProcAddress gpa = (GetProcAddress)dlsym(egl, "eglGetProcAddress");
    egl_error = (GetError)dlsym(egl, "eglGetError");
    Initialize initialize = (Initialize)dlsym(egl, "eglInitialize");
    BindApi bind_api = (BindApi)dlsym(egl, "eglBindAPI");
    CreateContext create_context = (CreateContext)dlsym(egl, "eglCreateContext");
    MakeCurrent make_current = (MakeCurrent)dlsym(egl, "eglMakeCurrent");
    DestroyContext destroy_context = (DestroyContext)dlsym(egl, "eglDestroyContext");
    Terminate terminate = (Terminate)dlsym(egl, "eglTerminate");
    if (!gpa || !egl_error || !initialize || !bind_api || !create_context || !make_current ||
        !destroy_context || !terminate)
        die("libEGL.so.1 lacks an EGL 1.5 entry point");

    QueryDevices query_devices = (QueryDevices)gpa("eglQueryDevicesEXT");
    QueryDeviceString query_device_string = (QueryDeviceString)gpa("eglQueryDeviceStringEXT");
    GetPlatformDisplay get_platform_display = (GetPlatformDisplay)gpa("eglGetPlatformDisplayEXT");
    if (!query_devices || !query_device_string || !get_platform_display)
        die("EGL device enumeration is not available");

    EGLDeviceEXT devs[32];
    EGLint ndev = 0;
    if (!query_devices(32, devs, &ndev)) die("eglQueryDevicesEXT failed");
    EGLDeviceEXT dev = NULL;
    for (EGLint i = 0; i < ndev && !dev; i++) {
        const char *rn = query_device_string(devs[i], EGL_DRM_RENDER_NODE_FILE_EXT);
        if (rn && !strcmp(rn, node)) dev = devs[i];
    }
    if (!dev) {
        fprintf(stderr, "vmlab-gpu-keepawake: no EGL device has render node %s (%s)\n", node,
                pci);
        return 1;
    }

    EGLDisplay dpy = get_platform_display(EGL_PLATFORM_DEVICE_EXT, dev, NULL);
    if (!dpy || !initialize(dpy, NULL, NULL)) die("eglInitialize failed");
    if (!bind_api(EGL_OPENGL_ES_API)) die("eglBindAPI failed");
    static const EGLint ctx_attr[] = {EGL_CONTEXT_MAJOR_VERSION, 2, EGL_NONE};
    EGLContext ctx = create_context(dpy, NULL, NULL, ctx_attr);
    if (!ctx) die("eglCreateContext failed");
    if (!make_current(dpy, NULL, NULL, ctx)) die("eglMakeCurrent failed");

    const unsigned char *(*get_string)(GLenum) = (const unsigned char *(*)(GLenum))gpa("glGetString");
    void (*gen_framebuffers)(GLsizei, GLuint *) = (void (*)(GLsizei, GLuint *))gpa("glGenFramebuffers");
    void (*bind_framebuffer)(GLenum, GLuint) = (void (*)(GLenum, GLuint))gpa("glBindFramebuffer");
    void (*gen_renderbuffers)(GLsizei, GLuint *) = (void (*)(GLsizei, GLuint *))gpa("glGenRenderbuffers");
    void (*bind_renderbuffer)(GLenum, GLuint) = (void (*)(GLenum, GLuint))gpa("glBindRenderbuffer");
    void (*renderbuffer_storage)(GLenum, GLenum, GLsizei, GLsizei) =
        (void (*)(GLenum, GLenum, GLsizei, GLsizei))gpa("glRenderbufferStorage");
    void (*framebuffer_renderbuffer)(GLenum, GLenum, GLenum, GLuint) =
        (void (*)(GLenum, GLenum, GLenum, GLuint))gpa("glFramebufferRenderbuffer");
    void (*viewport)(GLint, GLint, GLsizei, GLsizei) = (void (*)(GLint, GLint, GLsizei, GLsizei))gpa("glViewport");
    void (*clear_color)(GLfloat, GLfloat, GLfloat, GLfloat) =
        (void (*)(GLfloat, GLfloat, GLfloat, GLfloat))gpa("glClearColor");
    void (*clear)(GLbitfield) = (void (*)(GLbitfield))gpa("glClear");
    void (*finish)(void) = (void (*)(void))gpa("glFinish");
    if (!get_string || !gen_framebuffers || !bind_framebuffer || !gen_renderbuffers ||
        !bind_renderbuffer || !renderbuffer_storage || !framebuffer_renderbuffer || !viewport ||
        !clear_color || !clear || !finish)
        die("an OpenGL ES 2.0 entry point is missing");

    /* GL_RGBA4 is the colour-renderable format ES 2.0 guarantees. */
    GLuint fb, rb;
    gen_framebuffers(1, &fb);
    bind_framebuffer(GL_FRAMEBUFFER, fb);
    gen_renderbuffers(1, &rb);
    bind_renderbuffer(GL_RENDERBUFFER, rb);
    renderbuffer_storage(GL_RENDERBUFFER, GL_RGBA4, 16, 16);
    framebuffer_renderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb);
    viewport(0, 0, 16, 16);

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);

    const unsigned char *renderer = get_string(GL_RENDERER);
    printf("ready %s %s %s\n", pci, node, renderer ? (const char *)renderer : "?");
    fflush(stdout);

    struct timespec ts = {interval_ms / 1000, (interval_ms % 1000) * 1000000L};
    unsigned long n = 0;
    while (!stop) {
        if (parent > 0 && (long)getppid() != parent) break;
        clear_color((float)(n & 1), 0.f, 0.f, 1.f);
        clear(GL_COLOR_BUFFER_BIT);
        finish();
        n++;
        nanosleep(&ts, NULL);
    }
    make_current(dpy, NULL, NULL, NULL);
    destroy_context(dpy, ctx);
    terminate(dpy);
    printf("stopped after %lu clears\n", n);
    return 0;
}
