# pothos-heart v0.4 — autobio v1：取料换底到事实账本

**日期**：2026-10-30
**状态**：设计定稿，待实现
**范围**：autobio 的素材从 insights 导出的派生摘要换成 engine 的事实账本导出；消费 engine 的删号墓碑流；age 窗口从 insights 镜像搬到产品层自填。条目结构、更新规则、本人端点、判官都不动。

---

## 0. 目标

v0.3 的 autobio 吃两种素材：insights 导出的 15 个文本列（每一版记进 phrase ledger）加分类记忆。engine Insights 2.0 之后，整个 insights 导出（文本列与三个数组）都成了**事实账本（`human_facts`）的定期派生摘要**——再把它们当素材，就是让 autobio 吃二手货，而且同一个事实在账本和摘要里各喂一份。v1 把素材换成一手的：

- **事实账本镜像**：engine `GET /svc/v1/facts/changes` 的原子事实，带六个标签与复现计数，成为 autobio 的主素材。
- **分类记忆照旧**：`GET /svc/v1/memories/changes` 不变；engine 的 dreaming-plus（v2.6.0）让这条流的行数变多、节奏变密，`AUTOBIO_MIN_INTERVAL_SECS` 自然攒批，不需要新机制。
- **文本列退出**：insights 导出的文本列不再入账本、不再当素材；三个数组列照旧只走概念归并与打分。
- **墓碑流**：engine 的导出契约明文要求镜像方消费删号流（`GET /svc/v1/insights/deletions`）。v0.3 记为已知缺口，v1 补上。
- **age 窗口搬家**：engine 已把 `age_min` / `age_max` / `preferred_gender` 从 insights 里删掉（LLM 从对话判不准、配对也不该用它猜的），配对侧改由用户自填。`PUT /v1/profiles` 加这两个字段，hard filter 改读 `match_profiles`。

### 0.1 成功标准

1. 素材换底后，v0.3 的结构保证原样成立：每条可追溯到素材（指针换成 `f:` / `m:`）、本人删除立即生效且不写回、revise 链保留历史。
2. 墓碑流端到端：engine 对某用户 erase 之后，一轮 sweep 内这个人的四张镜像、autobio、判定、队列全部清净（测试钉住）。
3. 不配 `ENGINE_FACTS_SECRET`、或 engine 没有 facts 出口（404）时，autobio 只用分类记忆素材退化运行，其余步骤不受影响。
4. 条目质量照旧由维护者读样本判断，不写断言。

### 0.2 反目标

- 把 insights 导出的派生摘要（文本列或数组）再混进 autobio 素材——同一个事实两个来源，迟早互相矛盾。
- 为了「对齐」去删 v0.3 已落库的旧素材行或旧条目——旧行是已有条目的来源与历史，保留。

### 0.3 约束

- 只经 engine 的 HTTP 出口单向读，不跨 schema，不回写 engine；不依赖任何 `eros_engine_*` crate（MIT 红线）。
- 公开仓纪律：对外只说 "a product layer" / "a production deployment"。

---

## 1. 输入：engine 契约现状（v2.4.0 起已发布）

### 1.1 facts 出口

```
GET /svc/v1/facts/changes?since=<rfc3339>&after_id=<uuid>&limit=<n>
```

- Header `x-facts-export-secret`，engine 侧由 `FACTS_EXPORT_SECRET` 挂载，与 insights secret 是两把钥匙。
- 行字段：`id`、`user_id`、`content`、`category`、`domain`、`evidence_type`、`temporality`、`persistence`、`confidence`、`occurrences`、`session_count`、`recurrence`（`once` / `repeated_in_session` / `repeated_across_sessions`，engine 派生）、`first_seen_at`、`last_seen_at`。会话、消息、run 指针留在 engine 内，不导出。
- keyset `(last_seen_at, id)`。**复见会让同一行再次出现在流里**（`last_seen_at` 前移、计数变化），镜像按 `id` upsert。
- 行只在用户被 erase 时消失，没有逐条删除；删除看墓碑流（§1.3）。

