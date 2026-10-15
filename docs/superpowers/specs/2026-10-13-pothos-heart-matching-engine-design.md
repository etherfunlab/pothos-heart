# pothos-heart — 配对引擎立项与首版设计

**日期**：2026-10-13
**状态**：设计已批准，待实现计划
**范围**：立项决议 + 首版（硬过滤 + 软打分 + 候选端点）；engine 侧一支配套 PR

---

## 0. 这是什么

**pothos-heart 是开源的配对引擎**：吃伴侣对话推导出的用户画像，输出**可解释的**候选人排序。

关系对等：eros-engine 之于它服务的陪伴产品，正如 pothos-heart 之于 Pothos。Pothos 是一个陪伴产品的社交层
（"Meet people through what your companion has learned about you"），pothos-heart 是它背后的算法层。

**成功的样子**：

1. 社区拿到源码能自托管跑通端到端匹配，不需要我们的任何闭源件；
2. 一个下游部署的 Pothos 产品层用它当后端；
3. 对外立住"开源配对引擎"这个技术位置。

---

## 1. 立项决议

| 项 | 决议 |
|---|---|
| 仓 | GitHub public `etherfunlab/pothos-heart` |
| License | **MIT** |
| 栈 | Rust / Axum / sqlx + PostgreSQL |
| 形态 | **单 crate 单二进制**，不开 workspace |
| 与 engine 的耦合 | 独立服务，只走 engine HTTP API，**零代码依赖** |
| 开源边界 | 配对引擎**整体开源**（画像合并 + 三层匹配）；Pothos 产品层（UI / 计费 / 运营）闭源 |

### 1.1 MIT 带来一条红线

**pothos-heart 不得依赖任何 `eros_engine_*` crate。** eros-engine 是 AGPL-3.0-only，链接上去等于整体
按 AGPL 分发，MIT 声明随之名不副实。"独立服务 + 走 HTTP"因此不只是架构偏好，是 license 约束。

两个服务之间只有 HTTP，不存在许可冲突。

### 1.2 单 crate 不开 workspace

engine 是 4-crate 因为它真的大。pothos 首版是一个服务，三行能说清的不写三十行；真大了再拆。

### 1.3 这份立项作废两处既有结论

两处结论此前都已发到读者手上，所以在这里指出：

- **「配对算法闭源」的旧定位作废。** 三层匹配管线、画像合并、A2A 化学评估都进开源侧，护城河叙事从
  "算法闭源"移到"产品层 + 用户数据"。
- **「dreaming-full 不进开源侧、属商业逻辑」的旧约束作废**，结论见 §3。
- **engine 侧的数据出口早已落地，但表名不是旧提案写的那个。** `engine.companion_insights` 已在迁移 `0042`
  整表 drop，`engine.human_insights` 成为唯一权威（per-column 增量写）；`companion_insights_snapshot`
  （`0021`）已冻结，由 **`engine.human_insights_snapshot`** 接手。

### 1.4 对外纪律

Pothos 这个产品名本身已公开，仓名与 README 可以用。issue / PR / 代码注释一律用 "a product layer" /
"a downstream deployment" 的口径：不点名下游产品与它的仓库，不贴线上 URL，不引私有仓路径。
这条写在仓内 CLAUDE.md。

### 1.5 线上影响

pothos-heart 是全新服务，不碰任何线上流量。engine 侧唯一的改动是一条新出口端点：新增路由，
secret 不配则整条不挂，对现有用户没有影响。

---

## 2. 首版范围

pothos-heart 的完整形态是**两个独立子系统**，各自够一份 spec：

| 子系统 | 本轮 |
|---|---|
| 匹配管线（硬过滤 / 软打分 / A2A 化学评估） | **硬过滤 + 软打分 + 候选端点** |
| 画像合并（dreaming-full：跨 session 增量 + 去重 + 语义合并） | 下一轮 |
| LLM A2A 化学评估 | 下一轮 |

首版直接吃 engine 现成的 `human_insights`。那张表的四个 matching 列（`preferred_gender` / `age_min` /
`age_max` / `deal_breakers`）加两个 GIN 索引至今**没有任何消费者**（engine 的 teardown 记录里写明）—— pothos-heart 就是那个缺席的消费者。

先做匹配、后做画像合并的理由：画像合并是**质量优化**，而匹配是**能不能出东西**。先优化一个还没人消费的
画像，等于在没有读数的情况下调参。

**首版不调 LLM。**

---

## 3. 架构

数据流：engine 是画像权威 → pothos 持本地**只读镜像** → 硬过滤与软打分全在 pothos 本地库算。

选它的理由直接从立项前提推出来：配对引擎整体开源，三层就得整体在这个 repo 里；而硬过滤要在**全体候选**
上跑查询，pothos 就必须有一份本地可查询的画像集合。

被否的两条：让 engine 做硬过滤（把三层切成两半分在两个 repo，每加一个过滤维度要给 engine 发 PR）；
请求时批量拉画像（需要调用方先给出候选名册，而那份名册本身就是硬过滤的输出）。

### 3.1 engine 侧：一条中立的增量出口

一支 PR + 一支迁移。

