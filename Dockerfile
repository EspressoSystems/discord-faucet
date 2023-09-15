FROM ubuntu:jammy

ARG TARGETARCH

RUN apt-get update \
&&  apt-get install -y curl libcurl4 wait-for-it \
&&  rm -rf /var/lib/apt/lists/*

COPY target/$TARGETARCH/release/discord-faucet /bin/discord-faucet
RUN chmod +x /bin/discord-faucet

CMD [ "/bin/discord-faucet"]
