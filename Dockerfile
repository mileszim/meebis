# syntax=docker/dockerfile:1

# ---- Build stage -----------------------------------------------------------
# Build a fully static musl binary so the final image can be FROM scratch.
FROM rust:1-alpine AS build

# musl-dev for the static libc; gcc so the `cc` crate can compile the vendored
# Lua 5.1 sources that back EVAL/EVALSHA.
RUN apk add --no-cache musl-dev gcc

WORKDIR /src

# Derive the Rust musl target from the build platform so this works under
# buildx for both linux/amd64 and linux/arm64 (musl links statically, letting
# the final image be FROM scratch). TARGETARCH is provided by buildx.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) echo "x86_64-unknown-linux-musl" > /target ;; \
      arm64) echo "aarch64-unknown-linux-musl" > /target ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac
RUN rustup target add "$(cat /target)"

# Cache dependency compilation separately from the source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --target "$(cat /target)" \
    && rm -rf src

COPY . .
# Bust the mtime so cargo rebuilds the real main.rs over the dummy above.
RUN touch src/main.rs \
    && cargo build --release --target "$(cat /target)" \
    && cp "target/$(cat /target)/release/meebis" /meebis

# ---- Runtime stage ---------------------------------------------------------
FROM scratch

COPY --from=build /meebis /meebis

EXPOSE 6379

# Bind all interfaces so the server is reachable from outside the container.
ENTRYPOINT ["/meebis", "--bind", "0.0.0.0"]
