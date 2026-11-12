FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Dependencies first, so editing the app does not rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src src
COPY relay relay
RUN touch src/main.rs && cargo build --release --locked && mkdir /data

# Static binary on an empty image: no shell, no libc, nothing else to patch. Runs unprivileged.
FROM scratch
COPY --from=build /src/target/release/vigia /vigia
# A named volume mounted here inherits this ownership the first time it is created.
COPY --from=build --chown=65534:65534 /data /data
USER 65534:65534
ENV VIGIA_DB=/data/vigia.db PORT=8080
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["/vigia", "health"]
ENTRYPOINT ["/vigia"]
