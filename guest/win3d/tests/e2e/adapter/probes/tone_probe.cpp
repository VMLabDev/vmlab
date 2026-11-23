// Sound without breaks (card #447): play a sine through the default output
// for --seconds, the way an ordinary program does.
//
// A WASAPI shared-mode, event-driven stream in the mix format of the default
// render endpoint, on a thread MMCSS schedules as "Pro Audio" when avrt.dll
// offers it. Every channel carries the same --hz sine (997) at a quarter of
// full scale. The endpoint's volume is set to 100 percent and unmuted, so
// that the host's capture has the amplitude this program wrote.
//
// The host captures what vmlab-hv plays and counts the breaks in the sine
// (tests/e2e/adapter/audio.py). This program reports the format, how much it
// wrote, and how often its own wake-up came late or found the engine's
// buffer empty: a break that started in the guest.
#include "adapter.h"
#include <audioclient.h>
#include <endpointvolume.h>
#include <math.h>
#include <mmdeviceapi.h>

static const GUID float_subtype = {0x00000003, 0x0000, 0x0010, {0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71}};
static const GUID pcm_subtype = {0x00000001, 0x0000, 0x0010, {0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71}};

int main(int argc, char **argv)
{
    Report r("tone");
    const char *out = arg_str(argc, argv, "--out", "tone_probe.json");
    int seconds = arg_int(argc, argv, "--seconds", 60);
    double hz = arg_int(argc, argv, "--hz", 997);
    const double amplitude = 0.25, tau = 6.283185307179586;

    IMMDeviceEnumerator *en = NULL;
    IMMDevice *dev = NULL;
    IAudioClient *ac = NULL;
    IAudioRenderClient *rc = NULL;
    IAudioEndpointVolume *vol = NULL;
    WAVEFORMATEX *wf = NULL;
    if (r.failed(CoInitializeEx(NULL, COINIT_MULTITHREADED), "CoInitializeEx") ||
        r.failed(CoCreateInstance(__uuidof(MMDeviceEnumerator), NULL, CLSCTX_ALL, __uuidof(IMMDeviceEnumerator),
                                  (void **)&en),
                 "CoCreateInstance(MMDeviceEnumerator)") ||
        r.failed(en->GetDefaultAudioEndpoint(eRender, eConsole, &dev), "GetDefaultAudioEndpoint") ||
        r.failed(dev->Activate(__uuidof(IAudioClient), CLSCTX_ALL, NULL, (void **)&ac), "Activate(IAudioClient)") ||
        r.failed(ac->GetMixFormat(&wf), "GetMixFormat"))
        return r.finish(out);
    if (SUCCEEDED(dev->Activate(__uuidof(IAudioEndpointVolume), CLSCTX_ALL, NULL, (void **)&vol))) {
        vol->SetMute(FALSE, NULL);
        vol->SetMasterVolumeLevelScalar(1.0f, NULL);
    }

    bool is_float = wf->wFormatTag == WAVE_FORMAT_IEEE_FLOAT;
    bool is_pcm = wf->wFormatTag == WAVE_FORMAT_PCM;
    if (wf->wFormatTag == WAVE_FORMAT_EXTENSIBLE && wf->cbSize >= 22) {
        const GUID *sub = &((WAVEFORMATEXTENSIBLE *)wf)->SubFormat;
        is_float = !memcmp(sub, &float_subtype, sizeof(GUID));
        is_pcm = !memcmp(sub, &pcm_subtype, sizeof(GUID));
    }
    int channels = wf->nChannels, bits = wf->wBitsPerSample, rate = (int)wf->nSamplesPerSec;
    r.integer("rate", rate);
    r.integer("channels", channels);
    r.integer("bits", bits);
    r.str("sample_type", is_float ? "float" : is_pcm ? "pcm" : "other");
    r.num("hz", hz);
    r.num("amplitude", amplitude);
    if (!(is_float && bits == 32) && !(is_pcm && (bits == 16 || bits == 32))) {
        r.err("the mix format is not 32-bit float or 16- or 32-bit PCM");
        return r.finish(out);
    }

    HANDLE event = CreateEventW(NULL, FALSE, FALSE, NULL);
    UINT32 buffer_frames = 0;
    REFERENCE_TIME period = 0, min_period = 0;
    if (r.failed(ac->Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, wf, NULL),
                 "IAudioClient::Initialize") ||
        r.failed(ac->SetEventHandle(event), "SetEventHandle") ||
        r.failed(ac->GetBufferSize(&buffer_frames), "GetBufferSize") ||
        r.failed(ac->GetService(__uuidof(IAudioRenderClient), (void **)&rc), "GetService(IAudioRenderClient)"))
        return r.finish(out);
    ac->GetDevicePeriod(&period, &min_period);
    double period_ms = (double)period / 10000.0;
    r.integer("buffer_frames", buffer_frames);
    r.num("device_period_ms", period_ms);

    // The priority a media program asks for.
    bool mmcss = false;
    HMODULE avrt = LoadLibraryW(L"avrt.dll");
    if (avrt) {
        typedef HANDLE(WINAPI * set_fn)(LPCWSTR, LPDWORD);
        set_fn set = (set_fn)(void *)GetProcAddress(avrt, "AvSetMmThreadCharacteristicsW");
        DWORD task = 0;
        mmcss = set && set(L"Pro Audio", &task) != NULL;
    }
    r.boolean("mmcss", mmcss);

    unsigned long long written = 0, total = (unsigned long long)seconds * (unsigned long long)rate;
    unsigned late_wakes = 0, empty_wakes = 0, wakes = 0;
    double longest_wake_ms = 0;
    auto fill = [&](UINT32 frames) -> bool {
        BYTE *p = NULL;
        if (r.failed(rc->GetBuffer(frames, &p), "GetBuffer"))
            return false;
        for (UINT32 i = 0; i < frames; i++) {
            double v = amplitude * sin(tau * hz * (double)(written + i) / (double)rate);
            for (int c = 0; c < channels; c++) {
                if (is_float)
                    ((float *)p)[i * channels + c] = (float)v;
                else if (bits == 16)
                    ((short *)p)[i * channels + c] = (short)lrint(v * 32767.0);
                else
                    ((int *)p)[i * channels + c] = (int)lrint(v * 2147483647.0);
            }
        }
        written += frames;
        return !r.failed(rc->ReleaseBuffer(frames, 0), "ReleaseBuffer");
    };

    if (!fill(buffer_frames) || r.failed(ac->Start(), "IAudioClient::Start"))
        return r.finish(out);
    double start = now_ms(), last = start;
    while (written < total && r.errors.empty()) {
        if (WaitForSingleObject(event, 2000) != WAIT_OBJECT_0) {
            r.err("the audio engine sent no event for 2 s");
            break;
        }
        double now = now_ms();
        wakes++;
        if (now - last > longest_wake_ms)
            longest_wake_ms = now - last;
        if (now - last > 2.5 * period_ms)
            late_wakes++;
        last = now;
        UINT32 padding = 0;
        if (r.failed(ac->GetCurrentPadding(&padding), "GetCurrentPadding"))
            break;
        if (padding == 0)
            empty_wakes++;
        UINT32 frames = buffer_frames - padding;
        if ((unsigned long long)frames > total - written)
            frames = (UINT32)(total - written);
        if (frames && !fill(frames))
            break;
    }
    // Let the engine play what is queued.
    Sleep((DWORD)(1000.0 * buffer_frames / rate) + 200);
    ac->Stop();
    r.num("played_seconds", (now_ms() - start) / 1000.0);
    r.integer("frames_written", (long long)written);
    r.integer("wakes", wakes);
    r.integer("late_wakes", late_wakes);
    r.integer("empty_wakes", empty_wakes);
    r.num("longest_wake_ms", longest_wake_ms);
    return r.finish(out);
}
