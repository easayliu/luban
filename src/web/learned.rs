//! 学到的规则（形态拒绝、拒答回放等）的查看与清理，以及模型列表。

use super::*;

/// `GET /api/learned-rejections` 的一条：从上游响应学到的规则，见 [`store::LearnedRejection`]。
#[derive(Serialize)]
pub(super) struct LearnedRejectionView {
    /// `shape`（命中本地拒）、`deprecated`（命中转发前剥掉字段）、`empty_reply`（上游对这类
    /// 回过零输出，命中本地拒）、`refusal`（上游拒答过这条提示词，同一条命中时原样回放上游
    /// 那次的响应，见 [`store::LearnedReply`]）或 `app_refusal`（上游拒答过某个识别不了会话的
    /// 应用，同一模型 + 同一份 system 命中时回放）。
    kind: String,
    model: String,
    field: String,
    /// 形态规则被拒的那个取值；废弃字段规则为空串。
    value: String,
    /// 上游原话。
    message: String,
    /// 学到的时刻（Unix 秒）。
    learned_at: i64,
    /// 到期自动丢弃、重新向上游验证的时刻（Unix 秒），见 [`store::LEARNED_REJECTION_TTL_SECS`]。
    expires_at: i64,
}

pub(super) async fn list_learned_rejections(
    State(state): State<AppState>,
) -> Result<Json<Vec<LearnedRejectionView>>, ApiError> {
    let rows = state.store.learned_rejections_with_time().map_err(internal)?;
    Ok(Json(
        rows.into_iter()
            .rev()
            .map(|(r, learned_at)| LearnedRejectionView {
                kind: r.kind,
                model: r.model,
                field: r.field,
                value: r.value,
                message: r.message,
                learned_at,
                expires_at: learned_at + store::LEARNED_REJECTION_TTL_SECS,
            })
            .collect(),
    ))
}

/// 把回填时挑出来的过期旧行（[`proxy::SeededMemories::stale`]）从库里删掉，逐条打 warn。
/// 启动回填与每小时重建两处共用；删失败只告警，下次再试。
pub(super) fn drop_stale_learned_rules(store: &CredentialStore, stale: &[store::LearnedRejection]) {
    for r in stale {
        match store.forget_learned_rejection(r) {
            Ok(_) => tracing::warn!(
                kind = %r.kind, model = %r.model, field = %r.field, value = %r.value,
                "dropped a stale learned rule written by an older version: a refusal recorded as a request class (v0.3.89), or a refusal rule without the upstream reply to replay (before 0.3.98); it is relearned on the next hit"
            ),
            Err(e) => tracing::warn!(error = %e, "failed to drop a stale learned rule"),
        }
    }
}

/// `POST /api/learned-rejections/delete`：删掉一条学到的规则，库里与进程内一起删。
/// 用 POST 带体而不是 DELETE 带路径：`model` 里有 `[1m]` 这种字符，塞进路径段徒增转义。
#[derive(Deserialize)]
pub(super) struct ForgetLearnedReq {
    kind: String,
    model: String,
    field: String,
    #[serde(default)]
    value: String,
}

pub(super) async fn forget_learned_rejection(
    State(state): State<AppState>,
    Json(req): Json<ForgetLearnedReq>,
) -> Result<Json<Vec<LearnedRejectionView>>, ApiError> {
    let row = store::LearnedRejection {
        kind: req.kind,
        model: req.model,
        field: req.field,
        value: req.value,
        message: String::new(),
        reply: None,
    };
    let in_db = state.store.forget_learned_rejection(&row).map_err(internal)?;
    let in_mem = proxy::forget_learned_memory(
        &state.shape_rejections,
        &state.deprecated_fields,
        &state.empty_replies,
        &row,
    );
    if !in_db && !in_mem {
        return Err(not_found());
    }
    tracing::info!(kind = %row.kind, model = %row.model, field = %row.field, value = %row.value, "learned upstream rejection removed from the console");
    list_learned_rejections(State(state)).await
}

/// `POST /api/learned-rejections/delete-group`：删掉「同一种类、同一模型、同一类别」的一组规则，
/// 库里与进程内一起删，回删后的完整列表。类别是规则文案开头的 `[类别]`（控制台就按它折叠），
/// 不传即该模型该种类的全部。给拒答提示词那一格准备的：一个下游被 `reasoning_extraction` 盯上
/// 几小时就是几百条，按组删才不必连别的模型、别的类别一起清。
#[derive(Deserialize)]
pub(super) struct ForgetLearnedGroupReq {
    kind: String,
    model: String,
    #[serde(default)]
    category: Option<String>,
}

