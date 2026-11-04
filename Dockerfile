# Static musl builds of valkvork-controller, valkvork-agent and valkvork (NFR-03).
#
# Images (published by .github/workflows/release.yml):
#   docker buildx build --target controller -t valkvlabs/valkvork-controller .
#   docker buildx build --target agent      -t valkvlabs/valkvork-agent .
#
# Raw binaries (for the golden image or a tarball):
#   docker buildx build --platform linux/amd64 --target export --output dist/ .
#
# All binaries are statically linked: no glibc or OpenSSL, so the images are
# FROM scratch. The build runs natively on the target platform; building
# another architecture goes through emulation and is much slower.

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev protobuf-dev protoc
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
      -p valkvork-controller -p valkvork-agent -p valkvork-cli \
 && mkdir /out \
 && cp target/release/valkvork-controller target/release/valkvork-agent target/release/valkvork /out/

# `--target export --output dist/` writes just the binaries.
FROM scratch AS export
COPY --from=build /out/ /

# Controller: mount the config at /etc/valkvork and a volume for the database
# (database_path under /var/lib/valkvork). Runs unprivileged; the volume must
# be writable by uid 65532.
FROM scratch AS controller
COPY --from=build /out/valkvork-controller /out/valkvork /usr/local/bin/
USER 65532:65532
EXPOSE 50051 50052
ENTRYPOINT ["/usr/local/bin/valkvork-controller"]
CMD ["--config", "/etc/valkvork/controller.toml"]

# Agent: runs as root (it drives the Docker socket and bind mounts). Run with
#   -v /var/run/docker.sock:/var/run/docker.sock
#   -v /etc/machine-id:/etc/machine-id:ro
#   -v /var/lib/valkvork:/var/lib/valkvork   (same path inside and out, so
#                                            the /alloc bind mounts resolve on the host)
#   -v /etc/valkvork:/etc/valkvork:ro
# In production the golden image runs the binary under systemd instead.
FROM scratch AS agent
COPY --from=build /out/valkvork-agent /out/valkvork /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/valkvork-agent"]
CMD ["--config", "/etc/valkvork/agent.toml"]
