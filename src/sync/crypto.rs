//! End-to-end encryption for sync field values.
//!
//! Non-link field values are encrypted locally using ChaCha20-Poly1305 before
//! leaving the device, bound to their `(entity_id, kind, field, device_id, event_id)`
//! identity via authenticated data (AAD). The server sees only ciphertext envelopes prefixed
//! with [`E2E_PREFIX`].

use std::num::NonZeroU32;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

use super::types::{CellEvent, CellValue, EXISTENCE, LogRow, SyncResponse};
use crate::config::Config;
use crate::db::{EventId, Id};

pub const E2E_PREFIX: &str = "e2e:v1:";

/// A 32-byte symmetric key used for end-to-end encryption.
#[derive(Clone, PartialEq, Eq)]
pub struct EncryptionKey(pub(crate) [u8; 32]);

impl std::fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EncryptionKey([REDACTED])")
    }
}

impl EncryptionKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn to_base64(&self) -> String {
        STANDARD.encode(&self.0)
    }

    pub fn from_base64(encoded: &str) -> Result<Self> {
        let bytes = STANDARD
            .decode(encoded.trim())
            .context("invalid base64 encryption key")?;
        let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            anyhow::anyhow!("encryption key must be exactly 32 bytes, got {}", v.len())
        })?;
        Ok(Self(arr))
    }

    pub fn from_config_str(raw: &str) -> Self {
        let trimmed = raw.trim();
        if let Ok(key) = Self::from_base64(trimmed) {
            return key;
        }
        let mut bytes = [0u8; 32];
        ring::pbkdf2::derive(
            ring::pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(100_000).unwrap(),
            b"im-sync-config-key-v1",
            trimmed.as_bytes(),
            &mut bytes,
        );
        Self(bytes)
    }

    /// A 32-hex-character fingerprint sent with sync requests to identify the key.
    pub fn key_id(&self) -> String {
        let hmac_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        let tag = ring::hmac::sign(&hmac_key, b"im-sync-key-id-v1");
        tag.as_ref()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Derive the server authentication password and E2EE key from user credentials.
pub fn derive_credentials(email: &str, password: &str) -> (String, EncryptionKey) {
    let normalized = email.trim().to_lowercase();
    let salt = format!("im-sync-v1:{normalized}");
    let mut master = [0u8; 32];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(100_000).unwrap(),
        salt.as_bytes(),
        password.as_bytes(),
        &mut master,
    );

    let hmac_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &master);
    let auth_tag = ring::hmac::sign(&hmac_key, b"im-sync-auth-v1");
    let auth_password = STANDARD.encode(auth_tag.as_ref());

    let enc_tag = ring::hmac::sign(&hmac_key, b"im-sync-enc-v1");
    let mut enc_bytes = [0u8; 32];
    enc_bytes.copy_from_slice(enc_tag.as_ref());
    let encryption_key = EncryptionKey::from_bytes(enc_bytes);

    (auth_password, encryption_key)
}

/// Whether a cell should be encrypted. Existence and link fields remain plaintext
/// so server-side referential integrity, delete cascades, and LWW registers work.
pub fn should_encrypt(kind: &str, field: &str) -> bool {
    field != EXISTENCE && super::schema::link(kind, field).is_none()
}

/// Look up the configured encryption key override in `[sync] encryption_key`.
pub fn encryption_key(config: &Config) -> Option<EncryptionKey> {
    config
        .sync
        .encryption_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(EncryptionKey::from_config_str)
}

/// Encrypt a single cell value.
pub fn encrypt_value(
    key: &EncryptionKey,
    entity_id: Id,
    kind: &str,
    field: &str,
    device_id: Id,
    event_id: EventId,
    value: &CellValue,
) -> Result<CellValue> {
    let rng = SystemRandom::new();
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key.0)
        .map_err(|_| anyhow::anyhow!("invalid encryption key"))?;
    let sealing_key = LessSafeKey::new(unbound);
    encrypt_value_with(
        &sealing_key,
        &rng,
        entity_id,
        kind,
        field,
        device_id,
        event_id,
        value,
    )
}

fn encrypt_value_with(
    sealing_key: &LessSafeKey,
    rng: &SystemRandom,
    entity_id: Id,
    kind: &str,
    field: &str,
    device_id: Id,
    event_id: EventId,
    value: &CellValue,
) -> Result<CellValue> {
    let mut plaintext =
        serde_json::to_vec(value).context("failed to serialize cell value for encryption")?;
    let mut nonce_bytes = [0u8; 12];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate random nonce"))?;

    let aad = format!("{entity_id}\0{kind}\0{field}\0{device_id}\0{event_id}");
    sealing_key
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(aad.as_bytes()),
            &mut plaintext,
        )
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;

    let mut payload = Vec::with_capacity(12 + plaintext.len());
    payload.extend_from_slice(&nonce_bytes);
    payload.extend_from_slice(&plaintext);

    let b64 = STANDARD.encode(&payload);
    Ok(CellValue::Text(format!("{E2E_PREFIX}{b64}")))
}

