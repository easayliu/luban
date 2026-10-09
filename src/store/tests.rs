// 测试用 `mark_banned` 一句话造出「已封禁」状态就够了，不必每处都拼 BanContext。
#![allow(deprecated)]
use super::*;

/// 批量删除只删池里的记录，不存在的 id 不计入条数。
#[test]
fn delete_proxies_removes_only_given_ids() {
    let store = CredentialStore::open_in_memory().unwrap();
    let a = store.add_proxy(1, "a", "socks5h://10.0.0.1:1080").unwrap();
    let b = store.add_proxy(1, "b", "socks5h://10.0.0.2:1080").unwrap();
    let c = store.add_proxy(1, "c", "socks5h://10.0.0.3:1080").unwrap();
    assert_eq!(store.delete_proxies(&[a.id, c.id, 9999]).unwrap(), 2);
    let left: Vec<i64> =
        store.list_proxies(Scope::All).unwrap().into_iter().map(|p| p.id).collect();
    assert_eq!(left, vec![b.id]);
    assert_eq!(store.delete_proxies(&[]).unwrap(), 0);
}

/// 批量添加：已在池里的地址返回 None，其余照常写入。
#[test]
fn add_proxies_skips_urls_already_in_the_pool() {
    let store = CredentialStore::open_in_memory().unwrap();
    store.add_proxy(1, "old", "socks5h://10.0.0.1:1080").unwrap();
    let out = store
        .add_proxies(
            1,
            &[
                ("a".into(), "socks5h://10.0.0.1:1080".into()),
                ("b".into(), "socks5h://10.0.0.2:1080".into()),
            ],
        )
        .unwrap();
    assert!(out[0].is_none());
    assert_eq!(out[1].as_ref().unwrap().label, "b");
    assert_eq!(store.list_proxies(Scope::All).unwrap().len(), 2);
}

/// 全局默认会话上限的播种：同设备那条——缺失才写、显式值（含 `0`）不动、重复启动不改。
#[test]
fn seeds_the_default_session_limit_only_when_absent() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let read = |conn: &Connection| -> Option<String> {
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![DEFAULT_SESSION_LIMIT],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    };
    assert_eq!(read(&conn).as_deref(), Some(DEFAULT_SESSION_LIMIT_VALUE.to_string().as_str()));
    init_schema(&conn).unwrap();
    assert_eq!(read(&conn).as_deref(), Some(DEFAULT_SESSION_LIMIT_VALUE.to_string().as_str()));
    for explicit in ["0", "12"] {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![DEFAULT_SESSION_LIMIT, explicit],
        )
        .unwrap();
        init_schema(&conn).unwrap();
        assert_eq!(read(&conn).as_deref(), Some(explicit), "显式配置不该被迁移改写");
    }
}

/// 全局默认设备上限的播种：库里没有这一项才写入 [`DEFAULT_DEVICE_LIMIT_VALUE`]，
/// 已经有值的（包括显式写的 `0` = 不限）一个字不动，重复启动也不会改回去。
///
/// 判定本身不依赖这一行——[`CredentialStore::default_device_limit`] 在设置缺失时回落到
/// 同一个常量；写进表是为了让它在控制台里看得见、改得动。真正「多出一道上限」的是从
/// v0.2.8 之前的库升上来那一次，所以确实插入时会 warn 一条（日志不在断言范围内）。
#[test]
fn seeds_the_default_device_limit_only_when_absent() {
    // 空库：播种成默认值。
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let read = |conn: &Connection| -> Option<String> {
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![DEFAULT_DEVICE_LIMIT],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    };
    assert_eq!(read(&conn).as_deref(), Some(DEFAULT_DEVICE_LIMIT_VALUE.to_string().as_str()));
    // 再跑一遍（每次启动都会跑）：不重复写、值不变。
    init_schema(&conn).unwrap();
    assert_eq!(read(&conn).as_deref(), Some(DEFAULT_DEVICE_LIMIT_VALUE.to_string().as_str()));

    // 用户显式写过的值一律保留，"0"（不限）也不会被改成 5。
    for explicit in ["0", "12"] {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![DEFAULT_DEVICE_LIMIT, explicit],
        )
        .unwrap();
        init_schema(&conn).unwrap();
        assert_eq!(read(&conn).as_deref(), Some(explicit), "显式配置不该被迁移改写");
    }

    // 缺这一行时判定仍是同一个值——这一行不改变行为。
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute("DELETE FROM settings WHERE key = ?1", params![DEFAULT_DEVICE_LIMIT]).unwrap();
    let store = CredentialStore::with_conn(conn);
    assert_eq!(store.default_device_limit(), DEFAULT_DEVICE_LIMIT_VALUE);
}

/// 旧库（无 AUTOINCREMENT）经 init_schema 迁移后，删号腾出的 id 不再被复用。
#[test]
fn migrates_and_stops_id_reuse() {
    let conn = Connection::open_in_memory().unwrap();
    // 造一张旧表：非 AUTOINCREMENT，且只含早期列（模拟老库，后续列靠 ALTER 补）。
    conn.execute_batch(
        "CREATE TABLE credentials (
                id INTEGER PRIMARY KEY,
                label TEXT NOT NULL DEFAULT '',
                tier TEXT,
                access_token TEXT NOT NULL,
                refresh_token TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                priority INTEGER NOT NULL DEFAULT 0,
                disabled INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
    )
    .unwrap();
    for (id, tok) in [(1, "a"), (2, "b"), (3, "c")] {
        conn.execute(
            "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
                 VALUES (?1, ?2, ?3, 0)",
            params![id, tok, format!("r{tok}")],
        )
        .unwrap();
    }

    init_schema(&conn).unwrap();

    let ddl: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'credentials'", [], |r| r.get(0))
        .unwrap();
    assert!(ddl.contains("AUTOINCREMENT"), "迁移后应为 AUTOINCREMENT");

    // 既有行与其 id 全部保留（迁移把 sqlite_sequence 播种为 MAX(id)=3）。
    let cnt: i64 = conn.query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get(0)).unwrap();
    assert_eq!(cnt, 3);

    // 迁移后删掉最大 id，新插入应得 4，而非复用被删的 3。
    conn.execute("DELETE FROM credentials WHERE id = 3", []).unwrap();
    conn.execute(
        "INSERT INTO credentials (access_token, refresh_token, expires_at) VALUES ('d','rd',0)",
        [],
    )
    .unwrap();
    assert_eq!(conn.last_insert_rowid(), 4, "AUTOINCREMENT 不应复用被删的 id=3");

    // **升级后的库必须能按完整列清单读**，而且要在**第一次** init_schema 之后就查——此前
    // `rate_limit_tier` 那条 ADD COLUMN 排在重建之前，重建的复制清单里又没有它，于是旧库
    // 升上来这一列整列消失，`SELECT {COLS}` 报 `no such column`。这个测试原先只数行数、
    // 从不走 COLS，而且再跑一遍 init_schema 会把那列重新 ADD 回来（重启一次就「好了」），
    // 所以查得晚了照样是绿的。
    let store = CredentialStore::with_conn(conn);
    let all = store.list().unwrap();
    assert_eq!(all.len(), 3, "list 走的是完整列清单");
    let one = store.get(4).unwrap().expect("迁移后插入的那行");
    assert_eq!(one.access_token, "d");
    assert!(one.rate_limit_tier.is_none() && one.org_uuid.is_none());
    // 重建之后加的每一列都得真在表里，而不只是 ADD COLUMN 没报错。
    let cols: Vec<String> = store
        .conn
        .lock()
        .prepare("SELECT name FROM pragma_table_info('credentials')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for expected in COLS.split(',').map(str::trim) {
        assert!(cols.iter().any(|c| c == expected), "升级后的表缺列 {expected}: {cols:?}");
    }
    let conn = store.conn.into_inner();

    // 旧库里**已经有**排在重建之后才 ADD 的列、且有值（比如一份手工改过的库，或某个
    // 中间版本留下的）：重建按「新表有、旧表也有」复制，值必须原样留下。
    let conn2 = Connection::open_in_memory().unwrap();
    conn2
            .execute_batch(
                "CREATE TABLE credentials (
                    id INTEGER PRIMARY KEY,
                    label TEXT NOT NULL DEFAULT '',
                    tier TEXT,
                    access_token TEXT NOT NULL,
                    refresh_token TEXT NOT NULL,
                    expires_at INTEGER NOT NULL,
                    priority INTEGER NOT NULL DEFAULT 0,
                    disabled INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                    rate_limit_tier TEXT,
                    proxy TEXT,
                    org_uuid TEXT,
                    legacy_only TEXT
                );
                INSERT INTO credentials (id, access_token, refresh_token, expires_at,
                                         rate_limit_tier, proxy, org_uuid, legacy_only)
                VALUES (7, 'a', 'ra', 0, 'default_claude_max_20x', 'socks5h://p:1080', 'org-7', 'x');",
            )
            .unwrap();
    init_schema(&conn2).unwrap();
    let store2 = CredentialStore::with_conn(conn2);
    let c = store2.get(7).unwrap().expect("重建后行还在");
    assert_eq!(
        c.rate_limit_tier.as_deref(),
        Some("default_claude_max_20x"),
        "重建前就有的值不能丢"
    );
    assert_eq!(c.proxy.as_deref(), Some("socks5h://p:1080"));
    assert_eq!(c.org_uuid.as_deref(), Some("org-7"));
    assert!(c.subscription_created_at.is_none(), "旧表没有的列补出来是空");
    let store = CredentialStore::with_conn(conn);
    let conn = store.conn.into_inner();

    // 迁移后再次 init_schema 必须是无副作用的 no-op（RENAME 后 DDL 仍含 AUTOINCREMENT，
    // 不应二次重建而丢数据）。
    init_schema(&conn).unwrap();
    let after: i64 = conn.query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get(0)).unwrap();
    assert_eq!(after, 3, "二次 init_schema 不应改动数据");
}

/// `apply_profile` 是登录 / 手动刷新 / 自动刷新三条路共用的写回：只写 profile 给了的项，
/// 缺项不清库里已有的值；组织 id 退回交换响应里的兜底。`profile_incomplete` 决定自动刷新
/// 要不要顺手拉一次 profile——四列齐了就不再拉。
#[test]
fn apply_profile_backfills_only_what_the_profile_gives() {
    let (store, ids) = store_with(&["a"]);
    let id = ids[0];
    assert!(store.get(id).unwrap().unwrap().profile_incomplete(), "刚建的号 profile 列全空");

    let partial = crate::oauth::Profile {
        tier: Some("Max 5x".into()),
        account_uuid: Some("acct".into()),
        rate_limit_tier: Some("default_claude_max_5x".into()),
        ..Default::default()
    };
    store.apply_profile(id, &partial, Some("org-from-token")).unwrap();
    let c = store.get(id).unwrap().unwrap();
    assert_eq!(c.tier.as_deref(), Some("Max 5x"));
    assert_eq!(c.account_uuid.as_deref(), Some("acct"));
    assert_eq!(c.rate_limit_tier.as_deref(), Some("default_claude_max_5x"));
    assert_eq!(c.org_uuid.as_deref(), Some("org-from-token"), "profile 没给组织 id 就用交换响应的");
    assert!(c.subscription_created_at.is_none());
    assert!(c.profile_incomplete(), "订阅创建时刻还缺着，下次刷新还要拉");

    let full = crate::oauth::Profile {
        org_uuid: Some("org-from-profile".into()),
        subscription_created_at: Some("2026-04-15T13:03:55.239Z".into()),
        org_name: Some("Acme".into()),
        seat_tier: Some("team_standard".into()),
        subscription_status: Some("active".into()),
        extra_usage_enabled: Some(true),
        ..Default::default()
    };
    store.apply_profile(id, &full, Some("org-from-token")).unwrap();
    let c = store.get(id).unwrap().unwrap();
    assert_eq!(c.org_uuid.as_deref(), Some("org-from-profile"), "profile 给了就以 profile 为准");
    assert_eq!(c.subscription_created_at.as_deref(), Some("2026-04-15T13:03:55.239Z"));
    assert_eq!(c.tier.as_deref(), Some("Max 5x"), "这次 profile 没给的项不能被清掉");
    assert_eq!(c.account_uuid.as_deref(), Some("acct"));
    assert_eq!(c.org_name.as_deref(), Some("Acme"));
    assert_eq!(c.seat_tier.as_deref(), Some("team_standard"));
    assert_eq!(c.subscription_status.as_deref(), Some("active"));
    assert_eq!(c.extra_usage_enabled, Some(true));
    assert!(!c.profile_incomplete(), "五列齐了就不再拉");

    // 换成个人号（没有席位档）：组织那一组整组覆盖，席位档清空；没给组织名称的残缺响应不动它们。
    let personal = crate::oauth::Profile {
        org_name: Some("someone's Organization".into()),
        subscription_status: Some("active".into()),
        ..Default::default()
    };
    store.apply_profile(id, &personal, None).unwrap();
    let c = store.get(id).unwrap().unwrap();
    assert!(c.seat_tier.is_none() && c.extra_usage_enabled.is_none());
    store.apply_profile(id, &crate::oauth::Profile::default(), None).unwrap();
    let c = store.get(id).unwrap().unwrap();
    assert_eq!(c.org_name.as_deref(), Some("someone's Organization"));
}

/// 开机清扫：被删账号遗留的设备绑定被清掉；用量流水一律不动（已删账号的流水按设计
/// 留到保留期满，见 `remove`）。
#[test]
fn startup_sweep_drops_orphan_bindings_but_keeps_usage_logs() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
             VALUES (1, 'a', 'ra', 0)",
        [],
    )
    .unwrap();
    // 账号 2 已被（旧版逻辑）删掉，但历史数据还在。
    for cid in ["1", "2", "NULL"] {
        conn.execute(&format!("INSERT INTO usage_logs (cred_id) VALUES ({cid})"), []).unwrap();
    }
    for (did, cid) in [("d1", 1), ("d2", 2)] {
        conn.execute(
            "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)",
            params![did, cid],
        )
        .unwrap();
    }

    purge_orphan_rows(&conn).unwrap();

    let logs: Vec<Option<i64>> = conn
        .prepare("SELECT cred_id FROM usage_logs ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(logs, vec![Some(1), Some(2), None], "流水一条不删，含已删账号(2)的");
    let binds: Vec<i64> = conn
        .prepare("SELECT cred_id FROM device_bindings")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(binds, vec![1]);
}

/// 删号清掉设备绑定，但用量流水留着（随保留期自然裁掉），其它账号不受影响。
#[test]
fn delete_keeps_usage_logs_but_drops_bindings() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).unwrap();
    {
        let conn = store.conn.lock();
        for cid in [a.id, b.id] {
            conn.execute("INSERT INTO usage_logs (cred_id) VALUES (?1)", [cid]).unwrap();
        }
        conn.execute("INSERT INTO device_bindings (device_id, cred_id) VALUES ('d1', ?1)", [a.id])
            .unwrap();
    }

    assert!(store.delete(a.id).unwrap());

    let conn = store.conn.lock();
    let logs: Vec<i64> = conn
        .prepare("SELECT cred_id FROM usage_logs")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(logs, vec![a.id, b.id], "删号不删流水");
    let binds: i64 =
        conn.query_row("SELECT COUNT(*) FROM device_bindings", [], |r| r.get(0)).unwrap();
    assert_eq!(binds, 0);
}

/// 设备上限三态：账号独立值覆盖全局，0 跟随全局，负值明确不限。
#[test]
fn effective_device_limit_tri_state() {
    assert_eq!(effective_device_limit(3, 5), 3, "账号独立上限覆盖全局");
    assert_eq!(effective_device_limit(0, 5), 5, "未配置则跟随全局默认");
    assert_eq!(effective_device_limit(0, 0), 0, "全局也不限时不限");
    assert_eq!(effective_device_limit(-1, 5), 0, "账号明确不限，忽略全局默认");
}

/// 新增账号一律落在 P2；批量改优先级把选中的账号统一调档、其余不动。
#[test]
fn insert_defaults_to_p2_and_batch_priority() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).unwrap();
    let c = store.insert("c", None, "tc", "rc", 0, None, None, 1).unwrap();
    assert_eq!((a.priority, b.priority, c.priority), (2, 2, 2), "新账号都应是 P2");

    assert_eq!(store.set_priorities(&[a.id, c.id], 0).unwrap(), 2);
    let by_id: HashMap<i64, i64> =
        store.list().unwrap().into_iter().map(|x| (x.id, x.priority)).collect();
    assert_eq!(by_id[&a.id], 0);
    assert_eq!(by_id[&c.id], 0);
    assert_eq!(by_id[&b.id], 2, "未选中的账号不应被改动");
    assert_eq!(store.set_priorities(&[], 4).unwrap(), 0, "空列表为 no-op");
}

/// 批量升降档：各自加减、保留相对顺序，越界截到 P0..=P4，未选中的不动。
#[test]
fn shift_priorities_keeps_order_and_clamps() {
    let (store, ids) = store_with(&["a", "b", "c"]);
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    store.set_priority(a, 0).unwrap();
    store.set_priority(b, 1).unwrap();
    store.set_priority(c, 3).unwrap();
    let prio = |id| store.get(id).unwrap().unwrap().priority;

    assert_eq!(store.shift_priorities(&[a, b], 1).unwrap(), 2);
    assert_eq!((prio(a), prio(b), prio(c)), (1, 2, 3), "只动选中的，各自降一档");
    store.shift_priorities(&[a, b, c], -2).unwrap();
    assert_eq!((prio(a), prio(b), prio(c)), (0, 0, 1), "提高到顶截在 P0");
    store.shift_priorities(&[c], 4).unwrap();
    assert_eq!(prio(c), 4, "降低到底截在 P4");
    assert_eq!(store.shift_priorities(&[b, b], 1).unwrap(), 1);
    assert_eq!(prio(b), 1, "重复的 id 只升降一次");
}

/// 导入时缺 priority 落默认档 P2，不被当成 0 变成 P0；带了的截在 P0..=P4。
#[test]
fn import_without_priority_lands_on_default() {
    let (store, _) = store_with(&[]);
    let raw = |rt: &str, extra: &str| -> PortableCredential {
        serde_json::from_str(&format!(
            r#"{{"access_token":"at-{rt}","refresh_token":"{rt}"{extra}}}"#
        ))
        .unwrap()
    };
    store.import_credential(&raw("rt-a", "")).unwrap();
    store.import_credential(&raw("rt-b", r#","priority":0"#)).unwrap();
    store.import_credential(&raw("rt-c", r#","priority":7"#)).unwrap();
    let by_rt: HashMap<String, i64> =
        store.list().unwrap().into_iter().map(|c| (c.refresh_token, c.priority)).collect();
    assert_eq!(by_rt["rt-a"], PRIORITY_DEFAULT);
    assert_eq!(by_rt["rt-b"], PRIORITY_MIN);
    assert_eq!(by_rt["rt-c"], PRIORITY_MAX);
}

/// 迁移前把库设成某种旧口径：清掉两个标记，按需补回 P1..=P100 的标记，再写入优先级。
fn store_on_old_scale(priorities: &[i64], scale_p50: bool) -> (CredentialStore, Vec<i64>) {
    let labels: Vec<String> = (0..priorities.len()).map(|i| format!("c{i}")).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let (store, ids) = store_with(&labels);
    {
        let conn = store.conn.lock();
        conn.execute(
            "DELETE FROM settings WHERE key IN (?1, ?2)",
            [PRIORITY_SCALE_MIGRATED, PRIORITY_TIERS_MIGRATED],
        )
        .unwrap();
        if scale_p50 {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, '1')",
                [PRIORITY_SCALE_MIGRATED],
            )
            .unwrap();
        }
        for (id, p) in ids.iter().zip(priorities) {
            conn.execute("UPDATE credentials SET priority = ?2 WHERE id = ?1", params![id, p])
                .unwrap();
        }
    }
    (store, ids)
}

/// P1..=P100 的库按名次压到 5 档：P50 → P2，两侧最近的各占 P1/P3，再往外并进 P0/P4；
/// 标记落下后再跑不会二次压档。
#[test]
fn migrate_priority_tiers_from_p50_scale_once() {
    let (store, ids) = store_on_old_scale(&[1, 48, 49, 50, 51, 100], true);
    migrate_priority_tiers(&store.conn.lock()).unwrap();
    store.set_priority(ids[3], 3).unwrap();
    migrate_priority_tiers(&store.conn.lock()).unwrap();
    let got: Vec<i64> = ids.iter().map(|&id| store.get(id).unwrap().unwrap().priority).collect();
    assert_eq!(got, vec![0, 0, 1, 3, 3, 4]);
}

/// 没跑过 v0.3.196 平移的库还是最早的口径（默认 0、可为负数），直接按 0 为中线压档。
#[test]
fn migrate_priority_tiers_from_legacy_scale() {
    let (store, ids) = store_on_old_scale(&[-80, -1, 0, 0, 3, 500], false);
    migrate_priority_tiers(&store.conn.lock()).unwrap();
    let got: Vec<i64> = ids.iter().map(|&id| store.get(id).unwrap().unwrap().priority).collect();
    assert_eq!(got, vec![0, 1, 2, 2, 3, 4]);
}

/// 两个进程同时首次打开同一个库：一边在迁移事务里时，另一边的迁移要排队，轮到它时看到
/// 标记直接跳过，不能拿事务外读到的旧状态再压一遍（曾经会把 P2 压成 P1）。
#[test]
fn migrate_priority_tiers_waits_for_concurrent_migration() {
    let dir = std::env::temp_dir().join(format!("luban-tiers-race-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("t.db{suffix}")));
    }
    let ids: Vec<i64> = {
        let store = CredentialStore::open_at(&path).unwrap();
        let ids: Vec<i64> = ["a", "b", "c"]
            .iter()
            .map(|l| {
                let rt = format!("rt-{l}");
                store.insert(l, None, "t", &rt, 0, None, None, 1).unwrap().id
            })
            .collect();
        let conn = store.conn.lock();
        conn.execute("DELETE FROM settings WHERE key = ?1", [PRIORITY_TIERS_MIGRATED]).unwrap();
        for (id, p) in ids.iter().zip([49, 50, 51]) {
            conn.execute("UPDATE credentials SET priority = ?2 WHERE id = ?1", params![id, p])
                .unwrap();
        }
        ids
    };
    let open = || {
        let c = Connection::open(&path).unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c
    };

    // 「另一个进程」：迁移事务已经开了、写锁在手，还没提交。
    let first = open();
    first.execute_batch("BEGIN IMMEDIATE").unwrap();
    let second = std::thread::spawn({
        let conn = open();
        move || migrate_priority_tiers(&conn).unwrap()
    });
    std::thread::sleep(Duration::from_millis(200));
    // 写成与「拿旧值重算」不同的结果，第二个进程若越过标记重写就看得出来。
    for (id, p) in ids.iter().zip([0, 2, 4]) {
        first
            .execute("UPDATE credentials SET priority = ?2 WHERE id = ?1", params![id, p])
            .unwrap();
    }
    first
        .execute("INSERT INTO settings (key, value) VALUES (?1, '1')", [PRIORITY_TIERS_MIGRATED])
        .unwrap();
    first.execute_batch("COMMIT").unwrap();
    second.join().unwrap();

    let got: Vec<i64> = ids
        .iter()
        .map(|id| {
            first
                .query_row("SELECT priority FROM credentials WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        })
        .collect();
    assert_eq!(got, vec![0, 2, 4], "第二个进程不该在已压档的数据上再压一遍");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 新库建好就带着标记：新号直接落 P2，下次启动不会再被压档。
#[test]
fn fresh_db_is_already_on_tiers() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap();
    store.set_priority(a.id, 0).unwrap();
    migrate_priority_tiers(&store.conn.lock()).unwrap();
    assert_eq!(store.get(a.id).unwrap().unwrap().priority, 0);
}

/// 批量启停 / 设备上限 / 删除：只作用于选中的 id，且各自保持单账号接口的语义。
#[test]
fn batch_ops_only_touch_selected() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).unwrap();
    let c = store.insert("c", None, "tc", "rc", 0, None, None, 1).unwrap();
    // 给 a、b 各造一条设备绑定与用量日志：删号要清绑定、留流水。
    {
        let conn = store.conn.lock();
        for (did, cid) in [("d1", a.id), ("d2", b.id)] {
            conn.execute(
                "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)",
                params![did, cid],
            )
            .unwrap();
            conn.execute("INSERT INTO usage_logs (cred_id) VALUES (?1)", [cid]).unwrap();
        }
    }

    // 批量停用 a、b：c 不受影响；停用会清掉被选中账号的设备绑定。
    assert_eq!(store.set_disabled_many(&[a.id, b.id], true).unwrap(), 2);
    let by_id = |s: &CredentialStore| -> HashMap<i64, Credential> {
        s.list().unwrap().into_iter().map(|x| (x.id, x)).collect()
    };
    let m = by_id(&store);
    assert!(m[&a.id].disabled && m[&b.id].disabled);
    assert!(!m[&c.id].disabled, "未选中的账号不应被停用");
    {
        let conn = store.conn.lock();
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM device_bindings", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "停用应清掉这两个账号的设备绑定");
    }

    // 批量启用要清 ban_reason（模拟先被自动封禁）。
    store.mark_banned(a.id, "banned").unwrap();
    assert!(by_id(&store)[&a.id].ban_reason.is_some());
    assert_eq!(store.set_disabled_many(&[a.id], false).unwrap(), 1);
    let m = by_id(&store);
    assert!(!m[&a.id].disabled && m[&a.id].ban_reason.is_none(), "启用应清除封禁原因");

    // 批量设备上限：负值由 web 层收敛，这里验证按传入值原样落库。
    assert_eq!(store.set_device_limits(&[a.id, c.id], 5).unwrap(), 2);
    let m = by_id(&store);
    assert_eq!((m[&a.id].device_limit, m[&c.id].device_limit), (5, 5));
    assert_eq!(m[&b.id].device_limit, 0, "未选中的账号不应被改动");

    // 批量删除：账号没了，流水留着（随保留期自然裁掉）。
    assert_eq!(store.delete_many(&[a.id]).unwrap(), 1);
    let m = by_id(&store);
    assert!(!m.contains_key(&a.id) && m.contains_key(&b.id) && m.contains_key(&c.id));
    {
        let conn = store.conn.lock();
        let logs: Vec<i64> = conn
            .prepare("SELECT cred_id FROM usage_logs")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(logs, vec![a.id, b.id], "批量删号同样不删流水");
    }

    // 空列表一律 no-op，不误伤全表。
    assert_eq!(store.delete_many(&[]).unwrap(), 0);
    assert_eq!(store.set_disabled_many(&[], true).unwrap(), 0);
    assert_eq!(store.set_device_limits(&[], 9).unwrap(), 0);
    assert_eq!(store.list().unwrap().len(), 2, "空列表操作不应改动任何账号");
}

/// 刷新失败自动换号所依赖的那一步：停用坏号后，原本绑在它上面的设备必须能改选到别的号。
///
/// `select_for_device` 会优先命中既有绑定，所以只是「不再选中被停用的号」还不够——
/// 封号事件与冻结流水是取证材料：落地时快照账号侧读数、冻结封前流水；封后 10 分钟内
/// 到达的流水（触发那一发）也要补进去；删号、解封、裁剪都不能碰它们。
#[test]
fn record_ban_freezes_history_and_survives_deletion() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).unwrap();
    store.set_proxy(a.id, Some("socks5h://u:secret@exit1:1080")).unwrap();
    let now: i64 = store.conn.lock().query_row("SELECT unixepoch()", [], |r| r.get(0)).unwrap();
    let mut rec = UsageRecord {
        cred_id: Some(a.id),
        cred_label: "a".into(),
        device_id: Some("dev1".into()),
        model: Some("claude-opus-5".into()),
        ua: Some("claude-cli/2.1.259".into()),
        status: 200,
        cost_usd: Some(0.5),
        forensics: Forensics {
            proxy: Some("socks5h://u:***@exit1:1080".into()),
            shape: Some("{\"keys\":[\"model\"]}".into()),
            device_id_out: Some("d230ce6e-out".into()),
            simulated: true,
            sim_reason: Some("not_cc_shaped".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    // 封前 3 天的两条 + 10 天前的一条（窗口外，不该被冻结）。
    store.insert_usage_log_at(&rec, Some(now - 3 * 86400)).unwrap();
    rec.device_id = Some("dev2".into());
    store.insert_usage_log_at(&rec, Some(now - 3 * 86400 + 5)).unwrap();
    store.insert_usage_log_at(&rec, Some(now - 10 * 86400)).unwrap();
    // 模拟原因随流水落库、读回。
    let back = store.list_usage_logs(1).unwrap().remove(0);
    assert!(back.forensics.simulated);
    assert_eq!(back.forensics.sim_reason.as_deref(), Some("not_cc_shaped"));

    let ctx = BanContext {
        reason: "[403] permission_error: account disabled".into(),
        source: "forward",
        status: Some(403),
        error_type: Some("permission_error".into()),
        error_message: Some("Your account has been disabled for policy violations".into()),
        request_id: Some("req_x".into()),
        upstream_request_id: Some("up_x".into()),
    };
    assert!(store.record_ban(a.id, &ctx).unwrap());
    assert!(!store.record_ban(9999, &ctx).unwrap(), "不存在的号不落事件");

    let events = store.list_ban_events(None, 10).unwrap();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.cred_id, a.id);
    assert_eq!(ev.source, "forward");
    assert_eq!(ev.status, Some(403));
    assert_eq!(ev.error_type.as_deref(), Some("permission_error"));
    assert_eq!(ev.request_id.as_deref(), Some("req_x"));
    assert_eq!(ev.proxy.as_deref(), Some("socks5h://u:***@exit1:1080"), "代理密码要打码");
    assert_eq!(ev.lifetime_requests, 3);
    assert!((ev.lifetime_cost_usd - 1.5).abs() < 1e-9);
    assert_eq!(ev.requests_7d, 2);
    assert_eq!(ev.devices_7d, 2);
    assert_eq!(ev.devices_out_7d, 1, "两台来访设备伪装成同一个出站 device_id");
    assert_eq!(ev.device_ids_out_7d[0]["value"], "d230ce6e-out");
    assert_eq!(ev.device_ids_out_7d[0]["count"], 2);
    assert_eq!(ev.models_7d[0]["value"], "claude-opus-5");
    assert_eq!(ev.models_7d[0]["count"], 2);
    assert_eq!(ev.frozen_rows, 2, "只冻结 7 天窗口内的流水");
    let frozen = store.frozen_usage_logs(ev.id, 100, 0).unwrap().1;
    assert_eq!(frozen.len(), 2);
    assert_eq!(frozen[0].forensics.shape.as_deref(), Some("{\"keys\":[\"model\"]}"));
    assert_eq!(frozen[0].forensics.proxy.as_deref(), Some("socks5h://u:***@exit1:1080"));
    assert_eq!(
        frozen[0].forensics.device_id_out.as_deref(),
        Some("d230ce6e-out"),
        "出站 device_id 要随流水一起落库并进冻结表"
    );
    assert_eq!(
        store.list_usage_logs(10).unwrap()[0].forensics.device_id_out.as_deref(),
        Some("d230ce6e-out")
    );

    // 封后到达的（触发那一发）也进冻结表。
    rec.status = 403;
    rec.forensics.error_type = Some("permission_error".into());
    store.insert_usage_log(&rec).unwrap();
    let frozen = store.frozen_usage_logs(ev.id, 100, 0).unwrap().1;
    assert_eq!(frozen.len(), 3);
    assert_eq!(frozen[2].status, 403);
    assert_eq!(frozen[2].forensics.error_type.as_deref(), Some("permission_error"));

    // 翻页：总条数报的是整份，页只给要的那几条，越界页空着（页面靠 total 算页数）。
    let (total, page) = store.frozen_usage_logs(ev.id, 2, 0).unwrap();
    assert_eq!((total, page.len()), (3, 2));
    assert_eq!(page[0].ts, frozen[0].ts);
    let (total, page) = store.frozen_usage_logs(ev.id, 2, 2).unwrap();
    assert_eq!((total, page.len()), (3, 1));
    assert_eq!(page[0].status, 403, "第二页接着同一条时间线，不从头再来");
    assert!(store.frozen_usage_logs(ev.id, 2, 10).unwrap().1.is_empty());

    // 封后太久的不算。
    store.insert_usage_log_at(&rec, Some(now + FREEZE_TAIL_SECS + 60)).unwrap();
    assert_eq!(store.frozen_usage_logs(ev.id, 100, 0).unwrap().1.len(), 3);

    // 解封不清事件；删号不删事件与冻结流水；裁剪不碰冻结表。
    store.set_disabled(a.id, false).unwrap();
    assert_eq!(store.ban_counts().unwrap()[&a.id], 1);
    assert!(store.delete(a.id).unwrap());
    assert_eq!(store.list_ban_events(Some(a.id), 10).unwrap().len(), 1);
    assert_eq!(store.frozen_usage_logs(ev.id, 100, 0).unwrap().1.len(), 3);
    store.prune_usage_logs().unwrap();
    assert_eq!(store.frozen_usage_logs(ev.id, 100, 0).unwrap().1.len(), 3);
}

/// 老库升级：`usage_logs` 还没有取证列、两张新表都不存在。init_schema 之后要能正常写入、
/// 封号、冻结——补列走的是 ALTER，冻结用的列清单两张表必须对得上。
#[test]
fn legacy_usage_logs_table_gets_forensic_columns_and_freezes() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
            "CREATE TABLE credentials (
                id INTEGER PRIMARY KEY AUTOINCREMENT, label TEXT NOT NULL DEFAULT '',
                access_token TEXT NOT NULL, refresh_token TEXT NOT NULL,
                expires_at INTEGER NOT NULL, priority INTEGER NOT NULL DEFAULT 0,
                disabled INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())) STRICT;
             CREATE TABLE usage_logs (
                id INTEGER PRIMARY KEY, ts INTEGER NOT NULL DEFAULT (unixepoch()),
                cred_id INTEGER, cred_label TEXT NOT NULL DEFAULT '', device_id TEXT, model TEXT,
                path TEXT NOT NULL DEFAULT '', status INTEGER NOT NULL DEFAULT 0,
                has_usage INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER, output_tokens INTEGER,
                cache_creation_tokens INTEGER, cache_read_tokens INTEGER, ttft_ms INTEGER,
                total_ms INTEGER, unified_status TEXT, rl_5h_status TEXT, rl_5h_reset INTEGER) STRICT;",
        )
        .unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("legacy", None, "at", "rt", u64::MAX, None, None, 1).unwrap();
    let rec = UsageRecord {
        cred_id: Some(a.id),
        cred_label: "legacy".into(),
        status: 200,
        forensics: Forensics { proxy: Some("http://h:1".into()), ..Default::default() },
        ..Default::default()
    };
    store.insert_usage_log(&rec).unwrap();
    assert_eq!(
        store.list_usage_logs(10).unwrap()[0].forensics.proxy.as_deref(),
        Some("http://h:1")
    );
    assert!(store.mark_banned(a.id, "banned").unwrap());
    let ev = &store.list_ban_events(None, 10).unwrap()[0];
    assert_eq!(ev.source, "manual");
    assert_eq!(ev.frozen_rows, 1);
    assert_eq!(
        store.frozen_usage_logs(ev.id, 100, 0).unwrap().1[0].forensics.proxy.as_deref(),
        Some("http://h:1")
    );
}

