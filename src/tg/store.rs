//! Persistence for the Telegram layer: encrypted wallets, target orders,
//! and per-chat preferences.
//!
//! Key hierarchy (see docs/S2-DESIGN.md §3): the private key is sealed with
//! XChaCha20-Poly1305 under a random per-wallet DEK. The DEK is wrapped
//! either by the KEK held outside this store (environment `TG_WALLET_SECRETS_KEY`),
//! or — when the user opts into the passphrase layer — by an Argon2id key
//! derived from the user's passphrase only. A database breach alone therefore
//! reveals nothing usable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256};
use alloy::signers::local::PrivateKeySigner;
use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::error::{EngineError, Result};

const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// A wallet whose private key never rests in the clear.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedWallet {
    pub address: Address,
    /// True when the DEK is wrapped by the user's passphrase instead of the
    /// KEK; the bot then needs the passphrase typed per signing session.
    pub passphrase_layer: bool,
    /// Argon2id salt for the passphrase layer (unused otherwise).
    pub kdf_salt: [u8; 16],
    /// AEAD(dek, private key) — never readable without unwrapping the DEK.
    pub nonce_key: [u8; NONCE_LEN],
    pub key_ct: Vec<u8>,
    /// AEAD(wrap key, dek) — wrap key is the KEK or the passphrase-derived key.
    pub nonce_wrap: [u8; NONCE_LEN],
    pub dek_ct: Vec<u8>,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderDirection {
    Buy,
    Sell,
}

/// Everything `add_order` needs, bundled to keep the call readable.
#[derive(Debug, Clone)]
pub struct OrderSpec {
    pub token: Address,
    pub symbol: String,
    pub settlement: Address,
    pub direction: OrderDirection,
    pub amount: alloy::primitives::U256,
    pub limit_price_1e18: alloy::primitives::U256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Watching,
    Fired { tx: String },
    Cancelled,
}

/// A target-price limit order. `amount` is the token amount to sell (1e18
/// whole-unit scaled) or the settlement-asset spend for a buy; `limit_price_1e18`
/// uses the floor module's price convention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetOrder {
    pub id: u64,
    pub chat: i64,
    pub token: Address,
    pub symbol: String,
    pub settlement: Address,
    pub direction: OrderDirection,
    pub amount: alloy::primitives::U256,
    pub limit_price_1e18: alloy::primitives::U256,
    pub status: OrderStatus,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatPrefs {
    pub slippage_bps: u64,
    /// 0 = economy, 1 = standard, 2 = fast.
    pub gas_profile: u8,
    pub confirm_before_send: bool,
}