```
GET /svc/v1/insights/changes?since=<rfc3339>&after_user=<uuid>&limit=<n>
```

- **授权**：`x-insights-export-secret` header；env `INSIGHTS_EXPORT_SECRET` 不配则整条路由不挂。
  照 `x-byok-caller-secret` 的先例。
- **命名刻意中立。** engine 不认识 pothos，它只提供"画像增量导出"；谁消费不是它的事。这保住 engine
  的 OSS 中立定位，也让这条出口将来能喂别的下游。
- **返回全部 21 个 `human_insights` 数据列** + `updated_at`。出口中立意味着它不猜消费者要哪几列。
- `limit` 默认 500、上限 1000；响应带 `next` 游标（`updated_at` + `user_id`），为空即到底。
- **游标是 keyset `(updated_at, user_id)`，不是裸 `updated_at`。** 同一毫秒多行会在翻页时丢行。
- **读 `human_insights`，不读 `human_insights_snapshot`。** snapshot 是 append-only 全量归档
  （sweeper 每轮把所有人 `to_jsonb` 一遍），拿它做增量会收到海量重复行；`human_insights.updated_at`
  才是"这个人的画像变了"的权威信号。
- **迁移**：`CREATE INDEX ... ON engine.human_insights (updated_at, user_id)`。该表现有索引只有
  interests / personality_traits 两个 GIN，没有 `updated_at` 索引。纯加索引，零数据改动。

### 3.2 pothos 侧 schema

```sql
-- 软信号镜像：pull from engine，只读派生，pothos 永不写回
CREATE TABLE pothos.insight_mirror (
    user_id            UUID PRIMARY KEY,          -- 归属列，不加 FK
    interests          TEXT[] NOT NULL DEFAULT '{}',
    personality_traits TEXT[] NOT NULL DEFAULT '{}',
    deal_breakers      TEXT[] NOT NULL DEFAULT '{}',
    age_min            INT,
    age_max            INT,
    engine_updated_at  TIMESTAMPTZ NOT NULL,      -- 翻页游标 + 新鲜度
    synced_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_insight_mirror_interests ON pothos.insight_mirror USING GIN (interests);
CREATE INDEX idx_insight_mirror_traits    ON pothos.insight_mirror USING GIN (personality_traits);

-- 硬属性：push from 产品层，只读镜像
CREATE TABLE pothos.match_profiles (
    user_id         UUID PRIMARY KEY,             -- 归属列，不加 FK
    gender          TEXT NOT NULL,
    seeking_genders TEXT[] NOT NULL,
    birth_date      DATE,
    country         TEXT,
    region          TEXT,
    city            TEXT,
    active          BOOLEAN NOT NULL DEFAULT true,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 同步游标：单行
CREATE TABLE pothos.sync_cursor (
    id                SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_updated_at   TIMESTAMPTZ,
    last_user_id      UUID,
    synced_at         TIMESTAMPTZ
);
```

**只镜像首版要用的 5 列。** engine 出口返回 21 列，pothos 存 5 列 —— 出口中立、消费者按需取。
其余 16 列首版既不过滤也不打分，不镜像；dreaming-full 那轮再按它自己的需要加。
`preferred_gender` 也在不镜像之列：性别过滤走的是产品层推进来的 `seeking_genders`（§4.1），
对话里猜出来的偏好不参与。

**engine 画像里的 `city` / `location` / `nationality` 不镜像。** 它们是对话抽出来的自由文本
（"Singapore" / "新加坡" / "SG" 并存），首版既不做硬过滤也不参与打分 —— 地域全部走
`match_profiles` 的结构化三级。

**`seeking_genders TEXT[]` 而不是照搬 `sexual_orientation TEXT`。** 匹配真正要算的是"我找什么性别"。
取向字符串 → 找谁的映射是文化与产品决定，不该在一个 OSS 引擎里硬编码一张对照表；产品层展开后推进来，
pothos 的硬过滤退化成纯集合运算。

**存 `birth_date`，不存 `age`。** 年龄每天都在变，是派生量；一个事实一个权威载体，宁可每次查询算一次。

**零外键是结论不是遗漏。** 两张镜像表各只有 `user_id` 一个指向外部身份的列，正是归属列（删行毁证、
置空让行永远查不到）；而且 pothos 可能连的是跟 engine 完全不同的库，没有可引用的父表。
`sync_cursor` 没有引用列。

**`match_profiles` 有行且 `active = true` 就等于"在匹配池里"。** 准入、暂停、退出全归产品层，
引擎不需要一套自己的开关。

### 3.3 匹配是纯函数

给一个 `user_id`，实时在本地库跑一条 SQL 算出排序候选 + 分数 + 理由。**匹配结果不落表。**
"已看过 / 已划掉 / 已配上"是产品层状态，引擎不碰 —— 少一张表、少一套新鲜度问题。

### 3.4 同库 / 异库都成立

pothos 不引用任何 `engine.*` 表，连接串独立配置：可以跟 engine 同库挂 `pothos.*` schema，
也可以给它一个完全独立的 Postgres。

---

## 4. 匹配算法

### 4.1 硬过滤