#[test]
fn redact_proxy_hides_only_the_password() {
    assert_eq!(redact_proxy("socks5h://u:p@h:1"), "socks5h://u:***@h:1");
    assert_eq!(redact_proxy("http://h:8080"), "http://h:8080");
    assert_eq!(redact_proxy("http://u@h:8080"), "http://u@h:8080");
    assert_eq!(redact_proxy("http://u:p:q@h"), "http://u:***@h");
}

/// `mark_banned` 必须把它的 device_bindings 一并清掉，否则设备被钉死在坏号上，
/// [`valid_access_token_for_device`] 的重选循环会一直选回同一个，白转满上限。
#[test]
fn banned_credential_releases_its_devices() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).unwrap();

    // 先把设备粘到 a 上（a 是 id 更小的那个，同优先级下会被先选中）。
    let first = store
        .select_for_device(Select {
            device_id: Some("dev-1"),
            ttl_secs: 0,
            rate_limited: true,
            exclude: &[],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(first.id, a.id);
    // 再选一次仍命中既有绑定，确认粘性生效——这正是坏号会把设备钉死的原因。
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        a.id
    );

    // 模拟「a 的 refresh_token 被作废」后的停用。
    assert!(store.mark_banned(a.id, "[refresh 400] invalid_grant").unwrap());

    // 重选必须换到 b，而不是继续返回 a 或直接报错。
    let after = store
        .select_for_device(Select {
            device_id: Some("dev-1"),
            ttl_secs: 0,
            rate_limited: true,
            exclude: &[],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(after.id, b.id, "停用坏号后设备应改选到其它账号");

    // a 确实被停用并记了原因。
    let a2 = store.get(a.id).unwrap().unwrap();
    assert!(a2.disabled);
    assert_eq!(a2.ban_reason.as_deref(), Some("[refresh 400] invalid_grant"));

    // 池子空了要报错，而不是把停用的号又选回来。
    assert!(store.mark_banned(b.id, "[refresh 400] invalid_grant").unwrap());
    assert!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .is_err(),
        "无可用凭证时应报错"
    );
}

/// 模拟客户端（`sim:` 前缀）不写绑定，故此前在设备列表里完全看不到——用量与费用都在
/// `device_costs` 里，只是没人读。现在把它们作为伪设备追加在真实设备之后。
///
/// 同时钉住三件事：请求数**无条件**计（含没有 usage 的 4xx，否则限流排查时数字对不上）、
/// 不占 `device_count` 名额、跨账号合计仍然正确。
#[test]
fn lists_simulated_devices_with_request_counts() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let log = |cred: i64, dev: &str, cost: Option<f64>| {
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(cred),
                cred_label: "x".into(),
                device_id: Some(dev.into()),
                cost_usd: cost,
                ..Default::default()
            })
            .unwrap();
    };
    let sim = "sim:ff813c9166f0d2f3";
    log(a, sim, Some(0.01));
    log(a, sim, Some(0.02));
    // 模型认不出 → 无费用可计，但请求确实发生过，请求数照记。
    log(a, sim, None);
    // 同一个伪设备也可能落到别的账号上（换号重试／负载均衡）。
    log(b, sim, Some(0.05));

    let devs = store.list_devices(a).unwrap();
    assert_eq!(devs.len(), 1, "伪设备该出现在列表里: {devs:?}");
    let d = &devs[0];
    assert!(d.simulated, "该标记成模拟客户端");
    assert_eq!(d.device_id, sim);
    assert_eq!(d.request_count, 3, "没有 usage 的那条也要计数");
    assert!((d.cost_usd - 0.03).abs() < 1e-9, "本账号费用: {}", d.cost_usd);
    assert!((d.cost_usd_all - 0.08).abs() < 1e-9, "跨账号合计: {}", d.cost_usd_all);
    assert_eq!(d.created_at, None, "没有绑定就没有绑定时刻");
    assert_eq!(d.last_seen_at, None);

    // 不占设备名额——那是 device_bindings 的口径，伪设备一行都不写。
    assert_eq!(store.device_count(a).unwrap(), 0, "伪设备不该计入设备数");

    // 真实设备与伪设备并存时，真实的排在前面且不被标记。
    store.select_for_device(Select { device_id: Some("real-1"), ..Default::default() }).unwrap();
    log(a, "real-1", Some(1.0));
    let devs = store.list_devices(a).unwrap();
    assert_eq!(devs.len(), 2, "{devs:?}");
    assert!(!devs[0].simulated && devs[0].device_id == "real-1", "真实设备排前面: {devs:?}");
    assert!(devs[1].simulated, "伪设备排后面: {devs:?}");
    assert_eq!(store.device_count(a).unwrap(), 1, "只有真实设备占名额");
}

/// 设备明细必须与设备数同口径：条数等于 `device_count`、只含本凭证的绑定、
/// 超过 TTL 未活跃的不出现。否则后台会显示「设备 1/3，展开却列出 2 台」。
#[test]
fn list_devices_matches_device_count() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);

    // dev-1 粘到 a（同优先级下 id 小者先中），再来一次命中既有绑定、请求数 +1。
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        a
    );
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        a
    );
    // dev-2 是新设备：a 已有 1 台、b 还是 0 台，负载均衡会把它分给 b。
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-2"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        b
    );

    let a_devs = store.list_devices(a).unwrap();
    assert_eq!(a_devs.len() as i64, store.device_count(a).unwrap(), "条数应等于设备数");
    assert_eq!(a_devs.len(), 1, "只应列出绑到 a 的设备");
    assert_eq!(a_devs[0].device_id, "dev-1");
    assert_eq!(a_devs[0].request_count, 1, "第二次命中既有绑定应计数");
    assert_eq!(store.list_devices(b).unwrap()[0].device_id, "dev-2");

    // 把 dev-1 的活跃时间推到 TTL 之外：明细与计数应同步把它排除。
    store.set_setting(DEVICE_BINDING_TTL, "60").unwrap();
    store
        .conn
        .lock()
        .execute(
            "UPDATE device_bindings SET last_seen_at = unixepoch() - 600 WHERE device_id = ?1",
            ["dev-1"],
        )
        .unwrap();
    assert_eq!(store.device_count(a).unwrap(), 0);
    assert!(store.list_devices(a).unwrap().is_empty(), "超时绑定不应出现在明细里");
}

/// 把一条绑定的最后活跃时间往前推 `secs` 秒，模拟设备闲置。
fn age_binding(store: &CredentialStore, device_id: &str, secs: i64) {
    let n = store
        .conn
        .lock()
        .execute(
            "UPDATE device_bindings SET last_seen_at = unixepoch() - ?2 WHERE device_id = ?1",
            params![device_id, secs],
        )
        .unwrap();
    assert_eq!(n, 1, "要推的绑定得先存在");
}

/// 选号入参：TTL 一分钟、保留期一小时，即「名额一分钟就还、亲和性留一小时」。
fn soft(device_id: &str) -> Select<'_> {
    Select {
        device_id: Some(device_id),
        ttl_secs: 60,
        retention_secs: 3600,
        rate_limited: true,
        ..Default::default()
    }
}

/// 建库并把 TTL/保留期设成与 [`soft`] 一致——`device_count`/`list_devices` 读的是设置项，
/// 不跟着 `Select` 走，两边不一致的话断言的就不是同一套口径了。
fn soft_store(labels: &[&str]) -> (CredentialStore, Vec<i64>) {
    let (store, ids) = store_with(labels);
    store.set_setting(DEVICE_BINDING_TTL, "60").unwrap();
    store.set_setting(DEVICE_BINDING_RETENTION, "3600").unwrap();
    store.set_setting(SESSION_BINDING_TTL, "60").unwrap();
    store.set_setting(SESSION_BINDING_RETENTION, "3600").unwrap();
    (store, ids)
}

/// 模拟会话的选号入参：与 [`soft`] 同一套 TTL / 保留期，只是键换成会话键、没有设备身份。
fn soft_session(key: &str) -> Select<'_> {
    Select {
        session_key: Some(key),
        ttl_secs: 60,
        retention_secs: 3600,
        session_ttl_secs: 60,
        session_retention_secs: 3600,
        rate_limited: true,
        ..Default::default()
    }
}

/// 会话绑定的有效期与保留期是**单独**的一对设置：设备那对永不过期时，会话照样按自己的
/// TTL 释放名额、按自己的保留期清行；计数与明细读的也是会话那对。
#[test]
fn session_bindings_expire_on_their_own_ttl() {
    let (store, ids) = store_with(&["a"]);
    let a = ids[0];
    store.set_setting(DEVICE_BINDING_TTL, "0").unwrap();
    store.set_setting(SESSION_BINDING_TTL, "60").unwrap();
    store.set_setting(SESSION_BINDING_RETENTION, "600").unwrap();
    assert_eq!(store.session_binding_ttl(), 60);
    assert_eq!(store.session_binding_retention(), 600);
    fn sel(k: &str) -> Select<'_> {
        Select {
            session_key: Some(k),
            ttl_secs: 0,
            retention_secs: 0,
            session_ttl_secs: 60,
            session_retention_secs: 600,
            rate_limited: true,
            ..Default::default()
        }
    }
    assert_eq!(store.select_for_device(sel("s1")).unwrap().id, a);
    assert_eq!(store.session_count(a).unwrap(), 1);
    // 设备永不过期，会话 61 秒后不占名额；行还在（保留期 600 秒内），回来回原槽位。
    age_session_binding(&store, "s1", 61);
    assert_eq!(store.session_count(a).unwrap(), 0, "按会话自己的 TTL 释放");
    assert!(store.list_sessions(a).unwrap().is_empty());
    assert_eq!(store.session_slot(a, "s1").unwrap(), Some(0), "行还在");
    assert_eq!(store.select_for_device(sel("s2")).unwrap().id, a);
    assert_eq!(store.session_slot(a, "s2").unwrap(), Some(0), "释放了的槽位被复用");
    // 超过会话保留期：选号立刻当它不存在（行还在，由后台按自己的节奏删），s1 再来是新会话。
    age_session_binding(&store, "s1", 601);
    assert_eq!(store.select_for_device(sel("s3")).unwrap().id, a);
    assert_eq!(store.session_slot(a, "s1").unwrap(), Some(0), "行还在，后台还没跑");
    assert_eq!(store.prune_expired_bindings().unwrap(), (0, 1), "按会话自己的保留期清行");
    assert_eq!(store.session_slot(a, "s1").unwrap(), None);
    // 默认值：会话 30 分钟 / 1 天，与设备的 1 小时 / 7 天不同。
    let (fresh, _) = store_with(&["b"]);
    assert_eq!(fresh.session_binding_ttl(), DEFAULT_SESSION_BINDING_TTL_SECS);
    assert_eq!(fresh.session_binding_retention(), DEFAULT_SESSION_BINDING_RETENTION_SECS);
    assert_ne!(fresh.session_binding_ttl(), fresh.device_binding_ttl());
}

fn age_session_binding(store: &CredentialStore, key: &str, secs: i64) {
    let n = store
        .conn
        .lock()
        .execute(
            "UPDATE session_bindings SET last_seen_at = unixepoch() - ?2 WHERE session_key = ?1",
            params![key, secs],
        )
        .unwrap();
    assert_eq!(n, 1, "要推的绑定得先存在");
}

/// 模拟路径上没有设备身份的来访按会话键粘住账号并占**会话名额**：同键回同号、名额满了溢到
/// 别的号、全满时拒——与设备绑定逐条相同，但走另一张表、另一个上限，设备名额一个不占。
#[test]
fn session_key_binds_and_limits_simulated_sessions() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    store.set_setting(DEFAULT_SESSION_LIMIT, "1").unwrap();

    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, a);
    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, a, "同键回同号");
    assert_eq!(store.select_for_device(soft_session("s2")).unwrap().id, b, "a 满了溢到 b");
    let err = store.select_for_device(soft_session("s3")).unwrap_err();
    assert!(err.downcast_ref::<SessionLimitReached>().is_some(), "全满时拒: {err}");
    assert!(err.downcast_ref::<DeviceLimitReached>().is_none(), "拒的理由是会话不是设备");
    // 设备名额一个不占，会话名额各占一个；计数与明细同一口径。
    assert_eq!(store.device_count(a).unwrap(), 0);
    assert!(store.list_devices(a).unwrap().is_empty());
    assert_eq!(store.session_count(a).unwrap(), 1);
    assert_eq!(store.session_counts().unwrap().get(&b).copied(), Some(1));
    let list = store.list_sessions(a).unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].session_key, "s1");
    // 口径同设备绑定：建行那一轮不计，之后每命中一轮加一（`request_count` 的既有约定，
    // 见 `dormant_binding_still_steers_the_device_back_to_its_credential` 里的断言）。
    assert_eq!(list[0].request_count, 1, "第二轮命中既有绑定记一次");
    assert!(list[0].created_at > 0 && list[0].last_seen_at >= list[0].created_at);
    // 解绑腾出名额，s3 就能进来；再解一次是空操作。
    assert!(store.unbind_session(a, "s1").unwrap());
    assert!(!store.unbind_session(a, "s1").unwrap());
    assert_eq!(store.select_for_device(soft_session("s3")).unwrap().id, a);
    // 一键清空：只清这个号的，别的号不动；清完名额全空。
    assert_eq!(
        store.select_for_device(soft_session("s4")).unwrap_err().to_string(),
        SessionLimitReached.to_string()
    );
    assert_eq!(store.unbind_all_sessions(a).unwrap(), 1);
    assert_eq!(store.unbind_all_sessions(a).unwrap(), 0);
    assert_eq!(store.session_count(a).unwrap(), 0);
    assert_eq!(store.session_count(b).unwrap(), 1, "b 的不动");
    assert_eq!(store.select_for_device(soft_session("s4")).unwrap().id, a);
}

/// 设备身份在上游已收敛时（[`Select::per_session`]）带设备来访的选号入参：真实客户端，
/// 沿用来访会话 id，同 [`soft_session`] 的 TTL / 保留期。
fn per_session<'a>(device: &'a str, key: Option<&'a str>) -> Select<'a> {
    Select {
        device_id: Some(device),
        session_key: key,
        per_session: true,
        passthrough_session: true,
        ttl_secs: 60,
        retention_secs: 3600,
        session_ttl_secs: 60,
        session_retention_secs: 3600,
        rate_limited: true,
        ..Default::default()
    }
}

/// 按会话占名额的带设备来访：名额只看会话上限，设备上限不再生效；同一台设备的新会话跟着
/// 设备亲和落在上次那个号上（而不是被均衡到会话更少的号），那个号满了才溢出，亲和随之
/// 改到新号；真实客户端的会话不占派生槽位，后台列出的会话 id 就是来访那个。
#[test]
fn per_session_devices_are_limited_by_sessions_and_stick_to_their_home() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    store.set_setting(DEFAULT_DEVICE_LIMIT, "1").unwrap();
    store.set_setting(DEFAULT_SESSION_LIMIT, "2").unwrap();
    let sid1 = "lb:v2:sid:11111111-1111-4111-8111-111111111111";

    let (c, slot) = store.select_with_slot(per_session("d1", Some(sid1))).unwrap();
    assert_eq!((c.id, slot), (a, None), "沿用来访 id 的会话不带槽位回来");
    assert_eq!(store.session_slot(a, sid1).unwrap(), Some(PASSTHROUGH_SLOT));
    // 均衡会把 s2 放到会话更少的 b 上；亲和把它留在 d1 的号 a 上。
    assert_eq!(store.select_for_device(per_session("d1", Some("s2"))).unwrap().id, a);
    // 设备上限是 1，但不再生效：另一台设备照样进得来（a 满了，落到 b）。
    assert_eq!(store.select_for_device(per_session("d2", Some("s3"))).unwrap().id, b);
    // d1 的第三条会话：a 满了溢到 b，亲和跟着改到 b。
    assert_eq!(store.select_for_device(per_session("d1", Some("s4"))).unwrap().id, b);
    let home: i64 = store
        .conn
        .lock()
        .query_row("SELECT cred_id FROM device_bindings WHERE device_id = 'd1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(home, b);
    // 已有会话不受亲和影响，仍回原号。
    assert_eq!(store.select_for_device(per_session("d1", Some(sid1))).unwrap().id, a);
    // 全满时拒的是会话。
    let err = store.select_for_device(per_session("d3", Some("s5"))).unwrap_err();
    assert!(err.downcast_ref::<SessionLimitReached>().is_some(), "{err}");
    // 后台：真实会话的槽位是 -1，会话 id 是来访那个（这个号没有 account_uuid，钉不住）。
    let list = store.list_sessions(a).unwrap();
    let real = list.iter().find(|s| s.session_key == sid1).unwrap();
    assert_eq!(real.slot, PASSTHROUGH_SLOT);
    assert_eq!(real.session_id, "11111111-1111-4111-8111-111111111111");
    // 没带会话 id、按前缀指纹分的真实会话：出站没有会话 id，后台留空而不是把指纹当成 id。
    let pfx = "lb:v2:pfx:0123456789abcdef0123456789abcdef";
    assert!(store.unbind_session(a, sid1).unwrap());
    assert_eq!(store.select_for_device(per_session("d9", Some(pfx))).unwrap().id, a);
    let list = store.list_sessions(a).unwrap();
    assert_eq!(list.iter().find(|s| s.session_key == pfx).unwrap().session_id, "");
}

/// 按会话占名额而没有会话键的（额度探测）：按设备亲和选号，不写会话绑定、不占名额，
/// 名额全满时照样放行。
#[test]
fn per_session_probe_follows_device_home_without_taking_a_slot() {
    let (store, ids) = soft_store(&["a", "b"]);
    let b = ids[1];
    store.set_setting(DEFAULT_SESSION_LIMIT, "1").unwrap();
    // d1 的家在 b（a 先被别的设备占满）。
    assert_eq!(store.select_for_device(per_session("d0", Some("s0"))).unwrap().id, ids[0]);
    assert_eq!(store.select_for_device(per_session("d1", Some("s1"))).unwrap().id, b);
    // 两个号都满了，探测仍按亲和落到 b，且一条会话绑定都不写。
    assert_eq!(store.select_for_device(per_session("d1", None)).unwrap().id, b);
    assert_eq!(store.session_count(b).unwrap(), 1);
    assert_eq!(store.session_counts().unwrap().values().sum::<i64>(), 2);
}

/// 槽位分配里混着真实会话（-1）时，模拟会话照样从 0 起取最小空位。
#[test]
fn passthrough_sessions_do_not_take_derived_slots() {
    let (store, ids) = soft_store(&["a"]);
    let a = ids[0];
    assert_eq!(store.select_with_slot(per_session("d1", Some("real"))).unwrap().1, None);
    assert_eq!(store.select_with_slot(soft_session("sim1")).unwrap().1, Some(0));
    assert_eq!(store.select_with_slot(soft_session("sim2")).unwrap().1, Some(1));
    assert_eq!(store.session_count(a).unwrap(), 3, "真实会话照样占名额");
}

/// 匿名侧查询（[`Select::follow_only`]）：会话键有活跃绑定就跟到原号、带回原槽位，名额满了
/// 也照跟；找不到或已休眠就按负载选号。两种情况都不写绑定、不占名额，原绑定行一个字不动。
#[test]
fn follow_only_side_queries_never_bind() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    store.set_setting(DEFAULT_SESSION_LIMIT, "1").unwrap();
    let side = |key| Select { follow_only: true, ..soft_session(key) };

    let (c, slot) = store.select_with_slot(soft_session("s1")).unwrap();
    assert_eq!((c.id, slot), (a, Some(0)));
    let before = store.list_sessions(a).unwrap();
    // a 的名额已满，侧查询照样跟过去，带回主线程那个槽位。
    let (c, slot) = store.select_with_slot(side("s1")).unwrap();
    assert_eq!((c.id, slot), (a, Some(0)), "跟随主线程");
    assert_eq!(
        store.list_sessions(a).unwrap()[0].request_count,
        before[0].request_count,
        "不动绑定行"
    );
    // 没有主线程的键：不建绑定、不占名额，也不因名额满被拒。
    let (_, slot) = store.select_with_slot(side("lost")).unwrap();
    assert_eq!(slot, None);
    assert_eq!(store.session_slot(a, "lost").unwrap(), None);
    assert_eq!(store.session_slot(b, "lost").unwrap(), None);
    assert_eq!(store.session_counts().unwrap().get(&b).copied(), None, "b 一个名额都没占");
    // 主线程休眠：不跟（槽位可能已让给别的对话），按负载选、槽位不带回。
    age_session_binding(&store, "s1", 600);
    assert_eq!(store.select_with_slot(side("s1")).unwrap().1, None);
    assert_eq!(store.session_count(a).unwrap(), 0, "休眠的那条没被侧查询续上");
}

/// 会话槽位：新对话取该号上最小的空位，同键续用原槽位；休眠后原槽位被别的对话拿走就换
/// 最小空位、空着就回原位；解绑腾出的槽位被下一个对话复用；槽位派生的会话 id 恒定且各不同。
#[test]
fn session_slots_are_reused_by_later_conversations() {
    let (store, ids) = soft_store(&["a"]);
    let a = ids[0];
    let slot = |k: &str| store.session_slot(a, k).unwrap();
    // 选号直接把槽位带回来，与事后按键查的一致；设备绑定与裸请求没有槽位。
    let (c, s) = store.select_with_slot(soft_session("s1")).unwrap();
    assert_eq!((c.id, s), (a, Some(0)));
    let (c, s) = store.select_with_slot(soft_session("s2")).unwrap();
    assert_eq!((c.id, s), (a, Some(1)));
    let (c, s) = store.select_with_slot(soft_session("s1")).unwrap();
    assert_eq!((c.id, s), (a, Some(0)), "续用原槽位也带回来");
    assert_eq!(store.select_with_slot(soft("dev-1")).unwrap().1, None);
    assert_eq!(
        store.select_with_slot(Select { rate_limited: true, ..Default::default() }).unwrap().1,
        None
    );
    assert_eq!((slot("s1"), slot("s2")), (Some(0), Some(1)), "按最小空位分，同键续用");
    assert_eq!(slot("nope"), None);
    // s1 休眠，s3 来了拿走槽位 0；s1 回来只能拿 2。
    age_session_binding(&store, "s1", 600);
    assert_eq!(store.select_for_device(soft_session("s3")).unwrap().id, a);
    assert_eq!(slot("s3"), Some(0), "休眠绑定的槽位算空位");
    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, a);
    assert_eq!(slot("s1"), Some(2), "原槽位被占就取最小空位");
    // s2 休眠后没人占它的槽位，回来还是 1。
    age_session_binding(&store, "s2", 600);
    assert_eq!(store.select_for_device(soft_session("s2")).unwrap().id, a);
    assert_eq!(slot("s2"), Some(1), "原槽位空着就回原位");
    // 解绑 s3，下一个对话复用槽位 0。
    assert!(store.unbind_session(a, "s3").unwrap());
    assert_eq!(store.select_for_device(soft_session("s4")).unwrap().id, a);
    assert_eq!(slot("s4"), Some(0));
    // 列表带槽位与派生的会话 id：同槽位同 id、不同槽位不同 id、形态是 uuid。
    let list = store.list_sessions(a).unwrap();
    let cred = store.get(a).unwrap().unwrap();
    for s in &list {
        assert_eq!(
            s.session_id,
            crate::credentials::sim_slot_session_id(cred.account_uuid.as_deref(), a, s.slot)
        );
        assert_eq!(s.session_id.len(), 36, "{}", s.session_id);
    }
    let mut sids: Vec<&str> = list.iter().map(|s| s.session_id.as_str()).collect();
    sids.sort();
    sids.dedup();
    assert_eq!(sids.len(), list.len(), "各槽位的会话 id 互不相同: {list:?}");
    assert_ne!(
        crate::credentials::sim_slot_session_id(Some("u"), 1, 0),
        crate::credentials::sim_slot_session_id(Some("v"), 1, 0),
        "换账号另一组"
    );
    // 没有 account_uuid 的两个号（刚登录还没拉到 profile、或旧库）也不能算出同一组：
    // 按凭证 id 分；同一个号有没有拉到 uuid 会是两组，那是 uuid 回填那一刻的一次性切换。
    assert_ne!(
        crate::credentials::sim_slot_session_id(None, 1, 0),
        crate::credentials::sim_slot_session_id(None, 2, 0),
        "两个没有 uuid 的号，同槽位不同 id"
    );
    assert_eq!(
        crate::credentials::sim_slot_session_id(None, 1, 0),
        crate::credentials::sim_slot_session_id(Some("  "), 1, 0),
        "空白 uuid 当没有"
    );
    // 后台列表对没有 uuid 的号也与转发路径同一口径。
    let (store2, ids2) = soft_store(&["x", "y"]);
    assert_eq!(store2.select_for_device(soft_session("k")).unwrap().id, ids2[0]);
    let listed = store2.list_sessions(ids2[0]).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session_id, crate::credentials::sim_slot_session_id(None, ids2[0], 0));
    assert_ne!(listed[0].session_id, crate::credentials::sim_slot_session_id(None, ids2[1], 0));
}

/// 没有 slot 列的旧库（v0.3.126 / 127）升上来：补列并清掉存量行，再跑一遍不动。
#[test]
fn migrating_session_bindings_adds_the_slot_column_and_clears_old_rows() {
    // 先建齐全库并放一个真实凭证（无主行会被 purge_orphan_rows 扫掉，那不是这里要验的），
    // 再把 session_bindings 换成 v0.3.126 那张没有 slot 列的表。
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO credentials (id, label, access_token, refresh_token, expires_at) \
             VALUES (1, 'a', 't', 'r', 0)",
        [],
    )
    .unwrap();
    conn.execute_batch(
        "DROP TABLE session_bindings;
             CREATE TABLE session_bindings (
                session_key TEXT PRIMARY KEY, cred_id INTEGER NOT NULL,
                request_count INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                last_seen_at INTEGER NOT NULL DEFAULT (unixepoch())) STRICT;
             INSERT INTO session_bindings (session_key, cred_id) VALUES ('old', 1);",
    )
    .unwrap();
    init_schema(&conn).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM session_bindings", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "存量行清掉");
    // 键要带版本前缀，否则会被「清掉旧口径的行」那条语句扫走（见下一条测试）。
    conn.execute(
        "INSERT INTO session_bindings (session_key, cred_id, slot) VALUES ('lb:v2:pfx:new', 1, 3)",
        [],
    )
    .unwrap();
    init_schema(&conn).unwrap();
    let slot: i64 = conn
        .query_row(
            "SELECT slot FROM session_bindings WHERE session_key = 'lb:v2:pfx:new'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(slot, 3, "列已在就什么都不动");
}

/// 旧口径算出来的会话键（没有 `lb:v2:` 前缀）在开库时清掉，带版本的留着。口径见
/// `crate::proxy::session_binding_key`：两版的键都是 32 个 hex，不靠前缀分不出来。
#[test]
fn session_bindings_from_an_older_key_scheme_are_dropped() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO credentials (id, label, access_token, refresh_token, expires_at) \
             VALUES (1, 'a', 't', 'r', 0)",
        [],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO session_bindings (session_key, cred_id, slot) VALUES
                ('3f9a1c7e5b2d4680a1b2c3d4e5f60718', 1, 0),
                ('lb:v2:pfx:3f9a1c7e5b2d4680a1b2c3d4e5f60718', 1, 1),
                ('lb:v2:sid:7fe47444-c834-44e0-b568-d61e07daa35e', 1, 2);",
    )
    .unwrap();
    init_schema(&conn).unwrap();
    let keys: Vec<String> = conn
        .prepare("SELECT session_key FROM session_bindings ORDER BY slot")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        keys,
        vec![
            "lb:v2:pfx:3f9a1c7e5b2d4680a1b2c3d4e5f60718".to_string(),
            "lb:v2:sid:7fe47444-c834-44e0-b568-d61e07daa35e".to_string(),
        ],
        "旧口径那条清掉，带版本的两条留着"
    );
}

/// 绑定行记下最近一轮的模型：**只给后台列**，不参与键也不参与选号——同一条对话换个模型
/// 仍是同一条会话、占同一份名额（键里带模型的代价见 `crate::proxy::session_binding_key`）。
/// 没带模型的那轮（`count_tokens` 之类）保留上一轮的值。
#[test]
fn the_session_binding_records_its_latest_model() {
    let (store, ids) = soft_store(&["a"]);
    let a = ids[0];
    let on = |model| Select { model, ..soft_session("s1") };
    store.select_for_device(on(Some("claude-opus-5"))).unwrap();
    assert_eq!(store.list_sessions(a).unwrap()[0].last_model.as_deref(), Some("claude-opus-5"));
    store.select_for_device(on(Some("claude-fable-5-1"))).unwrap();
    let list = store.list_sessions(a).unwrap();
    assert_eq!(list.len(), 1, "换模型不另起会话: {list:?}");
    assert_eq!(list[0].last_model.as_deref(), Some("claude-fable-5-1"), "记最近那轮");
    store.select_for_device(on(None)).unwrap();
    assert_eq!(
        store.list_sessions(a).unwrap()[0].last_model.as_deref(),
        Some("claude-fable-5-1"),
        "没带模型的那轮不抹掉上一轮"
    );
}

/// 带设备身份的请求即使也带了会话键，只按设备绑定：一条请求不占两份名额。
#[test]
fn device_id_takes_precedence_over_the_session_key() {
    let (store, ids) = soft_store(&["a"]);
    let a = ids[0];
    let sel = Select { device_id: Some("dev-1"), ..soft_session("s1") };
    assert_eq!(store.select_for_device(sel).unwrap().id, a);
    assert_eq!(store.device_count(a).unwrap(), 1);
    assert_eq!(store.session_count(a).unwrap(), 0, "没写会话绑定");
    assert!(store.list_sessions(a).unwrap().is_empty());
}

/// 会话上限三态同设备上限：`0` 跟随全局、`> 0` 独立上限、`< 0` 明确不限；与设备上限互不
/// 影响——设备上限 1 时会话照样能开好几条，会话占满也不妨碍设备绑定。
#[test]
fn session_limit_tri_state_is_independent_of_the_device_limit() {
    let (store, ids) = soft_store(&["a"]);
    let a = ids[0];
    store.set_setting(DEFAULT_SESSION_LIMIT, "1").unwrap();
    store.set_setting(DEFAULT_DEVICE_LIMIT, "1").unwrap();
    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, a);
    assert!(
        store
            .select_for_device(soft_session("s2"))
            .unwrap_err()
            .downcast_ref::<SessionLimitReached>()
            .is_some()
    );
    // 账号独立上限 3。
    assert!(store.set_session_limit(a, 3).unwrap());
    assert_eq!(store.get(a).unwrap().unwrap().session_limit, 3);
    assert_eq!(store.select_for_device(soft_session("s2")).unwrap().id, a);
    assert_eq!(store.select_for_device(soft_session("s3")).unwrap().id, a);
    assert!(
        store
            .select_for_device(soft_session("s4"))
            .unwrap_err()
            .downcast_ref::<SessionLimitReached>()
            .is_some()
    );
    // 明确不限（批量接口）。
    assert_eq!(store.set_session_limits(&[a], -1).unwrap(), 1);
    assert_eq!(store.select_for_device(soft_session("s4")).unwrap().id, a);
    assert_eq!(store.select_for_device(soft_session("s5")).unwrap().id, a);
    assert_eq!(store.session_count(a).unwrap(), 5);
    // 设备名额仍是 0/1：会话一个都没占它。
    assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, a);
    assert!(
        store
            .select_for_device(soft("dev-2"))
            .unwrap_err()
            .downcast_ref::<DeviceLimitReached>()
            .is_some(),
        "设备上限照旧只管设备"
    );
    assert_eq!(effective_session_limit(0, 7), 7);
    assert_eq!(effective_session_limit(-1, 7), 0);
    assert_eq!(effective_session_limit(2, 7), 2);
    assert_eq!(store.default_session_limit(), 1);
}

/// 停用 / 封停 / 删除账号都要连带清掉它的会话绑定，否则会话被钉死在坏号上——与设备绑定
/// 那条（`banned_credential_releases_its_devices`）同一个道理。
#[test]
fn disabling_or_deleting_a_credential_releases_its_sessions() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, a);
    assert!(store.set_disabled(a, true).unwrap());
    assert_eq!(store.session_count(a).unwrap(), 0, "停用清绑定");
    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, b, "改选到 b");
    assert!(store.set_disabled(a, false).unwrap());
    assert!(store.mark_banned(b, "[401] revoked").unwrap());
    assert_eq!(store.session_count(b).unwrap(), 0, "封停清绑定");
    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, a);
    assert!(store.delete(a).unwrap());
    let n: i64 = store
        .conn
        .lock()
        .query_row("SELECT COUNT(*) FROM session_bindings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "删号不留无主的会话绑定");
}

