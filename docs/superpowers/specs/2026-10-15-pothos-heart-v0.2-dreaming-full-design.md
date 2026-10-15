# pothos-heart v0.2 — 画像合并（dreaming-full）设计

**日期**：2026-10-15
**状态**：设计已批准，待实现计划
**范围**：画像合并，即跨 session 增量、去重、语义合并，含跨用户可比。autobio 在 v0.3。

---

## 0. 目标

v0.1 直接拿 engine 导出的数组做匹配：短语规范化（trim + 小写）之后按字面全等求交集。v0.2 在 engine 的导出和匹配之间加一层画像合并：

- **跨 session 增量**：engine 的数组是整列覆盖，某一轮漏写的旧项在 engine 里就没了。pothos 把出现过的每一条短语都记进自己的账本。
- **去重**：同一个人的多条近义短语算作一项。
- **语义合并，含跨用户可比**：短语归到所有用户共用的概念上。A 的「常听老派爵士」和 B 的「周末泡爵士酒吧」在打分里算重合。

### 0.1 为什么

engine 的 structuring prompt 要求每个数组项写成 4~12 个汉字、带具体细节的短语，不是标签。两个人很少写出一字不差的短语。一个下游生产部署的聚合读数（2026-10-15，`human_insights` 全量 509 行）：

| 列 | 有值的用户 | 人均项数 | 平均字数 | 不同项 | ≥2 人共有的项 | 字面有任一重合的用户对 |
|---|---|---|---|---|---|---|
| interests | 257 | 5.8 | 12.6 | 1,473 | 16 | 19 / 32,896（0.06%） |
| personality_traits | 215 | 11.0 | 18.2 | 2,343 | 11 | 63 / 23,005（0.27%） |
| deal_breakers | 30 | — | — | — | — | 0 / 435 |

按字面比较，99.7% 以上的用户对在两个数组维度上都得 0 分，deal_breakers 过滤一次都不触发。

### 0.2 成功标准

1. 同一份数据上，interests / traits 两个维度有非零重叠的用户对比例明显高于字面比较的 0.06% / 0.27%。
2. deal_breakers 能在概念上触发：「受不了抽烟」对得上「烟瘾很大」，对不上「已经戒烟三年」。
3. 不配 LLM 的自托管实例，行为与 v0.1 一致。

1、2 由人读读数和词表来判断，不写成测试断言；3 有测试钉住（§7）。

### 0.3 约束

- **只经 engine 的 HTTP 出口单向读**：`GET /svc/v1/insights/changes`。不跨 schema 读 `engine.*`，不回写 engine。pothos 与 engine 同库（单独的 `pothos` schema）或用独立 Postgres，两种形态都要成立。
- 不依赖任何 `eros_engine_*` crate（MIT 红线）。
- v0.2 不给 engine 加新出口。

---

## 1. 输入：engine 出口里有什么

engine v2.2.0 起提供 `/svc/v1/insights/changes`，每行是 `human_insights` 的 21 个数据列加 `updated_at`。写入方式决定了 pothos 能拿到什么：

- **每个聊天回合都可能写一次。** post_process 先抽事实，再由 LLM 结合现有行输出结构化 JSON，然后逐列增量 upsert。
- **数组整列覆盖**：LLM 输出非空就替换旧值，SQL 层不做并集。LLM 少写一项，那一项就从 engine 里消失。
- **标量**：`COALESCE(new, old)`。文本列是 LLM 合并改写之后再覆盖。
- **`updated_at` 每次写入都会推进**，内容没变也一样。
- engine 不做任何近义去重。

v0.2 只用三个数组列（`interests` / `personality_traits` / `deal_breakers`）和 `age_min` / `age_max`。16 个文本列（`love_values`、`relationship_history` 等）留给 v0.3 的 autobio。

---

## 2. 架构与存储

sweeper 的 keyset 翻页方式不变，变的是每一行落到哪里：

```
engine export ──► insight_mirror        age_min / age_max / engine_updated_at（三个数组列移走）
              └─► observed_phrases      出现过的短语只增不删

归概念任务 ──► concepts                 所有维度共用一张词表
           └─► phrase_decisions         全局，不分用户
               phrase_concepts          每个短语 0~3 个概念

读侧 view  ──► user_concepts            每人每维度的概念，带「当前」标记
```

### 2.1 迁移 `0002`