标签词表归 engine 所有（engine 侧未知值已折叠成 `other` / `unknown`），pothos 原样存、只当提示用，不做枚举校验。`evidence_type = legacy_profile` 是 engine 上线账本时从旧画像播的种子，照常当素材。

### 1.2 insights 出口的新形状

engine v2.4.1 起，导出行是 13 个文本列（`occupation`、`mbti_guess`、`love_values`、`emotional_needs`、`life_rhythm`、`education`、`education_level`、`education_status`、`family`、`relationship_history`、`social_pattern`、`future_plans`、`finance_status`）加三个数组（`interests`、`personality_traits`、`deal_breakers`）加 `updated_at`：

- 地理四列（`city` / `location` / `hometown` / `nationality`）、`age_min` / `age_max`、`preferred_gender` 都没了。serde 对缺列容忍，v0.3 现在是静默退化（age 永远为 null），不是报错。
- `kinks` / `ideology` / `mental_health` 三列**永不导出**；但这些 domain 的原子事实照常走 facts 流——autobio 的 `intimacy` 等节由 facts 供料，不受影响。

v1 之后 pothos 只用这条出口的三个数组（概念归并、软打分、deal-breakers 硬过滤）。文本列到了也不读。

### 1.3 墓碑流

```
GET /svc/v1/insights/deletions?since=<rfc3339>&after_user=<uuid>&limit=<n>
```

- 用现有的 `x-insights-export-secret`。`/svc/v1/facts/deletions`（facts secret）是同一股流的另一个路由；pothos 的 sweeper 必有 insights secret，所以只拉这一条，一个游标。
- 行是用户级墓碑 `{user_id, erased_at}`，keyset `(erased_at, user_id)`。重复 erase 会把 `erased_at` 推后，同一用户可能再次出现，清除要幂等。
- engine 的契约：erase 之后 changes 流不会再报该用户的行，所以墓碑步骤在一轮里的位置不影响正确性；放在最前只是让同轮后续步骤不处理注定要删的数据。

### 1.4 最低 engine 版本

facts 出口与墓碑流都随 v2.4.0 发布。README 的 pin 从 v2.2.0 升到 **v2.4.0**（facts 出口在 engine 侧默认不挂，部署方要设 `FACTS_EXPORT_SECRET`）。

---

## 2. 存储：迁移 `0004`

```sql
-- 事实账本镜像。id 是 engine 的 fact id；recurrence 存 engine 导出的派生值。
CREATE TABLE pothos.fact_mirror (
    id              UUID PRIMARY KEY,
    user_id         UUID NOT NULL,                          -- 归属列，不加 FK
    content         TEXT NOT NULL,
    category        TEXT NOT NULL,
    domain          TEXT NOT NULL,
    evidence_type   TEXT NOT NULL,
    temporality     TEXT NOT NULL,
    persistence     TEXT NOT NULL,
    confidence      TEXT NOT NULL,
    occurrences     INT NOT NULL,
    session_count   INT NOT NULL,
    recurrence      TEXT NOT NULL,
    first_seen_at   TIMESTAMPTZ NOT NULL,
    last_seen_at    TIMESTAMPTZ NOT NULL,
    recorded_at     TIMESTAMPTZ NOT NULL DEFAULT now()      -- pothos 时钟，素材水位
);
CREATE INDEX idx_fact_mirror_user ON pothos.fact_mirror (user_id, recorded_at);

CREATE TABLE pothos.fact_cursor (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_seen_at    TIMESTAMPTZ,
    last_id         UUID,
    synced_at       TIMESTAMPTZ
);
INSERT INTO pothos.fact_cursor DEFAULT VALUES;

CREATE TABLE pothos.erasure_cursor (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_erased_at  TIMESTAMPTZ,
    last_user_id    UUID,
    synced_at       TIMESTAMPTZ
);
INSERT INTO pothos.erasure_cursor DEFAULT VALUES;

-- 条目 ↔ 事实素材。两根外键都从属、都 CASCADE，照 0003 的两张 sources 表。
CREATE TABLE pothos.autobio_fact_sources (
    entry_id  BIGINT NOT NULL REFERENCES pothos.autobio_entries (id) ON DELETE CASCADE,
    fact_id   UUID   NOT NULL REFERENCES pothos.fact_mirror (id) ON DELETE CASCADE,
    PRIMARY KEY (entry_id, fact_id)
);
CREATE INDEX idx_autobio_fact_sources_fact ON pothos.autobio_fact_sources (fact_id);

-- age 窗口搬家：LLM 猜的删掉，用户自填的进 match_profiles。
ALTER TABLE pothos.insight_mirror DROP COLUMN age_min, DROP COLUMN age_max;
ALTER TABLE pothos.match_profiles ADD COLUMN age_min INT, ADD COLUMN age_max INT;
```

