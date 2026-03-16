FROM rustlang/rust:nightly-alpine AS build

WORKDIR /app
RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM alpine:3.19

WORKDIR /app

RUN apk add --no-cache ca-certificates ffmpeg python3 py3-pip openssl iproute2 nodejs && \
    update-ca-certificates && \
    pip install --break-system-packages --upgrade yt-dlp && \
    mkdir -p /root/.config/yt-dlp && \
    echo -e "--js-runtimes node\n--remote-components ejs:github" > /root/.config/yt-dlp/config

COPY --from=build /app/target/release/astralink /usr/local/bin/astralink
COPY cookies.txt /app/cookies.txt

ENV RUST_LOG=${RUST_LOG:-info,astralink=debug} \
    RUST_BACKTRACE=1 \
    ASTRALINK_YT_COOKIES_FILE=/app/cookies.txt

EXPOSE 9000 9001

CMD ["astralink"]