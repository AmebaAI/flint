FROM python:3.13-alpine

RUN addgroup -S -g 10001 flint \
    && adduser -S -D -H -u 10001 -G flint flint \
    && mkdir -p /workspace \
    && chown 10001:10001 /workspace

COPY server.py /opt/flint-runtime/server.py

ENV PYTHONDONTWRITEBYTECODE=1
USER 10001:10001
WORKDIR /workspace
EXPOSE 8080
LABEL ai.ameba.flint.runtime.name="flint_local" \
    ai.ameba.flint.runtime.protocol="HTTP" \
    ai.ameba.flint.runtime.environment-variables="FLINT_FIXTURE_ALLOWED,FLINT_FIXTURE_UNAPPROVED,FLINT_FIXTURE_UNSET"
ENTRYPOINT ["python3", "/opt/flint-runtime/server.py"]