```sql
CREATE TABLE pothos.observed_phrases (
    user_id       UUID NOT NULL,                       -- 归属列，不加 FK
    dimension     TEXT NOT NULL
        CHECK (dimension IN ('interests', 'personality_traits', 'deal_breakers')),
    phrase        TEXT NOT NULL,                       -- canon 后的短语
    first_seen_at TIMESTAMPTZ NOT NULL,                -- 首次出现时的 engine updated_at
    last_seen_at  TIMESTAMPTZ NOT NULL,                -- 最近一次出现时的 engine updated_at
    PRIMARY KEY (user_id, dimension, phrase)
);
CREATE INDEX idx_observed_phrases_phrase ON pothos.observed_phrases (dimension, phrase);

CREATE TABLE pothos.concepts (
    label      TEXT PRIMARY KEY,                       -- canon 后的标签
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE pothos.phrase_decisions (
    dimension  TEXT NOT NULL
        CHECK (dimension IN ('interests', 'personality_traits', 'deal_breakers')),
    phrase     TEXT NOT NULL,
    model      TEXT NOT NULL,
    decided_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (dimension, phrase)
);

CREATE TABLE pothos.phrase_concepts (
    dimension TEXT NOT NULL,
    phrase    TEXT NOT NULL,
    concept   TEXT NOT NULL
        REFERENCES pothos.concepts (label) ON DELETE CASCADE ON UPDATE CASCADE,
    PRIMARY KEY (dimension, phrase, concept),
    FOREIGN KEY (dimension, phrase)
        REFERENCES pothos.phrase_decisions (dimension, phrase) ON DELETE CASCADE
);
CREATE INDEX idx_phrase_concepts_concept ON pothos.phrase_concepts (concept);
```

同一迁移里的其余几步：

- 把现有 `insight_mirror` 的三个数组按行展开，回填进 `observed_phrases`，`first_seen_at = last_seen_at = engine_updated_at`。然后删掉这三列，两个 GIN 索引随列一起删除。
- 新表照 `0001` 的写法做 Supabase lockdown：guarded `REVOKE` 加 `ENABLE ROW LEVEL SECURITY`。`0001` 里的 `REVOKE ALL ON ALL TABLES` 只覆盖当时已有的表，所以要重做一遍。

### 2.2 设计要点

- **词表跨维度共用。** deal_breakers 要与对方的 interests ∪ traits 求交集，三个维度必须落在同一套概念上。
- **映射按短语全局存。** 同一 `(dimension, phrase)` 只判一次，不同的人写出同一句话不会再花一次调用。主键带 `dimension`：同一句话作为 deal_breaker 和作为 trait，指的未必是同一件事。
- **「判过、无概念」是一种合法状态**：`phrase_decisions` 有行、`phrase_concepts` 没有行。用来收「暂无」「不详」这类无意义的项。
- **外键**：`phrase_concepts` 是从属表，两根外键都 `CASCADE`；FK 列有索引（`(dimension, phrase)` 命中主键前缀，`concept` 单独建索引）。`observed_phrases` 不指向 `phrase_decisions`，因为短语可能还没判过。`user_id` 仍是归属列，不加 FK。
- **「当前」由派生得到**：`last_seen_at = insight_mirror.engine_updated_at`，不另存标记列。
- **`observed_phrases` 不指向 `insight_mirror`**：两表之间唯一的关联列是 `user_id`，即归属列。清除一个用户时两张表都要删，README 的 Data lifecycle 一段写明。

### 2.3 sweeper 的写法

每个导出行在同一事务里（照 v0.1，与游标同页提交）：

1. upsert `insight_mirror` 的标量列；
2. 三个数组先过 v0.1 的 `canonical()`（trim、小写、去空、去重），再逐条 upsert 进 `observed_phrases`：新短语插入，`first_seen_at = last_seen_at = updated_at`；已有短语只把 `last_seen_at` 推到 `updated_at`。
3. 本次没出现的短语不动，`last_seen_at` 停在旧值。

### 2.4 `user_concepts` view

```sql
CREATE VIEW pothos.user_concepts AS
SELECT o.user_id, o.dimension,
       COALESCE(pc.concept, o.phrase)          AS concept,
       o.last_seen_at = m.engine_updated_at    AS is_current
FROM pothos.observed_phrases o
JOIN pothos.insight_mirror m USING (user_id)
LEFT JOIN pothos.phrase_decisions d USING (dimension, phrase)
LEFT JOIN pothos.phrase_concepts pc USING (dimension, phrase)
WHERE d.phrase IS NULL OR pc.concept IS NOT NULL;
```

- **没判过的短语，概念就是它自己**（字面兜底）。不配 LLM 的实例因此退化成 v0.1 的行为，不需要另写代码路径；LLM 暂时故障时，新短语先按字面参与匹配。
- 判成「无概念」的短语被 `WHERE` 排除，不参与匹配，也不按字面兜底。
- 用户的概念集合由 view 现算，不落表。按现有约 500 人、人均十几项的规模，每次请求聚合一遍没有压力。量大以后再考虑物化。

---

## 3. 归概念任务

### 3.1 调度

