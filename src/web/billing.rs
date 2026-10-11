//! 分层账单：只计费、不扣费，看「谁的号在什么时候、用什么模型、经哪把 Key 和哪个分组
//! 跑出了多少费用」。数据见 `store::billing`。
//!
//! 可见范围按身份收窄（handler 里判，中间件只放行登录的人）：
//! - **用户**：只看自己的号，按号 / 模型 / 分组 / 日拆；
//! - **代理**：默认看自己与名下每个用户的合计，按人 / 号 / 模型 / 分组 / 日拆；也可以只看
//!   自己或某一个下属，拆法同上（不再按人拆）。代理在账号池里本来就看得到下属的号（只读），
//!   费用与它同口径；
//! - **admin 与访客**：全部，按人 / 号 / 模型 / 接入 Key / 分组 / 日拆，可按任意一维筛选。
//!
//! 接入 Key 是 admin 的东西，按 Key 拆与按 Key 筛只给 admin 与访客。

use super::*;

/// 账单最长查多少天。
const MAX_RANGE_SECS: i64 = 400 * 86400;

#[derive(Deserialize)]
pub(super) struct BillingQuery {
    /// 起止（Unix 秒，`[from, to)`）。缺省为最近 7 天。
    #[serde(default)]
    from: Option<i64>,
    #[serde(default)]
    to: Option<i64>,
    by: store::BillingDim,
    #[serde(default)]
    owner_id: Option<i64>,
    #[serde(default)]
    cred_id: Option<i64>,
    #[serde(default)]
    key_id: Option<i64>,
    #[serde(default)]
    group_id: Option<i64>,
    #[serde(default)]
    model: Option<String>,
    /// 按日拆时的时区偏移（秒，东为正），缺省 0。
    #[serde(default)]
    tz_offset_secs: i64,
}

#[derive(Serialize)]
pub(super) struct BillingItem {
    #[serde(flatten)]
    row: store::BillingRow,
    /// 这一行的显示名：号主用户名、号名、Key 名、分组名；模型与日期由前端按 `key` 显示。
    /// 已删的号 / 账号 / Key / 分组为 null。
    label: Option<String>,
}

#[derive(Serialize)]
pub(super) struct BillingResp {
    from: i64,
    to: i64,
    rows: Vec<BillingItem>,
    total: store::BillingRow,
}

fn forbidden_view() -> ApiError {
    (StatusCode::FORBIDDEN, "this breakdown is not available to your account".into())
}

/// 按身份算出这次能看哪些号主，并核对拆分维度与筛选是否允许。
async fn scope_owners(
    state: &AppState,
    actor: &Actor,
    q: &BillingQuery,
) -> Result<Option<Vec<i64>>, ApiError> {
    use store::BillingDim as D;
    match actor.role {
        UserRole::Admin | UserRole::Viewer => Ok(q.owner_id.map(|o| vec![o])),
        UserRole::User => {
            if q.owner_id.is_some_and(|o| o != actor.id) {
                return Err(not_found());
            }
            if matches!(q.by, D::Owner | D::Key) || q.key_id.is_some() {
                return Err(forbidden_view());
            }
            Ok(Some(vec![actor.id]))
        }
        UserRole::Agent => {
            if matches!(q.by, D::Key) || q.key_id.is_some() {
                return Err(forbidden_view());
            }
            let children = state.store.child_user_ids(actor.id).await.map_err(internal)?;
            // 号、分组、模型的筛选与号主范围是「且」的关系，筛到范围外的号只会是空结果。
            match q.owner_id {
                None => {
                    let mut owners = children;
                    owners.push(actor.id);
                    Ok(Some(owners))
                }
                Some(o) if o == actor.id || children.contains(&o) => {
                    if matches!(q.by, D::Owner) {
                        return Err(forbidden_view());
                    }
                    Ok(Some(vec![o]))
                }
                Some(_) => Err(not_found()),
            }
        }
    }
}

/// 分层账单。
pub(super) async fn get_billing(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<BillingQuery>,
) -> Result<Json<BillingResp>, ApiError> {
    let now = chrono::Utc::now().timestamp();
    let to = q.to.unwrap_or(now + 3600);
    let from = q.from.unwrap_or(to - 7 * 86400).max(to - MAX_RANGE_SECS);
    if from >= to {
        return Err(bad_request("the start of the range must be before its end"));
    }
    let owners = scope_owners(&state, &actor, &q).await?;
    let filter = store::BillingFilter {
        // 起点向下对齐到小时桶：桶记的是整点，`[from, to)` 落在桶中间时把那一小时算进来。
        since: from.div_euclid(3600) * 3600,
        until: to,
        owners,
        cred_id: q.cred_id,
        key_id: q.key_id,
        group_id: q.group_id,
        model: q.model.clone().filter(|m| !m.is_empty()),
        tz_offset_secs: q.tz_offset_secs.clamp(-14 * 3600, 14 * 3600),
    };
    let by = q.by;
    let rows = state.store.billing_breakdown(&filter, by).await.map_err(internal)?;
    let labels: std::collections::HashMap<String, String> = match by {
        store::BillingDim::Owner => state
            .store
            .owner_names()
            .await
            .map_err(internal)?
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        store::BillingDim::Cred => state
            .store
            .credential_labels()
            .await
            .map_err(internal)?
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        store::BillingDim::Key => state
            .store
            .list_api_keys()
            .await
            .map_err(internal)?
            .into_iter()
            .map(|k| (k.id.to_string(), k.label))
            .collect(),
        store::BillingDim::Group => state
            .store
            .list_groups()
            .await
            .map_err(internal)?
            .into_iter()
            .map(|g| (g.id.to_string(), g.name))
            .collect(),
        store::BillingDim::Model | store::BillingDim::Day => Default::default(),
    };
    let mut total = store::BillingRow::default();
    let rows = rows
        .into_iter()
        .map(|row| {
            total.requests += row.requests;
            total.input_tokens += row.input_tokens;
            total.output_tokens += row.output_tokens;
            total.cache_write_tokens += row.cache_write_tokens;
            total.cache_read_tokens += row.cache_read_tokens;
            total.cost_usd += row.cost_usd;
            BillingItem { label: labels.get(&row.key).cloned(), row }
        })
        .collect();
    Ok(Json(BillingResp { from, to, rows, total }))
}
