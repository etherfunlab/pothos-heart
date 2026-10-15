# pothos-heart v0.3 — autobio 与 A2A 判官设计

**日期**：2026-10-15
**状态**：已实现（v0.3.0）
**范围**：入池用户的条目式小传（autobio），本人的读取与删除端点，以及读两篇小传给候选对打档的 A2A 判官。

---

## 0. 目标

v0.2 只用 engine 导出里的三个数组列做匹配。v0.3 把其余的画像材料用起来，并给匹配加上第三层：

- **autobio**：每个入池用户一份小传，由分章节的条目组成。每条只说一件事，带状态、时间锚点和来源；有新素材时增量增改退役，从不整篇重写。
- **A2A 判官**：硬过滤与软打分之后，对排在前面的候选对，读双方小传给出一个离散档位，候选按档位重排。
- **本人端点**：本人读自己的小传，可以删除其中任何一条。

### 0.1 为什么

- 软打分只能回答「两个人有没有说过同一类事」。合不合得来要看感情观、经历、需要和生活方式，这些在 16 个文本列和 engine 的提炼记忆里，v0.2 一列都没用。
- 文本列是 engine 逐轮滚动改写的一两句总结，旧版本不保留。pothos 照 v0.2 账本的做法把每一版都记下来，小传就能看到变化，而不只是最新一句。

### 0.2 业界参照

2026 年几家主流助手的用户记忆（结构细节多来自泄露的 system prompt 与逆向分析，作方向参考）：

| | 形态 | 写法 |
|---|---|---|
| ChatGPT | 分章节的画像，条目带月份与「最新状态」 | 后台整合，整篇重新生成 |
| Gemini | 四个固定章节，每条「陈述 + 依据（日期与用户原话）」 | 后台定期摘要 + 纠错账本 |
| Claude | 按主题分条 | 从每日整篇重写改为对话中逐条增量 |
| Grok | 一份固定标题的 markdown，一行一事实带日期 | 精确替换的增量编辑，先查重，不合并事实；角色扮演、虚构、玩笑不记 |

本设计取用的三点：

- **条目而不是散文。** 条目能精确删除、能追溯来源；整篇改写会丢条目、让旧内容复活，这是整篇重生成方案的主要用户反弹。
- **时间写进每一条。** 过时是这类系统被点名最多的失败。状态要随新素材重定，「考虑过」不能变成「做过」。
- **角色扮演不是自传。** 陪伴对话里入戏说的话单独存放，不与现实经历混写。

### 0.3 成功标准

1. 入池用户都有小传；每条可追溯到素材；本人删除的条目立即消失，之后的更新不会写回。
2. 判官档位给出了软打分之外的信息：维护者读判官理由，能看出档位依据的是小传内容，而不是重复软打分。
3. 不配 LLM 的实例，行为与 v0.2 一致；engine 没有 memories 出口时，autobio 只用账本素材照常运行。

1 的结构部分和 3 由测试钉住（§8）；条目写得好不好、2 是否成立，由维护者读样本判断，不写成断言。

### 0.4 约束

- 只经 engine 的 HTTP 出口单向读，不跨 schema，不回写 engine。
- 不依赖任何 `eros_engine_*` crate（MIT 红线）。
- v0.3 需要 engine 新增一条 memories 出口（§1.2），但不硬性依赖它。

---

## 1. 输入

### 1.1 insights 出口：15 个文本列

`/svc/v1/insights/changes` 已经返回 21 个数据列。v0.3 把其中 15 个文本列记进账本：`city`、`location`、`hometown`、`nationality`、`occupation`、`mbti_guess`、`love_values`、`emotional_needs`、`life_rhythm`、`education`、`family`、`relationship_history`、`social_pattern`、`future_plans`、`finance_status`。

`preferred_gender` 不记：性别以产品层推送的 `seeking_genders` 为准，对话里猜出来的偏好不参与。

### 1.2 memories 出口（engine 新增）

```
GET /svc/v1/memories/changes?since=<rfc3339>&after_id=<uuid>&limit=<n>
```