作为 sweeper 每轮的第二步，在镜像步骤之后执行，不论镜像步骤成败；间隔照 `SWEEP_INTERVAL_SECS`。sweeper 不启动（没配 `ENGINE_*` 或 `SWEEP_DISABLED`），它也不跑。

1. 取出还没判过的 `(dimension, phrase)`：`observed_phrases` 里有、`phrase_decisions` 里没有的，按最早的 `first_seen_at` 排序，每轮最多 `CONCEPT_MAX_PER_TICK` 条（默认 200）。这个上限用来框住单轮开销。
2. 每 40 条一批（代码常量），**串行**送给 LLM，后一批能看到前一批新建的概念。
3. 每批一个事务：新概念 `INSERT … ON CONFLICT DO NOTHING`，再写 decision 和 mapping。

sweeper 只该在一个实例上跑；多实例部署时，其余实例设 `SWEEP_DISABLED=1`。README 写明这一点。

### 3.2 prompt 契约

prompt 写在代码里。输入是**整张词表**（v0.2 不上 embedding 召回，词表全量放进 prompt），加上本批短语和各自的维度。输出：

```json
{"results": [{"i": 0, "concepts": ["爵士乐"]}, {"i": 1, "concepts": []}]}
```

prompt 里写明的规则：

- 每个短语给 0~3 个概念。
- 能复用词表就复用，跨语言也复用（「jazz」归到已有的「爵士乐」）；词表里没有才新建。
- **粒度准则**：取两个人会说「我们都喜欢 X」的那一级。是「爵士乐」，不是「音乐」，也不是「老派爵士」。
- **deal_breakers 写成被拒绝的那件事本身，保留极性**：「受不了抽烟」→「吸烟」，「已经戒烟三年」→「戒烟」，两者不是同一个概念。
- 空列表留给无意义的项。
- 新建概念的语言由 `pothos.toml` 的 `[concepts] label_language` 决定。开源默认 `en`，下游部署按自己的界面语言设置。

### 3.3 校验

逐条进行：

- 每个标签先过 `canon`，再检查长度上限（24 字符，只防异常输出，不是质量规则）；
- 每条最多 3 个概念；
- `i` 必须对上本批的短语，同一个 `i` 不得重复。

不合格的那一条跳过，下一轮重判，不影响同批其他条。整份响应解析不了时，整批不写，下一轮重来。

### 3.4 LLM 客户端

- 走 OpenAI 兼容的 `POST {LLM_BASE_URL}/chat/completions`，`Authorization: Bearer {LLM_API_KEY}`，模型取 `LLM_MODEL`。OpenRouter、Venice、本地 vLLM / ollama 都能接。
- temperature 0。不依赖 `response_format`，因为各家支持不一；从 `choices[0].message.content` 里取 JSON，容忍 ```` ```json ```` 围栏。
- 三个变量都配，或都不配。只配一部分时启动报错。都不配，归概念任务关闭。
- 超时 60 秒；不跟随重定向（照 engine 客户端）；key 的 header 标为 sensitive。
- 只对 5xx 和超时重试，同一次调用内最多 2 次。4xx 不重试。

### 3.5 读数

- 每条 decision 记下 `model` 和 `decided_at`。换模型后能查出哪些是旧模型判的；要重判，删掉对应的 `phrase_decisions` 行即可（mapping 随之级联删除）。
- 每轮日志记：判了几条、跳过几条、新建几个概念、词表大小，以及响应里有的 token usage。

### 3.6 已知风险

- **毒短语**：某条短语每次都让模型拒答，它就每轮占一个名额。单条校验加每轮上限把开销框住了，v0.2 不另做隔离。
- **词表膨胀与粒度不一**：同义概念（「爵士乐」「爵士音乐」）可能被分批建出来。整张词表放进 prompt 能减少这种情况，但消不掉。v0.2 不做概念合并工具，先看读数。
- **prompt 随词表增长**：词表大小进日志，大到不合适时再换成 embedding 召回候选概念。

---

## 4. 匹配怎么用概念

### 4.1 硬过滤

仍是一条 SQL。数组从 `insight_mirror` 换成从 `user_concepts` 聚合出的概念集合：

| 规则 | v0.1 | v0.2 |
|---|---|---|
| 性别、年龄 | `match_profiles` + mirror 的 `age_min` / `age_max` | 不变 |
| deal_breakers | 字面短语 `&&` | **双方都取当前**：A 当前的 deal_breaker 概念，与 B 当前的 interests ∪ traits 概念不相交；反向亦然 |

双方都取当前：B 历史里的「吸烟」不会触发 A 的拒绝，A 已经不再提起的旧 deal_breaker 也不再筛人。缺数据照旧放过：没有概念的人不会因此被排除。

### 4.2 软打分

`scoring.rs` 的重叠系数代码不动，输入从短语换成**历来出现过的**概念集合（不分当前与否）。一个人 83 条短语，归完可能只剩三四十个概念，`min(|A|,|B|)` 也按概念数算。

### 4.3 响应契约

形状不变，语义变了：`shared_interests` / `shared_traits` 放共有的**概念标签**，不再是双方一字不差的短语。不返回对方的原话，展示什么由产品层决定。OpenAPI 的字段描述和 README 跟着改。

`no_insights_yet` 仍按「`insight_mirror` 没有这个人的行」判定。

### 4.4 要盯的读数

累积会让项数多的人概念集合更大。重叠系数取 `min`，大集合的一方更容易「覆盖」对方，可能成为跟谁都有点像的万能候选。v0.1 就有这个性质，累积把它放大。v0.2 不为它改公式，先靠候选端点返回的各维度分数积累读数。

---

## 5. 配置汇总

| 项 | 位置 | 默认 | 说明 |
|---|---|---|---|
| `LLM_BASE_URL` / `LLM_API_KEY` / `LLM_MODEL` | env | 不配 | 三个全配或全不配；全不配时归概念关闭，退化为字面匹配 |
| `CONCEPT_MAX_PER_TICK` | env | 200 | 每轮最多判几条短语 |
| `[concepts] label_language` | `pothos.toml` | `en` | 新建概念的语言 |
| 批大小 | 代码常量 | 40 | |

---

## 6. 错误处理

| 情况 | 行为 |
|---|---|
| LLM 不可达、key 错、5xx 重试用尽 | 记日志；短语保持未判，按字面参与匹配；下一轮再试 |
| 整份响应解析不了 | 整批不写，下一轮重来 |
| 单条结果不合格 | 只跳过那一条 |
| 镜像步骤失败 | 归概念步骤照常执行，处理已有的账本；两步互不影响游标 |
| 没配 LLM | 归概念任务不启动，行为同 v0.1 |

---

## 7. 测试

TDD。`#[sqlx::test]` 跑真实 Postgres，LLM 用 wiremock 模拟。要钉住的：