新表照 `0001`–`0003` 的写法做 Supabase lockdown（REVOKE + RLS）。

### 2.1 设计要点

- **`recorded_at` 只在 engine 行实际变化时推进。** upsert 时 `EXCLUDED.last_seen_at > fact_mirror.last_seen_at` 才更新整行并把 `recorded_at` 置为 `now()`；overlap 重拉到的同状态行是 no-op。**复见即新素材**——recurrence 档与计数的变化是信息（「又提了一次」会影响条目的状态与把握），和 phrase ledger「同一版只推 `last_seen_at`、不算新素材」的规则不同，是有意的。
- **`observed_phrases` 不迁移。** dimension CHECK 不动（旧值继续合法），文本列只是停写；旧行保留——它们是已有条目的来源与历史。`autobio_observed_sources` 不再有新行，revise 并入旧来源时照旧连 `o:` 来源一起带过去。
- **phrase ledger 的 2000 字节上限保留**，它防的是 btree 索引项上限，对数组短语同样成立。
- **`fact_mirror.content` 不设长度上限**：主键是 UUID，没有索引压力；账本行是原子事实，engine 侧已约束形状。

---

## 3. sweeper：步骤与顺序

```
① 墓碑        /svc/v1/insights/deletions → 本地清除
② insights    数组短语进账本（文本列停写）
③ memories    照旧
④ facts       → fact_mirror
⑤ 归概念      照旧，只看三个数组维度
⑥ autobio     素材换底，见 §4
⑦ 判官        照旧
```

各步互不影响对方的游标；每页行与游标一个事务，照 v0.1 以来的先例（含 overlap 回退、`next` 不前进即止）。

### 3.1 墓碑步骤

- 每页一个事务：对页内每个 `user_id` 删掉——`insight_mirror`、`observed_phrases`、`memory_mirror`、`fact_mirror` 的镜像行；`autobio_entries`（**含 `vetoed`**——账号没了，「不得写回」失去约束对象）、`autobio_state`；`pair_verdicts` 与 `judge_queue` 里提到他的行——然后推游标。幂等：重复墓碑再删一遍，什么都删不到。
- **`match_profiles` 不动。** 它是产品层推送的数据，墓碑只代表 engine 侧的删除；产品层删号时要自己调 `DELETE /v1/profiles`（README 的 Data lifecycle 写明：先 erase engine，再删 pothos profile，顺序无所谓，两个都要调）。
- 404（engine 老于 v2.4.0）：记 debug 日志跳过，照 memories 先例。其他失败：游标不动，下一轮重试。

### 3.2 facts 步骤

- `ENGINE_FACTS_SECRET` 设了才跑。keyset `(last_seen_at, id)` 翻页 + overlap 几秒，照 memories 步骤的形状；upsert 规则见 §2.1。
- 404：记 debug 日志跳过（engine 没挂 facts 出口），autobio 只用记忆素材。其他失败：游标停在最后落库的页。

