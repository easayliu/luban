//! 跨模块的测试：一条用例同时用到好几个模块的方法（上号、分组、选号、流水、账单……），
//! 放在哪个模块里都不合适，集中在这里。

#![allow(deprecated)]

use sqlx::PgPool;

use super::super::*;
use super::PgStore;

fn billing_rec(cred: i64, key: Option<i64>, model: &str, cost: f64) -> UsageRecord {
    UsageRecord {
        cred_id: Some(cred),
        key_id: key,
        model: Some(model.into()),
        path: "/v1/messages".into(),
        status: 200,
        has_usage: true,
        input_tokens: Some(10),
        output_tokens: Some(5),
        cache_creation_tokens: Some(3),
        cache_read_tokens: Some(2),
        cost_usd: Some(cost),
        ..Default::default()
    }
}

/// 加解密往返；没有前缀的按明文原样读出；每次密文都不同。
#[test]
fn sealed_values_round_trip_and_plaintext_passes_through() {
    let a = seal("sk-ant-ort01-abc");
    let b = seal("sk-ant-ort01-abc");
    assert!(a.starts_with(SEALED_PREFIX) && a != b, "随机 nonce，两次密文不同");
    assert_eq!(open(&a).unwrap(), "sk-ant-ort01-abc");
    assert_eq!(open("plain-token").unwrap(), "plain-token");
    assert!(open("enc1:not-base64!").is_err());
}

/// 费用汇总：号主在写入那一刻定死（号后来换了主人，历史不跟着走）；分组按 Key 的顺序取第一个
/// 含这个号的，没绑分组的 Key 取号所在 id 最小的分组；Key 为空记 0。
#[sqlx::test]
async fn billing_attribution_is_fixed_at_write_time(pool: PgPool) {
    let store = PgStore::for_test(pool).await;
    let admin = store.admin_user().await.unwrap().id;
    let u = store.create_user("u", "", UserRole::User, admin).await.unwrap().unwrap().id;
    let default = store.default_group_id().await.unwrap();
    let g1 = store.create_group("g1", "").await.unwrap().unwrap();
    let g2 = store.create_group("g2", "").await.unwrap().unwrap();
    let cred = store.insert("a", None, "t", "r", 0, None, None, u).await.unwrap().id;
    store.set_credential_groups(&[cred], &[g1, g2]).await.unwrap().unwrap();
    let key = store.create_api_key("k", "key-b", &[g2, g1]).await.unwrap().unwrap();
    let t0 = 1_800_000_000;
    store.insert_usage_log_at(&billing_rec(cred, Some(key), "m1", 1.5), Some(t0)).await.unwrap();
    store.insert_usage_log_at(&billing_rec(cred, None, "m2", 0.5), Some(t0 + 10)).await.unwrap();
    // 号转给 admin 之后的费用记到 admin 名下，之前的仍归 u。
    sqlx::query("UPDATE credentials SET owner_id = $1 WHERE id = $2")
        .bind(admin)
        .bind(cred)
        .execute(&store.pool)
        .await
        .unwrap();
    store.insert_usage_log_at(&billing_rec(cred, None, "m2", 2.0), Some(t0 + 20)).await.unwrap();

    let all = BillingFilter { since: t0 - 3600, until: t0 + 3600, ..Default::default() };
    let by = async |dim: BillingDim, f: &BillingFilter| -> Vec<(String, f64, i64)> {
        store
            .billing_breakdown(f, dim)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.key, r.cost_usd, r.requests))
            .collect()
    };
    assert_eq!(
        by(BillingDim::Owner, &all).await,
        vec![(admin.to_string(), 2.0, 1), (u.to_string(), 2.0, 2)]
    );
    // 经 Key 来的那条算进 Key 顺序里第一个含这个号的 g2；没 Key 的取 id 最小的 g1。
    let groups = by(BillingDim::Group, &all).await;
    assert_eq!(groups, vec![(g1.to_string(), 2.5, 2), (g2.to_string(), 1.5, 1)]);
    assert!(!groups.iter().any(|(k, _, _)| *k == default.to_string()));
    assert_eq!(
        by(BillingDim::Key, &all).await,
        vec![("0".into(), 2.5, 2), (key.to_string(), 1.5, 1)]
    );
    let only_u = BillingFilter { owners: Some(vec![u]), ..all.clone() };
    assert_eq!(
        by(BillingDim::Model, &only_u).await,
        vec![("m1".into(), 1.5, 1), ("m2".into(), 0.5, 1)]
    );
    let row = &store.billing_breakdown(&only_u, BillingDim::Cred).await.unwrap()[0];
    assert_eq!(
        (row.input_tokens, row.output_tokens, row.cache_write_tokens, row.cache_read_tokens),
        (20, 10, 6, 4)
    );
}

