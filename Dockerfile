# 多阶段构建：构建阶段
# 版本必须 >= Cargo.toml 的 rust-version（MSRV 1.86，受 url/icu 传递依赖约束），
# 否则镜像构建会在依赖预编译阶段直接失败。
FROM rust:1.86-bookworm AS builder

WORKDIR /build

# 先复制 Cargo 清单以利用 Docker 层缓存：用占位 lib.rs + main.rs 预编译依赖。
# crate 同时有 lib 与 bin target，占位必须两者都造；否则依赖层无法命中、每次全量重编。
# 不吞错误（去掉旧的 `2>/dev/null || true`），预编译失败应立即暴露。
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo 'fn main() {}' > src/main.rs && \
    echo '' > src/lib.rs && \
    cargo build --release --locked && \
    rm -rf src

# 复制实际源码并构建 release 二进制（依赖层已缓存）
COPY src/ src/
RUN cargo build --release --locked

# 运行阶段：最小化镜像
FROM debian:bookworm-slim

# 安装 CA 证书（TLS 校验需要）
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# 创建非 root 用户
RUN useradd --system --no-create-home --shell /sbin/nologin anyproxy

# 复制二进制
COPY --from=builder /build/target/release/any-proxy /usr/local/bin/any-proxy

# 切换到非 root 用户
USER anyproxy

EXPOSE 8080

ENV LISTEN_ADDR=0.0.0.0:8080
ENV RUST_LOG=info

# 用二进制自带的 health-check 子命令，避免依赖镜像内的 wget/curl
# （debian-slim 不含 wget，旧的 wget 健康检查会让容器永久 unhealthy）。
HEALTHCHECK --interval=30s --timeout=5s --start-period=3s --retries=3 \
    CMD ["any-proxy", "health-check"]

ENTRYPOINT ["any-proxy"]
