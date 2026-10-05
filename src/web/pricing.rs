//! 公开价目（对接 New API「同步上游倍率」）。

use super::*;

// ---------- 公开价目（对接 New API 「同步上游倍率」） ----------

/// New API 的倍率基准：`model_ratio` 1.0 = $0.002 / 1K tokens = $2 / MTok。
/// 所以 `model_ratio = 输入价(USD/MTok) / 2`，`completion_ratio = 输出价 / 输入价`。
const NEWAPI_RATIO_BASE_USD_PER_MTOK: f64 = 2.0;

/// `GET /api/pricing` 单条：字段名与 New API 自己的 `/api/pricing` 一致，它的同步器按这些
/// 名字解析。`quota_type` 0 表示按 token 倍率计费（1 是按次定价，我们没有这种模型）。
/// `cache_ratio` / `create_cache_ratio` 是缓存读 / 缓存写(5m) 相对输入价的倍率——New API
/// 只有一档缓存写，官方 1 小时档（×2.0）它表达不了，这里按更常见的 5 分钟档给。
#[derive(Serialize)]
pub(super) struct PricingItem {
    pub(super) model_name: &'static str,
    pub(super) quota_type: u8,
    pub(super) model_ratio: f64,
    model_price: f64,
    pub(super) completion_ratio: f64,
    pub(super) cache_ratio: f64,
    pub(super) create_cache_ratio: f64,
    owner_by: &'static str,
    enable_groups: [&'static str; 1],
    supported_endpoint_types: [&'static str; 1],
}

impl From<&crate::pricing::ModelPrice> for PricingItem {
    fn from(p: &crate::pricing::ModelPrice) -> Self {
        PricingItem {
            model_name: p.model,
            quota_type: 0,
            model_ratio: p.input_per_mtok / NEWAPI_RATIO_BASE_USD_PER_MTOK,
            model_price: 0.0,
            completion_ratio: p.output_per_mtok / p.input_per_mtok,
            cache_ratio: p.cache_read_mult,
            create_cache_ratio: p.cache_write_5m_mult,
            owner_by: "anthropic",
            enable_groups: ["default"],
            supported_endpoint_types: ["anthropic"],
        }
    }
}

#[derive(Serialize)]
pub(super) struct PricingResp {
    success: bool,
    data: Vec<PricingItem>,
    /// New API 前端展示价目时会读分组倍率；只有一个默认分组、倍率 1。
    group_ratio: serde_json::Value,
    usable_group: serde_json::Value,
}

pub(super) fn pricing_items() -> Vec<PricingItem> {
    crate::pricing::price_table().iter().map(PricingItem::from).collect()
}

/// 公开价目：New API 「同步上游倍率」默认拉这条路径（列表格式）。价目来自
/// [`crate::pricing`] 的官方价目表，随二进制发版更新，不做运行时抓取——官方没有机器可读的
/// 价目接口，只能人工对齐文档。
pub(super) async fn get_pricing() -> Json<PricingResp> {
    Json(PricingResp {
        success: true,
        data: pricing_items(),
        group_ratio: serde_json::json!({ "default": 1 }),
        usable_group: serde_json::json!({ "default": "默认分组" }),
    })
}

/// 同一份价目的倍率映射格式（New API 同步器的另一种可识别形状）：
/// `{"success":true,"data":{"model_ratio":{...},"completion_ratio":{...},"cache_ratio":{...},
/// "create_cache_ratio":{...}}}`。有些部署把上游地址配成了 `/api/ratio_config`，两条都给。
pub(super) async fn get_ratio_config() -> Json<serde_json::Value> {
    let items = pricing_items();
    let map = |f: fn(&PricingItem) -> f64| -> serde_json::Map<String, serde_json::Value> {
        items.iter().map(|i| (i.model_name.to_string(), serde_json::json!(f(i)))).collect()
    };
    Json(serde_json::json!({
        "success": true,
        "data": {
            "model_ratio": map(|i| i.model_ratio),
            "completion_ratio": map(|i| i.completion_ratio),
            "cache_ratio": map(|i| i.cache_ratio),
            "create_cache_ratio": map(|i| i.create_cache_ratio),
        }
    }))
}