pub(super) async fn forget_learned_group(
    State(state): State<AppState>,
    Json(req): Json<ForgetLearnedGroupReq>,
) -> Result<Json<Vec<LearnedRejectionView>>, ApiError> {
    let category = req.category.as_deref().map(str::trim).filter(|c| !c.is_empty());
    let in_category = |message: &str| match category {
        None => true,
        Some(c) => message
            .strip_prefix('[')
            .and_then(|m| m.strip_prefix(c))
            .and_then(|m| m.strip_prefix(']'))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace)),
    };
    let rows = state.store.learned_rejections().map_err(internal)?;
    let mut deleted = 0usize;
    for r in rows
        .iter()
        .filter(|r| r.kind == req.kind && r.model == req.model && in_category(&r.message))
    {
        let in_db = state.store.forget_learned_rejection(r).map_err(internal)?;
        let in_mem = proxy::forget_learned_memory(
            &state.shape_rejections,
            &state.deprecated_fields,
            &state.empty_replies,
            r,
        );
        deleted += usize::from(in_db || in_mem);
    }
    if deleted == 0 {
        return Err(not_found());
    }
    tracing::info!(
        deleted, kind = %req.kind, model = %req.model, category = category.unwrap_or("-"),
        "a group of learned upstream rejections removed from the console"
    );
    list_learned_rejections(State(state)).await
}

/// `DELETE /api/learned-rejections[?kind=…]`：清空从上游学到的规则——库里的和进程内的一起清。
/// 不带 `kind` 清全部；带了只清那一种类（`shape` / `deprecated` / `empty_reply` / `refusal`），
/// 种类名对不上（含空串）回 400，什么都不动。
///
/// 逃生口：上游放开了某个取值或恢复了某个参数，本地却还在按学到的旧规则拒/剥。7 天保鲜期
/// 会自动过期，等不及就手动清。清错的代价只是每种组合再撞一次 400。按种类清是给拒答提示词
/// 那一格准备的：一个下游被分类器盯上，几小时就灌进上百条，不该为清它们把别的规则也清掉。
#[derive(Serialize)]
pub(super) struct ClearedLearnedResp {
    /// 从库里删掉的条数。
    deleted: usize,
}

#[derive(Deserialize)]
pub(super) struct ClearLearnedQuery {
    kind: Option<String>,
}

pub(super) async fn clear_learned_rejections(
    State(state): State<AppState>,
    Query(q): Query<ClearLearnedQuery>,
) -> Result<Json<ClearedLearnedResp>, ApiError> {
    // 两条分支都是**先删库、再清内存**：库删失败回 500 时内存原样不动，不会出现「接口报错、
    // 规则却已经从进程里消失」。反过来的窄窗口（库已删、内存还没清，或这中间刚学到一条新的
    // 只进了内存）由每小时按库重建兜底，最多多拦或多放一条到下个整点。
    let deleted = match q.kind.as_deref() {
        // 没带 `kind` 才是清全部；带了但是空串按未知种类 400，免得 `?kind=` 手滑成全清。
        None => {
            let deleted = state.store.clear_learned_rejections().map_err(internal)?;
            state.shape_rejections.write().clear();
            state.deprecated_fields.write().clear();
            *state.empty_replies.write() = Default::default();
            tracing::info!(deleted, "learned upstream rejections cleared from the console");
            deleted
        }
        Some(kind) => {
            let kind = kind.trim();
            if !proxy::LEARNED_KINDS.contains(&kind) {
                return Err(bad_request(format!("unknown rule kind: {kind:?}")));
            }
            let deleted = state.store.clear_learned_rejections_of_kind(kind).map_err(internal)?;
            proxy::clear_learned_memory_kind(
                &state.shape_rejections,
                &state.deprecated_fields,
                &state.empty_replies,
                kind,
            );
            tracing::info!(
                deleted,
                kind,
                "learned upstream rejections of one kind cleared from the console"
            );
            deleted
        }
    };
    Ok(Json(ClearedLearnedResp { deleted }))
}

/// `GET /api/models`：控制台（连通性测试的模型下拉）用的模型清单。
#[derive(Serialize)]
pub(super) struct ModelsResp {
    /// 价目表里列出的现役模型（见 [`crate::pricing::LISTED_MODELS`]），前端不再抄一份。
    listed: Vec<&'static str>,
    /// 最近 7 天客户端真实请求过的模型（去重、按最后出现时刻倒序）。客户端在用什么就列什么，
    /// 新模型上线不必等 luban 发版。
    recent: Vec<RecentModel>,
}

#[derive(Serialize)]
struct RecentModel {
    model: String,
    /// 最后一次出现的时刻（Unix 秒）。
    last_ts: i64,
}

pub(super) async fn list_models(
    State(state): State<AppState>,
) -> Result<Json<ModelsResp>, ApiError> {
    let recent = blocking(move || state.store.recent_models(7).map_err(internal))
        .await?
        .into_iter()
        .map(|(model, last_ts)| RecentModel { model, last_ts })
        .collect();
    Ok(Json(ModelsResp { listed: crate::pricing::LISTED_MODELS.to_vec(), recent }))
}
