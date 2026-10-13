# 图解 Omni Strike 的工作原理

[English](diagrams.md) · [Русский](diagrams.ru.md) · **简体中文**

下面的每张图都按代码的实际情况描述；实现每个步骤的文件名列在图的下方。GitHub 会直接渲染这些 Mermaid 代码块。

1. [系统概览](#1-系统概览)
2. [一次比较的完整流程](#2-一次比较的完整流程)
3. [可比性判定](#3-可比性判定)
4. [单一交易场所上的订单成本](#4-单一交易场所上的订单成本)
5. [路由选择单一场所还是拆单](#5-路由选择单一场所还是拆单)
6. [风控检查](#6-风控检查)
7. [交易模式与准入关卡](#7-交易模式与准入关卡)
8. [非托管执行](#8-非托管执行)
9. [Web UI 接受签署的内容](#9-web-ui-接受签署的内容)
10. [订单生命周期](#10-订单生命周期)
11. [交易场所 actor 的缓存与熔断器](#11-交易场所-actor-的缓存与熔断器)
12. [部署](#12-部署)

## 1. 系统概览

```mermaid
flowchart TB
  subgraph Clients["客户端"]
    UI["Web UI (Next.js)<br/>通过 EIP-6963 连接用户钱包"]
    SDK["TypeScript / Python SDK"]
    AG["通过 oa-mcp (stdio) 接入的 AI 智能体"]
  end
  subgraph Server["oa-server (Rust)"]
    API["HTTP API (Axum)<br/>鉴权 · 速率限制 · OpenAPI"]
    ENG["引擎<br/>比较 · 路由 · 策略 · 二元期权"]
    CORE["oa-core（纯逻辑，无 I/O）<br/>可比性 · 订单簿遍历 · 手续费<br/>成本 · 路由 · 风控"]
    ACT["交易场所 actor (Kameo)<br/>每个场所一个：缓存、去重、熔断器"]
    EXE["执行服务<br/>日志 · 上限 · 对账"]
  end
  V[("20 个交易场所<br/>REST · WebSocket · JSON-RPC")]
  DB[("PostgreSQL<br/>日志 · 报价 · 集成方")]
  UI -->|"/api/* 代理"| API
  SDK --> API
  AG --> API
  API --> ENG
  ENG --> CORE
  ENG --> ACT
  ACT --> V
  ENG --> EXE
  EXE --> V
  EXE --> DB
  ENG --> DB
```

`crates/core` · `crates/adapters` · `crates/app`（`engine.rs`、`venue.rs`、`execution.rs`、`api.rs`）

## 2. 一次比较的完整流程

```mermaid
sequenceDiagram
  autonumber
  participant C as 客户端
  participant A as API
  participant E as 引擎
  participant VA as 交易场所 actor
  participant K as oa-core
  C->>A: POST /v1/quotes/compare（合约、方向、数量、已接受的差异）
  A->>A: 鉴权、速率限制、校验
  A->>E: 比较
  E->>E: 剔除被地域策略排除的交易场所
  par 每个交易场所
    E->>VA: 合约目录（已缓存）
    VA-->>E: 挂牌合约
  end
  E->>E: 在每个场所精确匹配合约，否则为 NO_LISTING
  par 已挂牌的场所，受延迟预算限制
    E->>VA: 订单簿（缓存 / 流式推送）
    VA-->>E: 订单簿或失败
  end
  E->>K: 可比性、订单簿遍历、手续费、排名
  K-->>E: ranked / incomplete / ineligible / unavailable
  E-->>A: 响应
  A-->>C: JSON（小数以字符串表示）
  E--)E: 异步持久化观测数据
```

`engine.rs::compare`、`core/cost.rs`、`core/comparability.rs`

## 3. 可比性判定

两份合约逐项条款进行比较。只有在 `STRICTLY_COMPARABLE` 的情况下，或在 `ECONOMICALLY_SIMILAR_WITH_DIFFERENCES` 且调用方接受了每一个差异代码的情况下，才会在交易场所之间进行排名。

```mermaid
flowchart TD
  S(["合约 A 对比合约 B"]) --> F{"产品类别相同？<br/>定期期权或二元期权"}
  F -- "否" --> NC[NOT_COMPARABLE]
  F -- "是" --> CORE{"标的、看涨/看跌、<br/>行权价、到期时刻"}
  CORE -- "有一项不同" --> NC
  CORE -- "有一项未知" --> II[INSUFFICIENT_INFORMATION]
  CORE -- "全部相同" --> SEC{"行权方式、结算<br/>机制 / 资产 / 指数 / 时间窗口、<br/>权利金币种"}
  SEC -- "全部已知且相同" --> SC[STRICTLY_COMPARABLE]
  SEC -- "任一不同或未知" --> ES["ECONOMICALLY_SIMILAR_WITH_DIFFERENCES<br/>+ 原因代码"]
  ES --> ACC{"调用方接受了<br/>每一个代码？"}
  ACC -- "是" --> RANK["一起排名"]
  ACC -- "否" --> INEL["展示但不排名：<br/>DifferencesNotAccepted"]
  SC --> RANK
```

合约乘数会被归一化消除，绝不会成为差异原因。`core/comparability.rs`

## 4. 单一交易场所上的订单成本

```mermaid
flowchart TD
  B(["订单簿快照 + 请求数量"]) --> FR{"足够新鲜？<br/>max_age、时钟偏差"}
  FR -- "否" --> X1["不合格：Stale / ClockSkew"]
  FR -- "是" --> W["逐档遍历卖单<br/>直到覆盖所需数量"]
  W --> COV{"数量已覆盖？"}
  COV -- "否" --> PART["PARTIAL_COVERAGE：<br/>可见部分的均价"]
  COV -- "是" --> AVG["均价、权利金总额"]
  PART --> FEE
  AVG --> FEE{"该场所和该数量的<br/>手续费公式已知？"}
  FEE -- "否" --> X2["不合格：UnknownCosts<br/>绝不按零计价"]
  FEE -- "是" --> TOT["总成本 = 权利金 + 手续费<br/>按每单位标的计"]
  TOT --> R(["按总成本排名"])
```

`core/book.rs`、`core/fees.rs`、`core/cost.rs`

## 5. 路由选择单一场所还是拆单

```mermaid
flowchart TD
  Q(["买入意图 + 拆单策略"]) --> RV["可路由的交易场所：<br/>唯一原因 = PARTIAL_COVERAGE"]
  RV --> SUB["规模 ≤ max_legs 的所有子集<br/>关闭拆单时为 1"]
  SUB --> MERGE["合并卖单档位，按每单位标的<br/>从最便宜开始，在预算范围内"]
  MERGE --> RECOST["在各自场所上精确重算每条腿的成本<br/>固定手续费每条腿只付一次"]
  RECOST --> FULL{"是否有方案覆盖<br/>全部数量？"}
  FULL -- "否" --> P["PARTIAL：最优的部分方案，<br/>不可执行"]
  FULL -- "是" --> SINGLE{"单一场所<br/>能否覆盖？"}
  SINGLE -- "否" --> SCOV[SPLIT_COVERAGE]
  SINGLE -- "是" --> SAVE{"拆单节省 ≥ min_saving_abs<br/>且 ≥ min_saving_bps？"}
  SAVE -- "是" --> SCOST[SPLIT_COST]
  SAVE -- "否" --> ONE["SINGLE：最便宜的场所"]
  SCOV --> LIM
  SCOST --> LIM
  ONE --> LIM["每条腿的 IOC 限价 = 最差档位<br/>× 1 + 容差，向上取整到最小报价单位"]
  LIM --> RQ(["route_id + route_hash，一次性使用，30 秒"])
```

`core/routing.rs`、`engine.rs::route_quote` — 详见 [routing.md](routing.md)

## 6. 风控检查

在发送任何内容之前，所有检查都会执行，并返回所有违规项（不会短路）。

```mermaid
flowchart LR
  I(["订单意图"]) --> C1["紧急停止开关"]
  C1 --> C2["当前模式允许提交"]
  C2 --> C3["交易场所、网络、标的、<br/>产品类别均被允许"]
  C3 --> C4["允许卖出？<br/>主网上关闭"]
  C4 --> C5["数量 > 0，限价 > 0"]
  C5 --> C6["最差成本 ≤ 单笔订单上限"]
  C6 --> C7["已承诺支出 + 本订单<br/>≤ 时间窗口上限"]
  C7 --> C8["限价在参考价格的<br/>最大偏离范围内"]
  C8 --> C9["策略最大亏损有界"]
  C9 --> OK{"有任何违规？"}
  OK -- "是" --> REJ(["拒绝并返回违规列表"])
  OK -- "否" --> GO(["先写入日志，再发送"])
```

`core/risk.rs`、`engine.rs::risk_limits`；时间窗口上限会在提交时于锁内再次检查（`execution.rs::reserve_session_spend`）。

## 7. 交易模式与准入关卡

```mermaid
flowchart TD
  BUILD{"构建 features"} -->|"默认"| D["READ_ONLY · SIMULATION"]
  BUILD -->|testnet| T[TESTNET]
  BUILD -->|mainnet-nc| M[MAINNET_NONCUSTODIAL]
  BUILD -->|live| L["编译错误：<br/>托管式实盘不存在"]
  BUILD -->|"testnet + mainnet-nc"| L2["编译错误"]
  T --> TG{"启动检查：数据库名<br/>包含 'testnet'？"}
  M --> MG{"启动检查：数据库名<br/>包含 'mainnet_nc'？"}
  TG -- "否" --> STOP["拒绝启动"]
  MG -- "否" --> STOP
  TG -- "是" --> TK["仅测试网执行器<br/>测试密钥来自 .env.testnet"]
  MG -- "是" --> MK["仅用户签名执行器<br/>无密钥加载器"]
  TK --> KS{"OA_KILL_SWITCH=false？"}
  MK --> KS
  KS -- "否" --> RO["拒绝所有订单"]
  KS -- "是" --> EX["在上限内允许下单"]
```

`crates/app/src/lib.rs`、`config.rs`、`bin/server.rs`

## 8. 非托管执行

```mermaid
sequenceDiagram
  autonumber
  participant U as 用户钱包
  participant C as 客户端
  participant S as oa-server
  participant DB as Postgres
  participant V as 交易场所 / 链
  C->>S: POST /v1/routes/quote
  S-->>C: route_id、route_hash、各条腿 + IOC 限价
  C->>S: POST /v1/noncustodial/prepare（路由、用户账户）
  S->>S: 消费路由（一次性使用），风控检查
  S->>DB: VALIDATED 记录 + 已准备的执行（4.5 分钟后过期）
  S-->>C: 每条腿的 EIP-712 类型化数据，或一笔待发送的交易
  C->>U: eth_signTypedData_v4 / eth_sendTransaction
  U-->>C: 签名 / 交易哈希
  C->>S: POST /v1/noncustodial/submit
  S->>S: 恢复每个签名者 = 声明的账户（在消费之前）
  S->>DB: 取用已准备的执行（原子操作，跨实例仅一次）
  S->>DB: 再次检查时间窗口上限，记录置为 RESERVED / SUBMITTING
  S->>V: 转发已签名订单，或读取已上链的交易回执
  V-->>S: 成交
  S->>DB: 最终状态
  S-->>C: 执行报告
```

`execution.rs::prepare_user_signed` / `submit_user_signed`、`adapters/src/*_user.rs`

## 9. Web UI 接受签署的内容

即使服务器已被攻破，页面也会拒绝任何不符合以下规则的内容。

```mermaid
flowchart TD
  P(["来自服务器的已准备执行"]) --> D{"Derive 腿？"}
  D -- "是" --> DC{"chainId 为 1 或 11155111，<br/>verifyingContract = Derive Matching，<br/>message.owner = 已连接的钱包？"}
  DC -- "否" --> STOP(["拒绝"])
  DC -- "是" --> SIGN[eth_signTypedData_v4]
  D -- "否" --> TX{"交易类腿<br/>Thetanuts / CallPut"}
  TX --> TC{"Base 链、已知合约、<br/>ETH 金额 ≤ 允许值、<br/>授权：USDC、已知 spender、<br/>金额 ≤ 2 × 已签名的最差成本？"}
  TC -- "否" --> STOP
  TC -- "是" --> AL{"当前授权额度<br/>低于所需？"}
  AL -- "是" --> APP["发送精确金额的授权，等待回执"]
  AL -- "否" --> SEND
  APP --> SEND["发送交易，等待回执"]
  SIGN --> AUTH["Derive 登录：personal_sign 时间戳"]
  SEND --> SUB(["提交给服务器"])
  AUTH --> SUB
```

`web/lib/noncustodial.ts`、`web/lib/wallet.ts`

## 10. 订单生命周期

```mermaid
stateDiagram-v2
  [*] --> VALIDATED: 风控检查通过，已写入日志
  VALIDATED --> CANCELLED: 从未签名（过期）/ 提交时已达上限
  VALIDATED --> SUBMITTING: 已预留上限额度，发送前写入日志
  SUBMITTING --> ACKNOWLEDGED
  SUBMITTING --> FILLED
  SUBMITTING --> PARTIALLY_FILLED
  SUBMITTING --> REJECTED: 明确的 4xx
  SUBMITTING --> UNKNOWN: 超时 / 传输错误
  UNKNOWN --> FILLED: 通过交易场所对账
  UNKNOWN --> PARTIALLY_FILLED
  UNKNOWN --> REJECTED
  UNKNOWN --> CANCELLED
  ACKNOWLEDGED --> FILLED
  ACKNOWLEDGED --> CANCELLED: IOC 剩余部分
  PARTIALLY_FILLED --> FILLED
  PARTIALLY_FILLED --> CANCELLED: IOC 剩余部分
  FILLED --> [*]
  REJECTED --> [*]
  CANCELLED --> [*]
```

`UNKNOWN` 状态的订单会通过其预先计算的交易场所订单 id 进行查询（先快速尝试三次，然后每 10 秒一次，重启后也会继续）。它**绝不会被重新提交**。`execution.rs`（`submit_row`、`OrderTracker`）

## 11. 交易场所 actor 的缓存与熔断器

```mermaid
stateDiagram-v2
  [*] --> Closed
  Closed --> Closed: 请求成功（缓存已更新）
  Closed --> Closed: 失败（连续不足 5 次，缓存标记为 Failed）
  Closed --> Open: 连续 5 次失败 → 30 秒
  Closed --> Open: HTTP 429 → Retry-After（≤ 60 秒）
  Open --> Closed: 等待时间结束，允许下一次请求
  note right of Closed
    每个 key 同时只有一个进行中的请求，
    并发调用方共享该请求，
    订单簿按 LRU 限制数量 (256)
  end note
```

`crates/app/src/venue.rs`

## 12. 部署

```mermaid
flowchart LR
  NET(("互联网")) -->|"443 / 80"| CADDY["Caddy<br/>HTTPS · 安全响应头"]
  CADDY -->|"/"| WEB["web :3000"]
  CADDY -->|"/api/* 白名单 + token"| OA["oa-server :8080"]
  CADDY -->|"/v1/*（集成方密钥）"| OA
  CADDY -.->|"/v1/admin → 404"| X["已拦截"]
  OA --> PG[("Postgres<br/>不对外暴露")]
  PROM["Prometheus 127.0.0.1"] -.-> PG
  ADMIN["运维人员"] -->|"SSH 隧道"| OA
```

`deploy/docker-compose.yml`、`deploy/Caddyfile*` — 详见 [deployment.md](deployment.md)
