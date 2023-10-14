FROM ghcr.io/espressosystems/devops-rust:stable as BUILDER
COPY . .
RUN ls -lah
RUN cargo build --release