- 只导出 profile 层（`instance_id IS NULL`）且 `category` 有值的提炼行。逐轮原话 dump 与 relationship 层都不导出。
- 字段：`id`、`user_id`、`content`、`category`、`metadata`、`created_at`。`metadata` 是提取 prompt 写下的元数据（`domain` / `evidence_type` / `temporality` / `persistence` / `confidence`），词汇归部署方的 prompt 所有，pothos 原样存、只当提示用。
- keyset `(created_at, id)`，`limit` 默认 500、上限 1000，响应带 `next` 游标。照 insights 出口的先例：secret 不配则整条路由不挂。
- 这些行只插入、不改写；会话硬删时随之级联删除，出口不带删除（与 insights 出口相同的已知缺口）。

接口的最终形状由 engine 定，pothos 按发布的契约对接。

---

## 2. 存储

### 2.1 迁移 `0003`

```sql
-- 文本列并入账本。同一列的每一版是一行，「当前」照旧由 last_seen_at 派生。
ALTER TABLE pothos.observed_phrases DROP CONSTRAINT observed_phrases_dimension_check;
ALTER TABLE pothos.observed_phrases ADD CONSTRAINT observed_phrases_dimension_check
    CHECK (dimension IN (
        'interests', 'personality_traits', 'deal_breakers',
        'city', 'location', 'hometown', 'nationality', 'occupation', 'mbti_guess',
        'love_values', 'emotional_needs', 'life_rhythm', 'education', 'family',
        'relationship_history', 'social_pattern', 'future_plans', 'finance_status'));
ALTER TABLE pothos.observed_phrases
    ADD COLUMN id          BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE,  -- 供条目引用
    ADD COLUMN recorded_at TIMESTAMPTZ NOT NULL DEFAULT now();          -- pothos 时钟，素材水位

-- 下一轮重走一遍导出，把现有文本列收进账本。
UPDATE pothos.sync_cursor SET last_updated_at = NULL, last_user_id = NULL;

CREATE TABLE pothos.memory_mirror (
    id                UUID PRIMARY KEY,                    -- engine 的 memory id
    user_id           UUID NOT NULL,                       -- 归属列，不加 FK
    content           TEXT NOT NULL,
    category          TEXT NOT NULL,
    metadata          JSONB,
    engine_created_at TIMESTAMPTZ NOT NULL,
    recorded_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_memory_mirror_user ON pothos.memory_mirror (user_id, recorded_at);

CREATE TABLE pothos.memory_cursor (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_created_at TIMESTAMPTZ,
    last_id         UUID,
    synced_at       TIMESTAMPTZ
);
INSERT INTO pothos.memory_cursor DEFAULT VALUES;

CREATE TABLE pothos.autobio_entries (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id        UUID NOT NULL,                          -- 归属列，不加 FK
    section        TEXT NOT NULL CHECK (section IN
        ('basics', 'life', 'personality', 'love', 'intimacy', 'outlook', 'roleplay')),
    statement      TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('current', 'past', 'planned')),
    supersedes     BIGINT REFERENCES pothos.autobio_entries (id) ON DELETE SET NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_at     TIMESTAMPTZ,
    retired_reason TEXT CHECK (retired_reason IN ('revised', 'retracted', 'vetoed')),
    CHECK ((retired_at IS NULL) = (retired_reason IS NULL))
);
CREATE INDEX idx_autobio_entries_user ON pothos.autobio_entries (user_id);
CREATE INDEX idx_autobio_entries_supersedes ON pothos.autobio_entries (supersedes);

CREATE TABLE pothos.autobio_observed_sources (
    entry_id    BIGINT NOT NULL REFERENCES pothos.autobio_entries (id) ON DELETE CASCADE,
    observed_id BIGINT NOT NULL REFERENCES pothos.observed_phrases (id) ON DELETE CASCADE,
    PRIMARY KEY (entry_id, observed_id)
);
CREATE INDEX idx_autobio_observed_sources_observed ON pothos.autobio_observed_sources (observed_id);

CREATE TABLE pothos.autobio_memory_sources (
    entry_id  BIGINT NOT NULL REFERENCES pothos.autobio_entries (id) ON DELETE CASCADE,
    memory_id UUID   NOT NULL REFERENCES pothos.memory_mirror (id) ON DELETE CASCADE,
    PRIMARY KEY (entry_id, memory_id)
);
CREATE INDEX idx_autobio_memory_sources_memory ON pothos.autobio_memory_sources (memory_id);

CREATE TABLE pothos.autobio_state (
    user_id        UUID PRIMARY KEY,                       -- 归属列，不加 FK
    material_until TIMESTAMPTZ,                            -- 已消化到哪；NULL = 还没成功跑过
    ran_at         TIMESTAMPTZ NOT NULL,
    model          TEXT NOT NULL
);

CREATE TABLE pothos.pair_verdicts (
    user_lo    UUID NOT NULL,                              -- 归属列，不加 FK
    user_hi    UUID NOT NULL,                              -- 归属列，不加 FK
    lo_version TIMESTAMPTZ NOT NULL,
    hi_version TIMESTAMPTZ NOT NULL,
    grade      TEXT NOT NULL CHECK (grade IN ('strong', 'promising', 'unclear', 'poor')),
    rationale  TEXT NOT NULL,
    model      TEXT NOT NULL,
    judged_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_lo, user_hi),
    CHECK (user_lo < user_hi)
);
CREATE INDEX idx_pair_verdicts_hi ON pothos.pair_verdicts (user_hi);

CREATE TABLE pothos.judge_queue (
    user_lo      UUID NOT NULL,
    user_hi      UUID NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_lo, user_hi),
    CHECK (user_lo < user_hi)
);
CREATE INDEX idx_judge_queue_hi ON pothos.judge_queue (user_hi);
```

