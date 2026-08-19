FROM oven/bun:1-alpine

USER root
RUN addgroup -S -g 10001 runtime \
    && adduser -S -D -H -u 10001 -G runtime runtime \
    && mkdir -p /workspace \
    && chown 10001:10001 /workspace

COPY --chown=10001:10001 server.ts /opt/runtime/server.ts

USER 10001:10001
WORKDIR /workspace
EXPOSE 8080
LABEL ai.ameba.flint.runtime.descriptor="{\"name\":\"example\",\"protocol\":\"HTTP\"}"
ENTRYPOINT ["bun", "/opt/runtime/server.ts"]
