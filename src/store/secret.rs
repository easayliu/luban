//! 库内机密的静态加密：号的 access / refresh token、接入 Key 的明文。
//!
//! AES-256-GCM，每次加密随机 96 位 nonce，存成 `enc1:` + base64(nonce ‖ 密文)。没有这个前缀的
//! 值按明文读（SQLite 时代加密之前的存量格式，留着兼容）。
//!
//! **密钥**：环境变量 `LUBAN_SECRET_KEY`（64 位十六进制）优先，否则用数据目录下的
//! `secret.key`，不存在就生成一份（权限 0600）。它防的是「只拿到了库」——数据库的备份与转储、
//! 能连库的别的账号；密钥文件与库备份放在一起时，拿到两者的人照样能解开，要防这一层就把密钥
//! 放进环境变量。
//!
//! **密钥丢了，已加密的 token 就全部作废**（号得重新授权）。所以启动时先拿密钥试解一条已加密
//! 的记录，解不开直接拒绝启动——绝不能带着一把错的密钥跑起来，把每个号都当成 token 失效去
//! 停用。

use super::*;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

/// 加密值的前缀（带版本号，往后换算法时新旧可以并存）。
pub const SEALED_PREFIX: &str = "enc1:";

/// 存密钥的文件名（数据目录下）。
const KEY_FILE: &str = "secret.key";

static KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();

/// 测试用的固定密钥：测试不经 [`init_key`]，各处直接用内存库。
#[cfg(test)]
const TEST_KEY: [u8; 32] = [7u8; 32];

fn key() -> &'static [u8; 32] {
    #[cfg(test)]
    {
        KEY.get_or_init(|| TEST_KEY)
    }
    #[cfg(not(test))]
    {
        KEY.get().expect("the secret key must be initialized before the store is opened")
    }
}

/// 按 [`key`] 建好的 AES-GCM 实例，建一次反复用：每次加解密都 `Aes256Gcm::new` 会重做一遍
/// 密钥扩展，而列表、选号一次要解密成百上千个 token。
fn cipher() -> &'static Aes256Gcm {
    static CIPHER: std::sync::OnceLock<Aes256Gcm> = std::sync::OnceLock::new();
    CIPHER.get_or_init(|| Aes256Gcm::new(key().into()))
}

fn parse_hex_key(s: &str) -> Result<[u8; 32]> {
    let s = s.trim();
    anyhow::ensure!(
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "the secret key must be 64 hexadecimal characters (32 bytes)"
    );
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk)?, 16)?;
    }
    Ok(out)
}

/// 载入（或首次生成）密钥。打开库之前调一次，进程内只认第一次的结果。
pub fn init_key(data_dir: &std::path::Path) -> Result<()> {
    if KEY.get().is_some() {
        return Ok(());
    }
    let key = if let Some(raw) = std::env::var_os("LUBAN_SECRET_KEY") {
        parse_hex_key(&raw.to_string_lossy()).context("invalid LUBAN_SECRET_KEY")?
    } else {
        let path = data_dir.join(KEY_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => parse_hex_key(&raw)
                .with_context(|| format!("invalid secret key file: {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut bytes = [0u8; 32];
                rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                write_key_file(&path, &hex)?;
                tracing::warn!(
                    path = %path.display(),
                    "generated a new secret key for encrypting stored tokens; back it up together \
                     with the database (losing it means every account must be re-authorized), or set \
                     LUBAN_SECRET_KEY to keep it outside the data directory"
                );
                bytes
            }
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read {}", path.display()));
            }
        }
    };
    let _ = KEY.set(key);
    Ok(())
}

fn write_key_file(path: &std::path::Path, hex: &str) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("failed to create {}", path.display()))?;
    f.write_all(hex.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// 加密一个值。
pub(crate) fn seal(plain: &str) -> String {
    let mut nonce = [0u8; 12];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut nonce);
    let ct = cipher()
        .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
        .expect("AES-GCM encryption does not fail for in-memory inputs");
    let mut buf = Vec::with_capacity(12 + ct.len());
    buf.extend_from_slice(&nonce);
    buf.extend_from_slice(&ct);
    format!("{SEALED_PREFIX}{}", B64.encode(buf))
}