---

## 4. autobio 更新的变化

只换素材，不动调度骨架（选人闸门、最小间隔、每轮上限、每人一次调用一个事务、池内复查与行锁守卫，全部照 v0.3 §3）。

### 4.1 选人与水位

`due_users` 与 `material` 的素材 union 从 `observed_phrases + memory_mirror` 换成 **`fact_mirror + memory_mirror`**，水位语义不变（`recorded_at > material_until`，NULL 水位 = 全部素材）。

**首跑素材不设预算（已拍板）。** 重度用户的全量账本首跑约一两万 token，离主流模型的输入上限还远；每次运行记的 token 用量就是读数，素材真装不下再加预算与分轮水位，不提前做。被拒后按最小间隔重试同一份 prompt 的行为照 v0.3 错误表，不另设兜底。

### 4.2 prompt 素材格式

- 事实素材引用号 `f:<uuid>`，每条带：内容、`category` / `domain`、`evidence_type`、`temporality` / `persistence`、`confidence`、`recurrence`（附 `occurrences` 与 `session_count`）、首次与最近提到的时间。从旧到新按 `last_seen_at` 排。
- `first_seen_at` 早于水位的事实是**复见**，prompt 里标出来：这是「之前的素材又被提到」，用来确认或强化既有条目，不是新信息就不开新条目。
- 记忆素材 `m:<uuid>` 照旧。`o:` 引用号从 prompt 里消失，新 op 不再接受 `o:` 来源。
- 标签不预过滤：低置信、`unknown`、`other` 都给模型看，prompt 写明按置信与复现掂量措辞的把握（「可能」「似乎」）。
- roleplay 规则 key 在一等列：`evidence_type = 'roleplay_expression'` 的事实只进 `roleplay` 节（v0.3 靠 metadata 提示，v1 是结构化字段）。记忆素材的 roleplay 判法照旧。

### 4.3 校验

v0.3 §3.5 原样，来源一条改成：每个 `f:` / `m:` 来源存在、属于这个人，且是本次新素材或某条当前条目的来源；`o:` 作为新来源一律不合格（旧条目经 revise 自动并入的除外）。写入事务内复查来源存在——墓碑步骤或并发 erase 删掉素材时，那条 op 跳过，不影响同批其他条。

### 4.4 其余规则

v0.3 §3.4 的全部规则、七个章节、输出契约（add / revise / retire）、退化防护、重建流程（README 的 rebuild 说明）不变。重建时素材自然来自 `fact_mirror + memory_mirror` 全量。

---

## 5. age 窗口：`PUT /v1/profiles`

body 加两个可空字段：

```json
{ "gender": "f", "seeking_genders": ["m"], "birth_date": "1996-04-02",
  "country": "SG", "region": null, "city": "Singapore",
  "age_min": 25, "age_max": 40, "active": true }
```

- 可单边；省略 = null = 无偏好（PUT 整体替换的语义照旧）。
- 校验：各自在 `1..=130` 内，两个都给时 `age_min <= age_max`，违者 400。这是用户自填经产品层推来的，坏数据该打回去；v0.3 对镜像里倒置窗口的静默丢弃是对 LLM 噪音的容忍，不适用于 API 输入。
- hard filter 的双向年龄规则改读 `match_profiles.age_min / age_max`，「缺数据不排除任何人」的语义逐字保留。
- `insight_mirror` 的 age 两列删掉（§2），`ExportRow` 不再解析 age；成年与否是产品层的准入决定，pothos 不管（与 `seeking_genders` 展开同理）。

本人端点、判官、候选响应形状都不变。

---

## 6. 配置

