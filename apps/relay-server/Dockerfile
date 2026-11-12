# The CretLink relay server as a container image (ST-M007-015, REQ-N-023).
#
#   docker build -f apps/relay-server/Dockerfile -t cretlink-relay-server .
#
# The build context is the root of the repository. The image holds one
# statically linked program, its licence and the notices of the packages it is
# built from, on an empty base: no shell, no package manager and no other
# program. It runs as an unprivileged user and writes nothing, so it also runs
# with a read-only root file system. See docs/relay-server.md.

# The builder: the official Rust image on Alpine, whose host target links
# against musl and produces a static program. Pinned by digest so that a
# rebuild compiles with the same toolchain; the tag is there for people.
ARG RUST_IMAGE=rust:1.99.1-alpine3.22@sha256:9a744dc47d1ac2a7d17fc3cedfc8b4928b1696dfcba3274ab25bfb40988a9ff0

FROM ${RUST_IMAGE} AS build

# The TLS provider (aws-lc-rs, ADR-0017) compiles C and assembly; the notices
# are written by a script of this repository that runs on Node.js.
RUN apk add --no-cache build-base cmake perl nodejs

WORKDIR /src
COPY . .

# The version the program reports (`cretlink_shared::RELEASE_VERSION`). A
# release passes its version; any other build is a development build.
ARG CRETLINK_VERSION=0.0.0-dev
ENV CRETLINK_VERSION=${CRETLINK_VERSION}

# Build from the locked dependencies, then refuse a result that is not what the
# image promises: a static program that starts and reports its version.
RUN cargo build --release --locked --package cretlink-relay-server \
    && mkdir /out \
    && cp target/release/cretlink-relay-server /out/cretlink-relay-server \
    && ! readelf --program-headers /out/cretlink-relay-server | grep -q INTERP \
    && /out/cretlink-relay-server --help > /dev/null

# The licence texts and notices of every package that is linked into the
# program, from the same locked dependencies and for this target.
RUN node scripts/third-party-notices.mjs --package cretlink-relay-server \
        --target "$(rustc --version --verbose | sed -n 's/^host: //p')" \
        --output /out/THIRD-PARTY-NOTICES.txt \
    && cp LICENSE /out/LICENSE

# The image: nothing but the program and its licence texts.
FROM scratch

# What the image is and where its source lives. The registry of GitHub reads
# the source label to show the package with its repository.
ARG CRETLINK_VERSION=0.0.0-dev
LABEL org.opencontainers.image.title="CretLink relay server" \
      org.opencontainers.image.description="Forwards end-to-end encrypted CretLink sessions between two computers; holds no session key." \
      org.opencontainers.image.source="https://github.com/yveslaurentcreton/CretLink" \
      org.opencontainers.image.documentation="https://github.com/yveslaurentcreton/CretLink/blob/main/docs/relay-server.md" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${CRETLINK_VERSION}"

COPY --from=build --chmod=0555 /out/cretlink-relay-server /cretlink-relay-server
COPY --from=build --chmod=0444 /out/LICENSE /out/THIRD-PARTY-NOTICES.txt /

# A fixed user without a name, a home or any rights. The relay needs none: it
# listens on a port above 1024 and reads only the files it is given.
USER 65532:65532

# The relay's default port. Configure the rest with the environment variables
# of docs/relay-server.md and with mounted files (certificate, key, key file).
EXPOSE 9970

ENTRYPOINT ["/cretlink-relay-server"]
