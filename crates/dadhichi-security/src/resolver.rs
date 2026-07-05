//! Resolving `${...}` secret references for connectors and providers.
//!
//! A reference is `scheme:name`:
//!
//! - `env:NAME` reads the process environment,
//! - `vault:NAME` decrypts `NAME` from an unlocked [`Vault`],
//! - a bare `NAME` (no scheme) is treated as `env:NAME`.
//!
//! Anything missing, empty, or undecryptable resolves to `None`, so a caller can
//! refuse to launch the dependent server rather than proceed with a blank
//! credential — and a secret value is never surfaced in an error string.

use crate::vault::Vault;

/// Resolves secret references against the environment and an optional unlocked
/// credential [`Vault`].
#[derive(Debug, Default)]
pub struct SecretResolver {
    vault: Option<Vault>,
}

impl SecretResolver {
    /// A resolver with no vault — only `env:` references resolve.
    pub fn new() -> Self {
        Self::default()
    }

    /// A resolver backed by an unlocked `vault`, so `vault:` references resolve.
    pub fn with_vault(vault: Vault) -> Self {
        Self { vault: Some(vault) }
    }

    /// Whether a vault is available for `vault:` references.
    pub fn has_vault(&self) -> bool {
        self.vault.is_some()
    }

    /// Resolve `reference` to its secret value, or `None` if it is unavailable.
    ///
    /// A wrong vault passphrase (or tampered entry) surfaces here as `None`, not
    /// an error, so the caller treats it exactly like a missing secret.
    pub fn resolve(&self, reference: &str) -> Option<String> {
        let (scheme, name) = reference.split_once(':').unwrap_or(("env", reference));
        match scheme {
            "env" => std::env::var(name).ok().filter(|v| !v.is_empty()),
            "vault" => self.vault.as_ref()?.get(name).ok().flatten(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault_with(name: &str, secret: &str) -> Vault {
        let mut vault = Vault::new("test-passphrase");
        vault.put(name, secret).unwrap();
        vault
    }

    #[test]
    fn resolves_a_vault_reference() {
        let resolver = SecretResolver::with_vault(vault_with("github", "ghp_token"));
        assert!(resolver.has_vault());
        assert_eq!(
            resolver.resolve("vault:github").as_deref(),
            Some("ghp_token")
        );
    }

    #[test]
    fn missing_vault_entry_is_none() {
        let resolver = SecretResolver::with_vault(vault_with("github", "x"));
        assert_eq!(resolver.resolve("vault:absent"), None);
    }

    #[test]
    fn vault_reference_without_a_vault_is_none() {
        let resolver = SecretResolver::new();
        assert!(!resolver.has_vault());
        assert_eq!(resolver.resolve("vault:github"), None);
    }

    #[test]
    fn wrong_passphrase_resolves_to_none_not_error() {
        // Persist a vault under one passphrase, reopen under another.
        let mut sealed = Vault::new("right");
        sealed.put("api", "value").unwrap();
        let reopened = Vault::with_data("wrong", sealed.data().clone());
        let resolver = SecretResolver::with_vault(reopened);
        assert_eq!(resolver.resolve("vault:api"), None);
    }

    #[test]
    fn unknown_scheme_is_none() {
        let resolver = SecretResolver::with_vault(vault_with("k", "v"));
        assert_eq!(resolver.resolve("weird:k"), None);
    }

    #[test]
    fn absent_env_var_is_none() {
        let resolver = SecretResolver::new();
        // A name overwhelmingly unlikely to exist in the environment.
        assert_eq!(
            resolver.resolve("env:DADHICHI_DEFINITELY_UNSET_VAR_9x7q"),
            None
        );
    }
}