/// 休眠的会话软绑定：TTL 过了名额还回去，会话再来仍优先回原号；保留期过了才真删、
/// 回来就是新会话。
#[test]
fn dormant_session_binding_steers_the_session_back() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, a);
    age_session_binding(&store, "s1", 600);
    assert_eq!(store.session_count(a).unwrap(), 0, "休眠不占名额");
    assert!(store.list_sessions(a).unwrap().is_empty(), "明细口径同计数");
    assert_eq!(store.select_for_device(soft_session("s2")).unwrap().id, a);
    // 此刻 a 有 1 条活跃、b 一条没有，纯负载均衡会把 s1 判给 b。
    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, a, "软绑定带回 a");
    assert_eq!(store.session_count(a).unwrap(), 2);
    // 超过保留期：行被清掉，回来就是新会话，按负载均衡落到 b。
    age_session_binding(&store, "s1", 7200);
    assert_eq!(store.select_for_device(soft_session("s1")).unwrap().id, b);
}

/// 软绑定：TTL 过了名额就还回去，但设备再来时仍优先回原号——哪怕负载均衡指向别处。
///
/// 这是 thinking 签名能续上的前提：签名跟着账号走，会话隔一小时再续跑要是换了号，
/// 之后每一轮都要先撞一次 400 再降级重发（见 `crate::proxy::retry_demoted_thinking`）。
#[test]
fn dormant_binding_still_steers_the_device_back_to_its_credential() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);

    assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, a);
    age_binding(&store, "dev-1", 600);
    assert_eq!(store.device_count(a).unwrap(), 0, "休眠绑定不占名额");

    // 休眠期间来了台新设备：名额是空的，照样分给 a（同优先级取 id 小者）。
    assert_eq!(store.select_for_device(soft("dev-2")).unwrap().id, a);
    // 此刻 a 有 1 台活跃设备、b 一台都没有，纯负载均衡会把 dev-1 判给 b。
    assert_eq!(store.device_counts().unwrap().get(&b).copied().unwrap_or(0), 0);
    assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, a, "软绑定应把它带回 a");
    assert_eq!(store.device_count(a).unwrap(), 2, "回来就重新占名额");
}

/// 软绑定是「优先」不是「特权」：原号名额已满时照常改选并改绑，否则设备上限就被绕过了。
#[test]
fn dormant_binding_gives_way_when_its_credential_is_full() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    assert!(store.set_device_limit(a, 1).unwrap());

    assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, a);
    age_binding(&store, "dev-1", 600);
    // 休眠腾出的那个名额被 dev-2 占走。
    assert_eq!(store.select_for_device(soft("dev-2")).unwrap().id, a);

    // dev-1 回来时 a 已满：改选到 b，并且绑定要真的改过去（而不是留在 a 上）。
    assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, b);
    assert_eq!(store.device_count(a).unwrap(), 1);
    let a_devs: Vec<String> =
        store.list_devices(a).unwrap().into_iter().map(|d| d.device_id).collect();
    assert_eq!(a_devs, vec!["dev-2".to_string()]);
    assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, b, "改绑后应稳定在 b");
}

/// 保留期到点后设备就是台新设备，回不去原号——**与行删没删无关**。
///
/// 删行这件事挪去了后台（[`CredentialStore::prune_expired_bindings`]，见那里的记述），
/// 故这条用例钉的是两件事：一、行还在表里的时候选号就已经不认它了（否则后台跑之前那段
/// 时间里，设备会被送回一个本该忘掉的号）；二、后台真跑的时候那行会被删掉。
#[test]
fn binding_rows_are_forgotten_once_the_retention_window_passes() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);

    assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, a);
    age_binding(&store, "dev-1", 7200);
    // 让 a 上多一台活跃设备，好让下面的负载均衡有个明确去向。
    assert_eq!(store.select_for_device(soft("dev-2")).unwrap().id, a);

    let rows = |store: &CredentialStore| -> i64 {
        store
            .conn
            .lock()
            .query_row("SELECT COUNT(*) FROM device_bindings WHERE device_id = 'dev-1'", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    assert_eq!(rows(&store), 1, "后台还没跑，行还在表里");
    assert_eq!(
        store.select_for_device(soft("dev-1")).unwrap().id,
        b,
        "行还在也不算数：过了保留期就按负载均衡走"
    );

    // 上面那一发已经把 dev-1 改绑到 b 并刷新了 last_seen_at，故这行现在是活的、不该被清。
    assert_eq!(store.prune_expired_bindings().unwrap(), (0, 0), "活着的绑定不动");
    age_binding(&store, "dev-1", 7200);
    assert_eq!(store.prune_expired_bindings().unwrap(), (1, 0), "过了保留期的由后台删掉");
    assert_eq!(rows(&store), 0);
}

/// 后台清理认的是**当前配置**：保留期配成「永久」（`<= 0`）时一行都不许删。
#[test]
fn pruning_bindings_respects_a_forever_retention() {
    let (store, ids) = soft_store(&["a"]);
    assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, ids[0]);
    age_binding(&store, "dev-1", 7200);
    store.set_setting(DEVICE_BINDING_RETENTION, "0").unwrap();
    assert_eq!(store.prune_expired_bindings().unwrap(), (0, 0), "永久保留即一行不删");
}

/// 后台清理那两条 DELETE 必须走 `last_seen_at` 上的索引，不能退成整表扫。
///
/// 钉住它是因为这件事**刚从选号路径上挪下来**：会话绑定表按每条对话一行、默认留 24 小时，
/// 多客户端时几万行是常态，而那两条 DELETE 当年就是在整表扫。索引哪天被人顺手删掉，
/// 症状只是「后台任务慢一点」，没有任何人会注意到——除非这里拦一道。
#[test]
fn pruning_bindings_uses_the_last_seen_index() {
    let (store, _) = soft_store(&["a"]);
    let conn = store.conn.lock();
    for table in ["device_bindings", "session_bindings"] {
        let plan: String = conn
            .query_row(
                &format!(
                    "EXPLAIN QUERY PLAN \
                         DELETE FROM {table} WHERE last_seen_at < unixepoch() - 600"
                ),
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains(&format!("idx_{table}_seen")),
            "{table} 的保留期清理退成了整表扫：{plan}"
        );
    }
}

#[test]
fn effective_retention_tri_state() {
    assert_eq!(effective_retention(60, 3600), Some(3600), "正常配置按保留期删");
    assert_eq!(effective_retention(60, 30), Some(60), "保留期短于 TTL 时按 TTL 兜底");
    assert_eq!(effective_retention(60, 0), None, "保留期为 0 = 永久保留");
    assert_eq!(effective_retention(0, 3600), None, "绑定永不过期时不删任何行");
}

/// 设备明细里的费用：本账号一列只算本账号花的，跨账号合计要把换号前的也算进去，
/// 且不因解绑/重绑而归零（用量日志与绑定行是两套账）。
#[test]
fn list_devices_sums_cost_per_device() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        a
    );
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-2"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        b
    );

    // dev-1 在 a 上花了 0.5+0.25，换号后在 b 上又花了 1.0；dev-2 只在 b 上花了 0.125。
    log_cost(&store, a, "dev-1", Some(0.5));
    log_cost(&store, a, "dev-1", Some(0.25));
    log_cost(&store, b, "dev-1", Some(1.0));
    log_cost(&store, b, "dev-2", Some(0.125));
    // 模型未知的请求 cost_usd 为空，SUM 要能跳过而不是把整行算成 NULL。
    log_cost(&store, a, "dev-1", None);

    let d = &store.list_devices(a).unwrap()[0];
    assert_eq!(d.device_id, "dev-1");
    assert!((d.cost_usd - 0.75).abs() < 1e-9, "本账号只算 a 上的花费：{}", d.cost_usd);
    assert!((d.cost_usd_all - 1.75).abs() < 1e-9, "合计要含 b 上的：{}", d.cost_usd_all);

    // 没有任何用量日志的设备给 0，而不是 NULL 取值失败。
    assert_eq!(
        store.list_devices(b).unwrap().iter().find(|x| x.device_id == "dev-2").unwrap().cost_usd,
        0.125
    );

    // 解绑再重绑：请求数从零重数，费用是历史累计，不受影响。
    assert!(store.unbind_device(a, "dev-1").unwrap());
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        a
    );
    let d = &store.list_devices(a).unwrap()[0];
    assert_eq!(d.request_count, 0, "重绑后是新的一条绑定");
    assert!((d.cost_usd - 0.75).abs() < 1e-9, "费用不该被解绑清掉");
}

/// 走真实写入口落一条用量日志（只填与费用统计相关的字段）。
fn log_cost(store: &CredentialStore, cred_id: i64, device_id: &str, cost: Option<f64>) {
    store
        .insert_usage_log(&UsageRecord {
            cred_id: Some(cred_id),
            device_id: Some(device_id.to_string()),
            path: "/v1/messages".to_string(),
            status: 200,
            has_usage: true,
            cost_usd: cost,
            ..Default::default()
        })
        .unwrap();
}

/// 手动解绑：立刻腾出名额（计数与明细同步减一）、只动本凭证名下的那条绑定、
/// 重复解绑返回 false（后台据此给 404，而不是静默成功）。
#[test]
fn unbind_device_frees_slot_and_is_scoped_to_credential() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);

    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        a
    );
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-2"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        b
    );

    // 拿 b 的 id 去解 dev-1（模拟后台列表已过期、设备其实绑在 a 上）：不能误伤 a 的绑定。
    assert!(!store.unbind_device(b, "dev-1").unwrap(), "跨凭证解绑应无效");
    assert_eq!(store.device_count(a).unwrap(), 1, "误删他号绑定会让名额凭空消失");

    assert!(store.unbind_device(a, "dev-1").unwrap());
    assert_eq!(store.device_count(a).unwrap(), 0, "解绑后名额应立刻释放");
    assert!(store.list_devices(a).unwrap().is_empty());
    assert_eq!(store.device_count(b).unwrap(), 1, "不应波及其它账号");

    // 已经没有这条绑定了：再解一次要报「没删到」。
    assert!(!store.unbind_device(a, "dev-1").unwrap());

    // 解绑不是拉黑：设备下次请求重新走选号，仍可能落回同一个账号。
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        a
    );
    assert_eq!(store.device_count(a).unwrap(), 1);
}

/// 分位数来自指数直方图（OTel scale 5）：与 nearest-rank 精确值的相对误差在 1.1% 以内。
fn assert_close(est: i64, exact: i64) {
    let tol = (exact as f64 * 0.011).max(1.0);
    assert!((est - exact).abs() as f64 <= tol, "估计 {est} 偏离精确值 {exact} 超过 1.1%");
}

/// 延迟与缓存的趋势口径：分位数 nearest-rank（直方图近似）、桶边界按时区偏移切、
/// 吞吐只算生成阶段、汇总对整窗口算而不是各桶平均；按模型 / 按账号的拆分同一套数。
/// 全部读预聚合（usage_rollup），不扫流水。
#[test]
fn latency_and_cache_series_use_percentiles_and_local_buckets() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let now: i64 = store.conn.lock().query_row("SELECT unixepoch()", [], |r| r.get(0)).unwrap();
    // 最近那批记录（[last-104, last]）必须落在同一个小时桶里、又在真实时间的近一小时内
    // （`*_report` 的 recent 按 unixepoch() 往前一小时算，不能把 now 挪走）。整点后十来分钟内
    // 照 now-600 写会跨过整点、被拆成两个桶，桶数断言随钟点时红时绿；那时就整批放到上一个
    // 小时的末尾。
    let hour_start = now - now.rem_euclid(3600);
    let last = if now - hour_start >= 700 { now - 500 } else { hour_start - 1 };
    let rec = |cred: i64,
               model: &str,
               status: u16,
               ttft: i64,
               total: i64,
               out: i64,
               inp: i64,
               read: i64,
               write: i64| UsageRecord {
        cred_id: Some(cred),
        cred_label: if cred == a { "a".into() } else { "b".into() },
        model: Some(model.into()),
        status,
        has_usage: true,
        input_tokens: Some(inp),
        output_tokens: Some(out),
        cache_creation_tokens: Some(write),
        cache_read_tokens: Some(read),
        ttft_ms: Some(ttft),
        total_ms: Some(total),
        ..Default::default()
    };
    // 最近半小时：opus 五条成功（TTFT 100..500，吞吐 1000 token / 1s），一条失败（不进延迟）。
    for (i, ttft) in [100i64, 200, 300, 400, 500].iter().enumerate() {
        store
            .insert_usage_log_at(
                &rec(a, "claude-opus-5", 200, *ttft, ttft + 1000, 1000, 100, 60, 20),
                Some(last - 100 - i as i64),
            )
            .unwrap();
    }
    store
        .insert_usage_log_at(&rec(a, "claude-opus-5", 500, 9000, 9000, 0, 100, 0, 0), Some(last))
        .unwrap();
    // 三小时前：sonnet 在 b 上一条慢的，没有缓存。
    store
        .insert_usage_log_at(
            &rec(b, "claude-sonnet-5", 200, 4000, 4000, 0, 100, 0, 0),
            Some(now - 3 * 3600),
        )
        .unwrap();

    // 分位数：p50 = 300、p95 = 500（nearest-rank：ceil(5×0.95)=5）、平均 300；吞吐 1000 tok/s。
    let recent = store.ttft_summary(now - 3600).unwrap();
    assert_eq!((recent.count, recent.avg_ms), (5, 300), "条数与平均是精确累加的");
    assert_close(recent.p50_ms, 300);
    assert_close(recent.p95_ms, 500);
    assert!((recent.tokens_per_sec.unwrap() - 1000.0).abs() < 1e-6, "{:?}", recent.tokens_per_sec);
    // 整窗口（含三小时前那条）：六条，p50 取第 3 个 = 300，p95 取第 6 个 = 4000。
    let all = store.ttft_summary(now - 6 * 3600).unwrap();
    assert_eq!(all.count, 6);
    assert_close(all.p50_ms, 300);
    assert_close(all.p95_ms, 4000);
    assert_eq!(all.ts, now - 6 * 3600);
    // 逐小时桶：两个桶，慢的那条在自己的桶里；桶起点按偏移对齐。
    let hourly = store.ttft_series(now - 6 * 3600, 3600, 0).unwrap();
    assert_eq!(hourly.len(), 2, "{hourly:?}");
    assert_eq!(hourly[0].count, 1);
    assert_close(hourly[0].p95_ms, 4000);
    assert_eq!(hourly[0].tokens_per_sec, None, "没有输出 token 就没有吞吐");
    assert_eq!(hourly[0].ts % 3600, 0);
    let tz = 8 * 3600;
    let shifted = store.ttft_series(now - 6 * 3600, 86400, tz).unwrap();
    assert!(shifted.iter().all(|b| (b.ts + tz) % 86400 == 0), "日桶边界落在本地零点: {shifted:?}");
    // 一次扫描出三样，与分开算的一致。
    let report = store.ttft_report(now - 6 * 3600, 3600, 0).unwrap();
    assert_eq!(report.points.len(), 2);
    assert_eq!((report.summary.count, report.recent.count), (6, 5));
    assert_close(report.summary.p95_ms, 4000);
    assert_close(report.recent.p50_ms, 300);
    // 一个空集：全零、吞吐 None。
    let none = store.ttft_summary(now + 10).unwrap();
    assert_eq!((none.count, none.p50_ms), (0, 0));
    assert_eq!(none.tokens_per_sec, None);

    // 缓存：近一小时六条（含失败那条）输入 6×180 = 1080（100 裸 + 60 命中 + 20 写入 各五条，
    // 失败那条 100）——五条各 180、一条 100 → 1000；命中 300、写入 100。
    let cache = store.cache_summary(now - 3600).unwrap();
    assert_eq!((cache.input_tokens, cache.cached_tokens, cache.written_tokens), (1000, 300, 100));
    assert_eq!(cache.ts, now - 3600);
    let cache_all = store.cache_summary(now - 6 * 3600).unwrap();
    assert_eq!(cache_all.input_tokens, 1100);
    let cache_hourly = store.cache_series(now - 6 * 3600, 3600, 0).unwrap();
    assert_eq!(cache_hourly.len(), 2);
    assert_eq!(cache_hourly[1].written_tokens, 100);
    let cache_none = store.cache_summary(now + 10).unwrap();
    assert_eq!(cache_none.input_tokens, 0);
    let cache_report = store.cache_report(now - 6 * 3600, 3600, 0).unwrap();
    assert_eq!(cache_report.points.len(), 2);
    assert_eq!(cache_report.summary.input_tokens, 1100, "合计是各桶之和");
    assert_eq!(cache_report.recent.cached_tokens, 300);

    // 趋势与拆分都读预聚合，按主键（维度 + 桶）范围扫描，不碰流水表。
    let plan = {
        let conn = store.conn.lock();
        let mut stmt = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT bucket, key, label, requests, ttft_hist \
                   FROM usage_rollup WHERE dim = ?1 AND bucket >= ?2 ORDER BY bucket",
            )
            .unwrap();
        stmt.query_map(params!["model", 0i64], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join(" | ")
    };
    assert!(plan.contains("SEARCH usage_rollup USING PRIMARY KEY (dim=? AND bucket>?)"), "{plan}");

    // 拆分：按模型请求数降序（opus 6 条在前），延迟只算成功的；按账号带 label，limit 生效。
    let by_model = store.usage_breakdown(now - 6 * 3600, BreakdownBy::Model, 10).unwrap();
    assert_eq!(by_model.len(), 2);
    assert_eq!(by_model[0].key, "claude-opus-5");
    assert_eq!(by_model[0].requests, 6);
    assert_eq!(by_model[0].latency.count, 5);
    assert_close(by_model[0].latency.p50_ms, 300);
    assert_eq!(by_model[0].cache.cached_tokens, 300);
    assert_eq!(by_model[1].key, "claude-sonnet-5");
    assert_close(by_model[1].latency.p95_ms, 4000);
    // 省钱：opus-5 输入 $5/MTok，命中 300 省 0.9×5×300e-6 = 0.00135，写入 100（5m 档）多付
    // 0.25×5×100e-6 = 0.000125 → 0.001225；sonnet 那组没缓存是 0；按模型拆没有套餐。
    assert!(
        (by_model[0].cache_saved_usd - 0.001225).abs() < 1e-9,
        "{}",
        by_model[0].cache_saved_usd
    );
    assert_eq!(by_model[1].cache_saved_usd, 0.0);
    assert_eq!(by_model[0].tier, None);
    let _ = store.set_tier(a, Some("Max 5x"));
    let by_account = store.usage_breakdown(now - 6 * 3600, BreakdownBy::Account, 10).unwrap();
    assert_eq!(by_account[0].key, a.to_string());
    assert_eq!(by_account[0].label, "a");
    assert_eq!(by_account[1].label, "b");
    assert_eq!(by_account[1].tier, None);
    assert_eq!(store.usage_breakdown(now - 6 * 3600, BreakdownBy::Account, 1).unwrap().len(), 1);
    // 删掉的号：流水还在，label 退回流水自带的账号名。
    assert!(store.delete(b).unwrap());
    let deleted = store.usage_breakdown(now - 6 * 3600, BreakdownBy::Account, 10).unwrap();
    assert!(deleted.iter().any(|r| r.key == b.to_string() && r.label == "b"), "{deleted:?}");
    // 流水里也没名字（极老的行）才退成 #id。
    let ghost = b + 100;
    store
        .insert_usage_log_at(
            &UsageRecord {
                cred_label: String::new(),
                ..rec(ghost, "claude-sonnet-5", 200, 10, 20, 1, 1, 0, 0)
            },
            Some(now - 100),
        )
        .unwrap();
    let orphan = store.usage_breakdown(now - 3600, BreakdownBy::Account, 10).unwrap();
    assert!(orphan.iter().any(|r| r.label == format!("#{ghost}")), "{orphan:?}");

    // 本地拒绝按原因分类：`rejected_locally:<kind>` 归到 kind，裸的归 other，按条数降序。
    let reject = |tag: &str| UsageRecord {
        status: 429,
        forensics: Forensics { rewrites: Some(tag.into()), ..Default::default() },
        ..Default::default()
    };
    for tag in [
        "rejected_locally:device-limit",
        "rejected_locally:device-limit",
        "rejected_locally:session-limit",
        "rejected_locally",
    ] {
        store.insert_usage_log_at(&reject(tag), Some(now - 60)).unwrap();
    }
    store
        .insert_usage_log_at(&reject("rejected_locally:device-limit"), Some(now - 2 * 3600))
        .unwrap();
    let rejections = store.local_rejections(now - 3600).unwrap();
    assert_eq!(
        rejections,
        vec![
            ("device-limit".to_string(), 2),
            ("other".to_string(), 1),
            ("session-limit".to_string(), 1)
        ]
    );
    // 流水按模型与起点筛：拆分表点进来看明细走的就是这两个条件。
    let logs = store
        .query_usage_logs(UsageLogQuery {
            model: Some("claude-sonnet-5".into()),
            since: Some(now - 6 * 3600),
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert!(!logs.is_empty() && logs.iter().all(|l| l.model.as_deref() == Some("claude-sonnet-5")));
    let stats = store
        .usage_log_stats(UsageLogQuery {
            model: Some("claude-opus-5".into()),
            since: Some(now - 3600),
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(stats.total, 6);
}

/// 单账号统计：只算这个号、按起点截断；桶按时区偏移切；四个维度各自按请求数降序；
/// 错误与本地拒绝分开数；缓存写在只有细分档的老记录上拿两档相加。
#[test]
fn credential_stats_buckets_and_groups() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let day = 86_400;
    // 窗口起点取某天 UTC 零点，下面的时刻都相对它算，桶边界才好核对。
    let base = 20_000 * day;
    let rec = |cred: i64, model: &str, device: Option<&str>, status: u16, cost: f64| UsageRecord {
        cred_id: Some(cred),
        cred_label: "x".into(),
        model: Some(model.into()),
        device_id: device.map(Into::into),
        ua: Some("claude-cli/2.1.280".into()),
        status,
        has_usage: true,
        input_tokens: Some(10),
        output_tokens: Some(20),
        cache_creation_tokens: Some(3),
        cache_read_tokens: Some(100),
        cost_usd: Some(cost),
        ..Default::default()
    };
    let insert = |r: &UsageRecord, ts: i64| store.insert_usage_log_at(r, Some(ts)).unwrap();
    // 第一天：opus 两条成功（设备 d1）、sonnet 一条 429。
    insert(&rec(a, "claude-opus-5", Some("d1"), 200, 0.5), base + 3600);
    insert(&rec(a, "claude-opus-5", Some("d1"), 200, 0.5), base + 7200);
    insert(&rec(a, "claude-sonnet-5", None, 429, 0.0), base + 7300);
    // 第二天：一条本地拒绝，一条只有细分档缓存写的老记录。
    insert(
        &UsageRecord {
            cred_id: Some(a),
            status: 429,
            forensics: Forensics {
                rewrites: Some("rejected_locally:device-limit".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        base + day + 60,
    );
    insert(
        &UsageRecord {
            cache_creation_tokens: None,
            cache_5m_tokens: Some(4),
            cache_1h_tokens: Some(6),
            ..rec(a, "claude-opus-5", Some("d2"), 200, 1.0)
        },
        base + day + 120,
    );
    // 别的号、窗口之前的记录都不算。
    insert(&rec(b, "claude-opus-5", Some("d1"), 200, 9.0), base + 3600);
    insert(&rec(a, "claude-opus-5", Some("d1"), 200, 9.0), base - 10);

    let s = store.credential_stats(a, base, day, 0, 10).unwrap();
    assert_eq!(s.summary.requests, 5);
    assert_eq!(s.summary.errors, 2, "429 两条（含本地拒绝）");
    assert_eq!(s.summary.rejected, 1);
    assert!((s.summary.cost_usd - 2.0).abs() < 1e-9);
    assert_eq!(s.summary.cache_write_tokens, 3 * 3 + 10);
    assert_eq!(
        s.points.iter().map(|p| (p.ts, p.requests)).collect::<Vec<_>>(),
        vec![(base, 3), (base + day, 2)]
    );
    assert_eq!(s.by_model[0].key, "claude-opus-5");
    assert_eq!(s.by_model[0].requests, 3);
    assert_eq!(s.by_model[0].last_ts, base + day + 120);
    // d1 与空设备（sonnet 那条 + 本地拒绝）都是 2 条，并列按 key 排，空串在前。
    assert_eq!(
        s.by_device.iter().map(|g| (g.key.as_str(), g.requests)).collect::<Vec<_>>(),
        vec![("", 2), ("d1", 2), ("d2", 1)],
        "没带设备身份的归空串"
    );
    assert_eq!(s.by_device[1].tokens, 2 * (10 + 20 + 3 + 100));
    assert_eq!(s.by_status[0].key, "200");
    assert_eq!(
        s.by_status[1],
        CredentialStatsGroup {
            key: "429".into(),
            requests: 2,
            errors: 2,
            tokens: 133,
            cost_usd: 0.0,
            last_ts: base + day + 60,
        }
    );
    assert_eq!(s.by_client[0].key, "claude-cli/2.1.280");

    // 东八区：UTC 当天 17:00 之后落到本地第二天。
    let tz = 8 * 3600;
    insert(&rec(a, "claude-opus-5", Some("d1"), 200, 0.0), base + 17 * 3600);
    let local = store.credential_stats(a, base, day, tz, 10).unwrap();
    assert!(local.points.iter().any(|p| p.ts == base + day - tz));
    // group_limit 生效。
    assert_eq!(store.credential_stats(a, base, day, 0, 1).unwrap().by_model.len(), 1);
}

fn store_with(labels: &[&str]) -> (CredentialStore, Vec<i64>) {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let ids = labels
        .iter()
        // refresh_token 有 UNIQUE 约束，按 label 取值保证互不相同。
        .map(|l| {
            store
                .insert(l, None, &format!("tok-{l}"), &format!("refresh-{l}"), 0, None, None, 1)
                .unwrap()
                .id
        })
        .collect();
    (store, ids)
}

/// 导入的匹配顺序：`account_uuid` 优先，`refresh_token` 兜底。
///
/// 第一条是这套东西的关键——账号在源站重新授权过之后 refresh_token 已经换了值，只按 token
/// 认就会把同一个账号在目标库里变成两行，两行还各自去刷新同一个上游账号。
#[test]
fn import_matches_by_account_uuid_then_refresh_token() {
    let (store, _) = store_with(&["a"]);
    let base = PortableCredential {
        label: "acct".into(),
        tier: Some("max".into()),
        org_type: None,
        rate_limit_tier: None,
        access_token: "at-1".into(),
        refresh_token: "rt-1".into(),
        expires_at: 100,
        priority: Some(2),
        disabled: false,
        device_limit: 3,
        session_limit: 0,
        rpm_limit: 7,
        quota_pause_pct: None,
        quota_pause_pct_7d: None,
        ban_reason: None,
        account_uuid: Some("uuid-1".into()),
        org_uuid: None,
        subscription_created_at: None,
        org_name: None,
        seat_tier: None,
        subscription_status: None,
        extra_usage_enabled: None,
        resume_at: None,
        proxy: None,
    };
    assert_eq!(store.import_credential(&base).unwrap(), ImportOutcome::Added);
    let before = store.list().unwrap().len();

    // 同一个账号、新的 refresh_token（源站重新授权过）→ 覆盖那一行，不新增。
    let reauthed = PortableCredential {
        refresh_token: "rt-2".into(),
        label: "acct-renamed".into(),
        priority: Some(3),
        ..base.clone()
    };
    assert_eq!(store.import_credential(&reauthed).unwrap(), ImportOutcome::Updated);
    assert_eq!(store.list().unwrap().len(), before, "同一个账号不该变成两行");
    let got = store
        .list()
        .unwrap()
        .into_iter()
        .find(|c| c.account_uuid.as_deref() == Some("uuid-1"))
        .unwrap();
    assert_eq!(got.refresh_token, "rt-2");
    assert_eq!(got.label, "acct-renamed", "命中后是整行覆盖");
    assert_eq!(got.priority, 3);

    // 没有 uuid 的号（profile 没拉到）仍能按 refresh_token 认出来——这条兜底不能少。
    let no_uuid =
        PortableCredential { account_uuid: None, refresh_token: "rt-9".into(), ..base.clone() };
    assert_eq!(store.import_credential(&no_uuid).unwrap(), ImportOutcome::Added);
    let again = PortableCredential { label: "by-token".into(), ..no_uuid.clone() };
    assert_eq!(store.import_credential(&again).unwrap(), ImportOutcome::Updated);

    // 空 token 的记录直接报错：让调用方把它计进 failed，而不是写一行用不了的号进去。
    let empty = PortableCredential { access_token: "".into(), ..base.clone() };
    assert!(store.import_credential(&empty).is_err());
}

/// 同一个人既有个人订阅又占一个团队席位：同一个账号 UUID、两个组织 UUID，导入后得是两行，
/// 各自按组织认回自己那一行；组织 UUID 说不清时不乱认。
#[test]
fn import_matches_by_account_and_org_uuid() {
    let (store, _) = store_with(&[]);
    let personal = PortableCredential {
        label: "personal".into(),
        tier: Some("Max 20x".into()),
        org_type: Some("claude_max".into()),
        rate_limit_tier: None,
        access_token: "at-p".into(),
        refresh_token: "rt-p".into(),
        expires_at: 100,
        priority: Some(0),
        disabled: false,
        device_limit: 0,
        session_limit: 0,
        rpm_limit: 0,
        quota_pause_pct: None,
        quota_pause_pct_7d: None,
        ban_reason: None,
        account_uuid: Some("acct".into()),
        org_uuid: Some("org-personal".into()),
        subscription_created_at: None,
        org_name: None,
        seat_tier: None,
        subscription_status: None,
        extra_usage_enabled: None,
        resume_at: None,
        proxy: None,
    };
    let team = PortableCredential {
        label: "team".into(),
        tier: Some("Team Standard".into()),
        org_type: Some("claude_team".into()),
        access_token: "at-t".into(),
        refresh_token: "rt-t".into(),
        org_uuid: Some("org-team".into()),
        ..personal.clone()
    };
    assert_eq!(store.import_credential(&personal).unwrap(), ImportOutcome::Added);
    assert_eq!(store.import_credential(&team).unwrap(), ImportOutcome::Added, "另一个组织是另一行");

    // 团队那行在源站重新授权过（新 refresh_token），按组织认回团队那行，个人那行不动。
    let team2 = PortableCredential { refresh_token: "rt-t2".into(), ..team.clone() };
    assert_eq!(store.import_credential(&team2).unwrap(), ImportOutcome::Updated);
    let rows = store.list().unwrap();
    assert_eq!(rows.len(), 2);
    let by_label = |l: &str| rows.iter().find(|c| c.label == l).unwrap().clone();
    assert_eq!(by_label("team").refresh_token, "rt-t2");
    assert_eq!(by_label("personal").refresh_token, "rt-p");

    // 没带组织 UUID、账号下又有两行：说不清是哪个，不认，新增一行。
    let ambiguous =
        PortableCredential { org_uuid: None, refresh_token: "rt-x".into(), ..team.clone() };
    assert_eq!(store.import_credential(&ambiguous).unwrap(), ImportOutcome::Added);

    // 库里是还没回填组织 UUID 的旧号：账号下只有它一条空着的，认它。
    let (old, _) = store_with(&[]);
    let legacy = PortableCredential { org_uuid: None, ..personal.clone() };
    assert_eq!(old.import_credential(&legacy).unwrap(), ImportOutcome::Added);
    let fresh = PortableCredential { refresh_token: "rt-new".into(), ..personal.clone() };
    assert_eq!(old.import_credential(&fresh).unwrap(), ImportOutcome::Updated);
    assert_eq!(old.list().unwrap().len(), 1);
}

/// 导出的设置快照与导入都**绕开管理密码**：那是目标机器自己的门锁，不该被一次导入换掉。
#[test]
fn admin_password_never_travels_with_settings() {
    let (store, _) = store_with(&["a"]);
    store.set_setting(ADMIN_PASSWORD, "hash-of-source-box").unwrap();
    store.set_setting(CLIENT_API_KEY, "key-from-source").unwrap();

    let snapshot = store.settings_snapshot();
    assert!(!snapshot.contains_key(ADMIN_PASSWORD), "导出不带管理密码");
    assert_eq!(
        snapshot.get(CLIENT_API_KEY).map(String::as_str),
        Some("key-from-source"),
        "接入 key 要带上：不跟着走的话所有客户端都得重配"
    );

    // 手工把管理密码塞回文件里也不认。
    let (target, _) = store_with(&["b"]);
    target.set_setting(ADMIN_PASSWORD, "hash-of-target-box").unwrap();
    let mut incoming = snapshot.clone();
    incoming.insert(ADMIN_PASSWORD.into(), "hash-of-source-box".into());
    target.import_settings(&incoming).unwrap();
    assert_eq!(
        target.get_setting(ADMIN_PASSWORD).unwrap().as_deref(),
        Some("hash-of-target-box"),
        "目标机器的管理密码不该被导入改掉"
    );
    // 旧版文件里的全局接入 Key 导进来变成一把不绑定分组的接入 Key，设置项本身不再落库。
    assert_eq!(target.get_setting(CLIENT_API_KEY).unwrap(), None);
    let access = target.api_key_access("key-from-source").unwrap().expect("转成了接入 Key");
    assert!(access.groups.is_none(), "不绑定分组，用全部号");
}

/// 导出的每一项都要能原样导回来：迁移文件就是「导出的响应原样喂给导入」，
/// 中间掉一个字段（曾经掉过 priority）就是操作者在新机器上发现配置不对，而且很难看出来。
#[test]
fn export_round_trips_every_field() {
    let (src, _) = store_with(&["a"]);
    let full = PortableCredential {
        label: "full".into(),
        tier: Some("pro".into()),
        org_type: Some("claude_team".into()),
        rate_limit_tier: Some("default_claude_max_5x".into()),
        access_token: "at".into(),
        refresh_token: "rt".into(),
        expires_at: 1_800_000_000,
        priority: Some(4),
        disabled: true,
        device_limit: 6,
        session_limit: 0,
        rpm_limit: -1,
        quota_pause_pct: Some(95),
        quota_pause_pct_7d: Some(0),
        ban_reason: Some("banned upstream".into()),
        account_uuid: Some("uuid".into()),
        org_uuid: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
        subscription_created_at: Some("2026-04-15T13:03:55.239Z".into()),
        org_name: Some("Acme".into()),
        seat_tier: Some("team_standard".into()),
        subscription_status: Some("active".into()),
        extra_usage_enabled: Some(false),
        resume_at: Some(1_900_000_000),
        proxy: Some("socks5://127.0.0.1:1080".into()),
    };
    src.import_credential(&full).unwrap();
    let exported = src.export_credentials().unwrap();
    let out = exported.iter().find(|c| c.label == "full").expect("导出里该有它");

    let (dst, _) = store_with(&[]);
    dst.import_credential(out).unwrap();
    let back = &dst.export_credentials().unwrap()[0];
    assert_eq!(back.label, full.label);
    assert_eq!(back.tier, full.tier);
    assert_eq!(back.org_type, full.org_type);
    assert_eq!(back.org_name, full.org_name);
    assert_eq!(back.seat_tier, full.seat_tier);
    assert_eq!(back.subscription_status, full.subscription_status);
    assert_eq!(back.extra_usage_enabled, full.extra_usage_enabled);
    // 额度档原值也要跟着走：迁移后到下一次成功拉 profile 之前，statsig eval 的
    // `rateLimitTier` 全靠它。
    assert_eq!(back.rate_limit_tier, full.rate_limit_tier);
    assert_eq!(back.access_token, full.access_token);
    assert_eq!(back.refresh_token, full.refresh_token);
    assert_eq!(back.expires_at, full.expires_at);
    assert_eq!(back.priority, full.priority);
    assert_eq!(back.disabled, full.disabled);
    assert_eq!(back.device_limit, full.device_limit);
    assert_eq!(back.rpm_limit, full.rpm_limit);
    // 逐账号的提前停调度阈值同样要跟着走：`Some(0)`（这一档不停）与 `None`（跟随全局）
    // 是两个不同的值，导出再导入不能把前者抹成后者。
    assert_eq!(back.quota_pause_pct, Some(95));
    assert_eq!(back.quota_pause_pct_7d, Some(0));
    assert_eq!(back.ban_reason, full.ban_reason);
    assert_eq!(back.account_uuid, full.account_uuid);
    // 组织 id 与订阅创建时刻同理：迁移后不该等到下一次刷新才有。
    assert_eq!(back.org_uuid, full.org_uuid);
    assert_eq!(back.subscription_created_at, full.subscription_created_at);
    assert_eq!(back.resume_at, full.resume_at);
    // 代理串在读出来时会归一化（socks5 → socks5h），两侧同样处理过，故比的是归一化后的值。
    assert_eq!(back.proxy, out.proxy);
}

/// 裸请求速率上限：单号发满后自动分流到下一个号，全部发满才 429（[`BareRateLimited`]）。
/// 带 device_id 的请求不受此限——那条路由设备绑定 + `device_limit` 管着。
#[test]
fn bare_rate_limit_spills_to_next_credential_then_rejects() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    store.set_setting(BARE_RATE_LIMIT, "2").unwrap();

    // 前两条落在 a（同优先级、设备数都是 0 时 id 小者先中），第 3、4 条 a 已满 → 溢到 b。
    let picked: Vec<i64> = (0..4)
        .map(|_| {
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id
        })
        .collect();
    assert_eq!(picked, vec![a, a, b, b], "满了应换号而不是直接拒");

    // 两个号都满 → 拒绝，且带得出重试间隔（默认窗口 60s）。
    let err = store
        .select_for_device(Select {
            ttl_secs: 0,
            rate_limited: true,
            exclude: &[],
            ..Default::default()
        })
        .unwrap_err();
    let rl = err.downcast_ref::<BareRateLimited>().expect("应是裸请求限流错误");
    assert_eq!(rl.retry_after_secs, DEFAULT_BARE_RATE_WINDOW_SECS);

    // 带设备身份的请求照常放行：它受的是设备上限，不是这条。
    assert!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .is_ok()
    );

    // 上限设回 0（不限）即刻恢复，计数不再拦。
    store.set_setting(BARE_RATE_LIMIT, "0").unwrap();
    assert!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .is_ok()
    );
}

/// 生效的 RPM 上限与设备上限共用一套三态语义，改了一处另一处不能悄悄漂开。
#[test]
fn effective_rpm_limit_matches_the_device_limit_tri_state() {
    assert_eq!(effective_rpm_limit(30, 60), 30, "账号独立上限覆盖全局");
    assert_eq!(effective_rpm_limit(0, 60), 60, "未配置则跟随全局默认");
    assert_eq!(effective_rpm_limit(0, 0), 0, "全局也不限时不限");
    assert_eq!(effective_rpm_limit(-1, 60), 0, "账号明确不限，忽略全局默认");
}

/// 账号 RPM 上限：还没定下号的请求撞到上限时溢到下一个号，全部发满才 429
/// （[`RpmLimited`]，且 `sticky` 为假）。账号自己配的上限盖过全局默认。
///
/// 全程 `rate_limited: false`——RPM 与裸请求上限不同，它不看这个标志，每次选号都计。
#[test]
fn rpm_limit_spills_to_next_credential_then_rejects() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    store.set_setting(DEFAULT_RPM_LIMIT, "2").unwrap();
    let sel = Select { ttl_secs: 0, ..Default::default() };

    // 前两条落在 a（同优先级、设备数都是 0 时 id 小者先中），第 3、4 条 a 已满 → 溢到 b。
    let picked: Vec<i64> = (0..4).map(|_| store.select_for_device(sel).unwrap().id).collect();
    assert_eq!(picked, vec![a, a, b, b], "发满了应换号而不是直接拒");

    let err = store.select_for_device(sel).unwrap_err();
    let rl = err.downcast_ref::<RpmLimited>().expect("应是 RPM 限流错误");
    assert!(!rl.sticky, "没有设备绑定，拒的是整个候选池而不是某个号");
    assert!(
        (1..=RPM_WINDOW_SECS).contains(&rl.retry_after_secs),
        "重试间隔应落在一个窗口之内，实际 {}",
        rl.retry_after_secs
    );

    // 账号独立上限盖过全局默认：a 单独放宽到 5，窗口里已有的 2 条不妨碍它继续接。
    store.set_rpm_limit(a, 5).unwrap();
    assert_eq!(store.select_for_device(sel).unwrap().id, a);

    // 「明确不限」（-1）同样盖过全局默认：a 收紧到发不出，只剩 b 可选。
    store.set_rpm_limit(a, 1).unwrap();
    store.set_rpm_limit(b, -1).unwrap();
    assert_eq!(store.select_for_device(sel).unwrap().id, b, "-1 即不限，全局默认不再生效");
}

/// 粘性命中的号撞到 RPM 上限时**直接拒**，不改选别的号——改绑会让这条会话之后每一轮都
/// 先撞一次 thinking 签名 400。窗口松了之后这台设备还在原来那个号上。
#[test]
fn rpm_limit_rejects_the_bound_credential_instead_of_rebinding() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    store.set_rpm_limit(a, 1).unwrap();
    let sel = Select { device_id: Some("dev-1"), ttl_secs: 0, ..Default::default() };

    assert_eq!(store.select_for_device(sel).unwrap().id, a, "新设备先落在 a");

    let err = store.select_for_device(sel).unwrap_err();
    let rl = err.downcast_ref::<RpmLimited>().expect("应是 RPM 限流错误");
    assert!(rl.sticky, "拒的是这台设备绑定的那个号");
    assert!(rl.retry_after_secs >= 1, "retry-after 不得为 0，否则客户端立刻再撞一次");
    assert_eq!(store.device_count(b).unwrap(), 0, "b 空着也不该被改绑过去");

    // 放宽上限即刻恢复，且仍是原来那个号——绑定自始至终没被动过。
    store.set_rpm_limit(a, 10).unwrap();
    assert_eq!(store.select_for_device(sel).unwrap().id, a);
}

