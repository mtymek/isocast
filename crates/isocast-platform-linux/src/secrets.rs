//! Secrets (stream keys) in the desktop's keyring through the Secret
//! Service (GNOME Keyring, KWallet, KeePassXC): items labelled for Isocast
//! with the attributes `application`, `service` and `account`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use secret_service::{EncryptionType, SecretService};

fn attributes<'a>(service: &'a str, account: &'a str) -> HashMap<&'a str, &'a str> {
    HashMap::from([
        ("application", "isocast"),
        ("service", service),
        ("account", account),
    ])
}

/// Run `f` against the Secret Service on a runtime of its own (its D-Bus
/// connection needs one).
fn with_service<T>(f: impl AsyncFnOnce(&SecretService<'_>) -> Result<T>) -> Result<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the keyring connection")?;
    runtime.block_on(async {
        let service = SecretService::connect(EncryptionType::Dh)
            .await
            .context("no keyring (Secret Service) is running for this session")?;
        f(&service).await
    })
}

/// Whether a secret is kept. Never prompts, even for a locked keyring.
pub fn has_secret(service: &str, account: &str) -> Result<bool> {
    with_service(async |keyring| {
        let found = keyring.search_items(attributes(service, account)).await?;
        Ok(!found.unlocked.is_empty() || !found.locked.is_empty())
    })
}

/// The secret kept, if any. A locked keyring asks the person to unlock it.
pub fn secret(service: &str, account: &str) -> Result<Option<String>> {
    with_service(async |keyring| {
        let found = keyring.search_items(attributes(service, account)).await?;
        let item = match (
            found.unlocked.into_iter().next(),
            found.locked.into_iter().next(),
        ) {
            (Some(item), _) => item,
            (None, Some(item)) => {
                item.unlock().await.context("the keyring stayed locked")?;
                item
            }
            (None, None) => return Ok(None),
        };
        let bytes = item.get_secret().await?;
        Ok(Some(
            String::from_utf8(bytes).context("the kept secret isn't text")?,
        ))
    })
}

/// Keep `secret`, replacing any before.
pub fn set_secret(service: &str, account: &str, secret: &str) -> Result<()> {
    with_service(async |keyring| {
        let collection = keyring
            .get_default_collection()
            .await
            .context("the keyring has no default collection")?;
        collection
            .ensure_unlocked()
            .await
            .context("the keyring stayed locked")?;
        collection
            .create_item(
                &format!("Isocast: {service} ({account})"),
                attributes(service, account),
                secret.as_bytes(),
                true,
                "text/plain",
            )
            .await?;
        Ok(())
    })
}

/// Remove the secret. Returns whether there was one.
pub fn delete_secret(service: &str, account: &str) -> Result<bool> {
    with_service(async |keyring| {
        let found = keyring.search_items(attributes(service, account)).await?;
        let mut deleted = false;
        for item in found.unlocked.iter().chain(&found.locked) {
            item.delete().await?;
            deleted = true;
        }
        Ok(deleted)
    })
}
