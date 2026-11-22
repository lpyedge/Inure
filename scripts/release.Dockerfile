FROM debian:13-slim AS browser

RUN apt-get update \
    && apt-get install -y --no-install-recommends chromium ca-certificates unzip \
    && rm -rf /var/lib/apt/lists/*

FROM browser
ARG BUNDLE_FILE
RUN --mount=type=bind,source=.,target=/input,readonly \
    mkdir -p /opt/rig \
    && unzip -q "/input/$BUNDLE_FILE" -d /opt/rig \
    && test -x /opt/rig/rig-edge-browser

WORKDIR /opt/rig
EXPOSE 39871
ENTRYPOINT ["/opt/rig/rig-edge-browser"]
CMD ["--listen", "0.0.0.0:39871", "--human"]
