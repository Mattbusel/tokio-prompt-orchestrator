# Reproducible benchmark box: both proxies, the mock upstream and the load generator on one
# Linux container, talking over loopback.
#   docker build -t tpo -f Dockerfile ../..         (the proxy image, from the repo root)
#   docker build -t tpo-bench --build-arg TPO_IMAGE=tpo .
#   docker run --rm --cpus 4 --memory 4g tpo-bench
ARG TPO_IMAGE=ghcr.io/mattbusel/tokio-prompt-orchestrator:latest
FROM ${TPO_IMAGE} AS tpo

FROM python:3.12-slim-bookworm
ARG LITELLM_VERSION=1.109.1
RUN pip install --no-cache-dir "litellm[proxy]==${LITELLM_VERSION}" aiohttp psutil
COPY --from=tpo /usr/local/bin/orchestrator /usr/local/bin/orchestrator
WORKDIR /bench
COPY mock_upstream.py run.py litellm.yaml litellm-cache.yaml ./
ENTRYPOINT ["python", "run.py", "--orchestrator", "/usr/local/bin/orchestrator", "--litellm", "litellm"]