/// 窗口滚过去之后名额自己回来（口径同裸请求那条，只是窗口固定 60 秒）。
#[test]
fn rpm_window_expires_and_frees_the_slot() {
    let (store, ids) = store_with(&["a"]);
    let a = ids[0];
    store.set_rpm_limit(a, 1).unwrap();
    let sel = Select { ttl_secs: 0, ..Default::default() };

    assert!(store.select_for_device(sel).is_ok());
    assert!(store.select_for_device(sel).is_err(), "同一窗口内第二条应被拦");

    // 直接把窗口内的那条时间戳推到过期，等价于等了一个窗口。
    {
        let mut hits = store.rpm_rate.hits.lock();
        for q in hits.values_mut() {
            for t in q.iter_mut() {
                *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
            }
        }
    }
    assert!(store.select_for_device(sel).is_ok(), "过期后名额应回收");
}

/// 每设备 RPM：各设备各算各的，打满的那台被拒并拿到 retry-after，其余设备不受影响；
/// 窗口滚过去后名额自己回来。上限未配置时一条都不记（也就永远不拒）。
#[test]
fn device_rpm_limit_is_per_device_and_expires() {
    let (store, _) = store_with(&["a"]);

    // 没配上限 → 恒放行，且窗口表里一条都不该有（记了只会无界增长）。
    for _ in 0..5 {
        assert_eq!(store.take_device_rpm_slot("dev-1"), None, "未配置上限就是不限");
    }
    assert!(store.device_rate.hits.lock().is_empty(), "不限时不该记账");

    store.set_setting(DEVICE_RPM_LIMIT, "2").unwrap();
    assert_eq!(store.take_device_rpm_slot("dev-1"), None);
    assert_eq!(store.take_device_rpm_slot("dev-1"), None);
    let retry = store.take_device_rpm_slot("dev-1").expect("第三条该被拒");
    assert!((1..=RPM_WINDOW_SECS).contains(&retry), "retry-after 要落在窗口内且不为 0：{retry}");

    // 另一台设备有自己的窗口——一台刷疯了不该连累别人，这正是这道闸的目的。
    assert_eq!(store.take_device_rpm_slot("dev-2"), None, "别的设备照常");

    // 把 dev-1 窗口里的时间戳推到过期，等价于等了一个窗口。
    {
        let mut hits = store.device_rate.hits.lock();
        for t in hits.get_mut("dev-1").expect("dev-1 该有窗口").iter_mut() {
            *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
        }
    }
    assert_eq!(store.take_device_rpm_slot("dev-1"), None, "过期后名额应回收");
}

/// 每会话 RPM：各会话各算各的，与设备那道闸**互不干扰**（同一台设备上两个会话各有自己的
/// 窗口，这正是选会话粒度的目的）；窗口滚过去后名额自己回来，未配置上限时一条都不记。
#[test]
fn session_rpm_limit_is_per_session_and_independent_of_device() {
    let (store, _) = store_with(&["a"]);

    for _ in 0..5 {
        assert_eq!(store.take_session_rpm_slot("sess-1"), None, "未配置上限就是不限");
    }
    assert!(store.session_rate.hits.lock().is_empty(), "不限时不该记账");

    store.set_setting(SESSION_RPM_LIMIT, "2").unwrap();
    assert_eq!(store.take_session_rpm_slot("sess-1"), None);
    assert_eq!(store.take_session_rpm_slot("sess-1"), None);
    let retry = store.take_session_rpm_slot("sess-1").expect("第三条该被拒");
    assert!((1..=RPM_WINDOW_SECS).contains(&retry), "retry-after 要落在窗口内且不为 0：{retry}");

    // 同机的另一个会话有自己的桶——按设备一刀切时它会被上面那个挤没。
    assert_eq!(store.take_session_rpm_slot("sess-2"), None, "别的会话照常");

    // 两个窗口是两份计数：会话打满不该顺带把设备的桶也算上（反之同理）。
    store.set_setting(DEVICE_RPM_LIMIT, "1").unwrap();
    assert_eq!(store.take_device_rpm_slot("dev-1"), None, "设备的桶此刻还是空的");

    {
        let mut hits = store.session_rate.hits.lock();
        for t in hits.get_mut("sess-1").expect("sess-1 该有窗口").iter_mut() {
            *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
        }
    }
    assert_eq!(store.take_session_rpm_slot("sess-1"), None, "过期后名额应回收");
}

/// 设备窗口表的清扫：device_id 是客户端自报的，乱编 id 能把 map 撑大；超过阈值时清掉
/// 空窗口，但**窗口内还有记录的键一个都不能丢**——丢了等于给那台设备白送一轮名额。
#[test]
fn crowded_device_windows_are_swept_without_losing_live_ones() {
    let (store, _) = store_with(&["a"]);
    store.set_setting(DEVICE_RPM_LIMIT, "1").unwrap();

    let window = Duration::from_secs(RPM_WINDOW_SECS as u64);
    for i in 0..(DEVICE_RATE_MAX_KEYS + 10) {
        store.device_rate.try_take(format!("dev-{i}"), 1, window);
    }
    // 除了一台仍在窗口内的，其余全部推到过期。
    {
        let mut hits = store.device_rate.hits.lock();
        for (k, q) in hits.iter_mut() {
            if k != "dev-0" {
                for t in q.iter_mut() {
                    *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
                }
            }
        }
    }
    store.device_rate.sweep_if_crowded(window, DEVICE_RATE_MAX_KEYS);
    let hits = store.device_rate.hits.lock();
    assert_eq!(hits.len(), 1, "过期的键该被清掉");
    assert!(hits.contains_key("dev-0"), "还在窗口内的键不能被清掉");
}

/// 上游 429 打过冷却的号在选号时让位；绑定到它的设备**改绑**到新号（这正是 429 换号
/// 重试要的语义）；冷却结束后不自动回迁——粘性以最后一次选择为准。
#[test]
fn cooldown_makes_device_rebind_to_another_credential() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);

    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        a
    );
    store.mark_rate_limited(a, None, Duration::from_secs(300));
    assert!(store.rate_limited_secs(a) > 0, "应处于冷却中");

    // 绑定还在 a 上，但 a 在冷却 → 改选 b，并把绑定迁过去。
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        b
    );
    assert_eq!(store.list_devices(b).unwrap().len(), 1, "设备应已改绑到 b");
    assert!(store.list_devices(a).unwrap().is_empty(), "a 上不该再留着这台设备");
}

/// 模型级冷却只挡那一个模型：fable 被容量限制时，同一个号的 sonnet/opus 照常可用。
/// 这是「窗口没跑满却 429」那种情况的正解——号是好的，赶走整个号纯属自伤。
#[test]
fn model_scoped_cooldown_only_blocks_that_model() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let pick = |model| {
        store.select_for_device(Select { model: Some(model), ..Default::default() }).unwrap().id
    };

    store.mark_rate_limited(a, Some("claude-fable-5"), Duration::from_secs(300));
    assert_eq!(pick("claude-fable-5"), b, "fable 应让位给 b");
    assert_eq!(pick("claude-sonnet-5"), a, "同一个号的其它模型不该被牵连");
    // 模型级冷却不算「账号被限流」，控制台不该显示成账号出了问题。
    assert_eq!(store.rate_limited_secs(a), 0, "模型级冷却不计入账号级展示");

    // 账号级冷却则对所有模型生效。
    store.mark_rate_limited(a, None, Duration::from_secs(300));
    assert_eq!(pick("claude-sonnet-5"), b, "账号级冷却应挡下所有模型");
    assert!(store.rate_limited_secs(a) > 0);

    // 连通性测试成功那种「带模型」的解除：清账号级 + 被测模型格，别的模型格不动。
    store.clear_rate_limited(a, Some("claude-sonnet-5"));
    assert_eq!(store.rate_limited_secs(a), 0, "账号级冷却应已解除");
    assert_eq!(pick("claude-sonnet-5"), a, "被测模型应立即可用");
    assert_eq!(pick("claude-fable-5"), b, "sonnet 通了证明不了 fable 通，那一格要留着");

    // 手动解除：全部格一起清。
    store.clear_rate_limited(a, None);
    assert_eq!(pick("claude-fable-5"), a, "手动解除后所有模型都该回来");
}

/// 上游判过「套餐不含这个模型」的号，该模型选号时绕开它，其余模型照常；清掉后回来。
#[test]
fn denied_model_is_skipped_until_cleared() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let pick = |model: &str| {
        store.select_for_device(Select { model: Some(model), ..Default::default() }).unwrap().id
    };
    store.deny_model(a, "claude-fable-5-1[1m]", "plan", None).unwrap();
    assert_eq!(pick("claude-fable-5-1"), b, "[1m] 变体上学到的记录对裸 id 同样生效");
    assert_eq!(pick("claude-sonnet-5"), a, "其余模型不受影响");
    assert_eq!(store.denied_models(a).unwrap().len(), 1);
    assert!(store.all_model_denials().unwrap().contains_key(&a));
    assert_eq!(store.clear_model_denials(a, Some("claude-sonnet-5")).unwrap(), 0);
    assert_eq!(store.clear_model_denials(a, Some("claude-fable-5-1")).unwrap(), 1);
    assert_eq!(pick("claude-fable-5-1"), a, "解除后该模型回到这个号");
}

/// 全部号都被判过不支持 → 是「换模型」而不是「等一会」：报 ModelUnsupported，不报限流。
#[test]
fn all_denied_is_model_unsupported_not_rate_limited() {
    let (store, ids) = store_with(&["a", "b"]);
    for id in &ids {
        store.deny_model(*id, "claude-fable-5", "plan", None).unwrap();
    }
    let err = store
        .select_for_device(Select { model: Some("claude-fable-5"), ..Default::default() })
        .unwrap_err();
    let u = err.downcast_ref::<ModelUnsupported>().expect("应是 ModelUnsupported");
    assert_eq!(u.accounts, 2);
    assert!(store.select_for_device(Select::default()).is_ok(), "不带模型的请求不受影响");

    // 换号重试的形态：唯一的号刚被判过、同时也在 exclude 里——仍必须报 ModelUnsupported，
    // 而不是「排除后没号了」那条普通错误（那条会让转发环把上游 429 原样透传）。
    let (store, ids) = store_with(&["only"]);
    store.deny_model(ids[0], "claude-fable-5", "plan", None).unwrap();
    let err = store
        .select_for_device(Select {
            model: Some("claude-fable-5"),
            exclude: &[ids[0]],
            ..Default::default()
        })
        .unwrap_err();
    assert!(err.downcast_ref::<ModelUnsupported>().is_some(), "{err}");
}

/// 全部启用号都被判过、但还有一个只是限流暂停的 Max 号：那是「等一会」不是「换模型」，
/// 报 AllRateLimited + 它的恢复时刻，不能回 403 把两小时后回来的号说成不存在。
#[test]
fn paused_capable_account_turns_all_denied_into_rate_limited() {
    let (store, ids) = store_with(&["pro", "max"]);
    let (pro, max) = (ids[0], ids[1]);
    store.deny_model(pro, "claude-fable-5", "plan", None).unwrap();
    let resume_at = crate::credentials::now_secs() + 7200;
    store.pause_for_rate_limit(max, "quota", resume_at).unwrap();
    let sel =
        || store.select_for_device(Select { model: Some("claude-fable-5"), ..Default::default() });
    let err = sel().unwrap_err();
    let rl = err.downcast_ref::<AllRateLimited>().expect("应是 AllRateLimited");
    assert!(rl.retry_after_secs > 7000 && rl.retry_after_secs <= 7200, "{}", rl.retry_after_secs);
    // 暂停的那个号自己也被判过 → 真的没号能用，才是 ModelUnsupported。
    store.deny_model(max, "claude-fable-5", "plan", None).unwrap();
    assert!(sel().unwrap_err().downcast_ref::<ModelUnsupported>().is_some());
}

/// 学到的规则落库、重启可读回；过期的读不到；清空接口清得干净。
#[test]
fn learned_rejections_round_trip_and_expire() {
    let (store, _) = store_with(&["a"]);
    let row = |kind: &str, model: &str, field: &str, value: &str| LearnedRejection {
        kind: kind.into(),
        model: model.into(),
        field: field.into(),
        value: value.into(),
        message: "msg".into(),
        reply: None,
    };
    let rows = vec![
        row("shape", "claude-opus-5", "effort", "xhigh"),
        row("deprecated", "claude-haiku-4-5", "temperature", ""),
    ];
    store.remember_rejections(&rows).unwrap();
    // 重复写入是幂等的。
    store.remember_rejections(&rows[..1]).unwrap();
    let mut got = store.learned_rejections().unwrap();
    got.sort_by(|a, b| a.kind.cmp(&b.kind));
    assert_eq!(got.len(), 2);
    assert_eq!(got[1], rows[0]);
    assert_eq!(got[0], rows[1]);
    // 拒答那类带上游响应体：两列原样往返，体不截（message 仍截 500 字）。
    let long_body = format!("event: message_start\ndata: {{\"pad\":\"{}\"}}\n\n", "x".repeat(4000));
    let refusal = LearnedRejection {
        kind: "refusal".into(),
        model: "claude-opus-5".into(),
        field: "prompt_sha".into(),
        value: "deadbeef".into(),
        message: "m".repeat(600),
        reply: Some(LearnedReply { sse: true, body: long_body.clone() }),
    };
    store.remember_rejections(&[refusal]).unwrap();
    let back = store
        .learned_rejections()
        .unwrap()
        .into_iter()
        .find(|r| r.kind == "refusal")
        .expect("拒答行落库");
    assert_eq!(back.message.chars().count(), 500);
    assert_eq!(back.reply, Some(LearnedReply { sse: true, body: long_body }));
    assert!(store.forget_learned_rejection(&back).unwrap());

    // 人为把一条改成 8 天前学到的：读取时被当过期清掉。
    store
        .conn
        .lock()
        .execute(
            "UPDATE learned_rejections SET learned_at = unixepoch() - ?1 WHERE kind = 'shape'",
            [LEARNED_REJECTION_TTL_SECS + 86400],
        )
        .unwrap();
    let got = store.learned_rejections().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].kind, "deprecated");

    // 单条删除按四元组精确命中。
    store.remember_rejections(&rows).unwrap();
    assert!(store.forget_learned_rejection(&rows[0]).unwrap());
    assert!(!store.forget_learned_rejection(&rows[0]).unwrap(), "再删一次应无行");
    let (_, at) = store.learned_rejections_with_time().unwrap()[0].clone();
    assert!(at > 0);

    assert_eq!(store.clear_learned_rejections().unwrap(), 1);
    assert!(store.learned_rejections().unwrap().is_empty());
}

/// 删号连带清掉它的准入记录，别留孤儿行。
#[test]
fn deleting_a_credential_drops_its_denials() {
    let (store, ids) = store_with(&["a", "b", "c"]);
    for id in &ids {
        store.deny_model(*id, "claude-fable-5", "plan", None).unwrap();
    }
    assert!(store.delete(ids[0]).unwrap());
    assert_eq!(store.delete_many(&ids[1..2]).unwrap(), 1);
    let left = store.all_model_denials().unwrap();
    assert_eq!(left.keys().copied().collect::<Vec<_>>(), vec![ids[2]]);
    store.clear().unwrap();
    assert!(store.all_model_denials().unwrap().is_empty());
}

/// 到期的记录不再挡选号，且读列表时顺手清掉。
#[test]
fn expired_denial_is_ignored_and_purged() {
    let (store, ids) = store_with(&["a"]);
    let a = ids[0];
    let past = crate::credentials::now_secs() as i64 - 1;
    store.deny_model(a, "claude-fable-5", "plan", Some(past)).unwrap();
    assert_eq!(
        store
            .select_for_device(Select { model: Some("claude-fable-5"), ..Default::default() })
            .unwrap()
            .id,
        a
    );
    assert!(store.denied_models(a).unwrap().is_empty());
}

/// 等级变了（升级到 Max）就把旧套餐下学到的记录清掉；等级没变则留着。
#[test]
fn tier_change_clears_denials() {
    let (store, ids) = store_with(&["a"]);
    let a = ids[0];
    store.set_tier(a, Some("Pro")).unwrap();
    store.deny_model(a, "claude-fable-5", "plan", None).unwrap();
    store.set_tier(a, Some("Pro")).unwrap();
    assert_eq!(store.denied_models(a).unwrap().len(), 1, "等级没变，记录留着");
    store.set_tier(a, Some("Max 5x")).unwrap();
    assert!(store.denied_models(a).unwrap().is_empty(), "升级后记录作废");
}

/// fable/mythos 在同一优先级档内 Max 号优先、等级未知其次、Pro 垫底；其它模型不受影响，
/// 管理员设的优先级始终压过等级。
#[test]
fn premium_models_prefer_max_accounts_within_a_priority_tier() {
    let (store, ids) = store_with(&["pro", "max", "unknown"]);
    let (pro, max, unknown) = (ids[0], ids[1], ids[2]);
    store.set_tier(pro, Some("Pro")).unwrap();
    store.set_tier(max, Some("Max 20x")).unwrap();
    let pick = |model: &str| {
        store.select_for_device(Select { model: Some(model), ..Default::default() }).unwrap().id
    };
    assert_eq!(pick("claude-fable-5-1"), max, "fable 先落 Max");
    assert_eq!(pick("claude-sonnet-5"), pro, "非高档模型不排等级，按 id 兜底");
    store.set_disabled(max, true).unwrap();
    assert_eq!(pick("claude-fable-5-1"), unknown, "Max 不在时等级未知的排在 Pro 前面");
    store.set_disabled(max, false).unwrap();
    store.set_priority(pro, 1).unwrap();
    assert_eq!(pick("claude-fable-5-1"), pro, "优先级是主键，等级只在同档内排");
}

#[test]
fn model_denial_key_normalizes_variants() {
    assert_eq!(model_denial_key("Claude-Fable-5-1[1m]"), "claude-fable-5-1");
    assert_eq!(model_denial_key("claude-opus-4-6-20251114"), "claude-opus-4-6");
    assert_eq!(model_denial_key("claude-fable-5"), "claude-fable-5");
    assert_ne!(model_denial_key("claude-fable-5"), model_denial_key("claude-fable-5-1"));
    assert!(premium_model("claude-fable-5-1[1m]") && premium_model("claude-mythos-5"));
    assert!(!premium_model("claude-opus-5"));
}

/// 瞬时限速（容量 / 请求速率）也走选号门禁，gate 时长由 ladder 退避值决定（起步 2s）。
///
/// 与额度池满那档的区别只在持续时间：瞬时 gate 很短，避免同一个号被反复轰出 429；
/// 但不至于像 30s/60s 门禁那样让整池级联封死——ladder 每个号独立从 2s 起步，交错过期。
#[test]
fn transient_rate_limit_gates_selection_with_short_cooldown() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let pick = |model| {
        store.select_for_device(Select { model: Some(model), ..Default::default() }).unwrap().id
    };

    // 瞬时限速走 gate：被标记的号不参与选号。
    store.mark_rate_limited(a, Some("claude-opus-5"), Duration::from_secs(2));
    assert_eq!(pick("claude-opus-5"), b, "瞬时限速的短 gate 也应挡住选号");
    assert_eq!(store.rate_limited_secs(a), 0, "不该冒充账号级限流");

    let models = store.rate_limited_models(a);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].0, "claude-opus-5");
    assert!(models[0].2, "瞬时限速现在也走 gate，gated 应为 true");

    // 额度那档叠上去：长 gate 覆盖短 gate（取较晚的截止时刻）。
    store.mark_rate_limited(a, Some("claude-opus-5"), Duration::from_secs(300));
    assert_eq!(pick("claude-opus-5"), b, "额度池满那档仍是硬门禁");
    let models = store.rate_limited_models(a);
    assert_eq!(models.len(), 1, "同一个模型只该出现一行");
    assert!(models[0].2, "此刻挂着门禁，gated 应为 true");
    assert!(models[0].1 > 290, "展示的剩余时间应反映较长的那个门禁：{models:?}");
}

/// 账号级限流把调度开关**落库关掉**，到点惰性自动打开；人工关的号不会被自动打开。
#[test]
fn rate_limit_pause_persists_and_auto_resumes_when_due() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let pick = || store.select_for_device(Select::default()).map(|c| c.id);
    let now = crate::credentials::now_secs();

    // a 被限流暂停（还有一小时才到点）→ 落库停用，选号自然落到 b。
    store.pause_for_rate_limit(a, "上游限流：约 1 小时后自动恢复调度", now + 3600).unwrap();
    let paused = store.get(a).unwrap().unwrap();
    assert!(paused.disabled && paused.resume_at == Some(now + 3600));
    assert_eq!(pick().unwrap(), b, "被限流暂停的号不该再被选中");

    // 到点：不需要任何后台任务，下一次选号顺手把它放回来。
    store.pause_for_rate_limit(a, "已到点", now - 1).unwrap();
    assert_eq!(pick().unwrap(), a, "到点应自动回到调度池并按 (priority, id) 重新胜出");
    let back = store.get(a).unwrap().unwrap();
    assert!(!back.disabled && back.resume_at.is_none() && back.ban_reason.is_none());

    // 人工停用没有 resume_at，怎么等都不会自己打开。
    store.set_disabled(a, true).unwrap();
    assert_eq!(pick().unwrap(), b, "人工停用的号不参与调度");
    assert_eq!(store.get(a).unwrap().unwrap().resume_at, None, "人工停用不该有恢复时刻");
    assert_eq!(CredentialStore::resume_due(&store.conn.lock()).unwrap(), 0, "没有该恢复的号");
    assert!(store.get(a).unwrap().unwrap().disabled, "惰性恢复不该越过管理员的决定");
}

/// 订阅未生效的暂停：不写恢复时刻、到点不回来；只动启用中 / 限时暂停中的号，
/// 人工停用与封禁不碰；连通性测试那条恢复只认这一档，手动启用照常能打开。
#[test]
fn inactive_subscription_suspension_waits_for_a_human_or_a_passing_probe() {
    let (store, ids) = store_with(&["a", "b", "c", "d"]);
    let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);
    let pick = || store.select_for_device(Select::default()).map(|c| c.id);
    let now = crate::credentials::now_secs();
    let reason = format!("[{SUBSCRIPTION_PAUSE_TAG} 403] {ORG_OAUTH_SUSPEND_MARKER}");

    assert!(store.suspend_for_inactive_subscription(a, &reason).unwrap());
    let got = store.get(a).unwrap().unwrap();
    assert!(got.disabled && got.resume_at.is_none());
    assert_eq!(got.ban_reason.as_deref(), Some(reason.as_str()));
    // 与封号同形但不算封号：保活 / 遥测照跑，refresh_token 才不会在等续费时过期。
    assert!(got.is_subscription_paused() && !got.is_banned());
    assert_ne!(pick().unwrap(), a, "暂停的号不该再被选中");
    assert_eq!(CredentialStore::resume_due(&store.conn.lock()).unwrap(), 0, "不会到点自己回来");
    assert!(!store.resume_if_rate_limited(a).unwrap(), "手动解除限流不该放回这一档");
    assert!(!store.suspend_for_inactive_subscription(a, "again").unwrap(), "已暂停的不重写");

    // 额度暂停中的号：改成这一档（额度回来了没订阅照样不放行）。
    store.pause_for_rate_limit(b, "quota", now + 3 * 86400).unwrap();
    assert!(store.suspend_for_inactive_subscription(b, &reason).unwrap());
    assert_eq!(store.get(b).unwrap().unwrap().resume_at, None);

    // 人工停用 / 封禁：不碰。
    store.set_disabled(c, true).unwrap();
    assert!(!store.suspend_for_inactive_subscription(c, &reason).unwrap());
    assert_eq!(store.get(c).unwrap().unwrap().ban_reason, None);
    store.mark_banned(d, "封号").unwrap();
    assert!(!store.suspend_for_inactive_subscription(d, &reason).unwrap());
    let got = store.get(d).unwrap().unwrap();
    assert_eq!(got.ban_reason.as_deref(), Some("封号"));
    assert!(got.is_banned() && !got.is_subscription_paused());

    // 连通性测试通过：只放回这一档。
    assert!(!store.resume_if_subscription_suspended(c).unwrap());
    assert!(!store.resume_if_subscription_suspended(d).unwrap());
    assert!(store.get(d).unwrap().unwrap().is_banned());
    assert!(store.resume_if_subscription_suspended(a).unwrap());
    let back = store.get(a).unwrap().unwrap();
    assert!(!back.disabled && back.ban_reason.is_none());
    // 手动启用照常能打开。
    store.set_disabled(b, false).unwrap();
    assert!(!store.get(b).unwrap().unwrap().disabled);

    // 并发在飞的另一条后回来一发 429：不能把订阅暂停改写成「到点自己回来」。
    assert!(store.suspend_for_inactive_subscription(b, &reason).unwrap());
    assert!(!store.pause_for_rate_limit(b, "quota", now + 3600).unwrap());
    let got = store.get(b).unwrap().unwrap();
    assert!(got.is_subscription_paused() && got.resume_at.is_none());
    // 封号、人工停用同样不被 429 改写。
    assert!(!store.pause_for_rate_limit(d, "quota", now + 3600).unwrap());
    assert!(store.get(d).unwrap().unwrap().is_banned());
    assert!(!store.pause_for_rate_limit(c, "quota", now + 3600).unwrap());
    assert_eq!(store.get(c).unwrap().unwrap().resume_at, None);

    // 管理员把暂停中的号手动关掉：降成普通的手动停用，连通性测试通过也不再打开它。
    store.set_disabled(b, true).unwrap();
    let got = store.get(b).unwrap().unwrap();
    assert!(got.disabled && got.ban_reason.is_none() && !got.is_subscription_paused());
    assert!(!store.resume_if_subscription_suspended(b).unwrap());
    assert!(store.get(b).unwrap().unwrap().disabled, "人工停用不该被一次测试打开");
    // 限流暂停中的号被手动关掉同理：那句「几点恢复」不留下来冒充封号原因。
    store.set_disabled(b, false).unwrap();
    store.pause_for_rate_limit(b, "quota", now + 3600).unwrap();
    store.set_disabled(b, true).unwrap();
    let got = store.get(b).unwrap().unwrap();
    assert!(got.disabled && got.ban_reason.is_none() && got.resume_at.is_none());
    // 封号原因不因手动关而清掉。
    store.set_disabled(d, true).unwrap();
    assert_eq!(store.get(d).unwrap().unwrap().ban_reason.as_deref(), Some("封号"));
}