/// 按日拆：以给定时区的本地零点切日界。
#[sqlx::test]
async fn billing_days_follow_the_timezone(pool: PgPool) {
    let store = PgStore::for_test(pool).await;
    let admin = store.admin_user().await.unwrap().id;
    let cred = store.insert("a", None, "t", "r", 0, None, None, admin).await.unwrap().id;
    // UTC 2027-01-14 23:30 = 东八区 2027-01-15 07:30。
    let ts = 1_800_005_400 - (1_800_005_400 % 86400) + 23 * 3600 + 1800;
    store.insert_usage_log_at(&billing_rec(cred, None, "m", 1.0), Some(ts)).await.unwrap();
    let f = |tz| BillingFilter {
        since: ts - 86400,
        until: ts + 86400,
        tz_offset_secs: tz,
        ..Default::default()
    };
    let day = async |tz| {
        store.billing_breakdown(&f(tz), BillingDim::Day).await.unwrap()[0]
            .key
            .parse::<i64>()
            .unwrap()
    };
    let utc_day = ts - ts % 86400;
    assert_eq!(day(0).await, utc_day);
    assert_eq!(day(8 * 3600).await, utc_day + 86400 - 8 * 3600, "东八区已是第二天");
}

/// 按号主筛流水：只出本人名下号的记录。
#[sqlx::test]
async fn usage_logs_filter_by_owner(pool: PgPool) {
    let store = PgStore::for_test(pool).await;
    let admin = store.admin_user().await.unwrap().id;
    let user = store.create_user("u", "", UserRole::User, admin).await.unwrap().unwrap().id;
    let a = store.insert("a", None, "ta", "ra", 0, None, None, admin).await.unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, user).await.unwrap().id;
    sqlx::query(
        "INSERT INTO usage_logs (cred_id, path) VALUES ($1, '/v1/messages'), ($2, '/v1/messages')",
    )
    .bind(a)
    .bind(b)
    .execute(&store.pool)
    .await
    .unwrap();
    let q = UsageLogQuery { limit: 10, owner_id: Some(user), ..Default::default() };
    let logs = store.query_usage_logs(q.clone()).await.unwrap();
    assert_eq!(logs.iter().map(|l| l.cred_id).collect::<Vec<_>>(), vec![Some(b)]);
    assert_eq!(store.usage_log_stats(q).await.unwrap().total, 1);
}

