// The Qt fixture: a headless QCoreApplication so the run needs no
// display; qVersion() proves the bundled Qt 6 core answered.
#include <QCoreApplication>
#include <cstdio>

int main(int argc, char **argv) {
    QCoreApplication app(argc, argv);
    std::printf("QT_OK %s\n", qVersion());
    return 0;
}
