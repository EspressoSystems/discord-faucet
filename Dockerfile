FROM ubuntu:jammy

ARG TARGETARCH

RUN apt-get update \
&&  apt-get install -y curl libcurl4 wait-for-it \
&&  rm -rf /var/lib/apt/lists/*

COPY target/$TARGETARCH/release/discord-faucet /bin/discord-faucet
RUN chmod +x /bin/discord-faucet

ENV ESPRESSO_DISCORD_FAUCET_PORT=8111

CMD [ "/bin/discord-faucet"]

HEALTHCHECK --interval=2s --timeout=1s --retries=10 CMD curl --fail http://localhost:$ESPRESSO_DISCORD_FAUCET_PORT/healthcheck || exit 1