/// Decrypt a single cell value.
pub fn decrypt_value(
    key: &EncryptionKey,
    entity_id: Id,
    kind: &str,
    field: &str,
    device_id: Id,
    event_id: EventId,
    value: &CellValue,
) -> Result<CellValue> {
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key.0)
        .map_err(|_| anyhow::anyhow!("invalid encryption key"))?;
    let opening_key = LessSafeKey::new(unbound);
    decrypt_value_with(
        &opening_key,
        entity_id,
        kind,
        field,
        device_id,
        event_id,
        value,
    )
}

fn decrypt_value_with(
    opening_key: &LessSafeKey,
    entity_id: Id,
    kind: &str,
    field: &str,
    device_id: Id,
    event_id: EventId,
    value: &CellValue,
) -> Result<CellValue> {
    let decrypt_inner = || -> Result<CellValue> {
        let text = match value {
            CellValue::Text(text) => text,
            other => bail!("expected encrypted text cell value, got {}", other.class()),
        };
        let encoded = text
            .strip_prefix(E2E_PREFIX)
            .with_context(|| format!("encrypted cell value missing `{E2E_PREFIX}` prefix"))?;

        let raw = STANDARD
            .decode(encoded)
            .context("failed to decode base64 encrypted cell payload")?;

        if raw.len() < 12 + 16 {
            bail!("encrypted cell payload too short ({} bytes)", raw.len());
        }

        let (nonce_slice, ciphertext_and_tag) = raw.split_at(12);
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes.copy_from_slice(nonce_slice);

        let mut in_out = ciphertext_and_tag.to_vec();
        let aad = format!("{entity_id}\0{kind}\0{field}\0{device_id}\0{event_id}");

        let plaintext = opening_key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(aad.as_bytes()),
                &mut in_out,
            )
            .map_err(|_| {
                anyhow::anyhow!("decryption failed (wrong key or corrupted ciphertext/AAD)")
            })?;

        let decrypted: CellValue = serde_json::from_slice(plaintext)
            .context("failed to deserialize decrypted plaintext into CellValue")?;
        Ok(decrypted)
    };

    decrypt_inner().with_context(|| format!("failed to decrypt {kind}.{field} of {entity_id}"))
}

/// Encrypt all eligible cell events in an outgoing batch.
pub fn encrypt_events(events: &[CellEvent], key: &EncryptionKey) -> Result<Vec<CellEvent>> {
    let rng = SystemRandom::new();
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key.0)
        .map_err(|_| anyhow::anyhow!("invalid encryption key"))?;
    let sealing_key = LessSafeKey::new(unbound);

    let mut encrypted = Vec::with_capacity(events.len());
    for event in events {
        let mut ev = event.clone();
        if should_encrypt(&ev.kind, &ev.field) {
            if let Some(ref val) = ev.value {
                ev.value = Some(encrypt_value_with(
                    &sealing_key,
                    &rng,
                    ev.entity_id,
                    &ev.kind,
                    &ev.field,
                    ev.device_id,
                    ev.event_id,
                    val,
                )?);
            }
        }
        encrypted.push(ev);
    }
    Ok(encrypted)
}

/// Decrypt all encrypted cells in a server response.
pub fn decrypt_response(
    mut response: SyncResponse,
    key: &EncryptionKey,
    local_device_id: Id,
) -> Result<SyncResponse> {
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key.0)
        .map_err(|_| anyhow::anyhow!("invalid encryption key"))?;
    let opening_key = LessSafeKey::new(unbound);

    for event in &mut response.events {
        decrypt_log_row(&opening_key, event)?;
    }
    for verdict in &mut response.verdicts {
        if let Some(ref mut winner) = verdict.winner {
            decrypt_log_row(&opening_key, winner)?;
        }
        if let Some(ref mut tombstone) = verdict.tombstone {
            decrypt_log_row(&opening_key, tombstone)?;
        }
    }
    for notice in &mut response.superseded {
        if should_encrypt(&notice.winner.cell.kind, &notice.field) {
            if let Some(ref lost) = notice.lost {
                notice.lost = Some(decrypt_value_with(
                    &opening_key,
                    notice.entity_id,
                    &notice.winner.cell.kind,
                    &notice.field,
                    local_device_id,
                    notice.event_id,
                    lost,
                )?);
            }
        }
        decrypt_log_row(&opening_key, &mut notice.winner)?;
    }
    Ok(response)
}

