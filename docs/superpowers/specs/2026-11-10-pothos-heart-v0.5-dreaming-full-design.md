# pothos-heart v0.5 — dreaming-full：跟上 engine 的逐行删除

**日期**：2026-11-10
**状态**：设计定稿，待实现
**范围**：消费 engine 的逐行删除流（分类记忆与账本事实），清掉对应镜像行与 autobio 来源，来源全失的条目退役。autobio 的写法、判官、打分、候选响应都不动。

---

## 0. 目标

dreaming-full = engine 的 dreaming + pothos-heart，三块：autobio v1、用户删记忆、概念归并。

| 块 | 现状 | 本版 |
|---|---|---|
| autobio v1 | v0.4 已在 main | 不动；改写质量等部署后读样本再议 |
| 用户删记忆 | engine v2.6.4 有了逐行删除与删除流 | **本版**：pothos 这一半 |
| 概念归并（engine 记忆表的 merge / dedupe / generalize） | 没有读数表明重复事实已占满 prompt | 不做，等读数；要做也是 engine 的事 |

用户删掉 engine 里的一条记忆或一条事实之后，pothos 的镜像里那一行还在，autobio 里引用它的条目也还在，判官照旧读它。v0.5 补上这一环：engine 删一行，pothos 在一轮 sweep 内跟着删。

**删除请求不经过 pothos。** 产品层直接调 engine 的删除端点；pothos 仍然只单向读 engine，不拿任何 engine 的写密钥。用户要先看得见才能删，「列出我的记忆 / 事实」同样是 engine 的端点、产品层的页面，与 pothos 无关。

### 0.1 成功标准

1. engine 删掉一条分类记忆或一条事实后，一轮 sweep 内：对应镜像行消失；只靠它立着的 autobio 条目退役，`GET /v1/autobio` 不再返回；提到该用户的判定全部删除，候选里的 `chemistry` 回到 `null`，直到按新版 autobio 重判（测试钉住）。
2. 还有别的来源的条目原样保留，版本不动。
3. engine 没有逐行删除流（404）时该步骤跳过，其余步骤照常；整号墓碑流的行为不变。
4. 流里出现 pothos 不认识的 `kind` 时跳过那一行、游标照推，不报错。

### 0.2 反目标

- pothos 转发或代理删除请求、持有 engine 的写密钥。
- 按内容拦截「被删过的事」：用户之后又说了一遍，engine 重新记下的是一行新 id 的素材，autobio 可以据此重新写出来。被删的是那一行，不是这件事的谈论资格。要永久不写回，用的是本人删条目（`DELETE /v1/autobio/.../entries/...`，v0.3 起就有）。
- 为了清干净去重写还有其他来源的条目。

### 0.3 约束

沿用 v0.4 §0.3：只经 engine 的 HTTP 出口单向读，不跨 schema，不依赖任何 `eros_engine_*` crate；公开仓纪律。

---

## 1. 输入：engine 契约（v2.6.4 起已发布）