`user_concepts` view 加上 `o.dimension IN ('interests', 'personality_traits', 'deal_breakers')`；归概念任务取未判短语时同样只取这三个维度（`phrase_decisions` 的 CHECK 不变）。新表照 `0001` / `0002` 的写法做 Supabase lockdown。

### 2.2 设计要点

- **一本账本。** 文本列的一版与一条数组短语形状相同：用户、维度、值、首次与最近见到的时间。「当前」用同一种方法派生，所以不另建表。文本列只 trim，不转小写（保留专名），并在字符边界上截到 2000 字节以内，让账本行不超出 Postgres btree 索引项的上限；截断时记一条不含内容的日志。数组短语照旧过 `canonical()`。
- **素材水位用 pothos 时钟。** engine 在写入开始时打 `updated_at`、提交后才可见，按 engine 时间做水位会漏掉晚到的行。`recorded_at` 只在插入时写，同一版再次出现只推进 `last_seen_at`，不算新素材。
- **来源分两张表。** 每张都是从属表，两根外键都非空、都 `CASCADE`：条目没了链接没意义，素材没了链接也没意义。条目因此可能失去全部来源，下一次更新时它会被当作无依据的条目处理（§3.4）。
- **派生，不存储：**
  - 条目的时间锚点 `as_of` = 它所有来源里最晚的时间（账本行取 `first_seen_at`，记忆取 `engine_created_at`）；
  - 一个人小传的版本 = 他所有条目里 `created_at` 与 `retired_at` 的最大值。删除、修订、新增都会推进它。
- **`pair_verdicts` 与 `judge_queue` 的 `user_*` 都是归属列**，不加外键；`user_lo` 走主键前缀，`user_hi` 单独建索引，离开配对池时按两列删除。
- 退役的条目都保留，这本身就是修订历史；本人删除的条目同时是「不得写回」的依据（§3.4）。

---

## 3. autobio 更新

### 3.1 调度

sweeper 每轮的步骤，仍只在一个实例上跑：

```
① 镜像 insights        数组短语 + 15 个文本列进账本
② 归概念               只处理三个数组维度
③ 镜像 memories        新出口 → memory_mirror
④ autobio 更新
⑤ 判官                 §5
```

各步互不影响对方的游标与水位；前一步失败，后一步照常处理已有数据。

第④步选人，要同时满足：

