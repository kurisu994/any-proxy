# 多阶段构建：构建阶段
FROM rust:1.83-bookworm AS builder

WORKDIR /build

# 先复制 Cargo 文件以利用 Docker 缓存
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    mkdir -p tests && \
    cargo build --release 2>/dev/null || true

# 复制实际源码
COPY src/ src/
COPY tests/ tests/

# 构建 release 二进制
RUN cargo build --release

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

HEALTHCHECK --interval=30s --timeout=5s --start-period=3s --retries=3 \
    CMD wget -qO- http://localhost:8080/healthz || exit 1

ENTRYPOINT ["any-proxy"]
