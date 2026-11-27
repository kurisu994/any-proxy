# any-proxy 记忆银行（Memory Bank）

本目录保存 `any-proxy` 的长期项目记忆，按「变更频率 × 归属」分为四层，减少协作冲突并支持按代码路径自动注入。

> 若本文缺失相关条款或与项目入口（如 `AGENTS.md`）存在冲突，回查全局规范 `~/develop/Agent/rules/memory-bank.md`（写入门槛与个人日志归属以全局规范为底线）。

## 目录与四层结构

```text
memory-bank/
├── README.md                  ← AI 入口：索引、路径映射表与协作约定
├── 00-project.md              ← 项目定位、领域术语、里程碑与残余风险（开局必读）
├── 10-security-connector.md   ← 分领域编码规范（由 hook 按 paths 自动注入）
├── 11-relay-http.md
├── 12-runtime-resources.md
├── 13-build-deploy-ci.md
├── active/                    ← 当前活跃任务（按分支名拆分，单文件维护目标/验收/备注）
│   └── main.md
├── journal/                   ← 个人开发日志（确认归属后只追加，.gitattributes 配 merge=union）
│   └── <dev>.md
└── archive/YYYY-MM/           ← 已完成任务与历史里程碑归档（只增不改）
    └── progress-legacy.md
```

| 层 | 内容 | 变更频率 | 归属 | 写入方式 | Git 合并行为 |
|---|---|---|---|---|---|
| `00-project.md`、`1X-*.md` | 项目共识与领域规范 | 极低 | 团队 | 改写（保持与代码事实一致） | 人工解决冲突（刻意不设 union） |
| `active/<branch>.md` | 当前任务目标、验收标准、备注 | 任务内高频 | 单分支 | 自由更新 | 单分支维护 |
| `journal/<dev>.md` | 本人有效进展与踩坑记录 | 有增量才写 | 个人 | **确认归属后只追加、段末留空行、禁改历史** | `merge=union` 自动合并 |
| `archive/YYYY-MM/` | 已完成任务或里程碑归档 | 一次性 | 团队 | 写完不再改 | 新增文件不冲突 |

## 路径触发映射表

以下映射表由各 `1X-*.md` 的 `paths` frontmatter 实际抽取。已为 **Claude Code**（`.claude/hooks/inject-memory-bank.py`）与 **Antigravity**（`.agents/hooks/inject-memory-bank.py`）安装路径触发注入 hook；不支持自动注入的平台可按此表人工查阅：

| 规范文件 | 说明 | 触发路径 (`paths`) |
|---|---|---|
| `10-security-connector.md` | 安全内核规范：Target URL 解析、fail-closed AddressPolicy、Connector 连接固定与 TLS 握手、重定向状态机 | `src/target.rs` `src/resolver.rs` `src/connector.rs` `src/redirect.rs` `tests/integration.rs` `tests/tls_spike.rs` `tests/fixture.rs` |
| `11-relay-http.md` | HTTP Relay 编排规范：Axum 路由、C1 访问控制与不触网拒绝、Header/CORS/Trailer 清理、WebSocket 透明升级与错误日志 | `src/app.rs` `src/proxy.rs` `src/headers.rs` `src/error.rs` `src/telemetry.rs` `src/lib.rs` `src/ws.rs` `tests/relay.rs` |
| `12-runtime-resources.md` | 运行时资源边界规范：环境变量配置与启动 gate、并发 permit 生命周期、逐 frame idle timeout、安全连接池与预算限速 | `src/main.rs` `src/config.rs` `src/concurrency.rs` `src/body_timeout.rs` `src/budget.rs` `src/pool.rs` `tests/concurrency.rs` `tests/pool_and_ws.rs` `tests/streaming_memory.rs` |
| `13-build-deploy-ci.md` | 构建依赖、容器镜像、Caddy/deploy.sh 公网部署与 CI/Release 流水线规范 | `Cargo.toml` `Dockerfile` `docker-compose*.yml` `Caddyfile` `deploy.sh` `.env.example` `.github/workflows/**` |

- 单份规范体量上限：9,000 字符（`wc -m` 统计，硬上限 9,400），超限按代码路径维度继续拆分。
- 严禁往仓库里放入自动生成的汇总表（如「活跃任务一览」），避免无意义的派生冲突。

## 写入门槛

- 仅记录值得跨会话保留的实质进展、重要决策、可复用结论和未解决阻塞。
- 纯查询、无变化的例行检查，以及没有新结论的重复验证，不修改任何记忆文件。
- 有有效增量时只更新内容所属的必要层；不要为完成收尾而追加流水，也不把同一内容重复抄到各层。
- 不因采用本门槛而删除已有历史记录。

## 个人日志归属

写入 `journal/<dev>.md` 前，先确认本次任务的开发者身份；同一任务内已核实且用户、仓库或身份配置未变时可以复用结果。现有个人日志以各文件开头记载的归属为准，不互相复用。

1. 优先采用用户明确指定的日志身份或项目已有的身份映射。没有明确归属时，在**当前仓库**执行以下只读命令，核对有效作者身份及配置来源，不只读取全局配置：
   ```bash
   git var GIT_AUTHOR_IDENT
   git config --show-origin --get-regexp '^user\.(name|email)$'
   ```
2. Git 身份是开发者归属的证据，不等于登录者身份。若与用户说明、项目映射冲突，核对仓库/工作树配置及作者环境变量覆盖；不能用工作目录名、系统用户名、已有日志文件名或最近提交作者单独认定当前用户，也不要修改 Git 配置来迎合某个日志文件。
3. 将已确认身份与项目映射或现有日志中明确记载的归属对应，匹配后才追加。没有对应文件时创建本人的新日志，不复用他人的文件；无命名约定时用已核实的开发者名转小写、空白替换为 `-`，文件名不得包含路径分隔符。同名文件归属不明或冲突时先确认，不覆盖。
4. 新建日志注明开发者名、核实日期和身份来源即可，不必复制完整邮箱等个人资料。无法查询 Git、配置为共享/机器人身份或检查后仍有歧义时，先完成不依赖个人归属的工作，再向用户确认日志身份；确认前不写任何个人日志。
5. 个人日志只追加且段末留空行，不搬运他人历史。若误写发生在本任务内，只撤销可确认由自己加入的内容，保留原有记录；不要为纠正身份而重写整份日志。