- `match_profiles` 有这个人的行（`active` 不论）。只给选择了配对的人生成小传；产品层想让用户入池前先预览，可以先推一行 `active: false`；
- 有新素材：账本或记忆里有这个人 `recorded_at` 晚于 `material_until` 的行（`material_until` 为 NULL 时，有任何素材即可）；
- 距 `ran_at` 至少 `AUTOBIO_MIN_INTERVAL_SECS`（默认 3600），把聊天中逐轮刷新的文本列攒成一批。

按最早的待处理素材排序，每轮最多 `AUTOBIO_MAX_PER_TICK` 人（默认 50）。串行处理，每人一次调用、一个事务；新水位取本次运行开始的时刻。读素材之前、调用模型之前各查一次这个人是否仍在池里，已离开就跳过，不调用。

### 3.2 输入

prompt 写在代码里。用户消息包含：

- **当前时间。**
- **七个章节**：

  | key | 内容 |
  |---|---|
  | `basics` | 基本与来历：居住、籍贯、教育、职业、家庭 |
  | `life` | 生活与兴趣：作息、爱好、社交 |
  | `personality` | 性格与沟通 |
  | `love` | 感情：观念、经历、需要、不能接受的 |
  | `intimacy` | 亲密与性 |
  | `outlook` | 目标与现状：计划、经济 |
  | `roleplay` | 角色扮演中的倾向 |

- **当前条目**：id、章节、陈述、状态、时间锚点（年月）；已没有任何来源的条目标出来。
- **新素材**：每条一个引用号，`o:<id>` 指账本行，`m:<uuid>` 指记忆。带上类型（文本列名、数组维度，或记忆的 `category` 与 `metadata`）和时间（精确到秒）。素材从旧到新排列，prompt 里写明同一列的新版本取代旧版本。`material_until` 为 NULL 时（首次运行或重建），素材是这个人的全部。
- **本人删过的陈述**：`retired_reason = 'vetoed'` 的条目。

### 3.3 输出契约

```json
{"ops": [
  {"op": "add", "section": "love", "status": "current", "statement": "…", "sources": ["m:…", "o:12"]},
  {"op": "revise", "entry": 123, "status": "past", "statement": "…", "sources": ["m:…"]},
  {"op": "retire", "entry": 45}
]}
```

- `add`：新建一条。
- `revise`：旧条目退役（`revised`），新建一条，`supersedes` 指向旧条目。旧条目的来源由 pothos 自动并入新条目，模型只列新增的来源。
- `retire`：条目退役（`retracted`），用于被素材否定或已无依据的条目。

### 3.4 规则

prompt 里写明：

- 一条只说一件事，第三人称；保留具体细节、否定、程度和不确定性。
- 只写素材支持的内容。不推因果，不下人格结论，不做诊断。用户自己说出的因果可以记。
- 新素材与旧条目矛盾时，修订旧条目（改成 `past` 或改写），不让两种说法并存。`planned` 只有在素材说明已经发生之后才能改成 `past` / `current`。陈述里不写相对时间，时间由锚点表达。
- 与已有条目重复的素材，修订那条并入来源，不另开一条；可以引用其他当前条目的来源，用来合并重复条目。
- 没有任何来源的当前条目，要么由新素材重新支持，要么退役。
- 角色扮演的素材（`evidence_type` 为 `roleplay_expression`，或内容写明是角色扮演）只进 `roleplay` 一节；这一节的内容不进别的节。
- 本人删过的陈述，换个说法也不得写回。
- 「暂无」「不详」这类素材忽略。
- 新条目的语言由 `pothos.toml` 的 `[autobio] language` 决定。

### 3.5 校验

逐条进行：

- `section`、`status` 取值合法；
- `entry` 属于这个人且未退役；同一个 `entry` 在一份响应里只能被一个 op 动；
- 每个来源存在、属于这个人，并且是本次的新素材或某条当前条目的来源；
- `add` 至少一个来源；
- 陈述 trim 后非空、不含 NUL，长度不超过 300 字符（只防异常输出，不是质量规则）。

不合格的那条跳过，不影响同批其他条。整份响应解析不了时整轮不写、水位不动，`ran_at` 照样更新，过了最小间隔再重来，避免一份毒响应每轮都占名额。

### 3.6 读数与重建