- **迁移 `0002`**：v0.1 镜像里的数组正确回填进 `observed_phrases`，三列删除。
- **累积**：两次导出之间被 engine 丢掉的短语还在，`last_seen_at` 停在旧值；「当前」的派生正确。
- **归概念**：
  - prompt 里带着整张词表；
  - 已有概念复用，没有的新建；
  - 空列表记作「判过、无概念」；
  - 不合格条目跳过、下轮重判；
  - 解析失败时整批不写；
  - 第二批的 prompt 里看得到第一批新建的概念；
  - 每轮上限生效；
  - 不配 LLM 时任务不启动，只配一部分时启动报错。
- **view**：没判过的按字面；判成空的不出现；历来与当前分开算。
- **硬过滤**：deal_breakers 双向都只看当前，历史项不会筛人。
- **打分**：按概念算重叠；理由里是概念标签。
- **LLM 客户端**：请求是 OpenAI 兼容的形状；key 标为 sensitive；不跟随重定向；只对 5xx / 超时重试，最多 2 次。

**归概念的质量不写断言。** 粒度对不对、合并得准不准，只有人读过词表才算数。第一次接真模型跑出的词表由维护者阅读判断。

---

## 8. 交付

- 版本 `0.2.0`。README 补上 LLM 配置、概念语义、单实例跑 sweeper 的要求；Data lifecycle 一段把 `observed_phrases` 加进「要彻底清除还需另删」的范围。
- engine 仍要求 v2.2.0 或更高。
- pothos-heart 还没有部署，这一轮不碰任何线上流量。
- 开发与测试只打 wiremock，不调用付费模型。按上面读数里约 3,800 条不同短语、每批 40 条估算，首次回填约 100 次调用；按每轮 200 条，约 20 轮回填完，默认间隔下约 100 分钟。真跑由维护者触发。

---

## 9. 范围外

| 项 | 去向 |
|---|---|
| embedding 召回候选概念 | 词表大到 prompt 装不下时再做 |
| 概念合并、改名工具 | 等读数 |
| 两级分类体系（「爵士乐」之上的「音乐」） | 等读数 |
| 16 个文本列 | v0.3 autobio |
| engine 的 memories / snapshot 出口 | v0.3 按需再议 |
| 清除流程 | 已有缺口；`observed_phrases` 留存的更多，产品层接入前要先定 |
| LLM A2A 化学评估 | 未排期 |
| 部署位置 | 未定 |

---

## 10. 交接

- **模型选择**：下游部署用哪个模型做归概念，回复 eros-engine-web #181。这些短语里有性偏好和 deal_breakers，模型不能拒答；选型照那边的模型路由规则。