fn decrypt_log_row(opening_key: &LessSafeKey, row: &mut LogRow) -> Result<()> {
    if should_encrypt(&row.cell.kind, &row.cell.field) {
        if let Some(ref val) = row.cell.value {
            row.cell.value = Some(decrypt_value_with(
                opening_key,
                row.cell.entity_id,
                &row.cell.kind,
                &row.cell.field,
                row.cell.device_id,
                row.cell.event_id,
                val,
            )?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{EventId, Id};

    #[test]
    fn test_round_trip_encryption() {
        let key = EncryptionKey::from_bytes([42u8; 32]);
        let id = Id::new();
        let dev = Id::new();
        let ev = EventId::new();

        let values = vec![
            CellValue::Int(12345),
            CellValue::Real(98.75),
            CellValue::Text("secret task description".to_string()),
        ];

        for original in values {
            let encrypted = encrypt_value(&key, id, "task", "name", dev, ev, &original)
                .expect("encryption succeeds");
            assert!(matches!(&encrypted, CellValue::Text(s) if s.starts_with(E2E_PREFIX)));

            let decrypted = decrypt_value(&key, id, "task", "name", dev, ev, &encrypted)
                .expect("decryption succeeds");
            assert_eq!(original, decrypted);
        }
    }

    #[test]
    fn test_aad_mismatch_fails() {
        let key = EncryptionKey::from_bytes([42u8; 32]);
        let id1 = Id::new();
        let id2 = Id::new();
        let dev1 = Id::new();
        let dev2 = Id::new();
        let ev1 = EventId::new();
        let ev2 = EventId::new();
        let val = CellValue::Text("hello".to_string());

        let encrypted = encrypt_value(&key, id1, "task", "name", dev1, ev1, &val).unwrap();

        // Wrong entity id
        assert!(decrypt_value(&key, id2, "task", "name", dev1, ev1, &encrypted).is_err());
        // Wrong kind
        assert!(decrypt_value(&key, id1, "mood", "name", dev1, ev1, &encrypted).is_err());
        // Wrong field
        assert!(decrypt_value(&key, id1, "task", "body", dev1, ev1, &encrypted).is_err());
        // Wrong device id
        assert!(decrypt_value(&key, id1, "task", "name", dev2, ev1, &encrypted).is_err());
        // Wrong event id
        assert!(decrypt_value(&key, id1, "task", "name", dev1, ev2, &encrypted).is_err());
    }

    #[test]
    fn test_wrong_key_fails() {
        let key1 = EncryptionKey::from_bytes([1u8; 32]);
        let key2 = EncryptionKey::from_bytes([2u8; 32]);
        let id = Id::new();
        let dev = Id::new();
        let ev = EventId::new();
        let val = CellValue::Text("hello".to_string());

        let encrypted = encrypt_value(&key1, id, "task", "name", dev, ev, &val).unwrap();
        assert!(decrypt_value(&key2, id, "task", "name", dev, ev, &encrypted).is_err());
    }

    #[test]
    fn test_should_encrypt_preserves_existence_and_links() {
        // Existence
        assert!(!should_encrypt("task", EXISTENCE));
        assert!(!should_encrypt("mood", EXISTENCE));
        assert!(!should_encrypt("tracker", EXISTENCE));
        assert!(!should_encrypt("completion", EXISTENCE));

        // Links
        assert!(!should_encrypt("task", "parent_id"));
        assert!(!should_encrypt("mood", "todo_id"));
        assert!(!should_encrypt("tracker", "mood_id"));
        assert!(!should_encrypt("completion", "todo_id"));

        // Non-link fields
        assert!(should_encrypt("task", "name"));
        assert!(should_encrypt("task", "priority"));
        assert!(should_encrypt("task", "start_time"));
        assert!(should_encrypt("mood", "mood"));
        assert!(should_encrypt("mood", "score"));
        assert!(should_encrypt("tracker", "type"));
        assert!(should_encrypt("tracker", "score"));
        assert!(should_encrypt("completion", "count"));
    }

    #[test]
    fn test_key_derivation() {
        let (auth1, key1) = derive_credentials("User@Example.com ", "secret123");
        let (auth2, key2) = derive_credentials("user@example.com", "secret123");
        assert_eq!(auth1, auth2);
        assert_eq!(key1, key2);

        let (auth3, key3) = derive_credentials("other@example.com", "secret123");
        assert_ne!(auth1, auth3);
        assert_ne!(key1, key3);

        assert_eq!(key1.key_id(), key2.key_id());
        assert_ne!(key1.key_id(), key3.key_id());
        assert_eq!(key1.key_id().len(), 32);
    }

    #[test]
    fn test_from_config_str() {
        let raw = "custom-passphrase";
        let k1 = EncryptionKey::from_config_str(raw);
        let k2 = EncryptionKey::from_config_str(" custom-passphrase ");
        assert_eq!(k1, k2);

        let b64 = k1.to_base64();
        let k3 = EncryptionKey::from_config_str(&b64);
        assert_eq!(k1, k3);
    }
}