- 每次运行记下 `model`；日志记各类 op 的条数、跳过的条数，以及响应里有的 token usage。
- 退役条目即修订历史，维护者据此判断修订有没有丢信息。
- 换模型或改 prompt 后要重建某人：删掉他未被本人删除的条目，再删掉他的 `autobio_state` 行，下一轮用全部素材重写；被本人删除的条目保留，继续约束写回。README 写明。

### 3.7 已知风险

- **「不得写回」靠 prompt。** 删除在当下是结构性的（条目立即退役），之后的写回只能靠模型遵守。漏了就漏了，本人可以再删；不为残余漏出另做过滤。
- **文本列回退。** 某列从 A 改成 B 再改回 A 时，A 的账本行已存在，不产生新素材，小传会停在 B。罕见，先看读数。
- **prompt 随素材增长。** 首次运行带全部素材，单人素材眼下一次调用装得下。条目或素材大到不合适时再分章节供料。
- **发送前的同意窗口。** 调用模型前会再查一次是否在池里；查完到请求发出之间仍有极短的窗口，这期间离开的人，这一次的素材仍会发出，回来的结果由写入事务的行锁守卫丢弃。彻底消除要把行锁持有到外部调用结束，离开就得等上几分钟，不这样做。
- **判官每次请求都可能被拒绝的一对。** 回复不可用或请求被拒绝时删掉队列行，下次请求又会排进来，每次各付一次调用。不做退避，先看读数。

---

## 4. 本人端点

API-first：先用 `#[utoipa::path]` 与 `#[derive(ToSchema)]` 声明契约。

```
GET    /v1/autobio/{user_id}                      本人的当前小传
DELETE /v1/autobio/{user_id}/entries/{entry_id}   本人删除一条
```

`GET` 响应：

```json
{ "user_id": "…", "reason": null, "updated_at": "2026-10-15T…Z",
  "sections": [
    { "section": "love",
      "entries": [ { "id": 123, "statement": "…", "status": "current", "as_of": "2026-09-14T…Z" } ] } ] }
```

- 章节按 §3.2 的固定顺序，空章节不出现；只返回当前条目。`updated_at` 是小传版本，`as_of` 是条目的时间锚点，两者都是派生值。
- `match_profiles` 有这个人的行就能读，`active: false` 也一样（入池前预览）；没有行：404 `not_in_pool`。还没有小传：200，`sections` 为空，`reason: "no_autobio_yet"`。
- `DELETE`：条目退役（`vetoed`），204。条目不属于这个人或已退役：404 `entry_not_found`。删除推进小传版本，涉及他的判官缓存随之过期。
- 小传只给本人看。确认调用者就是本人是产品层的事；pothos 只认 caller secret。

**离开配对池。** `DELETE /v1/profiles/{user_id}` 同时删掉这个人的全部 autobio 数据：条目（含本人删过的）、`autobio_state`、涉及他的 `pair_verdicts` 与 `judge_queue`。离开即撤回同意；重新入池从头生成。autobio 写入、判定写入和判官入队都在自己的事务里先以 `FOR SHARE` 锁住相关的 `match_profiles` 行，行已不在就什么都不写，所以离开时正在进行的写入不会留下数据。镜像表（`insight_mirror`、`observed_phrases`、`memory_mirror`）照旧保留，README 的 Data lifecycle 一段写明。

---

## 5. A2A 判官

### 5.1 触发

在 `GET /v1/match/candidates` 里，只在配了模型时进行（不配模型时候选不带档位，排序同 v0.2）：

1. 硬过滤与软打分照旧，得出 `limit` 个候选。
2. 返回列表里的每一对都去 `pair_verdicts` 查判定，有就带上。判定按无序对存（`user_lo < user_hi`），A 看 B 与 B 看 A 共用一份。
3. 对排在前 `JUDGE_TOP_K` 名（默认 10）的候选，判定不存在，或任一方的小传版本比判定时新，就写进 `judge_queue`（已在队列里则不动）。任一方还没有小传，或已不在池里，这一对不判。
4. 版本过期的判定照样返回，同时排队重判。

### 5.2 响应

每个候选加一个字段：

```json
{ "user_id": "…", "score": 0.85, "chemistry": "promising", … }
```