对 self = A，候选 B 要同时满足：

1. `B.user_id <> A.user_id`
2. `B.active = true`
3. **双向性别**：`B.gender = ANY(A.seeking_genders)` 且 `A.gender = ANY(B.seeking_genders)`
4. **双向年龄**：A 的 `[age_min, age_max]` 覆盖 B 由 `birth_date` 算出的年龄，且反向成立
5. **双向 deal_breakers 不相交**：`NOT (A.deal_breakers && B.interests)`、
   `NOT (A.deal_breakers && B.personality_traits)`，及两条的反向

两条刻意的"放"：

- **缺数据一律放过，不筛掉。** 没填 `birth_date`、没抽出 `age_min` / `age_max` 的人照样进候选
  （规则 4 在任一侧缺数据时不生效）。没有确定的兜底之前一律放；提前收紧只会把人静默筛没，
  而你看不到自己筛掉了谁。
- **地域不进硬过滤。** 只在软打分里加分。等有读数再决定要不要收。

### 4.2 软打分：overlap coefficient

每个维度算 `|A∩B| / min(|A|,|B|)`，**不用** Jaccard 的 `|A∩B| / |A∪B|`。

原因是个系统性偏差：画像数组的长度反映的是**这个人聊了多少**，不是匹配度 —— Jaccard 会稳定惩罚
只被抽出两三个兴趣的新用户。取 `min` 对长度不对称免疫。

三个维度加权求和：

| 维度 | 来源 |
|---|---|
| interests 重叠 | `insight_mirror.interests` |
| personality_traits 重叠 | `insight_mirror.personality_traits` |
| 地域逐级命中 | `match_profiles` 的 `city` > `region` > `country`，取最细一级 |

权重进配置文件随镜像部署。**首版给一组初始值就上，不追求调到最优** —— 逼近"完美权重"就是在拟合
当前这批用户的噪声。

### 4.3 理由

每条候选返回重叠的具体项：`shared_interests: ["登山","爵士"]`、`shared_traits: [...]`、
`geo_match: "city" | "region" | "country" | null`，以及每个维度的分数。

这既是产品要展示的"为什么是他"，也是回看读数再调权重的依据 —— 放开的那一步必须留下读数。

---

## 5. 端点契约

**API-first**：先在 crate 里用 `#[utoipa::path]` + DTO `#[derive(ToSchema)]` 声明契约，再写 handler。

```
GET    /v1/match/candidates/{user_id}?limit=20   候选排序 + 分数 + 理由（limit 默认 20、上限 100）
PUT    /v1/profiles/{user_id}                     产品层 push 硬属性（upsert）
DELETE /v1/profiles/{user_id}                     退出匹配池
GET    /healthz                                   存活
```

**pothos-heart 只接受服务间调用**，授权是 `x-pothos-caller-secret`，**不认终端用户的 JWT** ——
它不该知道用户的身份系统长什么样。顺带的好处：候选人的画像永远不会直接落到浏览器里。

后台 sweeper 定期拉 engine 增量，间隔可配、可整个关掉。

---

## 6. 错误处理

| 情况 | 行为 |
|---|---|
| engine 不可达 / 出口 secret 错 | sweeper 记日志、**游标不动**、下轮重试；匹配端点继续用现有镜像服务。镜像陈旧好过整体不可用 |
| `user_id` 不在 `match_profiles` | 404 `not_in_pool` |
| 在池里但 `insight_mirror` 无行（还没聊过天） | **200 + 空候选 + `no_insights_yet`**，不是 404 —— 每个新用户必然经过这个状态，它是正常的 |
| 缺 / 错 caller secret | 403 |

---

## 7. 测试

sqlx Postgres 集成测试，照 engine 的模式但**不复用它的 crate**（§1.1 红线）。TDD。

要钉住的：

- 双向性别过滤；任一侧缺 `birth_date` / 年龄偏好时规则 4 放过
- deal_breakers 双向、对 interests 与 personality_traits 两个数组都生效
- overlap coefficient 的长度不对称性质（短列表不被惩罚）
- **keyset 翻页在同一 timestamp 多行时不丢行**
- engine 不可达时游标不动，镜像保持可查
- 端点：无 secret 403、不在池 404、空候选 200

---

## 8. 落地顺序

1. **engine PR**：`/svc/v1/insights/changes` + keyset 索引迁移。合并、发版、部署。
   开 PR 前先确认 engine 没有未合的 PR。
2. **pothos-heart**：建仓、schema、硬过滤 + 软打分 + 端点 + sweeper。可用 fixture 与第 1 步并行开发。
3. 对接真实 engine 出口。

---

## 9. 范围外（各自下一轮）

- **dreaming-full**：跨 session 增量 + 去重 + 语义合并
- **LLM A2A 化学评估**（三层匹配的第三层）
- 匹配结果持久化、"已看过 / 已划掉"状态（归产品层）
- 年龄过滤在下游部署上真正生效 —— 产品层的自填档案目前没有生日字段，不推 `birth_date`，年龄规则就不会触发
- 地域从"只打分"收成"可硬过滤"（等读数）
- 权重调优（等读数）
