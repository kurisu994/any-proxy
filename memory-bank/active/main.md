# 活跃任务：main（v0.1.0 后自用运维与文档维护）

> 本仓库直接在 `main` 分支演进，不建长期功能分支。2026-07-25 已拍板项目定位为「自用 / 实验项目，不按商业产品运作」。

## 目标

保持 `any-proxy` 自用部署形态稳定可用，跟进 `v0.1.0` 发布后的包可见性、依赖维护与文档事实对齐。

## 验收标准

- [x] M0 安全连接器、M1 完整 Relay、C1 安全带（批次 1 访问控制与启动 gate + 批次 2 出口预算与限速）完成并通过全部测试
- [x] D3 Caddy 公网 HTTPS 编排、`deploy.sh` 一键部署（三段快速探测）与多架构双源发布流水线（Docker Hub + GHCR）落地
- [x] `v0.1.0` tag 已打出（`51d472d` 修复 `upload-artifact` 含冒号 digest 文件名）
- [x] `AGENTS.md`、`DESIGN.md`、`TODOS.md` 与当前代码事实对齐，并完成记忆银行四层迁移
- [ ] 核实 Docker Hub（`kurisu003/any-proxy:0.1.0`）与 GHCR（`ghcr.io/kurisu994/any-proxy:0.1.0`）包可见性为 Public，且 arm64 产物可匿名拉取
- [ ] 关注上游 `rustls-native-certs` 更新以自然消除信息级告警 `RUSTSEC-2025-0134`（0 漏洞，不阻塞）

## 备注

- **定位关闭项（见 `TODOS.md`）**：C2 边缘形态、SBOM/provenance、Prometheus exporter、E2/E6 库级逃生口门控、C4 abuse kill-switch 均已明确关闭；重开条件为「长期向他人公网开放」。
