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
ENTRYPOINT ["python3", "/opt/flint-runtime/server.py"]