/// 号主被删之后才落库的上号请求插不进来；号主不存在的号（直接写库造出来的）不进调度；
/// 不带 owner 插入的号默认挂到 admin 名下、照常调度。
#[sqlx::test]
async fn credentials_need_a_living_owner_to_be_inserted_and_scheduled(pool: PgPool) {
    let store = PgStore::for_test(pool).await;
    let admin = store.admin_user().await.unwrap().id;
    let gone = store.create_user("gone", "", UserRole::User, admin).await.unwrap().unwrap().id;
    store.delete_user(gone, None).await.unwrap().unwrap();
    let err = store.insert("x", None, "t", "r-gone", u64::MAX, None, None, gone).await.unwrap_err();
    assert!(err.downcast_ref::<OwnerGone>().is_some());

    sqlx::query(
        "INSERT INTO credentials (access_token, refresh_token, expires_at, owner_id) \
         VALUES ('t1', 'r-orphan', 9999999999, $1)",
    )
    .bind(gone)
    .execute(&store.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO credentials (access_token, refresh_token, expires_at) \
         VALUES ('t2', 'r-default', 9999999999)",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    let default_owned =
        store.list().await.unwrap().into_iter().find(|c| c.refresh_token == "r-default").unwrap();
    assert_eq!(default_owned.owner_id, Some(admin));
    let picked = store.select_for_device(Select::default()).await.unwrap();
    assert_eq!(picked.id, default_owned.id, "无主的号不进调度");
}

/// 分组内的暂停与模型判断只看这把 Key 能用的号：本组全部暂停回 429；别的组里暂停的号
/// 不让本组「模型不支持」变成 429。
#[sqlx::test]
async fn paused_and_denied_checks_stay_inside_the_key_groups(pool: PgPool) {
    let store = PgStore::for_test(pool).await;
    let admin = store.admin_user().await.unwrap().id;
    let g1 = store.create_group("g1", "").await.unwrap().unwrap();
    let g2 = store.create_group("g2", "").await.unwrap().unwrap();
    let a = store.insert("a", None, "ta", "ra", u64::MAX, None, None, admin).await.unwrap().id;
    let b = store.insert("b", None, "tb", "rb", u64::MAX, None, None, admin).await.unwrap().id;
    store.set_credential_groups(&[a], &[g1]).await.unwrap().unwrap();
    store.set_credential_groups(&[b], &[g2]).await.unwrap().unwrap();
    let later = crate::credentials::now_secs() + 600;
    store.pause_for_rate_limit(a, "rate limited", later).await.unwrap();
    let only_g1 = [g1];
    let err = store
        .select_for_device(Select { groups: Some(&only_g1), ..Default::default() })
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<AllRateLimited>().is_some(), "本组全部暂停：429，{err:#}");

    store.deny_model(b, "claude-fable-5", "plan", None).await.unwrap();
    let only_g2 = [g2];
    let err = store
        .select_for_device(Select {
            groups: Some(&only_g2),
            model: Some("claude-fable-5"),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(
        err.downcast_ref::<ModelUnsupported>().is_some(),
        "别组暂停的号不该让这里回 429，{err:#}"
    );
}

/// 选号按 Key 绑定的分组：只在这些分组里选，排在前面的分组优先，前面的用不了才溢出；
/// 粘住的号不在这些分组里时改选；分组里一个号都没有时报错。
#[sqlx::test]
async fn selection_honours_key_groups_and_their_order(pool: PgPool) {
    let store = PgStore::for_test(pool).await;
    let admin = store.admin_user().await.unwrap().id;
    let g1 = store.create_group("g1", "").await.unwrap().unwrap();
    let g2 = store.create_group("g2", "").await.unwrap().unwrap();
    let empty = store.create_group("empty", "").await.unwrap().unwrap();
    let far = u64::MAX;
    let a = store.insert("a", None, "ta", "ra", far, None, None, admin).await.unwrap().id;
    let b = store.insert("b", None, "tb", "rb", far, None, None, admin).await.unwrap().id;
    store.set_credential_groups(&[a], &[g1]).await.unwrap().unwrap();
    store.set_credential_groups(&[b], &[g2]).await.unwrap().unwrap();
    let pick = async |groups: &[i64], device: Option<&str>| {
        store
            .select_for_device(Select {
                groups: Some(groups),
                device_id: device,
                ..Default::default()
            })
            .await
            .map(|c| c.id)
    };
    assert_eq!(pick(&[g2, g1], None).await.unwrap(), b, "排在前面的分组优先");
    assert_eq!(pick(&[g1, g2], None).await.unwrap(), a);
    assert_eq!(pick(&[g1], Some("dev-1")).await.unwrap(), a);
    // 同一台设备换一把只绑 g2 的 Key 来：粘住的 a 不在范围里，改选到 b。
    assert_eq!(pick(&[g2], Some("dev-1")).await.unwrap(), b);
    store.set_disabled(b, true).await.unwrap();
    assert_eq!(pick(&[g2, g1], None).await.unwrap(), a, "前面分组的号用不了就溢出到后面的");
    assert!(pick(&[empty], None).await.is_err());
    assert!(pick(&[], None).await.is_err(), "绑定的分组被删光：一个号都不能用，不放开成全部号");
    let all = store.select_for_device(Select { groups: None, ..Default::default() }).await.unwrap();
    assert_eq!(all.id, a, "不限分组（None）用全部号");
}

/// 按范围列号：代理和用户只看到自己名下的，admin 看全部。（`credentials_owned_by` 那半在
/// `pg::users` 的同名测试里。）
#[sqlx::test]
async fn credentials_list_scoped_by_owner(pool: PgPool) {
    let store = PgStore::for_test(pool).await;
    let admin = store.admin_user().await.unwrap().id;
    let user = store.create_user("user", "", UserRole::User, admin).await.unwrap().unwrap().id;
    let a = store.insert("a", None, "ta", "ra", 0, None, None, admin).await.unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, user).await.unwrap().id;
    let ids = async |s: Scope| {
        store.list_scoped(s).await.unwrap().iter().map(|c| c.id).collect::<Vec<_>>()
    };
    assert_eq!(ids(Scope::All).await, vec![a, b]);
    assert_eq!(ids(Scope::Owner(user)).await, vec![b]);
}

/// Key 唯一绑定的分组被删掉之后，这把 Key 一个号都选不到（而不是放开成全部号）。（Key 本身
/// 的断言在 `pg::groups` 的 `deleting_a_keys_only_group_fails_closed` 里。）
#[sqlx::test]
async fn selection_with_a_keys_only_group_deleted_fails_closed(pool: PgPool) {
    let store = PgStore::for_test(pool).await;
    let admin = store.admin_user().await.unwrap().id;
    let g = store.create_group("g", "").await.unwrap().unwrap();
    store.insert("a", None, "t", "r", u64::MAX, None, None, admin).await.unwrap();
    store.create_api_key("bound", "key-bound", &[g]).await.unwrap().unwrap();
    store.delete_group(g).await.unwrap().unwrap();
    let bound = store.api_key_access("key-bound").await.unwrap().unwrap();
    assert!(
        store
            .select_for_device(Select { groups: bound.groups.as_deref(), ..Default::default() })
            .await
            .is_err()
    );
}

/// 缺默认设备上限这一行时判定仍是同一个值——启动时补的那一行不改变行为。
#[sqlx::test]
async fn default_device_limit_falls_back_without_the_row(pool: PgPool) {
    PgStore::for_test(pool.clone()).await;
    sqlx::query("DELETE FROM settings WHERE key = $1")
        .bind(DEFAULT_DEVICE_LIMIT)
        .execute(&pool)
        .await
        .unwrap();
    // 重新打开会把这一行补回来，所以直接拿缓存里删掉它的状态来测。
    let store = PgStore::for_test(pool).await;
    store.settings.write().remove(DEFAULT_DEVICE_LIMIT);
    assert_eq!(store.default_device_limit(), DEFAULT_DEVICE_LIMIT_VALUE);
}
