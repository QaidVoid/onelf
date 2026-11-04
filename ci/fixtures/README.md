# Fixture images for the pinned distro tests
#
# One Dockerfile per distro the distro_fixtures integration test drives:
# ubuntu-24.04, alpine-3.20 and fedora-40. Each pins its base image by
# digest and installs an explicit package list at image-prep time, so a
# run of the tests needs no network beyond the one image build, and
# onelf itself never fetches anything: every package it packs comes from
# libraries the image already has installed, and the recipes carry no
# set pins or sysroots.
#
# The fixture application sources under src/ are compiled inside the
# container by assemble.sh with the image's own toolchain, so the
# applications, their library closure and the packer scan all come from
# the pinned distro rather than from the host.
#
# Building an image by hand, with docker or podman:
#
#   docker build -f ci/fixtures/Dockerfile.ubuntu-24.04 -t onelf-fixture:ubuntu-24.04 ci/fixtures
#   docker build -f ci/fixtures/Dockerfile.alpine-3.20  -t onelf-fixture:alpine-3.20  ci/fixtures
#   docker build -f ci/fixtures/Dockerfile.fedora-40    -t onelf-fixture:fedora-40    ci/fixtures
#
# Running the tests that consume them:
#
#   ONELF_DISTRO_FIXTURES=1 cargo test -p onelf --locked --test distro_fixtures
#
# Without ONELF_DISTRO_FIXTURES=1 and a container engine on PATH the
# tests announce `skip [docker]` and take no time, so the ordinary
# local suite stays unaffected. The CI distro job sets both, and claims
# the capability in ONELF_REQUIRED_CAPS so a skip there fails the job.
#
# The test accepts ONELF_CONTAINER_ENGINE to name the engine explicitly;
# otherwise it uses docker when present and podman otherwise. The
# capability the CI job claims is named docker because that is what the
# job runs on; a local podman run of the same fixtures is welcome.