| 项 | 位置 | 默认 | 说明 |
|---|---|---|---|
| `ENGINE_FACTS_SECRET` | env | 未设 | 设了才拉 facts 出口。设了它但没设 `ENGINE_BASE_URL` + `ENGINE_EXPORT_SECRET` 拒启（照「只设一个拒启」的先例）；长度校验与 `ENGINE_EXPORT_SECRET` 同规则 |

墓碑步骤不加开关：sweeper 开着它就跑，这是镜像方对 engine 契约的义务，不是可选功能。其余配置项全部沿用。

---

## 7. 错误处理（增量）

| 情况 | 行为 |
|---|---|
| facts 出口 404 | 记日志跳过；autobio 只用记忆素材 |
| facts 出口其他失败 | 游标停在最后落库的页，下一轮重试 |
| 墓碑流 404 | 记日志跳过 |
| 墓碑流其他失败 | 游标不动，下一轮重试 |
| autobio 来源在写入时已被 erase 删掉 | 那条 op 跳过，同批其他条照写 |

v0.3 的错误表其余原样。

---

## 8. 测试

TDD，`#[sqlx::test]` 真实 Postgres，engine 与 LLM 用 wiremock。要钉住的：

- **迁移 `0004`**：新表与 lockdown；`insight_mirror` 的 age 列删除；`match_profiles` 的 age 列；旧数据（0003 时代的账本行、条目、来源）原样存活。
- **facts 镜像**：keyset 翻页、overlap 幂等；`last_seen_at` 前进才更新行并推 `recorded_at`，同状态重拉不推；404 跳过、其余步骤继续。
- **墓碑**：一个墓碑清掉四张镜像 + autobio（含 vetoed）+ 判定 + 队列，`match_profiles` 保留；重复墓碑幂等；游标与页同事务；404 跳过。
- **autobio 换底**：`due_users` / `material` 只看 `fact_mirror + memory_mirror`；prompt 含 `f:` 素材的标签与复见标注、不含 `o:` 素材；`f:` 来源校验与落库（`autobio_fact_sources`）；`o:` 新来源不合格；revise 并入旧 `o:` 来源照旧；不配 facts secret 时只用记忆素材。
- **age 窗口**：PUT 校验（400 的各种边界）、整体替换语义（省略置 null）、hard filter 双向读 `match_profiles`、缺数据不排除。
- 条目质量不写断言，维护者读样本判断。

---

## 9. 交付

- 版本 `0.4.0`。
- README 改写：输入表（三条 changes 流 + 墓碑流）、Profile merging（文本列退出账本）、Autobio（素材描述换成 facts + memories）、API（`PUT /v1/profiles` 的 age 字段）、Data lifecycle（墓碑流的清除清单与产品层的删号责任）、Configuration（`ENGINE_FACTS_SECRET`）、engine pin **v2.4.0**（facts 出口与墓碑流；facts 出口要 engine 设 `FACTS_EXPORT_SECRET`，缺了退化为只用记忆素材）。
- pothos-heart 仍未部署，不碰线上流量；没有存量数据要回填或重建。
- 开发与测试只打 wiremock；真跑由维护者触发。

---

## 10. 范围外

| 项 | 去向 |
|---|---|
| 用户删记忆的转发（pothos → engine 逐行删） | dreaming-full 另立项 |
| 判官与软打分的任何演进 | 等读数 |
| 关系层素材（片段摘要、关系摘要、Journal） | engine 不导出，配对用不上 |
| 旧文本列账本行的回收 | 不删不迁，是历史 |
| 小传 embedding 进软打分 | 等读数 |
| facts 的逐条删除传播 | engine 没有逐条删除，只有用户级 erase |

---

## 11. 交接（留给实现者）

- fact 素材在 prompt 里的具体措辞与排版；复见标注的写法。
- 墓碑步骤的删除语句组织（一条 CTE 还是逐表）。
- `ENGINE_FACTS_SECRET` 的长度校验是否与 caller secret 同阈值，照现有 `ENGINE_EXPORT_SECRET` 的先例。
- README 英文措辞。
