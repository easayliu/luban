//! 库内机密的静态加密：号的 access / refresh token、接入 Key 的明文。
//!
//! AES-256-GCM，每次加密随机 96 位 nonce，存成 `enc1:` + base64(nonce ‖ 密文)。没有这个前缀的
//! 值按明文读（老库升级前的存量，[`encrypt_plaintext_tokens`] 启动时会把它们补加密）。
//!
//! **密钥**：环境变量 `LUBAN_SECRET_KEY`（64 位十六进制）优先，否则用数据目录下的
//! `secret.key`，不存在就生成一份（权限 0600）。它防的是「只拿到了库文件」——导出的备份、
//! 拷走的 `luban.db`、库文件被别的进程读到；密钥文件与库放在一起时，拿到整个目录的人照样能
//! 解开，要防这一层就把密钥放进环境变量。
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
                     with luban.db (losing it means every account must be re-authorized), or set \
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
    let cipher = Aes256Gcm::new(key().into());
    let mut nonce = [0u8; 12];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut nonce);
    let ct = cipher
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
    let plain = Aes256Gcm::new(key().into())
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| anyhow::anyhow!("failed to decrypt a stored secret (wrong secret key?)"))?;
    Ok(String::from_utf8(plain)?)
}

/// refresh_token 的指纹（sha256 十六进制）：密文每次都不一样，查重、按 token 找号都靠它。
pub(crate) fn token_fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// 给 `row_to_cred` 用：解不开时报成 rusqlite 的转换错误。启动时已经验过密钥（见
/// [`encrypt_plaintext_tokens`]），走到这里解不开只会是库被外部改坏了。
pub(super) fn open_column(stored: String, col: usize) -> rusqlite::Result<String> {
    open(&stored).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, e.into())
    })
}

/// 启动迁移：先拿密钥试解所有已加密的 token（解不开就拒绝启动），再把明文的补加密、补上
/// refresh_token 指纹。幂等，每次启动都跑。
pub(super) fn encrypt_plaintext_tokens(conn: &Connection) -> Result<()> {
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN refresh_token_hash TEXT", []);
    // 唯一约束从 refresh_token 本身挪到它的指纹上：密文随机，对它做唯一约束形同虚设。
    conn.execute_batch(
        "DROP INDEX IF EXISTS uq_credentials_refresh_token;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_credentials_refresh_hash
             ON credentials(refresh_token_hash);",
    )?;
    let rows: Vec<(i64, String, String, Option<String>)> = conn
        .prepare("SELECT id, access_token, refresh_token, refresh_token_hash FROM credentials")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut pending = Vec::new();
    for (id, access, refresh, hash) in rows {
        let (a, r) = match (open(&access), open(&refresh)) {
            (Ok(a), Ok(r)) => (a, r),
            _ => anyhow::bail!(
                "the secret key does not match the one used to encrypt the stored tokens \
                 (credential #{id}); restore the original secret.key or LUBAN_SECRET_KEY"
            ),
        };
        let sealed = access.starts_with(SEALED_PREFIX) && refresh.starts_with(SEALED_PREFIX);
        if !sealed || hash.is_none() {
            pending.push((id, a, r));
        }
    }
    if pending.is_empty() {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    {
        let mut stmt = tx.prepare(
            "UPDATE credentials SET access_token = ?2, refresh_token = ?3, \
                    refresh_token_hash = ?4 WHERE id = ?1",
        )?;
        for (id, access, refresh) in &pending {
            stmt.execute(params![id, seal(access), seal(refresh), token_fingerprint(refresh)])?;
        }
    }
    tx.commit()?;
    tracing::info!(count = pending.len(), "encrypted stored credential tokens");
    Ok(())
}
