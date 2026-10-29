# syntax=docker/dockerfile:1

ARG RUST_VERSION=1.93.0

# ── chef ─────────────────────────────────────────────────────────────────────
# cargo-chef is what makes a source-only rebuild take seconds instead of minutes:
# it reduces the manifest set to a recipe, cooks exactly the dependency crates,
# and only then copies our source over the top.
FROM rust:${RUST_VERSION}-slim-bookworm AS chef
# btleplug talks to BlueZ through the C libdbus, so the headers have to be here
# at build time even though the daemon never loads them itself. See the runtime
# stage for the corresponding library.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libdbus-1-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /src

# ── plan ─────────────────────────────────────────────────────────────────────
FROM chef AS plan
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# ── build ────────────────────────────────────────────────────────────────────
FROM chef AS build
# The commit being built, so the running container can say which build it is.
# Passed by the workflows; an unset value simply leaves the version bare.
ARG NEEWERTV_BUILD=""
ENV NEEWERTV_BUILD=${NEEWERTV_BUILD}
COPY --from=plan /src/recipe.json recipe.json
# Dependencies only. This layer is reused for every change to our own code.
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# ── runtime ──────────────────────────────────────────────────────────────────
# debian-slim is ~75 MB of that total on its own, so the image lands near 86 MB
# rather than the 60 MB target. Two ways down, both deferred until the Bluetooth
# backend is chosen:
#
#   * `bluer` (pure-Rust zbus) needs no libdbus at runtime, so the whole image can
#     become distroless or musl and drop to ~30 MB;
#   * `btleplug` pulls the C libdbus, which pins us to a glibc base with the
#     library present.
#
# Guessing now would mean rewriting this stage twice. The shell that comes with
# debian-slim is also genuinely useful while the Bluetooth layer is being built.
FROM debian:bookworm-slim AS final

LABEL org.opencontainers.image.title="neewertv" \
      org.opencontainers.image.description="Bridge a NEEWER NS03-series TV backlight onto MQTT" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/Hyper-Dragon/NeewerTV"

# ca-certificates covers MQTT over TLS, and libdbus-1-3 is what btleplug links
# against at runtime. Nothing else is needed: the Bluetooth link arrives over the
# host's D-Bus socket, which is bind-mounted in.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libdbus-1-3 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/neewertv /usr/local/bin/neewertv

# Root is deliberate, not an oversight.
#
# BlueZ's D-Bus policy on the *host* allows org.bluez only for root and the
# bluetooth group. The container's uid is the host's uid — there is no mapping —
# so an unprivileged uid here is simply refused on the bus, with an error that
# reads like a broken adapter. Two supported ways to avoid root:
#
#   1. add a <policy user="…"> block for a dedicated uid to the host's
#      /etc/dbus-1/system.d/bluetooth.conf, then run the container as that uid;
#   2. run as root, which is what docker-compose.example.yml does.
#
# Either way the only host resource reachable is the single bus socket below.
ENV WEB_INTERFACE=observe \
    WEB_BIND=0.0.0.0:8080 \
    LOG_LEVEL=info \
    LOG_FORMAT=text \
    BLE_KEEPALIVE_SECONDS=45 \
    ALERT_BUTTONS=red,blue,green \
    ALERT_BUTTON_LENGTH=10 \
    DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/dbus/system_bus_socket

EXPOSE 8080

# Self-probing: the binary talks to its own /healthz, so the image needs no curl.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["neewertv", "--healthcheck"]

ENTRYPOINT ["neewertv"]
