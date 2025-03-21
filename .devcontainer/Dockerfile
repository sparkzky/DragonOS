FROM ghcr.io/samuka007/ci-os-contest-image:sha256-d931355ccf6b83518b1751db4110d1bf4a7c8eef3f5e8891c6fbe79bef6dde54

RUN apt install -y --no-install-recommends \
    bison flex libssl-dev bridge-utils dnsmasq sudo iptables

RUN cargo install --git https://github.com/Samuka007/DADK.git --branch kpartx-docker-support

ENTRYPOINT ["tini", "--"]