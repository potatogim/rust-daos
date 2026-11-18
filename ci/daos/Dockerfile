# Single-node DAOS test system for the real_daos and real_daos_obj suites.
# Server, agent, dmg, the client library and a Rust toolchain in one image.
# Configuration and scripts are not baked in: ci/daos/up.sh mounts them from
# the checkout, and nothing is copied from the build context.
#
#   docker build --build-arg DAOS_VERSION=2.8.1 -t rust-daos-ci:2.8.1 ci/daos
#
# The CI image tag is <DAOS version>-<hash of this file> (image-tag.sh), so
# any change here, including the base image digest, gives a new tag, while
# changes to the scripts do not. A COPY from the context would need its
# source added to the hash in image-tag.sh. To move to a newer rockylinux:9,
# take the index digest from
#   docker buildx imagetools inspect rockylinux:9
# and replace the one below.

FROM rockylinux:9@sha256:03303863808e9cd79f0c58c833db3e5f9cde9f2abe16804a267b05dc54af86e4

ARG DAOS_VERSION=2.8.1
ENV DAOS_VERSION=${DAOS_VERSION}

# DAOS packages are signed with DAOS_2025_1_GPG_KEY from the release
# directory. The key file is fetched here and must hold exactly one public
# key whose fingerprint (and every fingerprint in the file) is the one below,
# HPE's DAOS package signing key, the same file for 2.6.4 and 2.8.1. The
# repo file is written here rather than taken from packages.daos.io, so only
# this baseurl and this checked key are used, with gpgcheck=1.
ARG DAOS_GPG_KEY=DAOS_2025_1_GPG_KEY
ARG DAOS_GPG_FPR=547AA4241A162E3B5EFDD1896965BFDECA69A315
RUN base="https://packages.daos.io/v${DAOS_VERSION}" \
 && key="/etc/pki/rpm-gpg/${DAOS_GPG_KEY}" \
 && curl -fsSL -o "$key" "${base}/${DAOS_GPG_KEY}" \
 && gpg --show-keys --with-colons "$key" >/tmp/key.txt \
 && pubs=$(awk -F: '$1 == "pub"' /tmp/key.txt | wc -l) \
 && fprs=$(awk -F: '$1 == "fpr" {print $10}' /tmp/key.txt | sort -u | tr '\n' ' ') \
 && if [ "$pubs" != 1 ] || [ "$fprs" != "$DAOS_GPG_FPR " ]; then \
        echo "DAOS key file: $pubs public keys, fingerprints '$fprs', expected one key $DAOS_GPG_FPR" >&2; \
        exit 1; \
    fi \
 && rm /tmp/key.txt \
 && rpm --import "$key" \
 && printf '%s\n' \
        '[daos-packages]' \
        "name=DAOS ${DAOS_VERSION} packages" \
        "baseurl=${base}/EL9/packages/x86_64/" \
        'enabled=1' \
        'gpgcheck=1' \
        "gpgkey=file://$key" \
        >/etc/yum.repos.d/daos.repo \
 && dnf -y install epel-release \
 && dnf -y install daos-server daos-admin daos-client daos-devel \
        gcc procps-ng iproute hostname util-linux pciutils \
 && dnf clean all \
 && rpm -q daos-server daos-client

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
# rustfmt and clippy because rust-toolchain.toml lists them; rustup would
# otherwise download them on the first cargo call. This toolchain is as old
# as the image, so test.sh runs `rustup update stable` before building.
RUN curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | \
        sh -s -- -y --no-modify-path --profile minimal \
        --default-toolchain stable --component rustfmt,clippy \
 && rustc --version && cargo --version

RUN mkdir -p /var/run/daos_server /var/run/daos_agent /var/db/daos_server /mnt/daos
