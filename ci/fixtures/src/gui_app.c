/* The GTK fixture: links GTK 3 and asks it to initialise, tolerating a
 * missing display, so the whole linked library chain must load from the
 * bundle in a headless container while the version report proves the
 * bundled GTK answered. */
#include <gtk/gtk.h>
#include <stdio.h>

int main(int argc, char **argv) {
    gboolean have_display = gtk_init_check(&argc, &argv);
    printf("GTK_OK %u.%u.%u display=%s\n",
           gtk_get_major_version(),
           gtk_get_minor_version(),
           gtk_get_micro_version(),
           have_display ? "yes" : "no");
    return 0;
}
