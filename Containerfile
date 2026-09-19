# Build a fully static binary, then ship it on a bare Alpine.
#
# Two things need a C toolchain here: rusqlite bundles SQLite from source, and
# russh's default crypto backend (aws-lc-rs) builds BoringSSL with cmake.

FROM docker.io/rust:1.97-alpine AS build

RUN apk add --no-cache \
        musl-dev \
        cmake \
        make \
        perl \
        clang-dev \
        llvm-dev \
        linux-headers

WORKDIR /src

# Dependencies first, so editing src/ does not rebuild russh and SQLite.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# cargo does not notice a source file whose mtime is older than the stale
# artefact from the dependency layer above.
RUN touch src/main.rs && cargo build --release \
    && strip target/release/newbbs

FROM docker.io/alpine:3.22

RUN adduser -D -h /var/lib/newbbs -u 1000 bbs
COPY --from=build /src/target/release/newbbs /usr/local/bin/newbbs

# `data_dir()` is $XDG_DATA_HOME/newbbs, so this puts the database -- and with
# it the ssh host key -- on the volume below.
ENV XDG_DATA_HOME=/var/lib
WORKDIR /var/lib/newbbs
VOLUME /var/lib/newbbs
USER bbs

EXPOSE 2222
ENTRYPOINT ["newbbs"]
CMD ["serve"]
