//! 经 luban 转发的回复里 thinking 签名 / `redacted_thinking` 密文的**签发者**登记（`thinking_blocks`）。
//!
//! 上游以「签名 / 密文无效」拒绝历史思考块时，拿请求里那些块的指纹查签发者
//! （[`CredentialStore::thinking_block_issuers`]）：签发自别的号即会话中途换过号，该降级重试救活；
//! 全部签发自当前号则是客户端自己改坏了，该原样回上游 400（API Key 直连也是 400）。只凭会话键
//! 找换号痕迹不可靠：不带会话 id 的来访键是缓存前缀指纹，system 里的时间一变键就变，新键下
//! 查不到旧号，真实的换号恢复会被误拦。
//!
//! 签发者在回复落地时登记（[`crate::proxy::UsageSniffer`] 嗅出签名与密文，`ReqLog::drop` 写入），
//! 保留 [`THINKING_BLOCK_RETENTION_SECS`]，由后台任务按小时清。
//!
//! `cred_id` 为 `None` 是**外来**：签发者不是现有任何一个号——降级重试救活过的那段历史就这么记
//! （上游已证明它在当前号上验不过，而降级只改出站体，客户端下一轮还会原样带着它来，不记下来
//! 下一轮就会因为「当前号在此会话应答过」被判成改坏而拦下）。外键级联到 `credentials`：号删了
//! 登记一并没了，后台异步登记撞上刚删的号会被拒掉，不会把旧 id 写回去。

use super::CredentialStore;
use anyhow::Result;

/// 一批指纹的签发者查询结果：命中了哪些号，以及**覆盖情况**——「查不到」与「确认是本号签发」
/// 是两回事，只有全部指纹都有记录且都属于当前号，才能据此关掉降级恢复。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ThinkingIssuers {
    /// 命中记录的签发者（去重）；`None` 是外来（见模块说明）。
    pub issuers: Vec<Option<i64>>,
    /// 有记录的指纹数（按去重后的指纹算）。
    pub known: usize,
    /// 查询的指纹数（去重后）。
    pub total: usize,
    /// 有记录的那些指纹（排好序）：救活时只有**不在**这里面的才记成外来、进救活缓存——已明确
    /// 登记为本号签发的合法块不是「救活过的历史」，不能给之后新出现的坏签名当重试凭据。
    pub known_fps: Vec<String>,
}

impl ThinkingIssuers {
    /// 有别的号（或外来）签发的块——会话换过号。
    pub fn any_other(&self, cred_id: i64) -> bool {
        self.issuers.iter().any(|&id| id != Some(cred_id))
    }

    /// 全部指纹都有记录、且都是当前号签发的：历史没问题，验不过只能是客户端改坏了。
    /// 调用方还得确认指纹列表没被截断（[`crate::proxy::HistoryFingerprints::truncated`]）。
    pub fn all_here(&self, cred_id: i64) -> bool {
        self.total > 0
            && self.known == self.total
            && self.issuers.iter().all(|&id| id == Some(cred_id))
    }
}

/// 签发者记录保留 90 天：比流水的 30 天长，会话中途换号的恢复靠它。
pub const THINKING_BLOCK_RETENTION_SECS: i64 = 90 * 24 * 3600;

/// 一条回复 / 一条请求里最多登记或查询这么多块：再多的只取靠后的（最近的历史最要紧）。
pub const THINKING_BLOCK_CAP: usize = 256;

/// 「救活过的历史指纹」缓存的寿命（[`CredentialStore::mark_history_recovered`]）：三小时没再救活就
/// 忘掉，与会话链的闲置口径一致；重启后靠落库的外来登记。
pub const RECOVERED_HISTORY_TTL: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);