/// 订阅暂停只认 luban 自己写的格式（开头 `[subscription-inactive NNN] ` + 固定片段，区分
/// 大小写）：封号原因里抄进来的上游原文——包括上游错误没带类型时 `[403] <原话>` 那种与旧格式
/// 同形的——哪怕含同样几个词，也不能被当成可以自动恢复的暂停。库里（GLOB）与内存里同一口径。
#[test]
fn subscription_pause_is_recognised_only_by_lubans_own_prefix() {
    let ours = format!(
        "[{SUBSCRIPTION_PAUSE_TAG} 403] {ORG_OAUTH_SUSPEND_MARKER} (subscription lapsed, or a Free plan without one); paused until enabled manually or a connectivity test passes"
    );
    // 上游错误没带类型时封号原因就是 `[403] <原话>`：原话以这几个词开头也不算。
    let ban = format!("[403] {ORG_OAUTH_SUSPEND_MARKER}; this organization has been disabled");
    assert!(is_subscription_pause_reason(&ours));
    assert!(is_subscription_pause_reason(&format!(
        "[{SUBSCRIPTION_PAUSE_TAG} 401] {ORG_OAUTH_SUSPEND_MARKER}"
    )));
    let upper = ours.replace("organization does not", "Organization does not");
    let mid = format!("[403] permission_error: your {ORG_OAUTH_SUSPEND_MARKER}");
    for other in [
        ban.as_str(),
        mid.as_str(),
        upper.as_str(),
        ORG_OAUTH_SUSPEND_MARKER,
        "[subscription-inactive 40] x",
        "[subscription-inactive 403]",
        "",
    ] {
        assert!(!is_subscription_pause_reason(other), "{other}");
    }

    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    store.suspend_for_inactive_subscription(a, &ours).unwrap();
    store
        .record_ban(b, &BanContext { reason: ban.clone(), source: "forward", ..Default::default() })
        .unwrap();
    let got = store.get(b).unwrap().unwrap();
    assert!(got.is_banned() && !got.is_subscription_paused(), "真封号不该被认成订阅暂停");
    assert!(!store.resume_if_subscription_suspended(b).unwrap(), "真封号不该被测试打开");
    assert!(store.resume_if_subscription_suspended(a).unwrap());
    // 库里的 GLOB 同样区分大小写。
    store.suspend_for_inactive_subscription(a, &upper).unwrap();
    assert!(!store.resume_if_subscription_suspended(a).unwrap(), "大小写不同就不是我们写的");

    // 批量停用与单个同口径：清掉暂停原因，之后测试通过也不再打开它。
    store.set_disabled(a, false).unwrap();
    store.suspend_for_inactive_subscription(a, &ours).unwrap();
    assert_eq!(store.set_disabled_many(&[a, b], true).unwrap(), 2);
    let got = store.get(a).unwrap().unwrap();
    assert!(got.disabled && got.ban_reason.is_none());
    assert!(!store.resume_if_subscription_suspended(a).unwrap());
    assert_eq!(store.get(b).unwrap().unwrap().ban_reason, Some(ban), "封号原因不动");
}

/// 连通性测试通过 → 自动回调度池；但只对**被限流暂停**的号生效。
#[test]
fn probe_success_resumes_only_rate_limited_pauses() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let now = crate::credentials::now_secs();

    store.pause_for_rate_limit(a, "上游限流", now + 7 * 24 * 3600).unwrap();
    assert!(store.resume_if_rate_limited(a).unwrap(), "限流暂停的号该被测试结果放回来");
    let back = store.get(a).unwrap().unwrap();
    assert!(!back.disabled && back.resume_at.is_none());

    // 人工停用 / 封号：测试通过也不动它——那是管理员的决定，或需要人工介入的终态。
    store.set_disabled(b, true).unwrap();
    assert!(!store.resume_if_rate_limited(b).unwrap());
    assert!(store.get(b).unwrap().unwrap().disabled, "人工停用不该被一次连通性测试打开");
    store.mark_banned(b, "封号").unwrap();
    assert!(!store.resume_if_rate_limited(b).unwrap());
    assert!(store.get(b).unwrap().unwrap().disabled, "封号更不该被测试打开");
}

/// 全部号都因限流暂停时，回的是 429 + 最早恢复时刻，而不是「没有可用凭证，请先登录」——
/// 后者会把人引去查登录，实际上号都在，只是在等额度回血。
#[test]
fn all_paused_reports_rate_limit_not_missing_credentials() {
    let (store, ids) = store_with(&["a", "b"]);
    let now = crate::credentials::now_secs();
    store.pause_for_rate_limit(ids[0], "限流", now + 5 * 3600).unwrap();
    store.pause_for_rate_limit(ids[1], "限流", now + 2 * 3600).unwrap();

    let err = store.select_for_device(Select::default()).expect_err("全员暂停应报错");
    let rl = err.downcast_ref::<AllRateLimited>().expect("应是限流错误而非「没有可用凭证」");
    assert!(
        (2 * 3600 - 5..=2 * 3600).contains(&rl.retry_after_secs),
        "应给出最早恢复的那个，实得 {}",
        rl.retry_after_secs
    );
}

/// 冷却是**硬门禁**：全部号都在冷却时直接拒（429 + retry-after），不再退回照常选。
/// 另外「本次已试过的号」（换号重试传进来的排除集）一律出局，重试不会再撞同一个号。
#[test]
fn cooldown_is_a_hard_gate_and_exclusions_are_hard() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    store.mark_rate_limited(a, None, Duration::from_secs(300));
    store.mark_rate_limited(b, None, Duration::from_secs(600));

    // 都在冷却 → 拒绝调度，并给出最早解冻那个号（a，300s）的剩余时间。
    let err = store
        .select_for_device(Select {
            ttl_secs: 0,
            rate_limited: true,
            exclude: &[],
            ..Default::default()
        })
        .expect_err("全员冷却时不该选出任何号");
    let rl = err.downcast_ref::<AllRateLimited>().expect("应是冷却硬门禁错误");
    assert!(
        (290..=300).contains(&rl.retry_after_secs),
        "retry-after 应取最早解冻的那个号，实得 {}",
        rl.retry_after_secs
    );

    // 逃生口：手动解除 a 的冷却后立刻可用。
    store.clear_rate_limited(a, None);
    assert_eq!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        a
    );
    // 排除集是硬的：a 已试过 → 只能是 b……但 b 还在冷却，硬门禁下同样拒绝。
    assert!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[a],
                ..Default::default()
            })
            .is_err(),
        "唯一剩下的候选在冷却中 → 拒绝"
    );
    store.clear_rate_limited(b, None);
    assert_eq!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[a],
                ..Default::default()
            })
            .unwrap()
            .id,
        b
    );
    // 两个都试过 → 明确报错，让调用方把最初那条 429 透传回去。
    assert!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[a, b],
                ..Default::default()
            })
            .is_err()
    );
}

/// 不计费的路径（`count_tokens` 等，`rate_limited = false`）不占名额：它不产生 usage、
/// 不消耗额度，拿它占名额只会把真正的请求挤掉，而客户端的 token 预估全靠它。
#[test]
fn non_billable_paths_do_not_consume_rate_slots() {
    let (store, _) = store_with(&["a"]);
    store.set_setting(BARE_RATE_LIMIT, "1").unwrap();

    // 不计入的路径打多少条都不占名额。
    for _ in 0..5 {
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: false,
                    exclude: &[],
                    ..Default::default()
                })
                .is_ok()
        );
    }
    // 名额仍是满的一格：计费路径的第一条照常放行，第二条才被拦。
    assert!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .is_ok()
    );
    assert!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .is_err(),
        "计费路径应照常受限"
    );
    // 被拦之后，不计费的路径依然畅通。
    assert!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: false,
                exclude: &[],
                ..Default::default()
            })
            .is_ok()
    );
}

/// 窗口过期后名额自动回收；窗口取值非法（0/负数）时退回默认，不会把人永久锁死。
#[test]
fn bare_rate_window_expires_and_rejects_bad_config() {
    let (store, _) = store_with(&["a"]);
    store.set_setting(BARE_RATE_LIMIT, "1").unwrap();
    assert!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .is_ok()
    );
    assert!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .is_err(),
        "同一窗口内第二条应被拦"
    );

    // 直接把窗口内的那条时间戳推到过期，等价于等了一个窗口。
    {
        let mut hits = store.bare_rate.hits.lock();
        for q in hits.values_mut() {
            for t in q.iter_mut() {
                *t -= Duration::from_secs(DEFAULT_BARE_RATE_WINDOW_SECS as u64 + 1);
            }
        }
    }
    assert!(
        store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .is_ok(),
        "过期后名额应回收"
    );

    store.set_setting(BARE_RATE_WINDOW_SECS, "0").unwrap();
    assert_eq!(store.bare_rate_window_secs(), DEFAULT_BARE_RATE_WINDOW_SECS, "非法窗口退回默认");
}

const REVOKED: &str = "[refresh 400] invalid_grant";

/// 刷新失败要自动换号：坏号被停用、设备改绑到下一个可用号，请求正常拿到 token。
///
/// 这是本次修复的核心——此前刷新失败直接抛错，而设备绑定在选号时就已写库，
/// 导致该设备永远选回同一个坏号、永远 503。
#[tokio::test]
async fn refresh_failure_fails_over_to_next_credential() {
    let (store, ids) = store_with(&["a", "b", "c"]);
    let tried = std::cell::RefCell::new(Vec::new());

    // a、b 的 refresh_token 已作废，c 正常。
    let (token, cred, _) = select_with_refresh_failover(
        &store,
        Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
        |c| {
            tried.borrow_mut().push(c.id);
            Box::pin(async move {
                Ok(if c.label == "c" {
                    TokenAttempt::Ready("good-token".into())
                } else {
                    TokenAttempt::Revoked(REVOKED.into())
                })
            })
        },
    )
    .await
    .unwrap();

    assert_eq!(token, "good-token");
    assert_eq!(cred.id, ids[2], "应换到第一个刷新得动的号");
    assert_eq!(*tried.borrow(), ids, "应按优先级依次试过 a、b、c");

    // a、b 被停用并记了原因；c 不受影响。
    for id in &ids[..2] {
        let c = store.get(*id).unwrap().unwrap();
        assert!(c.disabled, "作废的号应被停用");
        assert_eq!(c.ban_reason.as_deref(), Some(REVOKED));
    }
    assert!(!store.get(ids[2]).unwrap().unwrap().disabled);

    // 设备最终绑在 c 上，后续请求直接命中它，不再重走换号。
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        ids[2]
    );
}

/// 可重试错误（网络抖动、5xx、限流）**不得**停用凭证——误停一个健康账号的代价，
/// 远高于让客户端重试一次。
#[tokio::test]
async fn transient_refresh_error_does_not_disable() {
    let (store, ids) = store_with(&["a", "b"]);
    let calls = std::cell::Cell::new(0);

    let e = select_with_refresh_failover(
        &store,
        Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
        |_| {
            calls.set(calls.get() + 1);
            Box::pin(async { anyhow::bail!("请求 token 端点失败: connection reset") })
        },
    )
    .await
    .unwrap_err();

    assert!(e.to_string().contains("connection reset"), "应原样抛出底层错误: {e}");
    assert_eq!(calls.get(), 1, "可重试错误应立即返回，不该继续换号");
    for id in &ids {
        assert!(!store.get(*id).unwrap().unwrap().disabled, "可重试错误不得停用凭证");
    }
    // 绑定保留，客户端重试时仍落回同一个号。
    assert_eq!(
        store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap()
            .id,
        ids[0]
    );
}

fn refresh_failed(cred: &Credential) -> anyhow::Error {
    RefreshFailed {
        cred_id: cred.id,
        cred_label: cred.label.clone(),
        detail: "request to the token endpoint failed: connection refused".into(),
        already_paused: false,
    }
    .into()
}

/// 刷新没拿到结果（网络 / 代理）：这个号限时暂停、原因带完整错误链，当场换下一个号。
#[tokio::test]
async fn refresh_failure_pauses_the_credential_and_fails_over() {
    let (store, ids) = store_with(&["a", "b"]);

    let (token, cred, _) = select_with_refresh_failover(
        &store,
        Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
        |c| {
            let first = c.id == ids[0];
            Box::pin(async move {
                if first { Err(refresh_failed(&c)) } else { Ok(TokenAttempt::Ready("t".into())) }
            })
        },
    )
    .await
    .unwrap();

    assert_eq!((token.as_str(), cred.id), ("t", ids[1]));
    let paused = store.get(ids[0]).unwrap().unwrap();
    assert!(paused.disabled);
    assert!(paused.resume_at.is_some(), "限时暂停，不是封号");
    let reason = paused.ban_reason.unwrap();
    assert!(reason.starts_with(&format!("{REFRESH_FAIL_PAUSE_TAG} ")), "{reason}");
    assert!(reason.contains("connection refused"), "原因要带底层错误: {reason}");
}

/// 换不到别的号时报的是那次刷新失败（带号），不是选号那句「全员冷却」。
#[tokio::test]
async fn refresh_failure_without_another_credential_reports_the_credential() {
    let (store, ids) = store_with(&["a"]);

    let e = select_with_refresh_failover(
        &store,
        Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
        |c| Box::pin(async move { Err(refresh_failed(&c)) }),
    )
    .await
    .unwrap_err();

    let rf = e.downcast_ref::<RefreshFailed>().expect("应报 RefreshFailed");
    assert_eq!(rf.cred_id, ids[0]);
    assert!(store.get(ids[0]).unwrap().unwrap().resume_at.is_some());
}

/// 刷新失败换号有上限：本机网络挂了时每个号都会失败，不能一条请求挨个试完、停掉一整池。
#[tokio::test]
async fn refresh_failure_swaps_are_capped() {
    let (store, ids) = store_with(&["a", "b", "c"]);
    let tried = std::cell::RefCell::new(Vec::new());

    let e = select_with_refresh_failover(
        &store,
        Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
        |c| {
            tried.borrow_mut().push(c.id);
            Box::pin(async move { Err(refresh_failed(&c)) })
        },
    )
    .await
    .unwrap_err();

    assert!(e.downcast_ref::<RefreshFailed>().is_some(), "{e}");
    assert_eq!(tried.borrow().len(), MAX_REFRESH_FAIL_SWAPS);
    assert!(!store.get(ids[2]).unwrap().unwrap().disabled, "没试到的号不该被停");
}

/// 已经因刷新失败暂停着的号（等锁期间别的请求刚停的）：直接报那次失败、不再真刷，
/// 换号循环也不再写暂停，恢复时刻不被往后推。
#[tokio::test]
async fn refresh_paused_credential_is_not_refreshed_again() {
    let (store, ids) = store_with(&["a"]);
    let reason = format!("{REFRESH_FAIL_PAUSE_TAG} connection refused");
    let resume_at = crate::credentials::now_secs() + 60;
    assert!(store.pause_for_rate_limit(ids[0], &reason, resume_at).unwrap());
    let cred = store.get(ids[0]).unwrap().unwrap();
    let clients = crate::clients::ClientPool::new().unwrap();

    let Err(e) = fresh_token(&store, &clients, &cred, true).await else {
        panic!("暂停中的号不该再刷")
    };
    let rf = e.downcast_ref::<RefreshFailed>().expect("应报 RefreshFailed");
    assert!(rf.already_paused);
    assert_eq!(rf.detail, "connection refused");

    let e = select_with_refresh_failover(
        &store,
        Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
        |c| Box::pin(async move { Err(RefreshFailed::paused(c.id, c.label, "x").into()) }),
    )
    .await
    .unwrap_err();
    // 池里只剩这个暂停的号：选号直接报全池暂停，且认得出是刷新失败停的。
    let rl = e.downcast_ref::<AllRateLimited>().expect("应报 AllRateLimited");
    let rf = rl.refresh_failed.as_ref().expect("要带上刷新失败的号");
    assert_eq!(rf.cred_id, ids[0]);
    assert!(e.to_string().contains("token refresh failure"), "{e}");
    assert_eq!(store.get(ids[0]).unwrap().unwrap().resume_at, Some(resume_at));
}

/// 所有号的 refresh_token 都作废时要报错收场，不能死循环、也不能返回停用的号。
#[tokio::test]
async fn all_credentials_revoked_gives_up() {
    let (store, ids) = store_with(&["a", "b"]);
    let tried = std::cell::RefCell::new(Vec::new());

    let e = select_with_refresh_failover(
        &store,
        Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
        |c| {
            tried.borrow_mut().push(c.id);
            Box::pin(async { Ok(TokenAttempt::Revoked(REVOKED.into())) })
        },
    )
    .await
    .unwrap_err();

    // 号用完后是 select_for_device 先报「没有可用凭证」，而不是转满 MAX_REFRESH_FAILOVER 圈。
    assert!(
        e.to_string().contains("no available credentials"),
        "error message should identify the root cause: {e}"
    );
    assert_eq!(*tried.borrow(), ids, "每个号都应被试过一次，且只试一次");
    assert!(store.list().unwrap().iter().all(|c| c.disabled));
}

/// 走真实写入口落一条带限流头的流水（ts / 费用 / 两个 reset 由调用方指定）。
/// 刻意不裸 INSERT：快照与费用如今是写时落账（credential_stats），绕过写入口
/// 的行只进流水不进账本，测出来的就不是线上那条路径了。
///
/// 每条顺带记 10 个 token（输入/输出/缓存写/缓存读 各 1 + 3 + 2 + 4），于是窗口 token 数
/// 恒为「窗口内条数 × 10」，费用与请求数怎么断，token 就该怎么断。
fn log_row(
    store: &CredentialStore,
    cred_id: i64,
    ts: i64,
    cost: f64,
    r5: Option<i64>,
    r7: Option<i64>,
) {
    let rec = UsageRecord {
        cred_id: Some(cred_id),
        cost_usd: Some(cost),
        has_usage: true,
        input_tokens: Some(1),
        output_tokens: Some(3),
        cache_creation_tokens: Some(2),
        cache_read_tokens: Some(4),
        rl_5h_utilization: r5.map(|_| 0.5),
        rl_5h_reset: r5,
        rl_7d_utilization: r7.map(|_| 0.25),
        rl_7d_reset: r7,
        ..Default::default()
    };
    store.insert_usage_log_at(&rec, Some(ts)).unwrap();
}

/// 额度快照取「最新一条带限流信息的行」，窗口费用和请求数只算 `reset - 窗口` 之后的日志，
/// 且不串号。这条 SQL 从 1+2N 条查询合成了一条，口径必须逐项对上。
#[test]
fn latest_quotas_sums_only_current_window() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).unwrap().id;

    // 账号 a：reset=100_000，故 5h 窗口起点 82_000、7d 窗口起点 -504_800（含全部行）。
    let r5 = 100_000;
    log_row(&store, a, 10_000, 1.0, Some(r5), Some(r5)); // 5h 窗口外
    log_row(&store, a, 90_000, 2.0, Some(r5), Some(r5)); // 窗口内
    log_row(&store, a, 95_000, 4.0, Some(r5), Some(r5)); // 窗口内，且是最新快照行
    // 更晚但不带限流头的行：不该覆盖快照，费用仍要计入窗口。
    store
        .insert_usage_log_at(
            &UsageRecord { cred_id: Some(a), cost_usd: Some(8.0), ..Default::default() },
            Some(99_000),
        )
        .unwrap();
    log_row(&store, b, 95_000, 16.0, Some(r5), Some(r5)); // 他号，不得混入

    let q = store.latest_quotas().unwrap();
    let qa = q.get(&a).expect("a 应有快照");
    assert_eq!(qa.ts, 95_000, "快照应取最新一条带限流信息的行");
    assert_eq!(qa.cost_5h, Some(14.0), "只应含 ts >= reset-5h 的 2+4+8");
    assert_eq!(qa.cost_7d, Some(15.0), "7d 窗口覆盖全部 1+2+4+8");
    assert_eq!(qa.requests_5h, Some(3), "5h 窗口应计入 3 次请求");
    assert_eq!(qa.requests_7d, Some(4), "7d 窗口应计入 4 次请求");
    // token 与费用/请求数同窗口同断点：窗口内两条带 token 的流水各 10 个，那条没嗅探到
    // usage 的（各列 NULL）按 0 计而不是把整个和抹成 NULL。
    assert_eq!(qa.tokens_5h, Some(20), "5h 窗口内两条 ×10，无 usage 的那条按 0");
    assert_eq!(qa.tokens_7d, Some(30), "7d 窗口覆盖三条带 token 的流水");
    assert_eq!(q.get(&b).unwrap().cost_5h, Some(16.0), "费用不得跨账号串");
    assert_eq!(q.get(&b).unwrap().requests_5h, Some(1), "请求数不得跨账号串");
    assert_eq!(q.get(&b).unwrap().tokens_5h, Some(10), "token 不得跨账号串");

    // 单账号入口与批量入口必须给出同一份结果。
    assert_eq!(store.latest_quota(a).unwrap().unwrap().cost_5h, qa.cost_5h);
    assert_eq!(store.latest_quota(a).unwrap().unwrap().ts, qa.ts);
    assert!(store.latest_quota(999).unwrap().is_none(), "不存在的账号应为 None");
}

/// RPM 只数最近 60 秒，且不跨账号；窗口外的老流水与从未发过请求的号都不得混进来。
///
/// 时间基准取的是库里的 `unixepoch()`（与写入侧同源），所以这条用例也顺带钉住
/// 「两边同一个时钟」：若哪天读侧改用 Rust 的系统时间，边界上的行就会时有时无。
#[test]
fn recent_rpm_counts_only_the_last_minute() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).unwrap().id;
    let now: i64 = store.conn.lock().query_row("SELECT unixepoch()", [], |r| r.get(0)).unwrap();
    let hit = |cred_id, ts| {
        let rec = UsageRecord { cred_id: Some(cred_id), ..Default::default() };
        store.insert_usage_log_at(&rec, Some(ts)).unwrap();
    };
    hit(a, now);
    hit(a, now - 30);
    hit(a, now - 120); // 窗口外
    hit(b, now - 5);

    let rpm = store.recent_rpm().unwrap();
    assert_eq!(rpm.get(&a).copied(), Some(2), "两分钟前那条不在 60 秒窗口内");
    assert_eq!(rpm.get(&b).copied(), Some(1), "RPM 不得跨账号串");
    assert_eq!(store.recent_rpm_of(a).unwrap(), 2, "单账号入口须与批量口径一致");
    assert_eq!(store.recent_rpm_of(b).unwrap(), 1);

    // 从未发过请求的号压根不进 map（调用方按 0 处理），单账号入口直接给 0。
    let c = store.insert("c", None, "tc", "rc", 0, None, None, 1).unwrap().id;
    assert_eq!(store.recent_rpm().unwrap().get(&c), None);
    assert_eq!(store.recent_rpm_of(c).unwrap(), 0);

    // 全局 RPM 必须恰好是各账号之和：两个数会并排显示在同一屏上，对不上比看不到更糟。
    assert_eq!(store.total_rpm().unwrap(), 3);
    assert_eq!(
        store.total_rpm().unwrap(),
        store.recent_rpm().unwrap().values().sum::<i64>(),
        "全局与逐账号必须同口径（同一张表、同一个窗口）"
    );

    // 没落到任何账号头上的流水（选号前就失败的那些）不计入——它们压根没发出去。
    store
        .insert_usage_log_at(&UsageRecord { cred_id: None, ..Default::default() }, Some(now))
        .unwrap();
    assert_eq!(store.total_rpm().unwrap(), 3, "无账号的流水不进全局 RPM");
}

/// 只有一个窗口带 `reset` 时，窗口统计不得被连接条件的下界误伤成 0。
///
/// 这条护栏针对的是那个下界本身：它取「两个窗口起点里更早的那个」，而 SQLite 的
/// `min(NULL, x)` 是 **NULL**——不加 COALESCE 兜底的话，缺一个 reset 就会让整个
/// ON 条件恒假、一行流水都连不上，窗口费用与请求数齐刷刷变成 0。
#[test]
fn window_stats_survive_a_missing_reset() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).unwrap().id;

    // a：只有 5h 有 reset（窗口起点 82_000），7d 一直为空。
    log_row(&store, a, 10_000, 1.0, Some(100_000), None); // 5h 窗口外
    log_row(&store, a, 90_000, 2.0, Some(100_000), None); // 窗口内
    log_row(&store, a, 95_000, 4.0, Some(100_000), None); // 窗口内
    let qa = store.latest_quota(a).unwrap().unwrap();
    assert_eq!(qa.cost_5h, Some(6.0), "缺 7d reset 不该把 5h 窗口打成 0");
    assert_eq!(qa.requests_5h, Some(2));
    assert_eq!(qa.tokens_5h, Some(20));
    assert_eq!(qa.cost_7d, None, "没有 7d reset 就没有 7d 窗口可算");
    assert_eq!(qa.requests_7d, None);
    assert_eq!(qa.tokens_7d, None, "没有窗口就没有 token 可算，不能给 0");

    // b：反过来只有 7d 有 reset（窗口起点 100_000 - 604_800，含全部行）。
    log_row(&store, b, 90_000, 8.0, None, Some(100_000));
    log_row(&store, b, 95_000, 16.0, None, Some(100_000));
    let qb = store.latest_quota(b).unwrap().unwrap();
    assert_eq!(qb.cost_7d, Some(24.0), "缺 5h reset 不该把 7d 窗口打成 0");
    assert_eq!(qb.requests_7d, Some(2));
    assert_eq!(qb.tokens_7d, Some(20));
    assert_eq!(qb.cost_5h, None);
    assert_eq!(qb.tokens_5h, None);
}

/// 窗口 token 数按官方 `usage` 的四项相加，且**不看模型认不认得**。
///
/// 两处容易算漏，各钉一条：
/// - 缓存写只报了 5m/1h 细分、没报合计时要退回两档之和，否则这类响应的缓存写整段丢失；
/// - 模型不在价目表里时 `cost_usd` 为 NULL（费用算不出），但 token 是上游实报的，
///   该照数——把它跟着费用一起吞掉，卡片上就会出现「有请求、0 token」。
#[test]
fn window_tokens_sum_official_usage_fields() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;
    let reset = 100_000; // 5h 窗口起点 82_000

    // 只有 5m/1h 细分的一条：输入 10 + 输出 20 + 缓存写 (30+40) + 缓存读 50 = 150。
    store
        .insert_usage_log_at(
            &UsageRecord {
                cred_id: Some(a),
                has_usage: true,
                input_tokens: Some(10),
                output_tokens: Some(20),
                cache_5m_tokens: Some(30),
                cache_1h_tokens: Some(40),
                cache_read_tokens: Some(50),
                rl_5h_utilization: Some(0.5),
                rl_5h_reset: Some(reset),
                ..Default::default()
            },
            Some(90_000),
        )
        .unwrap();
    // 模型未知（cost_usd 为 None）但有 token 的一条：1 + 2 = 3。
    store
        .insert_usage_log_at(
            &UsageRecord {
                cred_id: Some(a),
                has_usage: true,
                input_tokens: Some(1),
                output_tokens: Some(2),
                ..Default::default()
            },
            Some(95_000),
        )
        .unwrap();

    let q = store.latest_quota(a).unwrap().unwrap();
    assert_eq!(q.tokens_5h, Some(153), "细分缓存写与未计价的行都要计入");
    assert_eq!(q.cost_5h, Some(0.0), "两条都没有 cost_usd，费用仍是 0");
    assert_eq!(q.requests_5h, Some(2));
}

/// 模型级冷却必须能被后台读到。
///
/// 这是一处真实的观测盲区：模型级 429（fable 撞超额池就是这一档）只写进
/// `(cred_id, 模型)` 那些格子，而控制台读的是账号级那一格，于是选号侧明明已经跳过
/// 这个模型、界面上却一片正常，「冷却中」那套筛选与徽章形同虚设。
#[test]
fn model_level_cooldown_is_visible_to_the_console() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;

    store.mark_rate_limited(a, Some("claude-fable-5"), Duration::from_secs(300));
    store.mark_rate_limited(a, Some("claude-opus-5"), Duration::from_secs(30));

    // 账号级那一格没被写过，account 档仍应是 0——模型级不等于账号被限流。
    assert_eq!(store.rate_limited_secs(a), 0, "模型级不该冒充账号级");

    let models = store.rate_limited_models(a);
    assert_eq!(models.len(), 2);
    // 剩得最久的排前面，展示顺序必须稳定（HashMap 迭代序是随机的）。
    assert_eq!(models[0].0, "claude-fable-5");
    assert!(models[0].1 > 290 && models[0].1 <= 300, "{models:?}");
    assert_eq!(models[1].0, "claude-opus-5");

    // 解除后即消失；账号级那档也照常工作（落库失败的兜底路径走它）。
    store.clear_rate_limited(a, None);
    assert!(store.rate_limited_models(a).is_empty());
    store.mark_rate_limited(a, None, Duration::from_secs(120));
    assert!(store.rate_limited_secs(a) > 110);
    assert!(store.rate_limited_models(a).is_empty(), "账号级不该混进模型级明细");
}

/// 最低客户端版本：未设置、空串、纯空白都等于「不限」（`None`），其余去掉首尾空白后原样返回。
/// 空白不归一成 `None` 的话，代理侧会拿一个空串去 `parse_version`，虽然也放行，但网页上
/// 会显示成「已配置」——两边说法不一致比闸本身更难查。
#[test]
fn blank_min_client_version_means_no_limit() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);

    assert_eq!(store.min_client_version(), None, "没配就是不限");
    store.set_setting(MIN_CLIENT_VERSION, "2.1.220").unwrap();
    assert_eq!(store.min_client_version().as_deref(), Some("2.1.220"));
    store.set_setting(MIN_CLIENT_VERSION, "  2.1  ").unwrap();
    assert_eq!(store.min_client_version().as_deref(), Some("2.1"), "首尾空白不带进判定");
    store.set_setting(MIN_CLIENT_VERSION, "   ").unwrap();
    assert_eq!(store.min_client_version(), None, "只剩空白等于没配");
    store.delete_setting(MIN_CLIENT_VERSION).unwrap();
    assert_eq!(store.min_client_version(), None);
}

/// 登录 scope：没配 / 配了空白都退回官方默认那一串；配了就按规整后的形态原样发出去。
/// 库里的值可能来自另一台机器的 import，故读出来还要再规整一遍（顺序不动、只去重与压空白）。
#[test]
fn oauth_scopes_fall_back_to_the_official_set() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);

    assert_eq!(store.oauth_scopes(), crate::config::SCOPES, "没配就是官方那一整套");
    store.set_setting(OAUTH_SCOPES, crate::config::SCOPES_MINIMAL).unwrap();
    assert_eq!(store.oauth_scopes(), crate::config::SCOPES_MINIMAL);
    store.set_setting(OAUTH_SCOPES, "  user:inference   user:profile  user:inference ").unwrap();
    assert_eq!(store.oauth_scopes(), "user:inference user:profile", "压成单空格、按输入顺序去重");
    store.set_setting(OAUTH_SCOPES, "   ").unwrap();
    assert_eq!(store.oauth_scopes(), crate::config::SCOPES, "只剩空白等于没配");
    store.delete_setting(OAUTH_SCOPES).unwrap();
    assert_eq!(store.oauth_scopes(), crate::config::SCOPES);
}

/// 设置项走内存缓存后，读写口径必须与直接查库一致（含删除与重开库）。
#[test]
fn settings_cache_matches_the_database() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);

    assert_eq!(store.get_setting(REQUIRE_DEVICE_ID).unwrap(), None);
    store.set_setting(REQUIRE_DEVICE_ID, "false").unwrap();
    assert_eq!(store.get_setting(REQUIRE_DEVICE_ID).unwrap().as_deref(), Some("false"));
    assert!(!store.require_device_id(), "缓存值要真的参与判定");

    // 缓存和库不能漂：直接查库应看到同一个值。
    let in_db: String = store
        .conn
        .lock()
        .query_row("SELECT value FROM settings WHERE key = ?1", [REQUIRE_DEVICE_ID], |r| r.get(0))
        .unwrap();
    assert_eq!(in_db, "false");

    store.set_setting(REQUIRE_DEVICE_ID, "true").unwrap();
    assert!(store.require_device_id(), "覆盖写要立刻生效");
    store.delete_setting(REQUIRE_DEVICE_ID).unwrap();
    assert_eq!(store.get_setting(REQUIRE_DEVICE_ID).unwrap(), None);
    assert!(store.require_device_id(), "删除后退回默认值（要求设备身份）");

    // 转发开关同样走缓存，且新键优先于旧键。
    store.set_setting(CACHE_SCOPE_GLOBAL, "false").unwrap();
    assert!(!store.forward_flags().system_shape, "旧键应在新键缺省时生效");
    store.set_setting(SYSTEM_SHAPE, "true").unwrap();
    assert!(store.forward_flags().system_shape, "新键存在就以新键为准");
}