`chemistry` 取 `"strong" | "promising" | "unclear" | "poor" | null`。列表按（档位，软打分）重排，档位的先后是 strong > promising > 未判 = unclear > poor。`poor` 只排到最后，不剔除。

### 5.3 判官调用

sweeper 第⑤步，每轮从队列取最早的若干对，最多 `JUDGE_MAX_PER_TICK`（默认 100）：

- 先读双方版本，再读当前条目，按章节分组，每条带状态与年月，两人标成「甲」「乙」。读到时判定已是最新的，直接删掉队列行；任一方没有当前条目，或调用前再查时已不在池里，删掉队列行、不调用。
- 一次调用，输出：

  ```json
  {"rationale": "…", "grade": "promising"}
  ```

- 档位语义，写进 prompt：

  | 档 | 含义 |
  |---|---|
  | `strong` | 有具体、互补的契合点，看不到明显冲突，值得优先介绍 |
  | `promising` | 合得来的地方多过冲突，值得认识 |
  | `unclear` | 信息不够判断，或好坏参半 |
  | `poor` | 有明显冲突：价值观、生活方式、感情期待，或踩到对方不能接受的点 |

- prompt 里写明：只根据写出来的内容判断；材料薄就判 `unclear`，不因材料少扣分；`roleplay` 一节是倾向不是现实；亲密与性与其他维度同等对待。理由的语言照 `[autobio] language`。
- 校验：`grade` 合法，`rationale` 非空、不含 NUL、不超过 2000 字符。
- 一个事务里 upsert `pair_verdicts`（带读取时的双方版本、模型、时间）并删掉队列行。读取之后小传又变了，这条判定自然过期，下次请求重新排队。

### 5.4 理由不对外

理由里有双方小传的内容，任何一方看到都违反「小传只给本人看」。v0.3 对外只给档位，B 的小传在构造上不会到达 A。理由留在库里供维护者读，是判断小传与判官质量的读数。

给用户看理由需要先有「对方同意」的机制，那是放宽门槛，不在本轮。

### 5.5 持久化原则

README 的「Nothing persisted about matches」改为：「已看过 / 已划掉 / 已配上」是产品层的状态，引擎不持久化；判官的判定按（这一对，双方小传版本）缓存。

---

## 6. 配置

| 项 | 位置 | 默认 | 说明 |
|---|---|---|---|
| `AUTOBIO_MAX_PER_TICK` | env | 50 | 每轮最多更新几个人的小传；0 关闭这一步 |
| `AUTOBIO_MIN_INTERVAL_SECS` | env | 3600 | 同一人两次更新的最小间隔，上限一年 |
| `JUDGE_TOP_K` | env | 10 | 每次候选请求最多为前几名排队 |
| `JUDGE_MAX_PER_TICK` | env | 100 | 每轮最多判几对；0 关闭这一步 |
| `AUTOBIO_MODEL` / `JUDGE_MODEL` | env | 用 `LLM_MODEL` | 只覆盖模型名，`LLM_BASE_URL` / `LLM_API_KEY` 共用 |
| `[autobio] language` | `pothos.toml` | `en` | 条目与判官理由的语言 |

LLM 客户端沿用 v0.2：OpenAI 兼容、不跟随重定向、key 标为 sensitive、只对 5xx 与超时重试。autobio 与判官的回复可能很长，每次尝试的超时是 300 秒；归概念保持 60 秒。不配 LLM 时 autobio 与判官都不启动：端点返回 `no_autobio_yet`，`chemistry` 全为 null，行为同 v0.2。

autobio 与判官都是 sweeper 的步骤，照归概念的先例：sweeper 不启动（没配 `ENGINE_*` 或 `SWEEP_DISABLED`）它们也不跑。候选端点在任何实例上都可以写 `judge_queue`，由跑 sweeper 的那个实例消化。

---

## 7. 错误处理

