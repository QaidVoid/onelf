/* The audio fixture: plays a short silent tone through ALSA's default
 * device and reports how it went. A host whose plugins the package
 * honours plays through the sound server and prints AUDIO_OK with the
 * distro that packed it; a host whose plugin cannot load under the
 * package's libc prints AUDIO_FALLBACK and exits zero, which is the
 * safe behaviour the matrix asserts instead of a crash. Either way it
 * then prints one MAP line per shared object the process has mapped,
 * so the matrix can hold them against what the resolver admitted. */
#include <alsa/asoundlib.h>
#include <stdio.h>
#include <string.h>

#ifndef DISTRO_TAG
#define DISTRO_TAG "unknown"
#endif

static int report(const char *outcome) {
    char line[4096];
    char last[4096] = "";
    FILE *maps = fopen("/proc/self/maps", "r");
    printf("%s\n", outcome);
    while (maps && fgets(line, sizeof line, maps)) {
        char *path = strchr(line, '/');
        if (!path || !strstr(path, ".so"))
            continue;
        path[strcspn(path, "\n")] = 0;
        if (strcmp(path, last) != 0)
            printf("MAP %s\n", path);
        snprintf(last, sizeof last, "%s", path);
    }
    if (maps)
        fclose(maps);
    return 0;
}

static int fallback(const char *why) {
    char outcome[256];
    snprintf(outcome, sizeof outcome, "AUDIO_FALLBACK %s", why);
    return report(outcome);
}

int main(void) {
    snd_pcm_t *pcm;
    int err = snd_pcm_open(&pcm, "default", SND_PCM_STREAM_PLAYBACK, 0);
    if (err < 0)
        return fallback(snd_strerror(err));
    err = snd_pcm_set_params(pcm, SND_PCM_FORMAT_S16_LE,
                             SND_PCM_ACCESS_RW_INTERLEAVED, 2, 48000, 1,
                             200000);
    if (err < 0) {
        snd_pcm_close(pcm);
        return fallback(snd_strerror(err));
    }
    short frames[2 * 480] = {0};
    for (int i = 0; i < 10; i++) {
        snd_pcm_sframes_t n = snd_pcm_writei(pcm, frames, 480);
        if (n < 0)
            n = snd_pcm_recover(pcm, (int)n, 1);
        if (n < 0) {
            snd_pcm_close(pcm);
            return fallback(snd_strerror((int)n));
        }
    }
    snd_pcm_drain(pcm);
    int result = report("AUDIO_OK " DISTRO_TAG);
    snd_pcm_close(pcm);
    return result;
}