/// 选号与后台列表数「活跃绑定」的那条 SQL 必须走 `last_seen_at` 索引：没跑过 ANALYZE 的库
/// 若按 cred 索引分组，会逐行回表扫完保留期内的全部绑定（见 [`active_counts_sql`]）。
#[test]
fn active_counts_sql_scans_by_last_seen() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    for table in ["device_bindings", "session_bindings"] {
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", active_counts_sql(table, 3600)))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map([3600], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let index = format!("idx_{table}_seen");
        assert!(
            plan.iter().any(|d| d.contains(&index) && d.contains("last_seen_at>?")),
            "{table} 应按 last_seen_at 范围扫描，实际计划：{plan:?}"
        );
    }
}

/// 后台统计走的只读连接：看得到主连接刚提交的写入，自己写不进去。
#[test]
fn reader_sees_committed_writes_and_rejects_writes() {
    let dir = std::env::temp_dir().join(format!("luban-reader-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("t.db{suffix}")));
    }

    let store = CredentialStore::open_at(&path).unwrap();
    assert_eq!(store.readers.len(), READER_POOL_SIZE, "文件库应开出独立的只读连接池");
    {
        // 一条只读连接被长查询占着时，下一条后台读拿另一条，不排队。parking_lot 的锁
        // 不可重入：池子退化成一条的话，这里会直接卡死而不是悄悄通过。
        let _busy = store.read_conn();
        let _other = store.read_conn();
    }
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;
    store
        .conn
        .lock()
        .execute(
            "INSERT INTO device_bindings (device_id, cred_id) VALUES ('d1', ?1), ('d2', ?1)",
            [a],
        )
        .unwrap();
    assert_eq!(store.device_counts().unwrap().get(&a).copied(), Some(2));
    assert!(
        store.read_conn().execute("DELETE FROM device_bindings", []).is_err(),
        "只读连接不应能写"
    );
    assert_eq!(store.device_counts().unwrap().get(&a).copied(), Some(2));

    // 所有改走只读连接的方法都在真正的只读连接上跑一遍：内存库测试里 reader 为 None、
    // 读退回可写主连接，有人往这些方法里加了写语句，只有这里会报出来。
    store
        .insert_usage_log(&UsageRecord {
            cred_id: Some(a),
            cred_label: "a".into(),
            status: 200,
            ..Default::default()
        })
        .unwrap();
    assert!(store.mark_banned(a, "banned").unwrap());
    let since = 0;
    store.latest_quotas().unwrap();
    store.latest_quota(a).unwrap();
    store.list_devices(a).unwrap();
    store.list_sessions(a).unwrap();
    store.session_counts().unwrap();
    store.recent_rpm().unwrap();
    store.recent_rpm_of(a).unwrap();
    store.total_rpm().unwrap();
    store.last_used().unwrap();
    store.cost_by_cred().unwrap();
    store.cache_report(since, 3600, 0).unwrap();
    store.ttft_report(since, 3600, 0).unwrap();
    store.usage_breakdown(since, BreakdownBy::Model, 12).unwrap();
    store.usage_breakdown(since, BreakdownBy::Account, 12).unwrap();
    store.credential_stats(a, since, 3600, 0, 20).unwrap();
    store.local_rejections(since).unwrap();
    let q = UsageLogQuery { limit: 10, ..Default::default() };
    assert_eq!(store.usage_log_stats(q.clone()).unwrap().total, 1);
    assert_eq!(store.query_usage_logs(q).unwrap().len(), 1);
    let ev = store.list_ban_events(None, 10).unwrap().remove(0);
    assert_eq!(store.ban_counts().unwrap().get(&a).copied(), Some(1));
    assert_eq!(store.frozen_usage_logs(ev.id, 10, 0).unwrap().0, 1);

    // 只读连接先于主连接关闭，主连接关库时才做得了 checkpoint、删得掉 WAL。
    drop(store);
    assert!(
        std::fs::metadata(dir.join("t.db-wal")).map(|m| m.len() == 0).unwrap_or(true),
        "关库后 WAL 应已 checkpoint 并删除"
    );
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("t.db{suffix}")));
    }
    let _ = std::fs::remove_dir(&dir);
}

/// 重开同一个库时，缓存要从库里重新装载（否则重启后设置全部凭空回到默认值）。
#[test]
fn settings_cache_is_reloaded_on_open() {
    let dir = std::env::temp_dir().join(format!("luban-settings-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    let _ = std::fs::remove_file(&path);

    {
        let conn = Connection::open(&path).unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        store.set_setting(BARE_RATE_LIMIT, "42").unwrap();
    }
    let conn = Connection::open(&path).unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    assert_eq!(store.bare_rate_limit(), 42, "重开库后设置应从库里装回来");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

/// 裁剪只动流水，不动账本：累计费用/最近使用/额度快照在裁剪后原样保留。
#[test]
fn prune_keeps_ledger() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;

    // 一条早已过保留期的旧流水（带限流头，会写快照）+ 一条刚发生的新流水（无头）。
    let old_ts = 1_000;
    log_row(&store, a, old_ts, 2.0, Some(old_ts + 100), Some(old_ts + 100));
    store
        .insert_usage_log(&UsageRecord {
            cred_id: Some(a),
            cost_usd: Some(1.0),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(store.prune_usage_logs().unwrap(), 1, "只裁过保留期的旧流水");
    assert_eq!(store.list_usage_logs(10).unwrap().len(), 1, "新流水应保留");
    assert_eq!(store.cost_of(a).unwrap(), 3.0, "累计费用是账本口径，不随裁剪变小");
    assert!(store.last_used_at(a).unwrap().is_some());
    let q = store.latest_quota(a).unwrap().expect("快照在账本里长存");
    assert_eq!(q.ts, old_ts, "快照仍是最后一次带限流头的那条");
    // 窗口统计只看还留着的流水：新流水 ts 在窗口起点之后，计入。
    assert_eq!(q.cost_5h, Some(1.0));
    assert_eq!(q.requests_5h, Some(1));
}

/// 「非流转流」标记要能落库并原样读回；旧库补出来的那一列默认 0（它们本就早于这个
/// 功能，没有一条是聚合来的），不能是 NULL——读取侧按 `i64` 取，NULL 会直接报错。
#[test]
fn sse_aggregated_round_trips_and_defaults_to_false() {
    // 先用**没有**该列的旧库建表，再走 init_schema 补列，走的正是升级路径。
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE usage_logs (
                id INTEGER PRIMARY KEY,
                ts INTEGER NOT NULL DEFAULT (unixepoch()),
                cred_id INTEGER, cred_label TEXT NOT NULL DEFAULT '',
                device_id TEXT, model TEXT, path TEXT NOT NULL DEFAULT '',
                status INTEGER NOT NULL DEFAULT 0,
                has_usage INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER, output_tokens INTEGER,
                cache_creation_tokens INTEGER, cache_read_tokens INTEGER,
                ttft_ms INTEGER, total_ms INTEGER
            ) STRICT;
             INSERT INTO usage_logs (cred_label) VALUES ('old');",
    )
    .unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let cred = store.insert("a", None, "t", "r", 0, None, None, 1).unwrap().id;

    for aggregated in [true, false] {
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(cred),
                sse_aggregated: aggregated,
                ..Default::default()
            })
            .unwrap();
    }

    let logs = store.query_usage_logs(UsageLogQuery { limit: 10, ..Default::default() }).unwrap();
    // 倒序：最新写入的（false）在前，然后是 true，最后是升级前就存在的那条。
    assert_eq!(
        logs.iter().map(|l| l.sse_aggregated).collect::<Vec<_>>(),
        vec![false, true, false],
        "标记要原样读回，且旧记录退化成 false 而不是读取失败"
    );
}

/// 请求明细的筛选与分页：按账号只出该账号的记录，页码不重叠，且**锚点之后新写入的记录
/// 不得挤动已在翻的页**——这正是页码翻页要带 `until_id` 的理由。
#[test]
fn usage_logs_filter_by_credential_and_paginate() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).unwrap().id;
    let log = |cred: i64, cost: f64| {
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(cred),
                cost_usd: Some(cost),
                ..Default::default()
            })
            .unwrap()
    };
    // a 四条、b 一条，交替写入，确保筛选不是靠「恰好连续」蒙对的。
    for (cred, cost) in [(a, 1.0), (a, 2.0), (b, 100.0), (a, 4.0), (a, 8.0)] {
        log(cred, cost);
    }

    let all = store.query_usage_logs(UsageLogQuery { limit: 10, ..Default::default() }).unwrap();
    assert_eq!(all.len(), 5, "不筛时是全部");

    // 统计与记录同一套条件：a 的四条、花费合计 15，最大 id 即锚点。
    let only_a = UsageLogQuery { cred_id: Some(a), ..Default::default() };
    let stats = store.usage_log_stats(only_a.clone()).unwrap();
    assert_eq!(stats.total, 4, "b 的那条不该计入");
    assert_eq!(stats.cost_usd, 15.0);
    let anchor = stats.max_id.expect("有记录就有锚点");

    let page = |n: i64| {
        store
            .query_usage_logs(UsageLogQuery {
                cred_id: Some(a),
                until_id: Some(anchor),
                offset: n * 3,
                limit: 3,
                request_id: None,
                model: None,
                since: None,
                session_key: None,
                session_id: None,
                owner_id: None,
            })
            .unwrap()
    };
    let first = page(0);
    assert_eq!(first.len(), 3);
    assert!(first.iter().all(|l| l.cred_id == Some(a)), "b 的那条不该出现");
    assert!(first.windows(2).all(|w| w[0].id > w[1].id), "按 id 倒序");

    let second = page(1);
    assert_eq!(second.len(), 1, "a 共 4 条，第二页只剩 1 条");
    assert!(second[0].id < first[2].id, "第二页不得与第一页重叠");
    assert!(page(2).is_empty(), "翻到底为空");

    // 翻页途中来了新请求：锚点之下的两页一字不变，锚点之上的统计才会长。
    let ids = |logs: &[UsageLog]| logs.iter().map(|l| l.id).collect::<Vec<_>>();
    log(a, 16.0);
    assert_eq!(ids(&page(0)), ids(&first), "新记录不得把第一页往后挤");
    assert_eq!(ids(&page(1)), ids(&second));
    let pinned = store.usage_log_stats(UsageLogQuery { until_id: Some(anchor), ..only_a.clone() });
    assert_eq!(pinned.unwrap().total, 4, "钉在锚点上的统计不动");
    assert_eq!(store.usage_log_stats(only_a).unwrap().total, 5, "不带锚点才看得到新记录");
}

/// 请求 id 落库、可精确查；筛选走索引而不是整表扫（按号翻页同样）。
#[test]
fn usage_logs_are_searchable_by_request_id_using_indexes() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let log = |cred: i64, rid: &str, up: Option<&str>| {
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(cred),
                request_id: Some(rid.into()),
                upstream_request_id: up.map(Into::into),
                ..Default::default()
            })
            .unwrap()
    };
    log(a, "lb-1", Some("req_up_1"));
    log(a, "lb-2", None);
    log(b, "lb-3", Some("req_up_3"));

    let by =
        |rid: &str| UsageLogQuery { request_id: Some(rid.into()), limit: 10, ..Default::default() };
    let hit = store.query_usage_logs(by("lb-3")).unwrap();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].cred_id, Some(b));
    assert_eq!(hit[0].upstream_request_id.as_deref(), Some("req_up_3"));
    assert_eq!(store.usage_log_stats(by("lb-3")).unwrap().total, 1, "统计与取页同一套条件");
    assert!(store.query_usage_logs(by("nope")).unwrap().is_empty());
    // 空白视同不筛。
    assert_eq!(store.query_usage_logs(by("   ")).unwrap().len(), 3);
    // 与按号筛叠加。
    let both = UsageLogQuery {
        cred_id: Some(a),
        request_id: Some("lb-3".into()),
        limit: 10,
        ..Default::default()
    };
    assert!(store.query_usage_logs(both).unwrap().is_empty(), "lb-3 是 b 的");

    // 查询计划必须走索引：按号翻页走 (cred_id, id)，按请求 id 走 request_id 索引。
    let plan = |q: &UsageLogQuery| usage_page_plan(&store, q);
    let p = plan(&UsageLogQuery {
        cred_id: Some(a),
        until_id: Some(100),
        limit: 10,
        ..Default::default()
    });
    assert!(p.contains("idx_usage_logs_cred_id"), "按号翻页应走 (cred_id, id) 索引: {p}");
    assert!(!p.contains("TEMP B-TREE"), "索引序即排序序，不该再排一遍: {p}");
    let p = plan(&by("lb-1"));
    assert!(p.contains("idx_usage_logs_request_id"), "按请求 id 应走索引: {p}");
    // 按模型下钻（拆分表点一行）：统计不带锚点、翻页时带锚点，取页恒带锚点。统计只扫
    // 窗口内的行；取页的子查询只取 id，(model, ts) 覆盖得住，不回表读整行。
    for until_id in [None, Some(100)] {
        let q = UsageLogQuery {
            model: Some("claude-opus-5".into()),
            since: Some(0),
            until_id,
            limit: 10,
            ..Default::default()
        };
        let p = usage_stats_plan(&store, &q);
        assert!(
            p.contains("idx_usage_logs_model_ts (model=? AND ts>?)"),
            "按模型统计应按 (model, ts) 圈窗口: {p}"
        );
        let p = plan(&q);
        assert!(
            p.contains("COVERING INDEX idx_usage_logs_model_ts"),
            "按模型取页的子查询应只在索引里取 id: {p}"
        );
    }
    // 按号统计（详情页首屏）走覆盖索引，费用列也在索引里。
    let p = usage_stats_plan(&store, &UsageLogQuery { cred_id: Some(a), ..Default::default() });
    assert!(p.contains("COVERING INDEX"), "按号统计不该回表: {p}");
}

/// 流水按**模拟会话键**筛：名额对话框里会话那一行点「看请求」走的就是它。键落进流水、
/// 与按号筛可叠、空白视同不筛，且要走那条部分索引（带设备身份的请求这一列为空、不进索引）。
#[test]
fn usage_logs_filter_by_session_key_using_a_partial_index() {
    let (store, ids) = store_with(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let log = |cred: i64, key: Option<&str>| {
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(cred),
                forensics: Forensics { session_key: key.map(Into::into), ..Default::default() },
                ..Default::default()
            })
            .unwrap()
    };
    let one = "lb:v2:sid:7fe47444-c834-44e0-b568-d61e07daa35e";
    let two = "lb:v2:pfx:3f9a1c7e5b2d4680a1b2c3d4e5f60718";
    log(a, Some(one));
    log(a, Some(one));
    log(a, Some(two));
    log(b, Some(one));
    log(a, None); // 带设备身份的那类：这一列为空

    let by = |key: &str| UsageLogQuery {
        session_key: Some(key.into()),
        limit: 10,
        ..Default::default()
    };
    let hit = store.query_usage_logs(by(one)).unwrap();
    assert_eq!(hit.len(), 3, "两个号上的同键请求都算");
    assert!(hit.iter().all(|l| l.forensics.session_key.as_deref() == Some(one)), "键随流水落库");
    assert_eq!(store.usage_log_stats(by(one)).unwrap().total, 3, "统计与取页同一套条件");
    assert_eq!(store.query_usage_logs(by(two)).unwrap().len(), 1);
    assert!(store.query_usage_logs(by("lb:v2:pfx:nope")).unwrap().is_empty());
    assert_eq!(store.query_usage_logs(by("  ")).unwrap().len(), 5, "空白视同不筛");
    // 与按号筛叠加——会话行点进来带的正是这两项。
    let scoped = UsageLogQuery {
        cred_id: Some(a),
        session_key: Some(one.into()),
        limit: 10,
        ..Default::default()
    };
    assert_eq!(store.query_usage_logs(scoped.clone()).unwrap().len(), 2, "b 上那条不算");
    assert_eq!(store.usage_log_stats(scoped).unwrap().total, 2);

    // 查询计划：按会话键取页要走 (session_key, id) 那条部分索引，不能整表扫。
    let plan = usage_page_plan(&store, &by(one));
    assert!(plan.contains("idx_usage_logs_session_key"), "应走会话键索引: {plan}");
    assert!(!plan.contains("TEMP B-TREE"), "索引序即排序序，不该再排一遍: {plan}");
}

/// 按**会话 id** 查：出站与来访两侧任一命中。走模拟路径时两者是两个不同的 uuid，而来查
/// 的人手里只会有其中一个（下游用户知道自己那个，从上游侧回查的人拿到出站那个），两边
/// 都得能查到同一批请求。两条部分索引要让那个 OR 走 MULTI-INDEX OR，不能退成整表扫。
#[test]
fn usage_logs_look_up_a_session_id_on_either_side() {
    let (store, ids) = store_with(&["a"]);
    let a = ids[0];
    let log = |out: Option<&str>, inn: Option<&str>| {
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(a),
                forensics: Forensics {
                    session_id: out.map(Into::into),
                    session_id_in: inn.map(Into::into),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
    };
    let client = "11111111-2222-4333-8444-555555555555";
    let upstream = "7fe47444-c834-44e0-b568-d61e07daa35e";
    log(Some(upstream), Some(client)); // 走模拟：两侧不同
    log(Some(upstream), Some(client));
    log(Some(client), Some(client)); // 没改身份：两侧同一个
    log(None, Some(client)); // 本地拒绝：只有来访那侧
    log(Some("99999999-9999-4999-8999-999999999999"), None); // luban 自己发的

    let by =
        |sid: &str| UsageLogQuery { session_id: Some(sid.into()), limit: 10, ..Default::default() };
    assert_eq!(
        store.query_usage_logs(by(client)).unwrap().len(),
        4,
        "下游拿自己那个 uuid 来查：被改过身份的两条、没改的一条、本地拒绝的一条"
    );
    assert_eq!(
        store.query_usage_logs(by(upstream)).unwrap().len(),
        2,
        "从上游侧回查：只有出站是它的那两条"
    );
    assert_eq!(store.usage_log_stats(by(client)).unwrap().total, 4, "统计与取页同一套条件");
    assert!(store.query_usage_logs(by("00000000-0000-4000-8000-000000000000")).unwrap().is_empty());
    assert_eq!(store.query_usage_logs(by("  ")).unwrap().len(), 5, "空白视同不筛");

    let plan = usage_page_plan(&store, &by(client));
    assert!(plan.contains("idx_usage_logs_session_id"), "出站那侧要走索引: {plan}");
    assert!(plan.contains("idx_usage_logs_session_id_in"), "来访那侧要走索引: {plan}");
    assert!(!plan.contains("SCAN usage_logs"), "不能整表扫: {plan}");
}

/// overage-in-use 标记随快照落账：带限流头的响应写入即更新，后续不带头的响应不得抹掉，
/// 下一条带头的响应按新值覆盖。这是「额度满但不 429（usage credits 在放行）」唯一的外显信号。
#[test]
fn overage_marker_lands_in_snapshot() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;

    store
        .insert_usage_log_at(
            &UsageRecord {
                cred_id: Some(a),
                rl_5h_utilization: Some(1.02),
                rl_5h_reset: Some(9_000),
                rl_overage_in_use: Some(true),
                ..Default::default()
            },
            Some(1_000),
        )
        .unwrap();
    assert_eq!(store.latest_quota(a).unwrap().unwrap().overage_in_use, Some(true));

    // 不带限流头的响应（CDN 拦截页之类）不动快照。
    store
        .insert_usage_log_at(
            &UsageRecord { cred_id: Some(a), cost_usd: Some(1.0), ..Default::default() },
            Some(2_000),
        )
        .unwrap();
    let q = store.latest_quota(a).unwrap().unwrap();
    assert_eq!(q.overage_in_use, Some(true), "无头响应不得抹掉标记");
    assert_eq!(q.ts, 1_000);

    // 额度恢复后上游不再报 overage → 按新值覆盖。
    store
        .insert_usage_log_at(
            &UsageRecord {
                cred_id: Some(a),
                rl_5h_utilization: Some(0.3),
                rl_5h_reset: Some(20_000),
                rl_overage_in_use: Some(false),
                ..Default::default()
            },
            Some(3_000),
        )
        .unwrap();
    assert_eq!(store.latest_quota(a).unwrap().unwrap().overage_in_use, Some(false));
}

fn win(name: &str, util: f64, reset: i64, status: &str) -> QuotaWindow {
    QuotaWindow {
        name: name.into(),
        status: Some(status.into()),
        utilization: Some(util),
        reset: Some(reset),
    }
}

/// 全窗口快照原样落库、原样读回——含 `7d_oi` 这类**没有专用列**的窗口。
///
/// 这一列存在的全部意义就是它：5h/7d 两组写死的列覆盖不到超额池，而实测里真正被拒的
/// 正是它，缺了它后台就只能看到「两个窗口都没满」却解释不了这个号为什么在烧钱。
#[test]
fn snapshot_keeps_windows_without_dedicated_columns() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;

    // 形态取自 proxy::rate_limit_scope 记录的那次真实 fable-5 429：基础窗口都很空，
    // 满掉的只有超额池。
    let windows = vec![
        win("5h", 0.20, 9_000, "allowed"),
        win("7d", 0.70, 90_000, "allowed"),
        win("7d_oi", 1.02, 400_000, "rejected"),
    ];
    store
        .insert_usage_log_at(
            &UsageRecord {
                cred_id: Some(a),
                rl_5h_utilization: Some(0.20),
                rl_5h_reset: Some(9_000),
                rl_7d_utilization: Some(0.70),
                rl_7d_reset: Some(90_000),
                rl_representative: Some("seven_day_overage_included".into()),
                rl_overage_in_use: Some(true),
                windows: windows.clone(),
                ..Default::default()
            },
            Some(1_000),
        )
        .unwrap();

    let q = store.latest_quota(a).unwrap().unwrap();
    assert_eq!(q.windows, windows, "全窗口快照应原样读回");
    // 专用列不受影响：窗口内费用/请求数仍靠它们反推窗口起点。
    assert_eq!(q.rl_5h_utilization, Some(0.20));
    assert_eq!(q.rl_7d_reset, Some(90_000));
    // 批量口径与单条口径是同一条 SQL，不能只有一边带窗口。
    assert_eq!(store.latest_quotas().unwrap().get(&a).unwrap().windows, windows);
}

/// **只**上报没有专用列的窗口时，照样要写出快照。
///
/// 旧判据是 `rl_5h_utilization.is_some() || rl_7d_utilization.is_some()`，于是这种账号
/// 永远写不进 credential_stats，卡片恒为「暂无数据」——哪怕它此刻正靠 usage credits 放行。
#[test]
fn snapshot_is_written_even_without_5h_or_7d() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;

    store
        .insert_usage_log_at(
            &UsageRecord {
                cred_id: Some(a),
                rl_overage_in_use: Some(true),
                windows: vec![win("7d_oi", 1.02, 400_000, "rejected")],
                ..Default::default()
            },
            Some(1_000),
        )
        .unwrap();

    let q = store.latest_quota(a).unwrap().expect("只有 7d_oi 的账号也必须有快照");
    assert_eq!(q.overage_in_use, Some(true));
    assert_eq!(q.windows.len(), 1);
    assert_eq!(q.windows[0].name, "7d_oi");
    // 没有 5h/7d 就没有窗口起点可反推，窗口内费用/请求数保持空。
    assert_eq!(q.cost_5h, None);
    assert_eq!(q.requests_7d, None);
}

/// 一个窗口都没有的响应（CDN 拦截页那类，只剩 unified-status）不得覆盖已有快照——
/// 否则等于拿一条信息更少的记录抹掉信息更多的。
#[test]
fn windowless_response_does_not_erase_snapshot() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;

    let windows = vec![win("5h", 0.5, 9_000, "allowed")];
    store
        .insert_usage_log_at(
            &UsageRecord {
                cred_id: Some(a),
                rl_5h_utilization: Some(0.5),
                rl_5h_reset: Some(9_000),
                windows: windows.clone(),
                ..Default::default()
            },
            Some(1_000),
        )
        .unwrap();
    store
        .insert_usage_log_at(
            &UsageRecord {
                cred_id: Some(a),
                unified_status: Some("rejected".into()),
                ..Default::default()
            },
            Some(2_000),
        )
        .unwrap();

    let q = store.latest_quota(a).unwrap().unwrap();
    assert_eq!(q.ts, 1_000, "无窗口的响应不该顶掉快照");
    assert_eq!(q.windows, windows);
}

/// 老库补出来的 `windows` 是 NULL，读回时退化成空列表（前端即按「只有 5h/7d」渲染），
/// 而不是把整张账号列表打成 500。存进脏 JSON 同理。
#[test]
fn legacy_and_corrupt_windows_degrade_to_empty() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;

    // 模拟老库：快照行有 5h/7d，windows 列为 NULL。
    {
        let conn = store.conn.lock();
        conn.execute(
            "INSERT INTO credential_stats
                     (cred_id, snapshot_ts, rl_5h_utilization, rl_5h_reset, windows)
                 VALUES (?1, 1000, 0.4, 9000, NULL)",
            [a],
        )
        .unwrap();
    }
    let q = store.latest_quota(a).unwrap().unwrap();
    assert!(q.windows.is_empty(), "老库的 NULL 应读成空列表");
    assert_eq!(q.rl_5h_utilization, Some(0.4), "专用列照常可用");

    store.conn.lock().execute("UPDATE credential_stats SET windows = '{oops'", []).unwrap();
    assert!(store.latest_quota(a).unwrap().unwrap().windows.is_empty(), "脏 JSON 不得报错");
}

/// 老库升级（账本为空、流水有历史）时 init_schema 一次性回填账本；账本非空则不重复。
#[test]
fn backfill_ledger_on_first_upgrade() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;

    // 模拟老库形态：流水是历史攒下的（裸 INSERT，从未落过账），账本是空表。
    {
        let conn = store.conn.lock();
        conn.execute(
            "INSERT INTO usage_logs (cred_id, ts, cost_usd, device_id) \
                 VALUES (?1, 1000, 2.0, 'd1')",
            [a],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_logs (cred_id, ts, cost_usd, rl_5h_utilization, rl_5h_reset) \
                 VALUES (?1, 2000, 3.0, 0.5, 9000)",
            [a],
        )
        .unwrap();
        init_schema(&conn).unwrap();
    }

    assert_eq!(store.cost_of(a).unwrap(), 5.0, "累计费用应回填齐全");
    assert_eq!(store.last_used_at(a).unwrap(), Some(2_000));
    let q = store.latest_quota(a).unwrap().expect("快照应从最新带限流头的行回填");
    assert_eq!(q.ts, 2_000);
    assert_eq!(q.rl_5h_utilization, Some(0.5));
    let dev_cost: f64 = store
        .conn
        .lock()
        .query_row(
            "SELECT cost_usd FROM device_costs WHERE device_id = 'd1' AND cred_id = ?1",
            [a],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dev_cost, 2.0, "设备费用应回填");

    // 账本已非空：再跑一遍 init_schema（每次启动都会跑）不得重复累计。
    init_schema(&store.conn.lock()).unwrap();
    assert_eq!(store.cost_of(a).unwrap(), 5.0, "重复启动不应翻倍");
}

/// reset 为空时对应窗口的费用与请求数留空（而非 0）：分不清「没用」和「不知道窗口起点」
/// 会让卡片把未知显示成已用 0。
#[test]
fn latest_quotas_leaves_cost_none_without_reset() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;
    log_row(&store, a, 40_000, 3.0, Some(50_000), None); // 在 5h 窗口(32_000 起)内

    let q = store.latest_quota(a).unwrap().unwrap();
    assert_eq!(q.cost_5h, Some(3.0));
    assert_eq!(q.requests_5h, Some(1));
    assert_eq!(q.cost_7d, None, "无 7d reset 时不应给出 0");
    assert_eq!(q.requests_7d, None, "无 7d reset 时请求数也应未知");
}

/// 单账号的「最近使用 / 累计费用」与全量聚合同口径，无日志时分别是 None 与 0。
#[test]
fn single_cred_stats_match_batch() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).unwrap().id;
    log_row(&store, a, 1_000, 1.5, None, None);
    log_row(&store, a, 2_000, 2.5, None, None);
    log_row(&store, b, 3_000, 7.0, None, None);
    let c = store.insert("c", None, "tc", "rc", 0, None, None, 1).unwrap().id; // 从未被用过

    let last = store.last_used().unwrap();
    let costs = store.cost_by_cred().unwrap();
    for id in [a, b] {
        assert_eq!(store.last_used_at(id).unwrap(), last.get(&id).copied());
        assert_eq!(store.cost_of(id).unwrap(), costs[&id]);
    }
    assert_eq!(store.last_used_at(a).unwrap(), Some(2_000));
    assert_eq!(store.cost_of(a).unwrap(), 4.0);
    assert_eq!(store.last_used_at(c).unwrap(), None, "无日志时是 None 而非 0");
    assert_eq!(store.cost_of(c).unwrap(), 0.0);
}

/// 转发形态开关：**未设置时必须全开**——否则升级到带开关的版本会让既有部署的转发形态
/// 悄悄变样。只有 `"0"`/`"false"`（忽略大小写与首尾空白）算关，其余取值一律视为开。
#[test]
fn forward_flags_default_on_and_parse_off() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);

    assert_eq!(store.forward_flags(), ForwardFlags::default(), "空库应等于默认值");
    assert!(ForwardFlags::default().spoof_identity, "默认必须是开");
    assert!(ForwardFlags::default().system_shape);
    assert!(
        !ForwardFlags::default().fable_refusal_fallback,
        "fable 那档替用户决定换模型作答，默认必须是关"
    );
    assert!(
        !ForwardFlags::default().opus_refusal_fallback,
        "opus 那档是官方不产生的形态，默认必须是关"
    );
    assert!(
        !ForwardFlags::default().sim_billing_only,
        "仅注 billing header 是实验性形态，默认必须是关"
    );

    // 每个键各用一种「关」的写法，确认逐项独立且解析口径一致。
    for (key, off) in [
        (SPOOF_IDENTITY_ENABLED, "0"),
        (SPOOF_DEVICE_ID, "0"),
        (NORMALIZE_DEVICE_FP, "0"),
        (SPOOF_BILLING_CCH, "false"),
        (CCH_REAL_RECOMPUTE, "0"),
        (CCH_SIM_COMPUTE, "0"),
        (FILL_CLIENT_HEADERS, " FALSE "),
        (MERGE_BETA, "False"),
        (SYSTEM_SHAPE, "0"),
        (ORIG_HEADER_CASE, "0"),
        (THINKING_SIGNATURE_RETRY, "0"),
        (SIMULATE_CC, "0"),
        (SIMULATE_FULL_SYSTEM, "0"),
        (FILL_ABSENT_TOOLS, "0"),
        (SIM_TRIM_TOOLS, "0"),
        (SIM_BILLING_ONLY, "0"),
        (SIM_BILLING_KEEP_USER_ID, "0"),
        (REAL_BILLING_KEEP_USER_ID, "0"),
        (SIM_MESSAGE_THREADS, "0"),
        (FILL_METADATA, "0"),
        (RATE_LIMIT_RETRY, "0"),
        (SYSTEM_CACHE_SCOPE, "0"),
        (SYSTEM_CACHE_TTL, "0"),
        (EAGER_TOOL_STREAMING, "0"),
        (NONSTREAM_AS_SSE, "0"),
        (STRIP_EXTRA_FIELDS, "0"),
        (TOOL_NAME_MIMIC, "0"),
        (INJECT_THINKING, "0"),
        (REDACTED_THINKING_RETRY, "0"),
        (REJECT_OPENAI_SHAPE, "0"),
        (REJECT_SESSION_CONFLICT, "0"),
        (REJECT_PROBES, "0"),
        (REJECT_REFUSALS, "0"),
        (REJECT_EMPTY_REPLIES, "0"),
        (REJECT_LEARNED_SHAPES, "0"),
        (REJECT_PROBES_STRICT, "0"),
        (API_TELEMETRY, "0"),
        (KEEPALIVE_TELEMETRY, "0"),
        (FABLE_REFUSAL_FALLBACK, "0"),
        (OPUS_REFUSAL_FALLBACK, "0"),
    ] {
        store.set_setting(key, off).unwrap();
    }
    let f = store.forward_flags();
    assert_eq!(
        f,
        ForwardFlags {
            spoof_identity: false,
            spoof_device_id: false,
            normalize_device_fp: false,
            billing_cch: false,
            cch_real_recompute: false,
            cch_sim_compute: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
            thinking_signature_retry: false,
            redacted_thinking_retry: false,
            simulate_cc: false,
            simulate_full_system: false,
            fill_absent_tools: false,
            sim_trim_tools: false,
            sim_billing_only: false,
            sim_billing_keep_user_id: false,
            real_billing_keep_user_id: false,
            sim_message_threads: false,
            fill_metadata: false,
            rate_limit_retry: false,
            cache_scope_global: false,
            cache_ttl_1h: false,
            eager_tool_streaming: false,
            nonstream_as_sse: false,
            strip_extra_fields: false,
            tool_name_mimic: false,
            inject_thinking: false,
            reject_openai_shape: false,
            reject_session_conflict: false,
            reject_probes: false,
            reject_probes_strict: false,
            reject_refusals: false,
            reject_empty_replies: false,
            reject_learned_shapes: false,
            api_telemetry: false,
            keepalive_telemetry: false,
            fable_refusal_fallback: false,
            opus_refusal_fallback: false,
        }
    );

    // 只开回一项，其余保持关闭：开关之间不得互相影响。
    store.set_setting(MERGE_BETA, "true").unwrap();
    let f = store.forward_flags();
    assert!(f.merge_beta);
    assert!(!f.spoof_identity && !f.billing_cch && !f.fill_client_headers);
    assert!(!f.orig_header_case);

    // 无法识别的取值算「开」，不能因为写错字把形态悄悄关掉。
    store.set_setting(SPOOF_IDENTITY_ENABLED, "yes").unwrap();
    assert!(store.forward_flags().spoof_identity);
}