| 情况 | 行为 |
|---|---|
| memories 出口 404（engine 尚无此出口） | 记日志，跳过这一步；autobio 只用账本素材 |
| memories 出口其他失败 | 记日志，游标不动，下一轮重试 |
| autobio：请求被拒绝（401 / 402 / 408 / 429 以外的 4xx，或回复没有内容） | 这个人本轮不写，水位不动，`ran_at` 更新；继续下一个人 |
| autobio：LLM 不可达、5xx 重试用尽 | 同上，并结束这一轮 |
| autobio：整份响应解析不了 | 这个人本轮不写，水位不动，`ran_at` 更新；继续下一个人 |
| autobio：单条 op 不合格 | 只跳过那条 |
| 判官：请求被拒绝 | 删掉队列行，继续下一对 |
| 判官：LLM 不可达、5xx 重试用尽 | 保留队列行并移到队尾，结束这一轮 |
| 判官：响应解析不了或档位不合法 | 删掉队列行；下次请求会重新排队 |
| 没配 LLM | autobio 与判官不启动 |

---

## 8. 测试

TDD。`#[sqlx::test]` 跑真实 Postgres，engine 与 LLM 用 wiremock 模拟。要钉住的：

- **迁移 `0003`**：维度扩充；已有账本行得到 `id` 与 `recorded_at`；`sync_cursor` 重置；`user_concepts` 与归概念只看三个数组维度。
- **账本**：文本列按版本入账，只 trim；`preferred_gender` 不入账；同一版再次出现只推进 `last_seen_at`，不改 `recorded_at`。
- **memories 镜像**：keyset 翻页、幂等 upsert、游标与页同事务；404 时跳过，其余步骤继续。
- **autobio**：
  - 选人：入池闸门、最小间隔、每轮上限、按 `recorded_at` 的水位；
  - prompt 里有当前条目、带引用号的新素材、本人删过的陈述；首次运行带全部素材；
  - `add` / `revise` / `retire` 正确落库；`revise` 并入旧来源并写 `supersedes`；
  - 逐条校验与跳过；解析失败时不写、水位不动、`ran_at` 更新；
  - 不配 LLM 时不启动。
- **端点**：`GET` 的形状、章节顺序、空章节不出现、`no_autobio_yet`、`not_in_pool`；`DELETE` 的 204 / 404 与版本推进；离开配对池时连带删除 autobio 数据，镜像保留。
- **判官**：
  - 只为前 K 名排队；无序对主键，两个方向共用；
  - 任一方没有小传不排队；过期判定照样返回并重新排队；
  - 按档位重排的顺序；
  - 判定落库带双方版本、删队列行；解析失败删队列行，不可达保留；
  - **`rationale` 不出现在任何响应里。**

条目质量与档位准不准不写断言，由维护者读样本判断。

---

## 9. 交付

- 版本 `0.3.0`。README 补上小传与判官、新端点与新配置、Data lifecycle、§5.5 的持久化原则、memories 出口对 engine 版本的要求（可选，缺了退化为只用账本）。
- engine 侧：开 issue 提 §1.2 的 memories 出口，engine 发版后 README 写明需要的版本。
- pothos-heart 还没有部署，这一轮不碰任何线上流量。产品层接入前池子是空的，上线后不会产生调用；之后每个入池用户首次生成一次调用，每轮最多 50 人。判官每次候选请求最多排 10 对，同一对两个方向共用一份判定。
- 开发与测试只打 wiremock，不调用付费模型。真跑由维护者触发，一次一轮。

---

## 10. 范围外

| 项 | 去向 |
|---|---|
| 给用户看判官理由 | 需要先有对方同意的机制 |
| 本人改写条目 | 只收删除；改写会让 pothos 持有第二份事实 |
| 把小传给配对对象看 | 同上，需要同意机制 |
| 小传 embedding 进软打分 | 等读数 |
| 小传反哺 engine 的聊天 | 未排期 |
| engine 侧删除的传播 | 已有缺口，产品层接入前要先定清除流程 |
| 导出逐轮原话 | 不做 |
| A2A 对话模拟 | 判官档位够用之前不做 |
| 判官档位进硬过滤 | 等读数 |

---

## 11. 交接

- **模型选择**：autobio 与判官用哪个模型，由下游部署按自己的模型路由规则决定。素材里有大量性相关内容，模型不能拒答，也不能淡化。
- **engine 出口**：§1.2 以 issue 形式提给 engine，接口形状由 engine 定。
