# Как работает Omni Strike: диаграммы

[English](diagrams.md) · **Русский** · [简体中文](diagrams.zh-CN.md)

Каждая диаграмма ниже описывает код в его текущем виде; под каждой указан файл, реализующий
соответствующий шаг. GitHub отображает блоки Mermaid напрямую.

1. [Обзор системы](#1-обзор-системы)
2. [Одно сравнение от начала до конца](#2-одно-сравнение-от-начала-до-конца)
3. [Вердикт о сопоставимости](#3-вердикт-о-сопоставимости)
4. [Стоимость ордера на одной площадке](#4-стоимость-ордера-на-одной-площадке)
5. [Маршрутизация: одна площадка или разбиение](#5-маршрутизация-одна-площадка-или-разбиение)
6. [Проверки рисков](#6-проверки-рисков)
7. [Режимы торговли и защитные барьеры](#7-режимы-торговли-и-защитные-барьеры)
8. [Некастодиальное исполнение](#8-некастодиальное-исполнение)
9. [Что веб-интерфейс соглашается подписать](#9-что-веб-интерфейс-соглашается-подписать)
10. [Жизненный цикл ордера](#10-жизненный-цикл-ордера)
11. [Актор площадки: кэш и circuit breaker](#11-актор-площадки-кэш-и-circuit-breaker)
12. [Развёртывание](#12-развёртывание)

## 1. Обзор системы

```mermaid
flowchart TB
  subgraph Clients["Клиенты"]
    UI["Веб-интерфейс (Next.js)<br/>кошелёк пользователя через EIP-6963"]
    SDK["SDK для TypeScript / Python"]
    AG["ИИ-агент через oa-mcp (stdio)"]
  end
  subgraph Server["oa-server (Rust)"]
    API["HTTP API (Axum)<br/>аутентификация · лимиты запросов · OpenAPI"]
    ENG["Движок<br/>сравнение · маршрут · стратегия · бинарные"]
    CORE["oa-core (чистая логика, без I/O)<br/>сопоставимость · проход по стакану · комиссии<br/>стоимость · маршрутизация · риск"]
    ACT["Акторы площадок (Kameo)<br/>по одному на площадку: кэш, дедупликация, breaker"]
    EXE["Сервис исполнения<br/>журнал · лимиты · сверка"]
  end
  V[("20 площадок<br/>REST · WebSocket · JSON-RPC")]
  DB[("PostgreSQL<br/>журнал · котировки · интеграторы")]
  UI -->|"прокси /api/*"| API
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

`crates/core` · `crates/adapters` · `crates/app` (`engine.rs`, `venue.rs`, `execution.rs`, `api.rs`)

## 2. Одно сравнение от начала до конца

```mermaid
sequenceDiagram
  autonumber
  participant C as Клиент
  participant A as API
  participant E as Движок
  participant VA as Акторы площадок
  participant K as oa-core
  C->>A: POST /v1/quotes/compare (контракт, направление, объём, принятые отличия)
  A->>A: аутентификация, лимит запросов, валидация
  A->>E: сравнение
  E->>E: отбросить площадки, исключённые гео-политикой
  par каждая площадка
    E->>VA: каталог (из кэша)
    VA-->>E: листинги
  end
  E->>E: точное совпадение контракта на каждой площадке или NO_LISTING
  par площадки с листингом, в пределах бюджета задержки
    E->>VA: стакан (из кэша / из потока)
    VA-->>E: стакан или сбой
  end
  E->>K: сопоставимость, проход по стакану, комиссии, ранжирование
  K-->>E: ranked / incomplete / ineligible / unavailable
  E-->>A: ответ
  A-->>C: JSON (десятичные числа строками)
  E--)E: асинхронное сохранение наблюдений
```

`engine.rs::compare`, `core/cost.rs`, `core/comparability.rs`

## 3. Вердикт о сопоставимости

Два контракта сравниваются по каждому условию. Ранжирование между площадками происходит только для
`STRICTLY_COMPARABLE` или для `ECONOMICALLY_SIMILAR_WITH_DIFFERENCES`, если вызывающая сторона
приняла каждый код отличия.

```mermaid
flowchart TD
  S(["Контракт A против контракта B"]) --> F{"То же семейство<br/>датированный опцион или бинарный?"}
  F -- "нет" --> NC[NOT_COMPARABLE]
  F -- "да" --> CORE{"Базовый актив, колл/пут,<br/>страйк, момент экспирации"}
  CORE -- "одно отличается" --> NC
  CORE -- "одно неизвестно" --> II[INSUFFICIENT_INFORMATION]
  CORE -- "все равны" --> SEC{"Стиль исполнения, механизм / актив /<br/>индекс / окно расчёта,<br/>валюта премии"}
  SEC -- "все известны и равны" --> SC[STRICTLY_COMPARABLE]
  SEC -- "что-то отличается или неизвестно" --> ES["ECONOMICALLY_SIMILAR_WITH_DIFFERENCES<br/>+ коды причин"]
  ES --> ACC{"Вызывающая сторона<br/>приняла каждый код?"}
  ACC -- "да" --> RANK["Ранжируются вместе"]
  ACC -- "нет" --> INEL["Отображаются, но не ранжируются:<br/>DifferencesNotAccepted"]
  SC --> RANK
```

Множители размера контракта нормализуются и никогда не являются причиной. `core/comparability.rs`

## 4. Стоимость ордера на одной площадке

```mermaid
flowchart TD
  B(["Снимок стакана + запрошенный объём"]) --> FR{"Достаточно свежий?<br/>max_age, расхождение часов"}
  FR -- "нет" --> X1["Ineligible: Stale / ClockSkew"]
  FR -- "да" --> W["Проход по аскам уровень за уровнем<br/>пока объём не покрыт"]
  W --> COV{"Объём покрыт?"}
  COV -- "нет" --> PART["PARTIAL_COVERAGE:<br/>средняя цена видимой части"]
  COV -- "да" --> AVG["Средняя цена, полная премия"]
  PART --> FEE
  AVG --> FEE{"Формула комиссии известна<br/>для этой площадки и объёма?"}
  FEE -- "нет" --> X2["Ineligible: UnknownCosts<br/>никогда не оценивается как ноль"]
  FEE -- "да" --> TOT["Итого = премия + комиссии<br/>на единицу базового актива"]
  TOT --> R(["Ранжирование по полной стоимости"])
```

`core/book.rs`, `core/fees.rs`, `core/cost.rs`

## 5. Маршрутизация: одна площадка или разбиение

```mermaid
flowchart TD
  Q(["Намерение купить + политика разбиения"]) --> RV["Маршрутизируемые площадки:<br/>единственная причина = PARTIAL_COVERAGE"]
  RV --> SUB["Каждое подмножество размера ≤ max_legs<br/>1, если разбиение выключено"]
  SUB --> MERGE["Слияние уровней asks, сначала самые дешёвые<br/>на единицу базового актива, в рамках бюджетов"]
  MERGE --> RECOST["Точный пересчёт стоимости каждой ноги на её площадке<br/>фиксированные комиссии платятся один раз на ногу"]
  RECOST --> FULL{"Какой-либо план покрывает<br/>весь объём?"}
  FULL -- "нет" --> P["PARTIAL: лучший частичный план,<br/>не исполним"]
  FULL -- "да" --> SINGLE{"Одна площадка<br/>покрывает его?"}
  SINGLE -- "нет" --> SCOV[SPLIT_COVERAGE]
  SINGLE -- "да" --> SAVE{"Разбиение экономит ≥ min_saving_abs<br/>и ≥ min_saving_bps?"}
  SAVE -- "да" --> SCOST[SPLIT_COST]
  SAVE -- "нет" --> ONE["SINGLE: самая дешёвая площадка"]
  SCOV --> LIM
  SCOST --> LIM
  ONE --> LIM["Лимит IOC на ногу = худший уровень<br/>× 1 + допуск, с округлением вверх до тика"]
  LIM --> RQ(["route_id + route_hash, одноразовые, 30 с"])
```

`core/routing.rs`, `engine.rs::route_quote` — подробности в [routing.md](routing.md)

## 6. Проверки рисков

Выполняется каждая проверка и возвращается каждое нарушение (без досрочного выхода), прежде чем
что-либо будет отправлено.

```mermaid
flowchart LR
  I(["Намерение ордера"]) --> C1["Kill switch"]
  C1 --> C2["Режим разрешает отправку"]
  C2 --> C3["Площадка, сеть, базовый актив,<br/>семейство разрешены"]
  C3 --> C4["Продажа разрешена?<br/>выключена в mainnet"]
  C4 --> C5["Количество > 0, лимит > 0"]
  C5 --> C6["Худшая стоимость ≤ лимит на ордер"]
  C6 --> C7["Зарезервированные траты + этот ордер<br/>≤ лимит окна"]
  C7 --> C8["Лимит в пределах максимального отклонения<br/>от референсной цены"]
  C8 --> C9["Максимальный убыток стратегии ограничен"]
  C9 --> OK{"Есть нарушения?"}
  OK -- "да" --> REJ(["Отклонено со списком нарушений"])
  OK -- "нет" --> GO(["Записано в журнал, затем отправлено"])
```

`core/risk.rs`, `engine.rs::risk_limits`; лимит окна повторно проверяется при отправке под
блокировкой (`execution.rs::reserve_session_spend`).

## 7. Режимы торговли и защитные барьеры

```mermaid
flowchart TD
  BUILD{"Features сборки"} -->|"по умолчанию"| D["READ_ONLY · SIMULATION"]
  BUILD -->|testnet| T[TESTNET]
  BUILD -->|mainnet-nc| M[MAINNET_NONCUSTODIAL]
  BUILD -->|live| L["ошибка компиляции:<br/>кастодиального live не существует"]
  BUILD -->|"testnet + mainnet-nc"| L2["ошибка компиляции"]
  T --> TG{"Запуск: имя базы данных<br/>содержит 'testnet'?"}
  M --> MG{"Запуск: имя базы данных<br/>содержит 'mainnet_nc'?"}
  TG -- "нет" --> STOP["отказ запуска"]
  MG -- "нет" --> STOP
  TG -- "да" --> TK["Только исполнители testnet<br/>тестовые ключи из .env.testnet"]
  MG -- "да" --> MK["Только исполнители с подписью пользователя<br/>без загрузчика ключей"]
  TK --> KS{"OA_KILL_SWITCH=false?"}
  MK --> KS
  KS -- "нет" --> RO["Каждый ордер отклоняется"]
  KS -- "да" --> EX["Ордера разрешены в пределах лимитов"]
```

`crates/app/src/lib.rs`, `config.rs`, `bin/server.rs`

## 8. Некастодиальное исполнение

```mermaid
sequenceDiagram
  autonumber
  participant U as Кошелёк пользователя
  participant C as Клиент
  participant S as oa-server
  participant DB as Postgres
  participant V as Площадка / блокчейн
  C->>S: POST /v1/routes/quote
  S-->>C: route_id, route_hash, ноги + лимиты IOC
  C->>S: POST /v1/noncustodial/prepare (маршрут, аккаунт пользователя)
  S->>S: использование маршрута (одноразово), проверки рисков
  S->>DB: строки VALIDATED + подготовленное исполнение (истекает через 4,5 мин)
  S-->>C: типизированные данные EIP-712 для каждой ноги или транзакция для отправки
  C->>U: eth_signTypedData_v4 / eth_sendTransaction
  U-->>C: подписи / хеши транзакций
  C->>S: POST /v1/noncustodial/submit
  S->>S: каждый восстановленный подписант = заявленный аккаунт (до использования)
  S->>DB: забрать подготовленное исполнение (атомарно, один раз во всех инстансах)
  S->>DB: повторная проверка лимита окна, строки RESERVED / SUBMITTING
  S->>V: пересылка подписанных ордеров или чтение квитанции из блока
  V-->>S: исполнения
  S->>DB: финальные состояния
  S-->>C: отчёт об исполнении
```

`execution.rs::prepare_user_signed` / `submit_user_signed`, `adapters/src/*_user.rs`

## 9. Что веб-интерфейс соглашается подписать

Даже если сервер скомпрометирован, страница отклоняет всё, что выходит за эти правила.

```mermaid
flowchart TD
  P(["Подготовленное исполнение от сервера"]) --> D{"Нога Derive?"}
  D -- "да" --> DC{"chainId 1 или 11155111,<br/>verifyingContract = Derive Matching,<br/>message.owner = подключённый кошелёк?"}
  DC -- "нет" --> STOP(["Отклонено"])
  DC -- "да" --> SIGN[eth_signTypedData_v4]
  D -- "нет" --> TX{"Нога-транзакция<br/>Thetanuts / CallPut"}
  TX --> TC{"Сеть Base, известный контракт,<br/>сумма ETH ≤ разрешённой,<br/>approve: USDC, известный spender,<br/>сумма ≤ 2 × подписанной худшей стоимости?"}
  TC -- "нет" --> STOP
  TC -- "да" --> AL{"Текущий allowance<br/>меньше нужного?"}
  AL -- "да" --> APP["Отправить approve на точную сумму, дождаться квитанции"]
  AL -- "нет" --> SEND
  APP --> SEND["Отправить транзакцию, дождаться квитанции"]
  SIGN --> AUTH["Вход в Derive: personal_sign метки времени"]
  SEND --> SUB(["Отправить на сервер"])
  AUTH --> SUB
```

`web/lib/noncustodial.ts`, `web/lib/wallet.ts`

## 10. Жизненный цикл ордера

```mermaid
stateDiagram-v2
  [*] --> VALIDATED: проверки рисков пройдены, записано в журнал
  VALIDATED --> CANCELLED: так и не подписан (истёк) / лимит достигнут при отправке
  VALIDATED --> SUBMITTING: лимит зарезервирован, запись в журнал до отправки
  SUBMITTING --> ACKNOWLEDGED
  SUBMITTING --> FILLED
  SUBMITTING --> PARTIALLY_FILLED
  SUBMITTING --> REJECTED: однозначный 4xx
  SUBMITTING --> UNKNOWN: тайм-аут / транспортная ошибка
  UNKNOWN --> FILLED: сверено с площадкой
  UNKNOWN --> PARTIALLY_FILLED
  UNKNOWN --> REJECTED
  UNKNOWN --> CANCELLED
  ACKNOWLEDGED --> FILLED
  ACKNOWLEDGED --> CANCELLED: остаток IOC
  PARTIALLY_FILLED --> FILLED
  PARTIALLY_FILLED --> CANCELLED: остаток IOC
  FILLED --> [*]
  REJECTED --> [*]
  CANCELLED --> [*]
```

Ордер в состоянии `UNKNOWN` ищется по заранее вычисленному идентификатору на площадке (три быстрые
попытки, затем каждые 10 с, в том числе после перезапуска). Он **никогда не отправляется повторно**.
`execution.rs` (`submit_row`, `OrderTracker`)

## 11. Актор площадки: кэш и circuit breaker

```mermaid
stateDiagram-v2
  [*] --> Closed
  Closed --> Closed: запрос успешен (кэш обновлён)
  Closed --> Closed: сбой (< 5 подряд, кэш помечен Failed)
  Closed --> Open: 5 сбоев подряд → 30 с
  Closed --> Open: HTTP 429 → Retry-After (≤ 60 с)
  Open --> Closed: задержка истекла, следующий запрос разрешён
  note right of Closed
    один выполняющийся запрос на ключ,
    параллельные вызывающие разделяют его,
    стаканы ограничены LRU (256)
  end note
```

`crates/app/src/venue.rs`

## 12. Развёртывание

```mermaid
flowchart LR
  NET(("Интернет")) -->|"443 / 80"| CADDY["Caddy<br/>HTTPS · заголовки"]
  CADDY -->|"/"| WEB["web :3000"]
  CADDY -->|"/api/* allowlist + токен"| OA["oa-server :8080"]
  CADDY -->|"/v1/* (ключ интегратора)"| OA
  CADDY -.->|"/v1/admin → 404"| X["заблокировано"]
  OA --> PG[("Postgres<br/>не опубликован")]
  PROM["Prometheus 127.0.0.1"] -.-> PG
  ADMIN["Оператор"] -->|"SSH-туннель"| OA
```

`deploy/docker-compose.yml`, `deploy/Caddyfile*` — подробности в [deployment.md](deployment.md)
