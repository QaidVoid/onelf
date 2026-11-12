/* The audio fixture: plays a short silent tone through ALSA's default
 * device and reports how it went. A host whose plugins the package
 * honours plays through the sound server and prints AUDIO_OK with the
 * distro that packed it; a host whose plugin cannot load under the
 * package's libc prints AUDIO_FALLBACK and exits zero, which is the
 * safe behaviour the matrix asserts instead of a crash. */
#include <alsa/asoundlib.h>
#include <stdio.h>

#ifndef DISTRO_TAG
#define DISTRO_TAG "unknown"
#endif

int main(void) {
    snd_pcm_t *pcm;
    int err = snd_pcm_open(&pcm, "default", SND_PCM_STREAM_PLAYBACK, 0);
    if (err < 0) {
        printf("AUDIO_FALLBACK %s\n", snd_strerror(err));
        return 0;
    }
    err = snd_pcm_set_params(pcm, SND_PCM_FORMAT_S16_LE,
                             SND_PCM_ACCESS_RW_INTERLEAVED, 2, 48000, 1,
                             200000);
    if (err < 0) {
        printf("AUDIO_FALLBACK %s\n", snd_strerror(err));
        snd_pcm_close(pcm);
        return 0;
    }
    short frames[2 * 480] = {0};
    for (int i = 0; i < 10; i++) {
        snd_pcm_sframes_t n = snd_pcm_writei(pcm, frames, 480);
        if (n < 0)
            n = snd_pcm_recover(pcm, (int)n, 1);
        if (n < 0) {
            printf("AUDIO_FALLBACK %s\n", snd_strerror((int)n));
            snd_pcm_close(pcm);
            return 0;
        }
    }
    snd_pcm_drain(pcm);
    snd_pcm_close(pcm);
    printf("AUDIO_OK %s\n", DISTRO_TAG);
    return 0;
}