/// 解密一个值；没有 [`SEALED_PREFIX`] 的按明文原样返回。
pub(crate) fn open(stored: &str) -> Result<String> {
    let Some(b64) = stored.strip_prefix(SEALED_PREFIX) else {
        return Ok(stored.to_owned());
    };
    let buf = B64.decode(b64).context("a sealed value is not valid base64")?;
    anyhow::ensure!(buf.len() > 12, "a sealed value is too short");
    let (nonce, ct) = buf.split_at(12);
    let plain = cipher()
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| anyhow::anyhow!("failed to decrypt a stored secret (wrong secret key?)"))?;
    Ok(String::from_utf8(plain)?)
}

/// refresh_token 的指纹（sha256 十六进制）：密文每次都不一样，查重、按 token 找号都靠它。
pub(crate) fn token_fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// 密钥校验值：库里存一份用当前密钥加密的它，启动时解开比对——跟库里有没有号、有没有接入
/// Key 无关，换了密钥都当场发现，不会混进第二把密钥加密的数据。
pub(super) const SECRET_CHECK_KEY: &str = "secret_key_check";
pub(super) const SECRET_CHECK_PLAINTEXT: &str = "luban-secret-key-check";

pub(super) fn mismatch(what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the secret key does not match the one used to encrypt the stored secrets ({what}); \
         restore the original secret.key or LUBAN_SECRET_KEY"
    )
}

use anyhow::Result;
use sqlx::PgConnection;

/// 核对密钥：库里已有校验值就解开比对，并试解每一把接入 Key 的密文；解不开就拒绝启动——
/// 绝不能带着一把错的密钥跑起来，把每个号都当成 token 失效去停用。还没有校验值（新库）
/// 就用当前密钥写一份。
pub(super) async fn verify_secret_key(conn: &mut PgConnection) -> Result<()> {
    let check: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
        .bind(SECRET_CHECK_KEY)
        .fetch_optional(&mut *conn)
        .await?;
    match check {
        Some(check) => {
            if open(&check).ok().as_deref() != Some(SECRET_CHECK_PLAINTEXT) {
                return Err(mismatch("key check"));
            }
        }
        None => {
            sqlx::query("INSERT INTO settings (key, value) VALUES ($1, $2)")
                .bind(SECRET_CHECK_KEY)
                .bind(seal(SECRET_CHECK_PLAINTEXT))
                .execute(&mut *conn)
                .await?;
        }
    }
    let keys: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, key_sealed FROM api_keys").fetch_all(&mut *conn).await?;
    for (id, sealed) in keys {
        if open(&sealed).is_err() {
            return Err(mismatch(&format!("access key #{id}")));
        }
    }
    let creds: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, access_token, refresh_token FROM credentials")
            .fetch_all(&mut *conn)
            .await?;
    for (id, access, refresh) in creds {
        if open(&access).is_err() || open(&refresh).is_err() {
            return Err(mismatch(&format!("credential #{id}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::CredentialStore;
    use super::*;

    /// 新库打开时写下校验值；校验值被换成别的密钥加密的东西时拒绝打开。
    #[sqlx::test]
    async fn rejects_a_mismatched_key_check(pool: PgPool) {
        CredentialStore::for_test(pool.clone()).await;
        let stored: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
            .bind(SECRET_CHECK_KEY)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(open(&stored).unwrap(), SECRET_CHECK_PLAINTEXT);
        sqlx::query(
            "UPDATE settings SET value = 'enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAA' WHERE key = $1",
        )
        .bind(SECRET_CHECK_KEY)
        .execute(&pool)
        .await
        .unwrap();
        assert!(CredentialStore::open(pool).await.is_err());
    }
}