/// 0.3.93 把 `reject_probes` 拆成三个键：旧库只写过 `reject_probes=0` 的，升级后两条新键
/// 沿用它（用户当时关掉的是整套，不能悄悄开回来）；旧键开着的新键也开；新键一旦写了
/// 就以新键为准，与旧键互不影响。
#[test]
fn forward_flags_reject_probes_legacy_value_seeds_split_switches() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);

    // 全新库：三个都默认开。
    let f = store.forward_flags();
    assert!(f.reject_probes && f.reject_refusals && f.reject_empty_replies);

    // 旧库只关过 reject_probes：两条新键跟着关。
    store.set_setting(REJECT_PROBES, "0").unwrap();
    let f = store.forward_flags();
    assert!(!f.reject_probes);
    assert!(!f.reject_refusals, "旧键关过 = 学到的拒答规则也别拦");
    assert!(!f.reject_empty_replies, "旧键关过 = 零输出规则也别拦");

    // 新键显式开：压过旧键；另一条没写的仍跟旧键。
    store.set_setting(REJECT_REFUSALS, "1").unwrap();
    let f = store.forward_flags();
    assert!(!f.reject_probes && f.reject_refusals && !f.reject_empty_replies);

    // 旧键开、新键显式关：新键为准。
    store.set_setting(REJECT_PROBES, "1").unwrap();
    store.set_setting(REJECT_EMPTY_REPLIES, "0").unwrap();
    let f = store.forward_flags();
    assert!(f.reject_probes && f.reject_refusals && !f.reject_empty_replies);
}

/// v0.3.91 的单一 `refusal_fallback` 旧键拆成 fable / opus 两档后：旧键只沿用到 fable
/// （关过的库升级后 fable 也别补），对 opus 不算数（旧库里开着，opus 仍按默认关）；
/// 新键一旦写了就以新键为准。
#[test]
fn forward_flags_split_refusal_fallback_keys_migrate_legacy_to_fable_only() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);

    // 旧库把总开关关了：fable 跟着关，opus 本来就关。
    store.set_setting(REFUSAL_FALLBACK_LEGACY, "0").unwrap();
    let f = store.forward_flags();
    assert!(!f.fable_refusal_fallback, "旧键关过 = fable 也别补");
    assert!(!f.opus_refusal_fallback);

    // 旧库把总开关明确开着：fable 开，opus **不**跟着开——默认关正是拆分的目的。
    store.set_setting(REFUSAL_FALLBACK_LEGACY, "1").unwrap();
    let f = store.forward_flags();
    assert!(f.fable_refusal_fallback);
    assert!(!f.opus_refusal_fallback, "旧键开着也不能把 opus 那条实验开关带开");

    // 新键存在就以新键为准，旧键不再作数；两档互不影响。
    store.set_setting(FABLE_REFUSAL_FALLBACK, "0").unwrap();
    store.set_setting(OPUS_REFUSAL_FALLBACK, "1").unwrap();
    let f = store.forward_flags();
    assert!(!f.fable_refusal_fallback, "fable 新键 0 压过旧键 1");
    assert!(f.opus_refusal_fallback, "opus 显式开才开");
}

/// 旧库上的单列 idx_usage_logs_cred、(cred_id, ts) 两条都会被换成以 (cred_id, ts) 开头的
/// 覆盖索引：前缀相同，并存只是多一份写入开销。
#[test]
fn migration_replaces_cred_index_with_composite() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute("CREATE INDEX idx_usage_logs_cred ON usage_logs(cred_id)", []).unwrap();
    conn.execute("CREATE INDEX idx_usage_logs_cred_ts ON usage_logs(cred_id, ts)", []).unwrap();
    init_schema(&conn).unwrap();

    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'usage_logs'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(names.iter().any(|n| n == "idx_usage_logs_cred_usage"), "覆盖索引应建好: {names:?}");
    assert!(!names.iter().any(|n| n == "idx_usage_logs_cred"), "旧单列索引应删掉: {names:?}");
    assert!(!names.iter().any(|n| n == "idx_usage_logs_cred_ts"), "旧复合索引应删掉: {names:?}");
}

/// 「近 N 天出现过的模型」：空 / NULL 模型名不算，只被连通性测试打过的不算，窗口外的不算；
/// 最新几条是 probe 的，退回它之前那条客户端流水的时刻。按时刻倒序。
#[test]
fn recent_models_skips_probe_empty_and_stale() {
    let store = CredentialStore::open_in_memory().unwrap();
    let now = chrono::Utc::now().timestamp();
    let log = |model: Option<&str>, device: Option<&str>, ts: i64| {
        store
            .insert_usage_log_at(
                &UsageRecord {
                    model: model.map(Into::into),
                    device_id: device.map(Into::into),
                    ..Default::default()
                },
                Some(ts),
            )
            .unwrap();
    };
    log(Some("opus"), Some("d1"), now - 300);
    // 最新那几条是连通性测试打的：退回它之前那条客户端流水的时刻。
    log(Some("opus"), Some("probe"), now - 10);
    log(Some("sonnet"), None, now - 100);
    log(Some("probe-only"), Some("probe"), now - 50);
    log(Some("stale"), Some("d1"), now - 8 * 86400);
    log(Some(""), Some("d1"), now - 20);
    log(None, Some("d1"), now - 20);

    assert_eq!(
        store.recent_models(7).unwrap(),
        vec![("sonnet".to_string(), now - 100), ("opus".to_string(), now - 300)]
    );
}

/// 对**实际执行的**流水取页 SQL（[`usage_log_page_sql`]）跑 EXPLAIN QUERY PLAN，各行用
/// ` | ` 连起来。
fn usage_page_plan(store: &CredentialStore, q: &UsageLogQuery) -> String {
    let (where_sql, mut params) = q.where_clause();
    params.push(rusqlite::types::Value::Integer(q.limit));
    params.push(rusqlite::types::Value::Integer(q.offset));
    explain(store, &usage_log_page_sql(&where_sql, params.len()), params)
}

/// 同上，对流水统计 SQL（[`usage_log_stats_sql`]）。
fn usage_stats_plan(store: &CredentialStore, q: &UsageLogQuery) -> String {
    let (where_sql, params) = q.where_clause();
    explain(store, &usage_log_stats_sql(&where_sql), params)
}

fn explain(store: &CredentialStore, sql: &str, params: Vec<rusqlite::types::Value>) -> String {
    let conn = store.conn.lock();
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.query_map(rusqlite::params_from_iter(params), |r| r.get::<_, String>(3))
        .unwrap()
        .map(|r| r.unwrap())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// 账号列表的额度窗口聚合：流水那一侧要按 (cred_id, ts) 范围扫、且只走覆盖索引。日志行
/// 很宽，回表是这条 SQL 在线上的大头（40 个号、7 天 40 万条流水时近 1 秒，覆盖后 0.08 秒）。
#[test]
fn quota_snapshots_sql_uses_covering_index() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {QUOTA_SNAPSHOTS_SQL}")).unwrap();
    let plan = stmt
        .query_map(params![WINDOW_5H_SECS, WINDOW_7D_SECS, None::<i64>], |r| r.get::<_, String>(3))
        .unwrap()
        .map(|r| r.unwrap())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        plan.contains("COVERING INDEX idx_usage_logs_cred_usage (cred_id=? AND ts>?)"),
        "额度窗口要走覆盖索引的范围扫描: {plan}"
    );
}

/// 迁移是幂等的：对已是 AUTOINCREMENT 的库再次 init_schema 不改动、不报错。
#[test]
fn migration_is_idempotent_on_fresh_db() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap(); // 全新库：基表已带 AUTOINCREMENT。
    init_schema(&conn).unwrap(); // 再来一次应无副作用。
    let ddl: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'credentials'", [], |r| r.get(0))
        .unwrap();
    assert!(ddl.contains("AUTOINCREMENT"));
}

/// 0.2.81 之前入库的 socks5 代理，迁移时一次性归一化成 socks5h（理由见
/// [`crate::clients::PROXY_SCHEME_UPGRADES`]）。不改写的话那些号会一直本机解析 DNS——正是
/// 那个改动要治的故障，而网页上没有自助修复的路：代理框里的值与库里一致 → 不算改动 →
/// 保存按钮是灰的。
///
/// socks4/socks4a 那两种已经不收了（见 [`crate::clients::PROXY_SCHEMES`]），存量行原样留着：
/// 清成直连就是拿真实 IP 打上游，比这个号不可用坏得多。
#[test]
fn migration_normalizes_stored_socks_schemes() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    for (id, proxy) in [
        (1, Some("socks5://u:p@example.com:1080")),
        (2, Some("socks4://10.0.0.1:1080")),
        (3, Some("socks5h://example.com:1080")),
        (4, Some("http://127.0.0.1:8080")),
        (5, None),
    ] {
        conn.execute(
            "INSERT INTO credentials (id, access_token, refresh_token, expires_at, proxy) \
                 VALUES (?1, 'a', ?3, 0, ?2)",
            params![id, proxy, format!("r{id}")], // refresh_token 有 UNIQUE 约束
        )
        .unwrap();
    }

    init_schema(&conn).unwrap(); // 改写就发生在这一次。

    let got = |id: i64| -> Option<String> {
        conn.query_row("SELECT proxy FROM credentials WHERE id = ?1", params![id], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(got(1).as_deref(), Some("socks5h://u:p@example.com:1080"), "socks5 该归一化");
    assert_eq!(got(2).as_deref(), Some("socks4://10.0.0.1:1080"), "socks4 原样留着，不清空");
    assert_eq!(got(3).as_deref(), Some("socks5h://example.com:1080"), "已是目标形态不该动");
    assert_eq!(got(4).as_deref(), Some("http://127.0.0.1:8080"), "http 不该动");
    assert_eq!(got(5), None, "直连（NULL）不该动");

    // 幂等：再跑一遍不该在 socks5h 前面再叠一层。
    init_schema(&conn).unwrap();
    assert_eq!(got(1).as_deref(), Some("socks5h://u:p@example.com:1080"));
    assert_eq!(got(2).as_deref(), Some("socks4://10.0.0.1:1080"));
}

/// 会话绑定的历史事件：新建、接手休眠绑定的槽位（新键 `slot_taken`、前任 `evicted`）、休眠后
/// 恢复（换了槽位）、上游失败换号改绑、手动解绑、停用账号、过期清理，以及槽位历史与 7 天清理。
#[test]
fn session_binding_events_follow_state_changes() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let kinds = |key: &str| -> Vec<String> {
        store.session_events(key).unwrap().into_iter().rev().map(|e| e.event).collect()
    };

    // 新建；同键续用不再记。
    store.select_for_device(soft_session("s1")).unwrap();
    store.select_for_device(soft_session("s1")).unwrap();
    assert_eq!(kinds("s1"), ["bound"]);
    let e = &store.session_events("s1").unwrap()[0];
    assert_eq!((e.cred_id, e.slot, e.cred_label.as_deref()), (Some(a), Some(0), Some("a")));

    // s1 休眠，s2 接手槽位 0；s1 回来换到槽位 1。
    age_session_binding(&store, "s1", 600);
    store.select_for_device(soft_session("s2")).unwrap();
    assert_eq!(kinds("s2"), ["bound", "slot_taken"]);
    let taken = &store.session_events("s2").unwrap()[0];
    assert_eq!(taken.other_key.as_deref(), Some("s1"));
    assert!(taken.idle_secs.is_some_and(|s| s >= 600), "{taken:?}");
    store.select_for_device(soft_session("s1")).unwrap();
    assert_eq!(kinds("s1"), ["bound", "evicted", "resumed"]);
    let resumed = &store.session_events("s1").unwrap()[0];
    assert_eq!((resumed.prev_slot, resumed.slot), (Some(0), Some(1)));
    // 槽位 0 的历史：s1 建、s2 建并接手、s1 被接手、s1 从这里离开。
    let slot0: Vec<(String, String)> = store
        .slot_events(a, 0)
        .unwrap()
        .into_iter()
        .rev()
        .map(|e| (e.session_key, e.event))
        .collect();
    assert_eq!(
        slot0,
        [
            ("s1".into(), "bound".into()),
            ("s2".into(), "bound".into()),
            ("s2".into(), "slot_taken".into()),
            ("s1".into(), "evicted".into()),
            ("s1".into(), "resumed".into()),
        ]
    );

    // 上游失败换号：原号在本轮已试过，改绑到 b，带原号、原槽位与原因。
    let tried = [a];
    let sel = Select { exclude: &tried, ..soft_session("s2") };
    assert_eq!(store.select_for_device(sel).unwrap().id, b);
    let rebound = &store.session_events("s2").unwrap()[0];
    assert_eq!(rebound.event, "rebound");
    assert_eq!(
        (rebound.prev_cred_id, rebound.prev_slot, rebound.cred_id, rebound.reason.as_deref()),
        (Some(a), Some(0), Some(b), Some("retried"))
    );

    // 手动解绑与停用账号各记一条解绑，带方式。
    assert!(store.unbind_session(b, "s2").unwrap());
    assert_eq!(store.session_events("s2").unwrap()[0].reason.as_deref(), Some("manual"));
    store.set_disabled(a, true).unwrap();
    let e = &store.session_events("s1").unwrap()[0];
    assert_eq!((e.event.as_str(), e.reason.as_deref()), ("unbound", Some("account_disabled")));

    // 过期清理记 expired；7 天前的事件被清掉。
    store.select_for_device(soft_session("s3")).unwrap();
    age_session_binding(&store, "s3", 7200);
    store
        .conn
        .lock()
        .execute(
            "UPDATE session_binding_events SET ts = ts - 8 * 86400 WHERE session_key = 's1'",
            [],
        )
        .unwrap();
    store.prune_expired_bindings().unwrap();
    assert_eq!(kinds("s3"), ["bound", "expired"]);
    assert!(store.session_events("s1").unwrap().is_empty(), "超过 7 天的事件清掉");
}

/// 历史事件的几处边界：同一秒里槽位来回被接手（按占用轮次去重，不靠事件时间戳）、活跃中在
/// 沿用来访 ID 与派生槽位之间切换（`reslotted`）、自动封停连带清掉的绑定记解绑。
#[test]
fn session_binding_events_edge_cases() {
    let (store, ids) = soft_store(&["a", "b"]);
    let (a, b) = (ids[0], ids[1]);
    let count = |key: &str, event: &str| {
        store.session_events(key).unwrap().iter().filter(|e| e.event == event).count()
    };

    // x 占槽位 0 后休眠，y 接手；y 休眠，x 回来又拿回 0（接手 y）；x 再休眠，z 接手。全在同一秒，
    // x 两次被接手都要记上，前任也不能认错。
    store.select_for_device(soft_session("x")).unwrap();
    age_session_binding(&store, "x", 600);
    store.select_for_device(soft_session("y")).unwrap();
    age_session_binding(&store, "y", 600);
    assert_eq!(store.select_with_slot(soft_session("x")).unwrap().1, Some(0));
    age_session_binding(&store, "x", 600);
    store.select_for_device(soft_session("z")).unwrap();
    assert_eq!(count("x", "evicted"), 2, "{:?}", store.session_events("x").unwrap());
    assert_eq!(count("y", "evicted"), 1);
    let z = &store.session_events("z").unwrap()[0];
    assert_eq!((z.event.as_str(), z.other_key.as_deref()), ("slot_taken", Some("x")));

    // 活跃中从派生槽位切到沿用来访 ID，再切回来：各记一条换槽位，带原槽位（p 落在空着的 b 上，
    // 槽位 0）。
    store.select_for_device(soft_session("p")).unwrap();
    let pass = Select { passthrough_session: true, ..soft_session("p") };
    assert_eq!(store.select_with_slot(pass).unwrap().1, None);
    let e = &store.session_events("p").unwrap()[0];
    assert_eq!((e.event.as_str(), e.prev_slot, e.slot), ("reslotted", Some(0), Some(-1)));
    store.select_for_device(soft_session("p")).unwrap();
    assert_eq!(count("p", "reslotted"), 2);
    // 续用不换槽位不记。
    store.select_for_device(soft_session("p")).unwrap();
    assert_eq!(count("p", "reslotted"), 2);

    // 自动封停：连带清掉的绑定记解绑。
    let bound_to_b = Select { exclude: &[a], ..soft_session("q") };
    assert_eq!(store.select_for_device(bound_to_b).unwrap().id, b);
    assert!(store.record_ban(b, &BanContext { reason: "x".into(), ..Default::default() }).unwrap());
    let e = &store.session_events("q").unwrap()[0];
    assert_eq!((e.event.as_str(), e.reason.as_deref()), ("unbound", Some("account_banned")));
}

/// 旧库第一次补 `slot_lost` 列时按现有数据初始化槽位归属：同槽位上早已被接手的旧绑定 A 标成
/// 已丢槽位，当前持有者 B 不动。之后解绑 B、槽位分给 C，C 不能被记成「接手 A」。
#[test]
fn upgrade_initializes_slot_owners() {
    // 键要带新口径前缀，否则建表迁移会把它们当旧口径的行清掉。
    const KA: &str = "lb:v2:sid:A";
    const KB: &str = "lb:v2:sid:B";
    const KC: &str = "lb:v2:sid:C";
    const KP: &str = "lb:v2:sid:P";
    let (store, ids) = soft_store(&["a"]);
    let a = ids[0];
    {
        let conn = store.conn.lock();
        for (key, idle) in [(KA, 7200), (KB, 600), (KP, 9000)] {
            let slot = if key == KP { -1 } else { 0 };
            conn.execute(
                "INSERT INTO session_bindings (session_key, cred_id, slot, last_seen_at) \
                 VALUES (?1, ?2, ?3, unixepoch() - ?4)",
                params![key, a, slot, idle],
            )
            .unwrap();
        }
        // 模拟旧库：去掉这一列再走一遍建表迁移，补列那一步就会触发初始化。
        conn.execute("ALTER TABLE session_bindings DROP COLUMN slot_lost", []).unwrap();
        init_schema(&conn).unwrap();
        let lost = |k: &str| -> i64 {
            conn.query_row(
                "SELECT slot_lost FROM session_bindings WHERE session_key = ?1",
                [k],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!((lost(KA), lost(KB), lost(KP)), (1, 0, 0));
        // 再跑一遍（列已在）不改动。
        conn.execute("UPDATE session_bindings SET slot_lost = 0", []).unwrap();
        init_schema(&conn).unwrap();
        assert_eq!(lost(KA), 0, "列已在时不重新初始化");
        conn.execute("UPDATE session_bindings SET slot_lost = 1 WHERE session_key = ?1", [KA])
            .unwrap();
    }
    assert!(store.unbind_session(a, KB).unwrap());
    assert_eq!(store.select_with_slot(soft_session(KC)).unwrap().1, Some(0));
    let events: Vec<String> =
        store.session_events(KC).unwrap().into_iter().map(|e| e.event).collect();
    assert_eq!(events, ["bound"], "C 不该被记成接手 A");
    assert!(store.session_events(KA).unwrap().is_empty());
}

// ---------- 用量预聚合（usage_rollup） ----------

/// 桶下标按 OpenTelemetry 指数直方图的约定：下标 i 收 (b^i, b^(i+1)]，b = 2^(2^-5)。
/// 2 的整数次幂恰好落在桶的上边界上。
#[test]
fn latency_hist_follows_otel_exponential_buckets() {
    assert_eq!(LatencyHist::index(1), -1, "1 = b^0 是 (b^-1, b^0] 的上边界");
    assert_eq!(LatencyHist::index(2), 31);
    assert_eq!(LatencyHist::index(1024), 10 * 32 - 1);
    assert_eq!(LatencyHist::index(3), 50, "log2(3)·32 = 50.7 → ceil 51 → 50");
    assert_eq!(LatencyHist::index(0), -1, "0 ms 按 1 ms 记");
    let mut h = LatencyHist::default();
    for v in [1, 3, 3, 250, 1024, 60_000] {
        h.record(v);
    }
    let bytes = h.encode();
    assert_eq!(LatencyHist::decode(&bytes), Some(h.clone()), "编码往返");
    assert_eq!(LatencyHist::decode(&bytes[..bytes.len() - 1]), None, "截断的数据不认");
    assert_eq!(LatencyHist::decode(&[]), None);
    assert_eq!(LatencyHist::default().percentile(0.5), 0, "空直方图");
}

/// 一个确定性的伪随机序列（xorshift），测试数据可复现。
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// 分位数估计与 nearest-rank 精确值的相对误差不超过 (b-1)/(b+1) ≈ 1.08%（再加 1 ms 取整）。
#[test]
fn latency_hist_percentiles_stay_within_relative_error() {
    let mut seed = 0x9e37_79b9_7f4a_7c15;
    let mut values: Vec<i64> = (0..20_000)
        .map(|_| {
            // 对数均匀地铺在 50 ms ～ 120 s：TTFT 实际落的范围。
            let u = (xorshift(&mut seed) % 1_000_000) as f64 / 1_000_000.0;
            (50.0 * (120_000.0f64 / 50.0).powf(u)).round() as i64
        })
        .collect();
    let mut h = LatencyHist::default();
    for &v in &values {
        h.record(v);
    }
    values.sort_unstable();
    for p in [0.5, 0.95, 0.99] {
        let rank = ((values.len() as f64 * p).ceil() as usize).clamp(1, values.len());
        let exact = values[rank - 1];
        let est = h.percentile(p);
        let rel = (est - exact).abs() as f64 / exact as f64;
        assert!(
            rel <= 0.0109 || (est - exact).abs() <= 1,
            "p{p}: 估计 {est}，精确 {exact}，相对误差 {rel}"
        );
    }
}

/// 随机写一批流水，汇总读出来的拆分表与趋势合计要与从原始行精确算出来的一致：可累加的各项
/// 逐一相等，分位数在 1.1% 以内。
#[test]
fn rollup_matches_aggregates_computed_from_raw_logs() {
    let (store, ids) = store_with(&["a", "b", "c"]);
    let now: i64 = store.conn.lock().query_row("SELECT unixepoch()", [], |r| r.get(0)).unwrap();
    // 窗口起点对齐到 15 分钟，免得边缘那一桶的取舍影响比对。
    let since = first_bucket(now - 6 * 3600);
    let models = ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"];
    let mut seed = 42u64;
    let mut raw = Vec::new();
    for _ in 0..3000 {
        let r = xorshift(&mut seed);
        let cred = ids[(r % 3) as usize];
        let model = models[(r / 3 % 3) as usize];
        let status: u16 = if r.is_multiple_of(10) { 529 } else { 200 };
        let ttft = 100 + (xorshift(&mut seed) % 20_000) as i64;
        let rec = UsageRecord {
            cred_id: Some(cred),
            cred_label: format!("label-{cred}"),
            model: Some(model.into()),
            status,
            has_usage: true,
            input_tokens: Some((r % 5000) as i64),
            output_tokens: Some((r % 3000) as i64),
            cache_creation_tokens: (!r.is_multiple_of(7)).then_some((r % 900) as i64),
            cache_5m_tokens: Some((r % 400) as i64),
            cache_1h_tokens: Some((r % 300) as i64),
            cache_read_tokens: Some((r % 20_000) as i64),
            ttft_ms: (!r.is_multiple_of(13)).then_some(ttft),
            total_ms: Some(ttft + (r % 30_000) as i64),
            ..Default::default()
        };
        let ts = since + (xorshift(&mut seed) % (now - since).max(1) as u64) as i64;
        store.insert_usage_log_at(&rec, Some(ts)).unwrap();
        raw.push(rec);
    }

    // 从原始行精确算：按模型分组的请求数、token、省下的钱、延迟平均 / 吞吐 / 分位。
    struct Exact {
        requests: i64,
        input: i64,
        cached: i64,
        written: i64,
        saved: f64,
        ttft: Vec<i64>,
        gen_tokens: i64,
        gen_ms: i64,
    }
    let mut exact: HashMap<String, Exact> = HashMap::new();
    for rec in &raw {
        let e = exact.entry(rec.model.clone().unwrap()).or_insert(Exact {
            requests: 0,
            input: 0,
            cached: 0,
            written: 0,
            saved: 0.0,
            ttft: vec![],
            gen_tokens: 0,
            gen_ms: 0,
        });
        let plain = rec.input_tokens.unwrap_or(0);
        let cached = rec.cache_read_tokens.unwrap_or(0);
        let written = rec
            .cache_creation_tokens
            .unwrap_or(rec.cache_5m_tokens.unwrap_or(0) + rec.cache_1h_tokens.unwrap_or(0));
        e.requests += 1;
        e.input += plain + written + cached;
        e.cached += cached;
        e.written += written;
        e.saved += cache_saved_usd(
            rec.model.as_deref(),
            plain,
            cached,
            rec.cache_creation_tokens,
            rec.cache_5m_tokens,
            rec.cache_1h_tokens,
        );
        if rec.status == 200
            && let Some(t) = rec.ttft_ms
        {
            e.ttft.push(t);
            if let (Some(total), Some(out)) = (rec.total_ms, rec.output_tokens)
                && total > t
                && out > 0
            {
                e.gen_tokens += out;
                e.gen_ms += total - t;
            }
        }
    }
    let nearest = |sorted: &[i64], p: f64| {
        sorted[((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1]
    };
    let rows = store.usage_breakdown(since, BreakdownBy::Model, 10).unwrap();
    assert_eq!(rows.len(), 3);
    for row in &rows {
        let e = exact.get_mut(&row.key).unwrap();
        e.ttft.sort_unstable();
        assert_eq!(row.requests, e.requests);
        assert_eq!(
            (row.cache.input_tokens, row.cache.cached_tokens, row.cache.written_tokens),
            (e.input, e.cached, e.written)
        );
        assert!(
            (row.cache_saved_usd - e.saved).abs() < 1e-9,
            "{} vs {}",
            row.cache_saved_usd,
            e.saved
        );
        assert_eq!(row.latency.count, e.ttft.len() as i64);
        assert_eq!(row.latency.avg_ms, e.ttft.iter().sum::<i64>() / e.ttft.len() as i64);
        let tps = e.gen_tokens as f64 * 1000.0 / e.gen_ms as f64;
        assert!((row.latency.tokens_per_sec.unwrap() - tps).abs() < 1e-9);
        assert_close(row.latency.p50_ms, nearest(&e.ttft, 0.5));
        assert_close(row.latency.p95_ms, nearest(&e.ttft, 0.95));
    }
    // 按账号拆与整窗口合计：请求数加起来等于总数，缓存合计与按模型的一致。
    let by_cred = store.usage_breakdown(since, BreakdownBy::Account, 10).unwrap();
    assert_eq!(by_cred.iter().map(|r| r.requests).sum::<i64>(), 3000);
    let mut labels: Vec<&str> = by_cred.iter().map(|r| r.label.as_str()).collect();
    labels.sort_unstable();
    assert_eq!(labels, ["a", "b", "c"], "号还在，名字取账号表的");
    let cache = store.cache_report(since, 3600, 0).unwrap();
    assert_eq!(cache.summary.input_tokens, exact.values().map(|e| e.input).sum::<i64>());
    let ttft = store.ttft_report(since, 3600, 0).unwrap();
    assert_eq!(ttft.summary.count, exact.values().map(|e| e.ttft.len() as i64).sum::<i64>());
    assert_eq!(
        ttft.points.iter().map(|b| b.count).sum::<i64>(),
        ttft.summary.count,
        "各桶之和等于合计"
    );
}

/// 汇总表一行里比对用的几列：维度、桶、键、名字、请求数、输入 token、直方图。
type RollupRowSnapshot = (String, i64, String, String, i64, i64, Option<Vec<u8>>);

/// 老库升级时的回填与写入时逐条累加，得到的汇总逐格相同。
#[test]
fn rollup_backfill_equals_live_accumulation() {
    let (store, ids) = store_with(&["a", "b"]);
    let now: i64 = store.conn.lock().query_row("SELECT unixepoch()", [], |r| r.get(0)).unwrap();
    let mut seed = 7u64;
    for i in 0..500 {
        let r = xorshift(&mut seed);
        let rec = UsageRecord {
            cred_id: if i % 50 == 0 { None } else { Some(ids[(r % 2) as usize]) },
            cred_label: if r.is_multiple_of(2) { "a".into() } else { "b".into() },
            model: (i % 40 != 0).then(|| "claude-sonnet-5".into()),
            status: if r.is_multiple_of(9) { 400 } else { 200 },
            input_tokens: Some((r % 900) as i64),
            cache_read_tokens: Some((r % 5000) as i64),
            ttft_ms: Some(50 + (r % 9000) as i64),
            total_ms: Some(10_000),
            output_tokens: Some((r % 700) as i64),
            ..Default::default()
        };
        store.insert_usage_log_at(&rec, Some(now - (r % 30_000) as i64)).unwrap();
    }
    let snapshot = |store: &CredentialStore| -> Vec<RollupRowSnapshot> {
        let conn = store.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT dim, bucket, key, label, requests, input_tokens, ttft_hist
                   FROM usage_rollup ORDER BY dim, bucket, key",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    };
    let live = snapshot(&store);
    assert!(!live.is_empty());
    store.conn.lock().execute("DELETE FROM usage_rollup", []).unwrap();
    backfill_rollup(&store.conn.lock()).unwrap();
    assert_eq!(snapshot(&store), live, "回填与逐条累加一致");
    // 汇总不空时不再回填（不会重复计）。
    backfill_rollup(&store.conn.lock()).unwrap();
    assert_eq!(snapshot(&store), live);
}

/// 汇总保留 90 天，随流水裁剪一起裁；清空库时一起清掉。
#[test]
fn rollup_is_pruned_after_ninety_days_and_cleared_with_the_store() {
    let (store, ids) = store_with(&["a"]);
    let now: i64 = store.conn.lock().query_row("SELECT unixepoch()", [], |r| r.get(0)).unwrap();
    let rec = UsageRecord { cred_id: Some(ids[0]), status: 200, ..Default::default() };
    store.insert_usage_log_at(&rec, Some(now - 91 * 86400)).unwrap();
    store.insert_usage_log_at(&rec, Some(now - 89 * 86400)).unwrap();
    store.insert_usage_log_at(&rec, Some(now - 100)).unwrap();
    let count = |store: &CredentialStore| -> i64 {
        store
            .conn
            .lock()
            .query_row("SELECT COUNT(*) FROM usage_rollup WHERE dim = 'all'", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count(&store), 3);
    store.prune_usage_logs().unwrap();
    assert_eq!(count(&store), 2, "超过 90 天的那一桶裁掉，89 天前的还在（流水只留 8 天）");
    store.clear().unwrap();
    assert_eq!(count(&store), 0);
}

/// 两个进程同时首次升级：A 已在回填（拿着写锁、尚未提交），B 再来回填必须等 A 提交，然后看到
/// 汇总已不空、跳过——不能各自回填一遍把历史累加两次。用两条连接打开同一个文件库确定性复现。
#[test]
fn concurrent_backfills_do_not_double_count() {
    let path = std::env::temp_dir().join(format!(
        "luban-rollup-backfill-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let open = || {
        let c = Connection::open(&path).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c
    };
    let a = open();
    init_schema(&a).unwrap();
    // 两条流水直接写进表（绕过写入时累加），模拟升级前的老库：有流水、没有汇总。
    a.execute_batch(
        "INSERT INTO usage_logs (ts, status) VALUES (1000, 200), (2000, 200);
         DELETE FROM usage_rollup;",
    )
    .unwrap();
    // A 正在回填：拿着写锁，已写下一格、还没提交。
    a.execute_batch(
        "BEGIN IMMEDIATE;
         INSERT INTO usage_rollup (dim, bucket, key, requests) VALUES ('all', 900, '', 2);",
    )
    .unwrap();
    let b = open();
    let backfill = std::thread::spawn(move || backfill_rollup(&b));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!backfill.is_finished(), "B 必须等 A 的写锁，不能先读到「空」");
    a.execute_batch("COMMIT").unwrap();
    backfill.join().unwrap().expect("等到 A 提交后正常返回");
    let total: i64 = a
        .query_row("SELECT SUM(requests) FROM usage_rollup WHERE dim = 'all'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 2, "只有 A 那一份，B 看到不空就跳过了");
    drop(a);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// 趋势接口的网格：桶宽向上取整到 15 分钟的整数倍、偏移对齐到 15 分钟、窗口起点向上对齐。
#[test]
fn series_grid_reports_the_granularity_actually_used() {
    assert_eq!(series_grid(0, 60, 0), (0, 900, 0), "比汇总桶细的桶宽拼不出来，取一个汇总桶");
    assert_eq!(series_grid(0, 1000, 0).1, 1800, "不是整倍数的向上取整");
    assert_eq!(series_grid(0, 3600, 0).1, 3600);
    assert_eq!(series_grid(0, 86400, 0).1, 86400);
    assert_eq!(series_grid(0, 86400, 5 * 3600 + 1800).2, 19800, "UTC+5:30 本身就对齐");
    assert_eq!(series_grid(0, 86400, 1000).2, 900, "不对齐的偏移取最近的 15 分钟");
    assert_eq!(series_grid(901, 3600, 0).0, 1800, "窗口起点向上对齐");
    assert_eq!(series_grid(900, 3600, 0).0, 900);
}

/// 老库升级：settings 里的管理 / 访客密码搬进 users（带旧哈希前缀），settings 里那几个键删掉；
/// 存量号与出口代理挂到 admin 名下；再跑一遍迁移什么都不变。
#[test]
fn legacy_console_passwords_and_owners_migrate_once() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
         CREATE TABLE proxies (
             id INTEGER PRIMARY KEY AUTOINCREMENT, label TEXT NOT NULL DEFAULT '',
             url TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch())) STRICT;
         CREATE UNIQUE INDEX uq_proxies_url ON proxies(url);
         INSERT INTO proxies (label, url) VALUES ('p', 'http://h:1');",
    )
    .unwrap();
    for (k, v) in
        [(ADMIN_PASSWORD, "aaaa"), (VIEWER_PASSWORD, "vvvv"), (ADMIN_PASSWORD_CANONICAL, "cccc")]
    {
        conn.execute("INSERT INTO settings (key, value) VALUES (?1, ?2)", [k, v]).unwrap();
    }
    init_schema(&conn).unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let admin = store.admin_user().unwrap();
    assert_eq!(store.user_password_hash(admin.id).unwrap().unwrap(), "sha256:aaaa");
    let viewer = store.viewer_user().unwrap().unwrap();
    assert_eq!(store.user_password_hash(viewer.id).unwrap().unwrap(), "sha256:vvvv");
    for k in CONSOLE_AUTH_KEYS {
        assert_eq!(store.get_setting(k).unwrap(), None, "{k} 应已删掉");
    }
    assert_eq!(
        store.proxy_owner(store.list_proxies(Scope::All).unwrap()[0].id).unwrap(),
        Some(admin.id)
    );
    // 唯一约束改成「人 + 地址」：别人可以存同一个地址，同一个人不行。
    let other = store.create_user("u1", "", UserRole::User, admin.id).unwrap().unwrap();
    store.add_proxy(other.id, "mine", "http://h:1").unwrap();
    assert!(store.add_proxy(admin.id, "dup", "http://h:1").is_err());
}

/// 删除账号：代理名下还有用户、或名下还有号时拒绝；admin / 访客不能删；删掉时连带出口代理。
#[test]
fn delete_user_refuses_while_it_still_owns_things() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let agent = store.create_user("agent", "", UserRole::Agent, admin).unwrap().unwrap().id;
    let user = store.create_user("user", "", UserRole::User, agent).unwrap().unwrap().id;
    assert!(
        store.create_user("AGENT", "", UserRole::User, admin).unwrap().is_none(),
        "用户名不区分大小写"
    );
    let cred = store.insert("c", None, "t", "r", 0, None, None, user).unwrap().id;
    store.add_proxy(user, "p", "http://h:1").unwrap();

    assert_eq!(store.delete_user(admin, None).unwrap(), Err(DeleteUserError::Protected));
    assert_eq!(store.delete_user(agent, None).unwrap(), Err(DeleteUserError::HasChildren(1)));
    assert_eq!(store.delete_user(user, None).unwrap(), Err(DeleteUserError::HasCredentials(1)));
    store.remove(&[cred]).unwrap();
    assert_eq!(store.delete_user(user, None).unwrap(), Ok(()));
    assert!(store.list_proxies(Scope::Owner(user)).unwrap().is_empty());
    assert_eq!(store.delete_user(agent, None).unwrap(), Ok(()));
    assert_eq!(store.delete_user(agent, None).unwrap(), Err(DeleteUserError::NotFound));
}

/// 按范围列号：代理和用户只看到自己名下的，admin 看全部。
#[test]
fn credentials_list_by_owner_scope() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let user = store.create_user("user", "", UserRole::User, admin).unwrap().unwrap().id;
    let a = store.insert("a", None, "ta", "ra", 0, None, None, admin).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, user).unwrap().id;
    let ids = |s: Scope| store.list_scoped(s).unwrap().iter().map(|c| c.id).collect::<Vec<_>>();
    assert_eq!(ids(Scope::All), vec![a, b]);
    assert_eq!(ids(Scope::Owner(user)), vec![b]);
    assert!(store.credentials_owned_by(&[b], user).unwrap());
    assert!(!store.credentials_owned_by(&[a, b], user).unwrap());
}

/// 管理关系在写语句里核对：用户被转走之后，原代理改不了它的密码、停不了它、删不了它。
#[test]
fn managed_writes_recheck_the_parent_at_write_time() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let a1 = store.create_user("a1", "", UserRole::Agent, admin).unwrap().unwrap().id;
    let a2 = store.create_user("a2", "", UserRole::Agent, admin).unwrap().unwrap().id;
    let u = store.create_user("u", "", UserRole::User, a1).unwrap().unwrap().id;
    assert!(store.reset_managed_user_password(u, "h1", Some(a1)).unwrap());
    store.set_user_parent(u, a2).unwrap();
    assert!(!store.reset_managed_user_password(u, "h2", Some(a1)).unwrap());
    assert!(!store.set_user_disabled(u, true, Some(a1)).unwrap());
    assert_eq!(store.delete_user(u, Some(a1)).unwrap(), Err(DeleteUserError::NotFound));
    assert_eq!(store.user_password_hash(u).unwrap().as_deref(), Some("h1"));
    // 代理管不了别的代理；admin 都管得了。
    assert!(!store.set_user_disabled(a2, true, Some(a1)).unwrap());
    assert!(store.reset_managed_user_password(u, "h3", Some(a2)).unwrap());
    assert!(store.set_user_disabled(a2, true, None).unwrap());
}

/// 按号主筛流水：只出本人名下号的记录。
#[test]
fn usage_logs_filter_by_owner() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let user = store.create_user("u", "", UserRole::User, admin).unwrap().unwrap().id;
    let a = store.insert("a", None, "ta", "ra", 0, None, None, admin).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, user).unwrap().id;
    store
        .conn
        .lock()
        .execute_batch(&format!(
            "INSERT INTO usage_logs (cred_id, path) VALUES ({a}, '/v1/messages'), ({b}, '/v1/messages');"
        ))
        .unwrap();
    let q = UsageLogQuery { limit: 10, owner_id: Some(user), ..Default::default() };
    let logs = store.query_usage_logs(q.clone()).unwrap();
    assert_eq!(logs.iter().map(|l| l.cred_id).collect::<Vec<_>>(), vec![Some(b)]);
    assert_eq!(store.usage_log_stats(q).unwrap().total, 1);
}

