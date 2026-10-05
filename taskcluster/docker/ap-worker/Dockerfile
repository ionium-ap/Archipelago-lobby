ARG PYTHON_VERSION=3.12
ARG SRC=topsrcdir
ARG DOCKER_SRC=/
# The Archipelago base to build a worker for. Each one has a pin file in bases/
ARG AP_BASE=0.6.7


FROM docker.io/library/alpine:latest AS enemizer

ADD --checksum=sha256:efab6784cbfe4189a01e0e25226943afd7f71e7c2f10f74b88bfa34fdac972ab https://github.com/Ijwu/Enemizer/releases/latest/download/ubuntu.16.04-x64.zip Enemizer.zip
RUN apk update && apk add unzip && unzip Enemizer.zip -d /Enemizer

FROM ghcr.io/astral-sh/uv:python${PYTHON_VERSION}-trixie AS builder
ARG SRC
ARG DOCKER_SRC
ARG AP_BASE

ENV UV_PYTHON_DOWNLOADS=0

WORKDIR /ap

# %include ap-worker

COPY ${SRC}/ap-worker /ap/ap-worker
COPY ${DOCKER_SRC}/prepare_worlds.sh /ap/prepare_worlds.sh
COPY ${DOCKER_SRC}/setup.sh /ap/setup.sh
COPY ${DOCKER_SRC}/bases/${AP_BASE}.env /ap/base.env

RUN sh /ap/setup.sh /ap/base.env ${AP_BASE} && rm /ap/setup.sh /ap/base.env

FROM docker.io/library/python:${PYTHON_VERSION}-slim-trixie
ARG DOCKER_SRC

RUN apt update && apt install -y curl libtk8.6 libpq5
RUN curl -fsSL https://github.com/watchexec/watchexec/releases/download/cli-v1.20.5/watchexec-1.20.5-x86_64-unknown-linux-gnu.deb -o /tmp/watchexec.deb
RUN dpkg -i /tmp/watchexec.deb

RUN mkdir /ap
WORKDIR /ap

COPY ${DOCKER_SRC}/host.yaml /ap/archipelago/host.yaml
COPY --from=enemizer /Enemizer /ap/enemizer
RUN chmod +x /ap/enemizer/EnemizerCLI.Core

COPY --from=builder /ap/archipelago/ /ap/archipelago/
COPY --from=builder /ap/supported_worlds/ /ap/supported_worlds/
COPY --from=builder /ap/ap-worker/ /ap/ap-worker/

ENV PATH="/ap/archipelago/.venv/bin:$PATH"