/// thinking 签名或 `redacted_thinking` 密文的指纹：SHA-256 前 32 个十六进制字符（128 位）。
/// 原串是上千字节的 base64，不存原文。
pub fn thinking_block_fingerprint(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

impl CredentialStore {
    /// 登记一批指纹由 `cred_id` 签发（`None` = 外来）；已有的不改（同一指纹只认首次签发者）。
    ///
    /// 号已经不在了就不写：先查一遍 `credentials` 兜住常规情形，外键兜住查完到写入之间号被删的
    /// 竞态（外键报错按「没写」处理，不往上抛——这是后台任务，号都没了登记也没意义）。
    pub async fn remember_thinking_blocks(
        &self,
        cred_id: Option<i64>,
        fps: &[String],
    ) -> Result<()> {
        if fps.is_empty() {
            return Ok(());
        }
        let res = sqlx::query(
            "INSERT INTO thinking_blocks (fp, cred_id, ts) \
             SELECT unnest($1::text[]), $2, unixepoch() \
              WHERE $2::bigint IS NULL OR EXISTS (SELECT 1 FROM credentials WHERE id = $2) \
             ON CONFLICT (fp) DO NOTHING",
        )
        .bind(fps)
        .bind(cred_id)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            // 23503 = foreign_key_violation：号在查与写之间被删了。
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23503") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// 这批指纹的签发者与覆盖情况（[`ThinkingIssuers`]）。没登记过的指纹——功能上线前的历史、
    /// 超过保留期、或签名根本不是经 luban 发出的——算进 `total` 不算进 `known`。
    pub async fn thinking_block_issuers(&self, fps: &[String]) -> Result<ThinkingIssuers> {
        let mut uniq: Vec<&str> = fps.iter().map(String::as_str).collect();
        uniq.sort_unstable();
        uniq.dedup();
        if uniq.is_empty() {
            return Ok(ThinkingIssuers::default());
        }
        let rows: Vec<(String, Option<i64>)> =
            sqlx::query_as("SELECT fp, cred_id FROM thinking_blocks WHERE fp = ANY($1)")
                .bind(&uniq)
                .fetch_all(&self.pool)
                .await?;
        let mut issuers: Vec<Option<i64>> = rows.iter().map(|(_, id)| *id).collect();
        issuers.sort_unstable();
        issuers.dedup();
        let mut known_fps: Vec<String> = rows.iter().map(|(fp, _)| fp.clone()).collect();
        known_fps.sort_unstable();
        Ok(ThinkingIssuers { issuers, known: rows.len(), total: uniq.len(), known_fps })
    }

    /// 记下刚降级重试救活过的那段历史的**指纹**（含上游点名的那块；进程内、同步可见）：救活成功
    /// 的回复发出去**之前**就记，客户端紧接着的下一轮带着同一段历史来，不会撞上「外来登记还没
    /// 落库、而本号的成功流水已可见」的窗口被判成改坏。按指纹而不是按会话：会话里之后新出现的、
    /// 不在这段历史里的未知签名不能借光放行。条目活 [`RECOVERED_HISTORY_TTL`]。顺手清掉过期条目
    /// （只在救活时写，量很小）。
    pub fn mark_history_recovered(&self, fps: &[String]) {
        let now = std::time::Instant::now();
        let mut map = self.recovered_history.lock();
        map.retain(|_, t| now.duration_since(*t) < RECOVERED_HISTORY_TTL);
        for fp in fps {
            map.insert(fp.clone(), now);
        }
    }

    /// 这批指纹里有没有近期（[`RECOVERED_HISTORY_TTL`] 内）救活过的。
    pub fn history_recovered_recently(&self, fps: &[String]) -> bool {
        let now = std::time::Instant::now();
        let map = self.recovered_history.lock();
        fps.iter()
            .any(|fp| map.get(fp).is_some_and(|t| now.duration_since(*t) < RECOVERED_HISTORY_TTL))
    }

    /// 清空救活缓存：凭证被清空 / 删除时调用，旧状态不该再影响重新导入后的号。
    pub(super) fn forget_recovered_history(&self) {
        self.recovered_history.lock().clear();
    }

    /// 裁掉超过保留期（[`THINKING_BLOCK_RETENTION_SECS`]）的登记，返回删除条数。
    pub async fn prune_thinking_blocks(&self) -> Result<usize> {
        let n = sqlx::query("DELETE FROM thinking_blocks WHERE ts < unixepoch() - $1")
            .bind(THINKING_BLOCK_RETENTION_SECS)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::super::select::tests::soft_store;
    use super::*;
    use sqlx::PgPool;

    /// 同一指纹只认首次签发者；查询给出覆盖情况（部分未知不算「全是本号」）；外来（NULL）算
    /// 「别的号」；不存在的号不写；过期的会被清掉。
    #[sqlx::test]
    async fn issuers_report_coverage_first_writer_wins_and_are_pruned(pool: PgPool) {
        let (store, ids) = soft_store(pool.clone(), &["a", "b"]).await;
        let (ia, ib) = (ids[0], ids[1]);
        let a = thinking_block_fingerprint("sigA");
        let b = thinking_block_fingerprint("sigB");
        let f = thinking_block_fingerprint("foreign");
        let unknown = thinking_block_fingerprint("nope");
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        store.remember_thinking_blocks(Some(ia), &[a.clone(), b.clone()]).await.unwrap();
        store.remember_thinking_blocks(Some(ib), std::slice::from_ref(&a)).await.unwrap();
        store.remember_thinking_blocks(None, std::slice::from_ref(&f)).await.unwrap();
        store
            .remember_thinking_blocks(Some(999_999), std::slice::from_ref(&unknown))
            .await
            .unwrap();

        let got = store.thinking_block_issuers(&[a.clone(), b.clone(), a.clone()]).await.unwrap();
        let mut ab = vec![a.clone(), b.clone()];
        ab.sort();
        assert_eq!(
            got,
            ThinkingIssuers { issuers: vec![Some(ia)], known: 2, total: 2, known_fps: ab },
            "a 仍记在 a 名下"
        );
        assert!(got.all_here(ia));
        assert!(!got.any_other(ia));
        assert!(got.any_other(ib));

        let partial = store.thinking_block_issuers(&[a.clone(), unknown.clone()]).await.unwrap();
        assert_eq!(
            partial,
            ThinkingIssuers {
                issuers: vec![Some(ia)],
                known: 1,
                total: 2,
                known_fps: vec![a.clone()]
            }
        );
        assert!(!partial.all_here(ia), "有一个认不出就不能说全是本号的；不存在的号也没写进去");

        let foreign = store.thinking_block_issuers(&[a.clone(), f.clone()]).await.unwrap();
        assert_eq!(foreign.issuers, vec![None, Some(ia)]);
        assert!(foreign.any_other(ia), "外来算别的号");
        assert!(!foreign.all_here(ia));

        assert_eq!(store.thinking_block_issuers(&[]).await.unwrap(), ThinkingIssuers::default());
        let none = store.thinking_block_issuers(std::slice::from_ref(&unknown)).await.unwrap();
        assert_eq!(
            none,
            ThinkingIssuers { issuers: vec![], known: 0, total: 1, known_fps: vec![] }
        );

        sqlx::query("UPDATE thinking_blocks SET ts = unixepoch() - $1 WHERE fp = $2")
            .bind(THINKING_BLOCK_RETENTION_SECS + 60)
            .bind(&a)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(store.prune_thinking_blocks().await.unwrap(), 1);
        let got = store.thinking_block_issuers(&[a, b.clone()]).await.unwrap();
        assert_eq!(
            got,
            ThinkingIssuers { issuers: vec![Some(ia)], known: 1, total: 2, known_fps: vec![b] },
            "只剩 b"
        );
    }

    /// 救活缓存写了立刻可见、按指纹区分（一批里有一个命中即算）；没救活过的指纹查不到；
    /// 清空凭证时缓存一并清掉。
    #[sqlx::test]
    async fn recovery_cache_is_immediate_per_fingerprint_and_cleared(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = thinking_block_fingerprint("recovered");
        let b = thinking_block_fingerprint("never");
        assert!(!store.history_recovered_recently(std::slice::from_ref(&a)));
        store.mark_history_recovered(std::slice::from_ref(&a));
        assert!(store.history_recovered_recently(std::slice::from_ref(&a)));
        assert!(store.history_recovered_recently(&[b.clone(), a.clone()]), "一批里有一个命中");
        assert!(!store.history_recovered_recently(std::slice::from_ref(&b)), "新签名不借光");
        store.clear().await.unwrap();
        assert!(!store.history_recovered_recently(std::slice::from_ref(&a)), "清空后缓存没了");
    }

    /// 号删了它的登记随之没了（外键级联），外来的留着；清空全部凭证时登记整表清掉。重新导入
    /// 拿到新 id 后，旧指纹查不到、不会被认成「别的号」。
    #[sqlx::test]
    async fn registrations_follow_credential_deletion(pool: PgPool) {
        let (store, ids) = soft_store(pool.clone(), &["a", "b"]).await;
        let (ia, ib) = (ids[0], ids[1]);
        let a = thinking_block_fingerprint("sigA");
        let b = thinking_block_fingerprint("sigB");
        let f = thinking_block_fingerprint("foreign");
        store.remember_thinking_blocks(Some(ia), std::slice::from_ref(&a)).await.unwrap();
        store.remember_thinking_blocks(Some(ib), std::slice::from_ref(&b)).await.unwrap();
        store.remember_thinking_blocks(None, std::slice::from_ref(&f)).await.unwrap();

        assert!(store.delete(ia).await.unwrap());
        let got = store.thinking_block_issuers(&[a.clone(), b.clone(), f.clone()]).await.unwrap();
        assert_eq!(got.known, 2, "a 的登记随号删掉，b 与外来还在");
        assert!(!got.issuers.contains(&Some(ia)));
        // 删号之后迟到的后台登记写不进去。
        store.remember_thinking_blocks(Some(ia), std::slice::from_ref(&a)).await.unwrap();
        assert_eq!(store.thinking_block_issuers(std::slice::from_ref(&a)).await.unwrap().known, 0);

        store.clear().await.unwrap();
        let got = store.thinking_block_issuers(&[a, b, f]).await.unwrap();
        assert_eq!(got.known, 0, "清空凭证时登记整表清掉");
    }
}