本版向 engine 提的契约（[eros-engine #504](https://github.com/etherfunlab/eros-engine/issues/504)）随 v2.6.4 发布。

### 1.1 逐行删除流

```
GET /svc/v1/rows/deletions?since=<rfc3339>&after_id=<uuid>&limit=<n>
```

- 响应 `{deletions: [{kind, id, deleted_at}], next}`。`kind` 是 `memory`（`/svc/v1/memories/changes` 导出的行）或 `fact`（`/svc/v1/facts/changes` 导出的行），其他 `kind` 跳过；`id` 是那条 changes 流里的同一个 id。只有 id，没有内容，也没有 `user_id`。
- keyset `(deleted_at, id)`，`next` 语义同其他流。`deleted_at` 是删除事务的开始时间，续拉时照其他流回退几秒。
- 两把读密钥任一把都能开；pothos 用必有的 insights secret（`x-insights-export-secret`）。engine 两把读密钥都没设时这条路由不挂载（404）。
- 被删的 id 不会再出现在 changes 流里；同一件事被重新提取时是**新 id**。
- **墓碑的 id 要留着。** engine 的 mirror 须知：删除之前取出的 changes 页可能晚于墓碑到达，写入 changes 页的行之前要先查它的 id 有没有墓碑。
- 整号 erase 不在这条流里，仍然只走现有的墓碑流。
- 随关系硬删除而级联消失的记忆没有墓碑（engine 的已知限制，见 §7）。

### 1.2 engine 那一半（pothos 不依赖）

- 本人列出与删除：`GET` / `DELETE /v2/comp/user/{user_id}/facts` 与 `/memories`，产品层直接调。
- 删掉事实后，engine 在下一次重建 sweep（默认五分钟一轮）重建画像；账本撑不起任何画像时画像被清空，以空字段、更新的 `updated_at` 出现在 insights 流里，pothos 的 insights 步骤照常处理，不需要改动。
- 关系层的 episode / arc / journal 与原话行不可逐行读删，也不导出。

### 1.3 最低 engine 版本

README 的 pin 从 v2.4.0 升到 **v2.6.4**。

---

## 2. 存储：迁移 `0005`

```sql
-- 逐行删除流的 keyset 位置。一行。
CREATE TABLE pothos.row_deletion_cursor (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_deleted_at TIMESTAMPTZ,
    last_id         UUID,
    synced_at       TIMESTAMPTZ
);
INSERT INTO pothos.row_deletion_cursor DEFAULT VALUES;

-- 已消费的逐行墓碑。留着 id，changes 步骤写镜像前查它（§3.4）。
-- 没有 user_id：engine 的墓碑不带，整号 erase 也不动这张表。
CREATE TABLE pothos.row_tombstones (
    kind       TEXT NOT NULL,                       -- 'memory' | 'fact'
    id         UUID NOT NULL,
    deleted_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (kind, id)
);

-- 条目因来源被删而退役，单列一个原因，和模型自己撤回的分开。
ALTER TABLE pothos.autobio_entries
    DROP CONSTRAINT autobio_entries_retired_reason_check,
    ADD  CONSTRAINT autobio_entries_retired_reason_check
         CHECK (retired_reason IN ('revised', 'retracted', 'vetoed', 'source_deleted'));
```

新表照 `0001`–`0004` 做 Supabase lockdown。没有新的镜像表，也没有新的来源表：`row_tombstones` 只存 id；三张 `autobio_*_sources` 对镜像行本来就是 `ON DELETE CASCADE`，删镜像行即断链。

---

## 3. sweeper：新步骤

```
① 整号墓碑    照旧
② 逐行删除    新：/svc/v1/rows/deletions → 本地清除
③ insights    照旧
④ memories    照旧
⑤ facts       照旧
⑥ 归概念      照旧
⑦ autobio     照旧
⑧ 判官        照旧
```

放在 changes 之前的理由同墓碑步骤：不让同轮后续步骤处理注定要删的行。位置不影响正确性——本轮 changes 拉到、随后才被删的行，下一轮的 ② 会清掉；晚到的 changes 页由 §3.4 的墓碑检查挡住。

### 3.1 一页一个事务

对页内每一行，按 `kind`：

1. `memory` → 删 `memory_mirror` 里该 id；`fact` → 删 `fact_mirror` 里该 id；其他 `kind` 跳过、不落表。镜像里没有这个 id（从没拉到过、或已被墓碑清掉）时镜像一侧是 no-op。`memory` / `fact` 的墓碑不论镜像里有没有这一行，都写进 `row_tombstones`（已存在则不动）。
2. 删之前记下引用这一行的条目 id（从对应的 sources 表取），删除后级联断链。
3. 页内全部删完后，对第 2 步记下的条目：**仍是当前条目（`retired_at IS NULL`）且三张 sources 表里一条来源都不剩的**，置 `retired_at = now()`、`retired_reason = 'source_deleted'`。
4. 第 3 步退役了条目的每个用户：删掉 `pair_verdicts` 里提到他的行（`user_lo` 或 `user_hi`），照墓碑步骤的那条语句。`judge_queue` 不动。
5. 推游标，提交。

要点：

- 第 4 步删判定而不靠版本失效，两个理由：判定的 `rationale` 引用了双方 autobio 的原文，留着就是被删内容的副本；而且退役的若是该用户最后一条当前条目，他不再有 autobio 版本，candidates 不会再让他的配对入队，旧档位会永久挂着。没有条目退役的用户，判定不动。

- 第 3 步只看「本页断过链的条目」，不扫全表。
- 「来源都不剩」要数三张表：`autobio_fact_sources`、`autobio_memory_sources`、`autobio_observed_sources`（v0.3 时代的旧来源仍算来源）。
- 已退役的条目（`revised` / `retracted` / `vetoed`）不改：断链即可，原因与时间保持原样。`vetoed` 的「不得写回」继续生效。
- 重拉到同一行是 no-op：镜像行已不在，没有条目可记，游标照推。

### 3.2 对下游的效果，全部靠已有机制

- **本人读**：`GET /v1/autobio` 只列当前条目，退役即消失。
- **判官**：判定已在 §3.1 第 4 步删掉，候选的 `chemistry` 为 `null`。用户还有当前条目时，下一次 candidates 请求照 v0.3 的规则把 top K 里缺判定的配对入队（需要配了 `LLM_*`），按新版本重判。最后一条当前条目退役后 `has_current` 为假，该用户不再参与判官，`chemistry` 保持 `null`，直到他重新有条目。
- **autobio 步骤**：删除不是新素材，不推 `recorded_at`、不触发更新。下一次因新素材更新时，prompt 里的当前条目与素材自然不含已删的行；`source_deleted` 的条目不进 `deleted_by_owner` 清单。
- **重建**（README 的 rebuild 说明）：从 `fact_mirror + memory_mirror` 全量重写，自然不含已删的行。

### 3.3 失败

照墓碑步骤的先例：

| 情况 | 行为 |
|---|---|
| 流 404（engine 没有这条流） | 记 debug 日志跳过，游标不动 |
| 其他失败 | 游标停在最后落库的页，下一轮重试 |
| `next` 不前进 | 记 warn，结束本轮 |

这一步不加开关：sweeper 开着它就跑，镜像方跟随上游删除是义务不是功能。

### 3.4 changes 步骤的墓碑检查

memories 与 facts 两个步骤写镜像时，跳过 `row_tombstones` 里已有的 `(kind, id)`（upsert 语句里加一个 `NOT EXISTS`）。被跳过的行不算落库行数，游标照常前进。这是 engine 对 mirror 的要求（§1.1）；pothos 的 sweeper 单实例顺序执行，正常情况下碰不到，检查是为晚到的页兜底。

---

## 4. API、配置

- API 不变。OpenAPI 文档无新增路径。
- 不加配置项、不加密钥：逐行删除流用现有的 `ENGINE_EXPORT_SECRET`。

---

## 5. 测试

TDD，`#[sqlx::test]` 真实 Postgres，engine 用 wiremock。要钉住的：

- **迁移 `0005`**：游标表、墓碑表与 lockdown；`source_deleted` 可写入；0004 时代的条目与来源原样存活。
- **流**：keyset 翻页、overlap 幂等、游标与页同事务；404 跳过且不影响后续步骤；未知 `kind` 跳过且游标照推。
- **清除**：`memory` 与 `fact` 各删对镜像行；镜像里没有的 id 镜像侧 no-op、墓碑照样落表；未知 `kind` 不落表。
- **墓碑检查**：memories / facts 的 changes 页里带着已有墓碑的 id 时不写镜像，同页其他行照写，游标照推。
- **条目**：唯一来源被删 → 退役为 `source_deleted`、`GET /v1/autobio` 不再返回、版本前移；还有来源（含只剩 `o:` 旧来源）→ 保留、版本不动；一页里先后删掉同一条目的两个来源 → 退役一次；已退役条目的原因与时间不变；`vetoed` 语句仍在 `deleted_by_owner` 里。
- **判官**：条目退役的用户，提到他的判定全部删除，候选的 `chemistry` 为 `null`；他还有当前条目时 candidates 请求把配对重新入队；最后一条条目退役的用户不入队。只断链、没有条目退役的用户，判定原样保留。
- **与墓碑并存**：同一用户先逐行删、再整号墓碑，两步都幂等。

---

## 6. 交付

- 版本 `0.5.0`。
- README：输入一节加逐行删除流；Autobio 一节写明来源被删后条目的去向；Data lifecycle 加一段「单条删除」——产品层调 engine 删，pothos 一轮 sweep 内跟上，以及 §7 的几处残留；engine pin 升到 **v2.6.4**。
- 开发与测试只打 wiremock。

---

## 7. 已知缺口（接受）

- **短语账本追不到事实。** `observed_phrases` 存的是 insights 数组里出现过的每个短语，软打分用「曾经有过的全部概念」。事实被删、engine 重建画像后，对应短语不再是 current（deal-breakers 立即不再用它），但留在账本里继续参与软打分，也可能作为 `shared_interests` / `shared_traits` 的概念标签出现在候选里。短语与事实之间没有 id 关联，逐行清不了；整号 erase 会清。
- **退役条目的文字留着。** 因来源被删而退役的条目、以及历史链上更早的版本，语句文本仍在 `autobio_entries` 里，不经任何 API 返回，也不进任何 prompt；整号 erase 或 `DELETE /v1/profiles` 时清掉。
- **没有墓碑的删除。** 记忆随关系硬删除级联消失时 engine 不发墓碑，pothos 的镜像行与引用它的条目留到整号 erase。
- **混合来源的条目不重写。** 条目引用了被删的行和别的行时原样保留，语句里可能还带着被删那一行贡献的措辞；下一次模型 revise 它时才会按剩余来源改写。这种情况下判定也不删，`rationale` 里引用的仍是这条语句。

---

## 8. 范围外

| 项 | 去向 |
|---|---|
| autobio 改写质量、条目粒度、章节、判官输入 | v0.4 部署后读样本再议 |
| 概念归并（engine 记忆表） | 等读数；engine 立项 |
| 关系层产物的读与删 | engine 与产品层；不导出，pothos 无关 |
| 短语账本的逐行清除 | 需要短语↔事实的关联，engine 不导出；见 §7 |
| 删除端点、列表端点、用户页面 | engine 与产品层 |

---

## 9. 交接（留给实现者）

- 一页内删除与退役的语句组织（逐行还是按 `kind` 批量、CTE 还是多条）。
- 游标表、墓碑表、`source_deleted` 的最终命名；约束名以迁移实际生成的为准。
- README 英文措辞。
