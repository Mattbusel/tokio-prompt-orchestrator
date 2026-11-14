# syntax=docker/dockerfile:1.7
# tokio-prompt-orchestrator: OpenAI-compatible LLM proxy in one static-ish binary.
#
#   docker run --rm -p 8080:8080 ghcr.io/mattbusel/tokio-prompt-orchestrator          # echo mode, no key
#   docker run --rm -p 8080:8080 -e OPENAI_API_KEY=sk-... \
#     ghcr.io/mattbusel/tokio-prompt-orchestrator --provider openai --model gpt-4o-mini
#
# Then point any OpenAI client at http://localhost:8080/v1.

FROM rust:1-bookworm AS builder
WORKDIR /src
# .cargo/config.toml moves target-dir for local agent sessions; build into ./target here.
ENV CARGO_TARGET_DIR=/src/target
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --features web-api --bin orchestrator \
 && cp target/release/orchestrator /orchestrator

# Distroless: glibc + CA certificates, no shell, runs as uid 65532.
FROM gcr.io/distroless/cc-debian12:nonroot
# Links the GHCR package to the repo (visibility, README, permissions).
LABEL org.opencontainers.image.source="https://github.com/Mattbusel/tokio-prompt-orchestrator" \
      org.opencontainers.image.description="OpenAI-compatible LLM proxy: shares identical prompts, caps spend, fails fast" \
      org.opencontainers.image.licenses="MIT"
COPY --from=builder /orchestrator /usr/local/bin/orchestrator
WORKDIR /home/nonroot
EXPOSE 8080
ENV RUST_LOG=info
# --host 0.0.0.0 is fixed so the port is reachable; arguments after the image name replace CMD.
ENTRYPOINT ["/usr/local/bin/orchestrator", "--host", "0.0.0.0"]
CMD ["--provider", "echo"]