/// 改自己的密码：会话被撤了、或哈希已被别人改掉，在途的改密都写不进去。
#[test]
fn own_password_change_requires_a_live_session_and_the_old_hash() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let u = store.create_user("u", "h0", UserRole::User, admin).unwrap().unwrap().id;
    store.create_session("tok", u, "tag0", None).unwrap();
    // 管理员先重置（撤会话、换哈希），在途请求按旧哈希写入失败。
    assert!(store.reset_managed_user_password(u, "h-admin", None).unwrap());
    assert!(!store.change_own_password(u, "tok", "h0", "h-user", "t").unwrap());
    assert_eq!(store.user_password_hash(u).unwrap().as_deref(), Some("h-admin"));
    // 会话还在、哈希也对得上才写，写完其余会话作废、当前会话换指纹。
    store.create_session("tok2", u, "x", None).unwrap();
    store.create_session("tok3", u, "x", None).unwrap();
    assert!(store.change_own_password(u, "tok2", "h-admin", "h-new", "tag-new").unwrap());
    assert!(store.session_lookup("tok3").unwrap().is_none());
    assert_eq!(store.session_lookup("tok2").unwrap().unwrap().pw_tag, "tag-new");
}

/// 建号与转移在存储层核对上级：上级不存在、或角色不对都拒。
#[test]
fn create_and_move_recheck_the_parent() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let agent = store.create_user("agent", "", UserRole::Agent, admin).unwrap().unwrap().id;
    let user = store.create_user("u", "", UserRole::User, agent).unwrap().unwrap().id;
    let invalid = |r: Result<Option<User>>| {
        r.err().is_some_and(|e| e.downcast_ref::<InvalidParent>().is_some())
    };
    assert!(
        invalid(store.create_user("a2", "", UserRole::Agent, agent)),
        "代理只能挂在 admin 名下"
    );
    assert!(invalid(store.create_user("u2", "", UserRole::User, user)), "用户下面不能再挂用户");
    store.set_user_parent(user, admin).unwrap();
    assert_eq!(store.delete_user(agent, None).unwrap(), Ok(()));
    assert!(invalid(store.create_user("u3", "", UserRole::User, agent)), "上级已删");
    assert!(store.set_user_parent(user, agent).is_err());
}

/// 号主被删之后才落库的上号请求插不进来；号主不存在的号（直接写库造出来的）不进调度；
/// 不带 owner 插入的号默认挂到 admin 名下、照常调度。
#[test]
fn credentials_need_a_living_owner_to_be_inserted_and_scheduled() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let gone = store.create_user("gone", "", UserRole::User, admin).unwrap().unwrap().id;
    store.delete_user(gone, None).unwrap().unwrap();
    let err = store.insert("x", None, "t", "r-gone", u64::MAX, None, None, gone).unwrap_err();
    assert!(err.downcast_ref::<OwnerGone>().is_some());

    store
        .conn
        .lock()
        .execute_batch(&format!(
            "INSERT INTO credentials (access_token, refresh_token, expires_at, owner_id) \
             VALUES ('t1', 'r-orphan', 9999999999, {gone});
             INSERT INTO credentials (access_token, refresh_token, expires_at) \
             VALUES ('t2', 'r-default', 9999999999);"
        ))
        .unwrap();
    let default_owned =
        store.list().unwrap().into_iter().find(|c| c.refresh_token == "r-default").unwrap();
    assert_eq!(default_owned.owner_id, Some(admin));
    let picked = store.select_for_device(Select::default()).unwrap();
    assert_eq!(picked.id, default_owned.id, "无主的号不进调度");
}

/// 加解密往返；没有前缀的按明文原样读出；每次密文都不同。
#[test]
fn sealed_values_round_trip_and_plaintext_passes_through() {
    let a = secret::seal("sk-ant-ort01-abc");
    let b = secret::seal("sk-ant-ort01-abc");
    assert!(a.starts_with(secret::SEALED_PREFIX) && a != b, "随机 nonce，两次密文不同");
    assert_eq!(secret::open(&a).unwrap(), "sk-ant-ort01-abc");
    assert_eq!(secret::open("plain-token").unwrap(), "plain-token");
    assert!(secret::open("enc1:not-base64!").is_err());
}

/// 启动迁移：存量明文 token 加密落库、补指纹，读出来照旧是明文；查重靠指纹。
#[test]
fn plaintext_tokens_are_encrypted_at_startup() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO credentials (access_token, refresh_token, expires_at) VALUES ('at-1', 'rt-1', 0)",
        [],
    )
    .unwrap();
    init_schema(&conn).unwrap();
    let (at, rt, hash): (String, String, String) = conn
        .query_row(
            "SELECT access_token, refresh_token, refresh_token_hash FROM credentials",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(at.starts_with("enc1:") && rt.starts_with("enc1:"), "库里只剩密文");
    assert_eq!(hash, secret::token_fingerprint("rt-1"));
    let store = CredentialStore::with_conn(conn);
    let cred = store.list().unwrap().remove(0);
    assert_eq!((cred.access_token.as_str(), cred.refresh_token.as_str()), ("at-1", "rt-1"));
    // 同一个 refresh_token 再插一次撞指纹的唯一约束。
    let admin = store.admin_user().unwrap().id;
    assert!(store.insert("dup", None, "at-2", "rt-1", 0, None, None, admin).is_err());
    // 刷新回写同样加密、同步指纹。
    store.update_tokens(cred.id, "at-3", "rt-3", 0).unwrap();
    assert_eq!(store.get(cred.id).unwrap().unwrap().refresh_token, "rt-3");
    let hash: String = store
        .conn
        .lock()
        .query_row("SELECT refresh_token_hash FROM credentials", [], |r| r.get(0))
        .unwrap();
    assert_eq!(hash, secret::token_fingerprint("rt-3"));
}

/// 解不开的密文（密钥不对）：启动迁移直接报错，不带着错密钥跑起来。
#[test]
fn undecryptable_tokens_refuse_to_start() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO credentials (access_token, refresh_token, expires_at) \
         VALUES ('enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA', 'enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA', 0)",
        [],
    )
    .unwrap();
    let err = init_schema(&conn).unwrap_err();
    assert!(format!("{err:#}").contains("secret key does not match"), "{err:#}");
}

/// 老库升级：建默认分组、存量号进默认分组；旧的全局接入 Key 迁成一把不绑分组的 Key。
#[test]
fn groups_and_keys_migrate_from_a_legacy_database() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
         INSERT INTO settings VALUES ('client_api_key', 'legacy-key');",
    )
    .unwrap();
    init_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO credentials (access_token, refresh_token, expires_at) VALUES ('a', 'r', 0)",
        [],
    )
    .unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let default = store.default_group_id().unwrap();
    let cred = store.list().unwrap()[0].id;
    assert_eq!(store.credential_groups(cred).unwrap(), vec![default]);
    assert_eq!(store.get_setting(CLIENT_API_KEY).unwrap(), None);
    let access = store.api_key_access("legacy-key").unwrap().unwrap();
    assert!(access.groups.is_none());
    assert!(store.api_key_access("wrong").unwrap().is_none());
}

/// 分组可见范围：默认分组人人可见；开放给代理的，代理和它名下的用户都能用；admin 直属用户
/// 只看开放给自己的；用户不能单独被开放（只能开放给代理或 admin 直属用户）。
#[test]
fn group_visibility_follows_grants_and_inheritance() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let default = store.default_group_id().unwrap();
    let agent = store.create_user("agent", "", UserRole::Agent, admin).unwrap().unwrap().id;
    let sub = store.create_user("sub", "", UserRole::User, agent).unwrap().unwrap().id;
    let direct = store.create_user("direct", "", UserRole::User, admin).unwrap().unwrap().id;
    let g1 = store.create_group("g1", "").unwrap().unwrap();
    let g2 = store.create_group("g2", "").unwrap().unwrap();
    assert_eq!(store.create_group("G1", "").unwrap(), Err(GroupError::NameTaken));
    store.set_group_grants(g1, &[agent]).unwrap().unwrap();
    store.set_group_grants(g2, &[direct]).unwrap().unwrap();
    assert_eq!(store.set_group_grants(g2, &[sub]).unwrap(), Err(GroupError::InvalidGrantee));
    assert_eq!(store.set_group_grants(default, &[agent]).unwrap(), Err(GroupError::DefaultGroup));
    let vis = |id: i64| {
        let u = store.user_by_id(id).unwrap().unwrap();
        let mut v: Vec<i64> = store.visible_group_ids(&u).unwrap().into_iter().collect();
        v.sort();
        v
    };
    assert_eq!(vis(agent), vec![default, g1]);
    assert_eq!(vis(sub), vec![default, g1], "代理名下的用户继承代理的分组");
    assert_eq!(vis(direct), vec![default, g2]);
}

/// 删分组：默认分组删不了；只剩这一个分组的号挪进默认分组，还有别的分组的不动；Key 上的
/// 绑定一并清掉。
#[test]
fn deleting_a_group_rehomes_its_only_members() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let default = store.default_group_id().unwrap();
    let g1 = store.create_group("g1", "").unwrap().unwrap();
    let g2 = store.create_group("g2", "").unwrap().unwrap();
    let a = store.insert("a", None, "ta", "ra", 0, None, None, admin).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", 0, None, None, admin).unwrap().id;
    store.set_credential_groups(&[a], &[g1]).unwrap().unwrap();
    store.set_credential_groups(&[b], &[g1, g2]).unwrap().unwrap();
    assert_eq!(store.set_credential_groups(&[a], &[]).unwrap(), Err(GroupError::Empty));
    assert_eq!(store.set_credential_groups(&[a], &[9999]).unwrap(), Err(GroupError::UnknownGroup));
    let key = store.create_api_key("k", "key-1", &[g1, g2]).unwrap().unwrap();
    assert_eq!(store.delete_group(default).unwrap(), Err(GroupError::DefaultGroup));
    store.delete_group(g1).unwrap().unwrap();
    assert_eq!(store.credential_groups(a).unwrap(), vec![default]);
    assert_eq!(store.credential_groups(b).unwrap(), vec![g2]);
    assert_eq!(store.api_key_access("key-1").unwrap().unwrap().groups, Some(vec![g2]));
    assert_eq!(store.list_api_keys().unwrap()[0].id, key);
}

/// 选号按 Key 绑定的分组：只在这些分组里选，排在前面的分组优先，前面的用不了才溢出；
/// 粘住的号不在这些分组里时改选；分组里一个号都没有时报错。
#[test]
fn selection_honours_key_groups_and_their_order() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let g1 = store.create_group("g1", "").unwrap().unwrap();
    let g2 = store.create_group("g2", "").unwrap().unwrap();
    let empty = store.create_group("empty", "").unwrap().unwrap();
    let far = u64::MAX;
    let a = store.insert("a", None, "ta", "ra", far, None, None, admin).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", far, None, None, admin).unwrap().id;
    store.set_credential_groups(&[a], &[g1]).unwrap().unwrap();
    store.set_credential_groups(&[b], &[g2]).unwrap().unwrap();
    let pick = |groups: &[i64], device: Option<&str>| {
        store
            .select_for_device(Select {
                groups: Some(groups),
                device_id: device,
                ..Default::default()
            })
            .map(|c| c.id)
    };
    assert_eq!(pick(&[g2, g1], None).unwrap(), b, "排在前面的分组优先");
    assert_eq!(pick(&[g1, g2], None).unwrap(), a);
    assert_eq!(pick(&[g1], Some("dev-1")).unwrap(), a);
    // 同一台设备换一把只绑 g2 的 Key 来：粘住的 a 不在范围里，改选到 b。
    assert_eq!(pick(&[g2], Some("dev-1")).unwrap(), b);
    store.set_disabled(b, true).unwrap();
    assert_eq!(pick(&[g2, g1], None).unwrap(), a, "前面分组的号用不了就溢出到后面的");
    assert!(pick(&[empty], None).is_err());
    assert!(pick(&[], None).is_err(), "绑定的分组被删光：一个号都不能用，不放开成全部号");
    let all = store.select_for_device(Select { groups: None, ..Default::default() }).unwrap();
    assert_eq!(all.id, a, "不限分组（None）用全部号");
}

/// 停用的 Key 认不出来；库里有 Key 时 `has_api_keys` 为真（停用的也算，不会因此变成放行）。
#[test]
fn disabled_api_keys_are_rejected() {
    let store = CredentialStore::open_in_memory().unwrap();
    assert!(!store.api_keys_required().unwrap());
    let id = store.create_api_key("k", "key-x", &[]).unwrap().unwrap();
    assert!(store.api_key_access("key-x").unwrap().is_some());
    store.update_api_key(id, "k", true, &[], None).unwrap().unwrap();
    assert!(store.api_key_access("key-x").unwrap().is_none());
    assert!(store.api_keys_required().unwrap());
    assert_eq!(store.reveal_api_key(id).unwrap().as_deref(), Some("key-x"));
    let sealed: String = store
        .conn
        .lock()
        .query_row("SELECT key_sealed FROM api_keys WHERE id = ?1", [id], |r| r.get(0))
        .unwrap();
    assert!(sealed.starts_with("enc1:"), "库里存的是密文");
}

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

/// 费用汇总：号主在写入那一刻定死（号后来换了主人，历史不跟着走）；分组按 Key 的顺序取第一个
/// 含这个号的，没绑分组的 Key 取号所在 id 最小的分组；Key 为空记 0。
#[test]
fn billing_attribution_is_fixed_at_write_time() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let u = store.create_user("u", "", UserRole::User, admin).unwrap().unwrap().id;
    let default = store.default_group_id().unwrap();
    let g1 = store.create_group("g1", "").unwrap().unwrap();
    let g2 = store.create_group("g2", "").unwrap().unwrap();
    let cred = store.insert("a", None, "t", "r", 0, None, None, u).unwrap().id;
    store.set_credential_groups(&[cred], &[g1, g2]).unwrap().unwrap();
    let key = store.create_api_key("k", "key-b", &[g2, g1]).unwrap().unwrap();
    let t0 = 1_800_000_000;
    store.insert_usage_log_at(&billing_rec(cred, Some(key), "m1", 1.5), Some(t0)).unwrap();
    store.insert_usage_log_at(&billing_rec(cred, None, "m2", 0.5), Some(t0 + 10)).unwrap();
    // 号转给 admin 之后的费用记到 admin 名下，之前的仍归 u。
    store
        .conn
        .lock()
        .execute("UPDATE credentials SET owner_id = ?1 WHERE id = ?2", [admin, cred])
        .unwrap();
    store.insert_usage_log_at(&billing_rec(cred, None, "m2", 2.0), Some(t0 + 20)).unwrap();

    let all = BillingFilter { since: t0 - 3600, until: t0 + 3600, ..Default::default() };
    let by = |dim: BillingDim, f: &BillingFilter| -> Vec<(String, f64, i64)> {
        store
            .billing_breakdown(f, dim)
            .unwrap()
            .into_iter()
            .map(|r| (r.key, r.cost_usd, r.requests))
            .collect()
    };
    assert_eq!(
        by(BillingDim::Owner, &all),
        vec![(admin.to_string(), 2.0, 1), (u.to_string(), 2.0, 2)]
    );
    // 经 Key 来的那条算进 Key 顺序里第一个含这个号的 g2；没 Key 的取 id 最小的 g1。
    let groups = by(BillingDim::Group, &all);
    assert_eq!(groups, vec![(g1.to_string(), 2.5, 2), (g2.to_string(), 1.5, 1)]);
    assert!(!groups.iter().any(|(k, _, _)| *k == default.to_string()));
    assert_eq!(by(BillingDim::Key, &all), vec![("0".into(), 2.5, 2), (key.to_string(), 1.5, 1)]);
    let only_u = BillingFilter { owners: Some(vec![u]), ..all.clone() };
    assert_eq!(by(BillingDim::Model, &only_u), vec![("m1".into(), 1.5, 1), ("m2".into(), 0.5, 1)]);
    let row = &store.billing_breakdown(&only_u, BillingDim::Cred).unwrap()[0];
    assert_eq!(
        (row.input_tokens, row.output_tokens, row.cache_write_tokens, row.cache_read_tokens),
        (20, 10, 6, 4)
    );
}

/// 按日拆：以给定时区的本地零点切日界。
#[test]
fn billing_days_follow_the_timezone() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let cred = store.insert("a", None, "t", "r", 0, None, None, admin).unwrap().id;
    // UTC 2027-01-14 23:30 = 东八区 2027-01-15 07:30。
    let ts = 1_800_005_400 - (1_800_005_400 % 86400) + 23 * 3600 + 1800;
    store.insert_usage_log_at(&billing_rec(cred, None, "m", 1.0), Some(ts)).unwrap();
    let f = |tz| BillingFilter {
        since: ts - 86400,
        until: ts + 86400,
        tz_offset_secs: tz,
        ..Default::default()
    };
    let day = |tz| {
        store.billing_breakdown(&f(tz), BillingDim::Day).unwrap()[0].key.parse::<i64>().unwrap()
    };
    let utc_day = ts - ts % 86400;
    assert_eq!(day(0), utc_day);
    assert_eq!(day(8 * 3600), utc_day + 86400 - 8 * 3600, "东八区已是第二天");
}

/// 升级时把存量流水回填进费用汇总一次，之后不再重复回填。
#[test]
fn billing_backfills_existing_usage_logs_once() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO credentials (id, access_token, refresh_token, expires_at) VALUES (5, 'a', 'r', 0);
         INSERT INTO usage_logs (ts, cred_id, model, path, cost_usd, input_tokens)
         VALUES (1800000000, 5, 'm', '/v1/messages', 0.25, 7), (1800000100, 5, 'm', '/v1/messages', 0.75, 3);
         DELETE FROM billing_hourly; DELETE FROM settings WHERE key = 'billing_backfilled';",
    )
    .unwrap();
    init_schema(&conn).unwrap();
    init_schema(&conn).unwrap();
    let store = CredentialStore::with_conn(conn);
    let f = BillingFilter { since: 1_799_990_000, until: 1_800_010_000, ..Default::default() };
    let rows = store.billing_breakdown(&f, BillingDim::Cred).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].requests, rows[0].cost_usd, rows[0].input_tokens), (2, 1.0, 10));
}

/// 配过接入 Key 之后把 Key 全删了，转发仍要求带 Key（不退回「不校验」）。
#[test]
fn deleting_every_key_keeps_auth_required() {
    let store = CredentialStore::open_in_memory().unwrap();
    assert!(!store.api_keys_required().unwrap(), "从没配过：不校验");
    let id = store.create_api_key("k", "key-only", &[]).unwrap().unwrap();
    store.delete_api_key(id).unwrap();
    assert!(store.api_keys_required().unwrap(), "删光了也仍要求带 Key");
    assert!(store.api_key_access("key-only").unwrap().is_none());
}

/// Key 唯一绑定的分组被删掉：这把 Key 变成一个号都不能用，而不是全部号；显式不绑分组的
/// Key 才是全部号。
#[test]
fn deleting_a_keys_only_group_fails_closed() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let g = store.create_group("g", "").unwrap().unwrap();
    store.insert("a", None, "t", "r", u64::MAX, None, None, admin).unwrap();
    store.create_api_key("bound", "key-bound", &[g]).unwrap().unwrap();
    store.create_api_key("all", "key-all", &[]).unwrap().unwrap();
    store.delete_group(g).unwrap().unwrap();
    let bound = store.api_key_access("key-bound").unwrap().unwrap();
    assert_eq!(bound.groups, Some(vec![]));
    assert!(
        store
            .select_for_device(Select { groups: bound.groups.as_deref(), ..Default::default() })
            .is_err()
    );
    assert_eq!(store.api_key_access("key-all").unwrap().unwrap().groups, None);
    let keys = store.list_api_keys().unwrap();
    assert!(!keys.iter().find(|k| k.label == "bound").unwrap().all_groups);
}

/// 加密迁移之后库文件里捞不到旧明文：改写过的页清零、VACUUM 重写整库、WAL 截断。
#[test]
fn encrypting_tokens_leaves_no_plaintext_in_the_file() {
    let dir = std::env::temp_dir().join(format!("luban-scrub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("luban.db");
    let _ = std::fs::remove_file(&path);
    let marker = |i: usize| format!("PLAINTEXT-REFRESH-TOKEN-MARKER-{i:04}-{}", "x".repeat(60));
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
             CREATE TABLE credentials (id INTEGER PRIMARY KEY AUTOINCREMENT, label TEXT NOT NULL DEFAULT '',
                 tier TEXT, org_type TEXT, access_token TEXT NOT NULL, refresh_token TEXT NOT NULL,
                 expires_at INTEGER NOT NULL, priority INTEGER NOT NULL DEFAULT 2,
                 disabled INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 updated_at INTEGER NOT NULL DEFAULT (unixepoch())) STRICT;",
        )
        .unwrap();
        for i in 0..100 {
            conn.execute(
                "INSERT INTO credentials (access_token, refresh_token, expires_at) VALUES (?1, ?2, 0)",
                params![format!("at-{i}"), marker(i)],
            )
            .unwrap();
        }
    }
    {
        let conn = Connection::open(&path).unwrap();
        init_schema(&conn).unwrap();
    }
    let mut bytes = std::fs::read(&path).unwrap();
    if let Ok(wal) = std::fs::read(dir.join("luban.db-wal")) {
        bytes.extend(wal);
    }
    let needle = b"PLAINTEXT-REFRESH-TOKEN-MARKER";
    assert!(!bytes.windows(needle.len()).any(|w| w == needle), "库文件里还能找到明文 token");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 密钥校验值或接入 Key 的密文解不开（换了密钥）：拒绝启动，哪怕库里一个号都没有。
#[test]
fn a_wrong_secret_key_is_caught_without_any_credentials() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute(
        "UPDATE settings SET value = 'enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' WHERE key = 'secret_key_check'",
        [],
    )
    .unwrap();
    assert!(format!("{:#}", init_schema(&conn).unwrap_err()).contains("secret key does not match"));

    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO api_keys (label, key_hash, key_sealed) VALUES ('k', 'h', 'enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA')",
        [],
    )
    .unwrap();
    let err = init_schema(&conn).unwrap_err();
    assert!(format!("{err:#}").contains("access key"), "{err:#}");
}

/// 分组内的暂停与模型判断只看这把 Key 能用的号：本组全部暂停回 429；别的组里暂停的号
/// 不让本组「模型不支持」变成 429。
#[test]
fn paused_and_denied_checks_stay_inside_the_key_groups() {
    let store = CredentialStore::open_in_memory().unwrap();
    let admin = store.admin_user().unwrap().id;
    let g1 = store.create_group("g1", "").unwrap().unwrap();
    let g2 = store.create_group("g2", "").unwrap().unwrap();
    let a = store.insert("a", None, "ta", "ra", u64::MAX, None, None, admin).unwrap().id;
    let b = store.insert("b", None, "tb", "rb", u64::MAX, None, None, admin).unwrap().id;
    store.set_credential_groups(&[a], &[g1]).unwrap().unwrap();
    store.set_credential_groups(&[b], &[g2]).unwrap().unwrap();
    let later = crate::credentials::now_secs() + 600;
    store.pause_for_rate_limit(a, "rate limited", later).unwrap();
    let only_g1 = [g1];
    let err = store
        .select_for_device(Select { groups: Some(&only_g1), ..Default::default() })
        .unwrap_err();
    assert!(err.downcast_ref::<AllRateLimited>().is_some(), "本组全部暂停：429，{err:#}");

    store.deny_model(b, "claude-fable-5", "plan", None).unwrap();
    let only_g2 = [g2];
    let err = store
        .select_for_device(Select {
            groups: Some(&only_g2),
            model: Some("claude-fable-5"),
            ..Default::default()
        })
        .unwrap_err();
    assert!(
        err.downcast_ref::<ModelUnsupported>().is_some(),
        "别组暂停的号不该让这里回 429，{err:#}"
    );
}

/// 绑定的分组删光后，只改名字、停用再启用都不会把这把 Key 变成全部号；显式选「全部号」才是。
#[test]
fn editing_an_orphaned_key_never_widens_it() {
    let store = CredentialStore::open_in_memory().unwrap();
    let g = store.create_group("g", "").unwrap().unwrap();
    let id = store.create_api_key("bound", "key-orphan", &[g]).unwrap().unwrap();
    store.delete_group(g).unwrap().unwrap();
    let scope = |s: &CredentialStore| s.api_key_access("key-orphan").unwrap().map(|a| a.groups);
    store.update_api_key(id, "renamed", false, &[], None).unwrap().unwrap();
    assert_eq!(scope(&store), Some(Some(vec![])), "改名不放开");
    store.update_api_key(id, "renamed", true, &[], None).unwrap().unwrap();
    store.update_api_key(id, "renamed", false, &[], None).unwrap().unwrap();
    assert_eq!(scope(&store), Some(Some(vec![])), "停用再启用不放开");
    store.update_api_key(id, "renamed", false, &[], Some(false)).unwrap().unwrap();
    assert_eq!(scope(&store), Some(Some(vec![])), "显式只限分组、列表为空：仍是一个号都不能用");
    store.update_api_key(id, "renamed", false, &[], Some(true)).unwrap().unwrap();
    assert_eq!(scope(&store), Some(None), "显式选全部号才是全部号");
}

/// 清理被别的连接的读快照挡住时如实回「没清完」并留下标记；快照放掉后重试清干净、删标记。
#[test]
fn scrub_reports_busy_and_retries_until_clean() {
    let dir = std::env::temp_dir().join(format!("luban-scrub-busy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("luban.db");
    for f in ["luban.db", "luban.db-wal", "luban.db-shm"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let a = Connection::open(&path).unwrap();
    a.pragma_update(None, "journal_mode", "WAL").unwrap();
    init_schema(&a).unwrap();
    let needle = "PLAINTEXT-LEFTOVER-MARKER-0123456789";
    a.pragma_update(None, "secure_delete", "OFF").unwrap();
    a.execute("INSERT INTO settings (key, value) VALUES ('tmp', ?1)", [needle]).unwrap();
    a.execute("UPDATE settings SET value = 'sealed' WHERE key = 'tmp'", []).unwrap();
    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN; SELECT COUNT(*) FROM settings;").unwrap();
    let _: i64 = reader.query_row("SELECT COUNT(*) FROM settings", [], |r| r.get(0)).unwrap();
    assert!(!secret::scrub_freed_pages(&a).unwrap(), "读快照挡着时不能报成功");
    let pending: bool = a
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM settings WHERE key = 'secret_scrub_pending')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(pending, "没清完要留标记等重试");
    reader.execute_batch("COMMIT;").unwrap();
    drop(reader);
    secret::scrub_if_pending(&a).unwrap();
    let pending: bool = a
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM settings WHERE key = 'secret_scrub_pending')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!pending, "清干净后删标记");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend(std::fs::read(dir.join("luban.db-wal")).unwrap_or_default());
    assert!(
        !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
        "库文件或 WAL 里还有旧明文"
    );
    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 留下明文残留的那几步各自在同一个事务里落下清理标记：之后的迁移失败、进程退出，下次启动
/// 照样按标记清理，不会因为「token 已是密文、校验值已写」而跳过。
#[test]
fn scrub_marker_survives_a_failed_startup() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let pending = |c: &Connection| -> bool {
        c.query_row(
            "SELECT EXISTS (SELECT 1 FROM settings WHERE key = 'secret_scrub_pending')",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    let clear = |c: &Connection| {
        c.execute("DELETE FROM settings WHERE key = 'secret_scrub_pending'", []).unwrap();
    };
    assert!(!pending(&conn), "正常启动清完不留标记");

    conn.execute(
        "INSERT INTO credentials (access_token, refresh_token, expires_at) VALUES ('at-plain', 'rt-plain', 0)",
        [],
    )
    .unwrap();
    secret::encrypt_plaintext_tokens(&conn).unwrap();
    assert!(pending(&conn), "加密明文 token 时就要落标记");
    clear(&conn);

    conn.execute("INSERT INTO settings (key, value) VALUES ('client_api_key', 'legacy-key')", [])
        .unwrap();
    migrate_groups(&conn).unwrap();
    assert!(pending(&conn), "搬旧版全局 Key 时就要落标记");
    clear(&conn);

    conn.execute("DELETE FROM settings WHERE key = 'secret_key_check'", []).unwrap();
    secret::ensure_secret_check(&conn).unwrap();
    assert!(pending(&conn), "首次写校验值时就要落标记");

    // 模拟上一轮在清理前失败：再启动一次要把欠着的清理补上。
    init_schema(&conn).unwrap();
    assert!(!pending(&conn), "下次启动补清理并删标记");
}