impl Default for ChatPrefs {
    fn default() -> Self {
        Self {
            slippage_bps: 100,
            gas_profile: 1,
            confirm_before_send: true,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    wallets: BTreeMap<String, SealedWallet>,
    orders: Vec<TargetOrder>,
    chats: BTreeMap<String, ChatPrefs>,
    next_order_id: u64,
}

/// JSON-file-backed store. `kek` is the environment-held master key; without
/// it, KEK-wrapped wallets cannot be opened at all (the `db_breach` test
/// relies on exactly that).
pub struct Store {
    path: PathBuf,
    kek: Option<[u8; KEY_LEN]>,
    data: StoreFile,
}

impl Store {
    pub fn load(path: &Path, kek: Option<[u8; KEY_LEN]>) -> Result<Self> {
        let data = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| EngineError::Config(format!("tg store {}: {e}", path.display())))?,
            Err(_) => StoreFile::default(),
        };
        Ok(Self {
            path: path.to_path_buf(),
            kek,
            data,
        })
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| EngineError::Config(format!("tg store dir: {e}")))?;
        }
        let bytes = serde_json::to_vec_pretty(&self.data)
            .map_err(|e| EngineError::Config(format!("tg store encode: {e}")))?;
        std::fs::write(&self.path, bytes)
            .map_err(|e| EngineError::Config(format!("tg store write: {e}")))
    }

    /// Raw store bytes — for the "database breach" test.
    pub fn raw_bytes(&self) -> Result<Vec<u8>> {
        std::fs::read(&self.path).map_err(|e| EngineError::Config(format!("tg store read: {e}")))
    }

    // ------------------------------------------------------------- wallets

    fn seal(key: &B256, wrap_key: &[u8; KEY_LEN]) -> Result<SealedParts> {
        let wrap_cipher = XChaCha20Poly1305::new(Key::from_slice(wrap_key));
        let mut dek = [0u8; KEY_LEN];
        rand::rngs::OsRng.fill_bytes(&mut dek);
        let dek_cipher = XChaCha20Poly1305::new(Key::from_slice(&dek));
        let mut nonce_key = [0u8; NONCE_LEN];
        let mut nonce_wrap = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_key);
        rand::rngs::OsRng.fill_bytes(&mut nonce_wrap);
        // The key is sealed under the DEK; only the DEK sees the wrap key.
        let key_ct = dek_cipher
            .encrypt(XNonce::from_slice(&nonce_key), key.as_slice())
            .map_err(|_| EngineError::Config("wallet seal failed".to_string()))?;
        let dek_ct = wrap_cipher
            .encrypt(XNonce::from_slice(&nonce_wrap), dek.as_slice())
            .map_err(|_| EngineError::Config("wallet seal failed".to_string()))?;
        dek.zeroize();
        Ok(SealedParts {
            nonce_key,
            key_ct,
            nonce_wrap,
            dek_ct,
        })
    }

    fn unseal(w: &SealedWallet, wrap_key: &[u8; KEY_LEN]) -> Result<B256> {
        let cipher = XChaCha20Poly1305::new(Key::from_slice(wrap_key));
        let dek = cipher
            .decrypt(XNonce::from_slice(&w.nonce_wrap), w.dek_ct.as_slice())
            .map_err(|_| {
                EngineError::SafetyRefused("wallet: wrong key or passphrase".to_string())
            })?;
        let cipher2 = XChaCha20Poly1305::new(Key::from_slice(&dek));
        let key = cipher2
            .decrypt(XNonce::from_slice(&w.nonce_key), w.key_ct.as_slice())
            .map_err(|_| EngineError::SafetyRefused("wallet: corrupt ciphertext".to_string()))?;
        let mut key_arr = [0u8; KEY_LEN];
        key_arr.copy_from_slice(&key);
        Ok(B256::from(key_arr))
    }

    fn wrap_key_for(&self, w: &SealedWallet, passphrase: Option<&str>) -> Result<[u8; KEY_LEN]> {
        if w.passphrase_layer {
            let pass = passphrase.ok_or_else(|| {
                EngineError::SafetyRefused("wallet: passphrase required".to_string())
            })?;
            derive_passphrase_key(pass, &w.kdf_salt)
        } else {
            self.kek.ok_or_else(|| {
                EngineError::SafetyRefused(
                    "wallet: master key unavailable — cannot open wallet".to_string(),
                )
            })
        }
    }

    /// Generates a wallet from OS entropy. Returns the signer plus the key
    /// hex, which is shown to the user exactly once.
    pub fn create_wallet(
        &mut self,
        chat: i64,
        passphrase: Option<&str>,
    ) -> Result<(PrivateKeySigner, String)> {
        let mut key = B256::ZERO;
        rand::rngs::OsRng.fill_bytes(key.as_mut_slice());
        let signer = PrivateKeySigner::from_bytes(&key)
            .map_err(|e| EngineError::Config(format!("wallet key: {e}")))?;
        let key_hex = hex::encode(key.as_slice());

        let (wrap_key, kdf_salt, passphrase_layer) = match passphrase {
            Some(pass) => {
                let mut salt = [0u8; 16];
                rand::rngs::OsRng.fill_bytes(&mut salt);
                (derive_passphrase_key(pass, &salt)?, salt, true)
            }
            None => {
                let kek = self.kek.ok_or_else(|| {
                    EngineError::SafetyRefused(
                        "wallet: master key unavailable — set TG_WALLET_SECRETS_KEY".to_string(),
                    )
                })?;
                (kek, [0u8; 16], false)
            }
        };

        let parts = Self::seal(&key, &wrap_key)?;
        key.zeroize();
        let sealed = SealedWallet {
            address: signer.address(),
            passphrase_layer,
            kdf_salt,
            nonce_key: parts.nonce_key,
            key_ct: parts.key_ct,
            nonce_wrap: parts.nonce_wrap,
            dek_ct: parts.dek_ct,
            created_at: now(),
        };
        self.data.wallets.insert(chat.to_string(), sealed);
        self.save()?;
        Ok((signer, key_hex))
    }

    /// Imports a user-provided key (typically generated on their own machine).
    pub fn import_wallet(
        &mut self,
        chat: i64,
        key_hex: &str,
        passphrase: Option<&str>,
    ) -> Result<Address> {
        let raw = hex::decode(key_hex.trim().trim_start_matches("0x"))
            .map_err(|_| EngineError::Config("wallet: key must be 32-byte hex".to_string()))?;
        if raw.len() != KEY_LEN {
            return Err(EngineError::Config(
                "wallet: key must be 32 bytes".to_string(),
            ));
        }
        let mut key = B256::from_slice(&raw);
        let signer = PrivateKeySigner::from_bytes(&key)
            .map_err(|e| EngineError::Config(format!("wallet key: {e}")))?;

        let (wrap_key, kdf_salt, passphrase_layer) = match passphrase {
            Some(pass) => {
                let mut salt = [0u8; 16];
                rand::rngs::OsRng.fill_bytes(&mut salt);
                (derive_passphrase_key(pass, &salt)?, salt, true)
            }
            None => {
                let kek = self.kek.ok_or_else(|| {
                    EngineError::SafetyRefused(
                        "wallet: master key unavailable — set TG_WALLET_SECRETS_KEY".to_string(),
                    )
                })?;
                (kek, [0u8; 16], false)
            }
        };
        let parts = Self::seal(&key, &wrap_key)?;
        key.zeroize();
        self.data.wallets.insert(
            chat.to_string(),
            SealedWallet {
                address: signer.address(),
                passphrase_layer,
                kdf_salt,
                nonce_key: parts.nonce_key,
                key_ct: parts.key_ct,
                nonce_wrap: parts.nonce_wrap,
                dek_ct: parts.dek_ct,
                created_at: now(),
            },
        );
        self.save()?;
        Ok(signer.address())
    }

    /// Unseals the wallet for signing. With the passphrase layer active a
    /// passphrase is mandatory — the KEK alone cannot open the wallet.
    pub fn open_wallet(&self, chat: i64, passphrase: Option<&str>) -> Result<PrivateKeySigner> {
        let w =
            self.data.wallets.get(&chat.to_string()).ok_or_else(|| {
                EngineError::SafetyRefused("wallet: none for this chat".to_string())
            })?;
        let wrap_key = self.wrap_key_for(w, passphrase)?;
        let key = Self::unseal(w, &wrap_key)?;
        PrivateKeySigner::from_bytes(&key)
            .map_err(|e| EngineError::Config(format!("wallet key: {e}")))
    }

    pub fn wallet_address(&self, chat: i64) -> Option<Address> {
        self.data.wallets.get(&chat.to_string()).map(|w| w.address)
    }

    /// Explicit, user-driven only — the bot never removes wallets silently.
    pub fn remove_wallet(&mut self, chat: i64) -> Result<()> {
        self.data.wallets.remove(&chat.to_string());
        self.save()
    }

    // -------------------------------------------------------------- orders

    pub fn add_order(&mut self, chat: i64, spec: OrderSpec) -> TargetOrder {
        let OrderSpec {
            token,
            symbol,
            settlement,
            direction,
            amount,
            limit_price_1e18,
        } = spec;
        let id = self.data.next_order_id.max(1);
        self.data.next_order_id = id + 1;
        let order = TargetOrder {
            id,
            chat,
            token,
            symbol,
            settlement,
            direction,
            amount,
            limit_price_1e18,
            status: OrderStatus::Watching,
            created_at: now(),
        };
        self.data.orders.push(order.clone());
        let _ = self.save();
        order
    }

    pub fn orders_for(&self, chat: i64) -> Vec<TargetOrder> {
        self.data
            .orders
            .iter()
            .filter(|o| o.chat == chat)
            .cloned()
            .collect()
    }

    pub fn watching_orders(&self) -> Vec<TargetOrder> {
        self.data
            .orders
            .iter()
            .filter(|o| o.status == OrderStatus::Watching)
            .cloned()
            .collect()
    }

    pub fn set_order_status(&mut self, id: u64, status: OrderStatus) -> Result<()> {
        let order = self
            .data
            .orders
            .iter_mut()
            .find(|o| o.id == id)
            .ok_or_else(|| EngineError::SafetyRefused(format!("order {id}: not found")))?;
        order.status = status;
        self.save()
    }

    // ---------------------------------------------------------------- prefs

    pub fn prefs(&self, chat: i64) -> ChatPrefs {
        self.data
            .chats
            .get(&chat.to_string())
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_prefs(&mut self, chat: i64, prefs: ChatPrefs) -> Result<()> {
        self.data.chats.insert(chat.to_string(), prefs);
        self.save()
    }
}

struct SealedParts {
    nonce_key: [u8; NONCE_LEN],
    key_ct: Vec<u8>,
    nonce_wrap: [u8; NONCE_LEN],
    dek_ct: Vec<u8>,
}

fn derive_passphrase_key(passphrase: &str, salt: &[u8; 16]) -> Result<[u8; KEY_LEN]> {
    let mut out = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|_| EngineError::Config("wallet: passphrase KDF failed".to_string()))?;
    Ok(out)
}
