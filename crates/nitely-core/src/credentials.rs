//! Read-only access to the Aera agent-work credentials file
//! (`%USERPROFILE%\.aera-work\credentials.json`, owned by `scripts/work/credentials.mjs`).
//! Nitely never writes it and never prints its values.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub bitbucket: Bitbucket,
    #[serde(default)]
    pub linear: Linear,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bitbucket {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub api_token: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Linear {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub team_key: String,
    #[serde(default)]
    pub project_id: String,
}

impl Bitbucket {
    pub fn usable(&self) -> bool {
        !self.email.is_empty() && !self.api_token.is_empty()
    }
}

impl Linear {
    pub fn usable(&self) -> bool {
        !self.api_key.is_empty() && !self.project_id.is_empty()
    }
}

/// The Authorization header git sends to bitbucket.org for Nitely's own pushes and fetches (Basic
/// auth with Bitbucket's fixed username for API tokens over Git; the account email works only for
/// the REST API). None without a token. It goes to git through its environment only, never on a
/// command line and never printed (`playbook::git_remote`).
pub fn git_auth_header(creds: &Credentials) -> Option<String> {
    use base64::Engine;
    let token = &creds.bitbucket.api_token;
    if token.is_empty() {
        return None;
    }
    let pair = format!("x-bitbucket-api-token-auth:{token}");
    Some(format!("Authorization: Basic {}", base64::engine::general_purpose::STANDARD.encode(pair)))
}

/// `NITELY_CREDENTIALS` overrides the path (tests, or a second account).
pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("NITELY_CREDENTIALS") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".aera-work")
        .join("credentials.json")
}

/// `Ok(None)` when the file does not exist: Nitely then shows git and agent state only.
pub fn load() -> Result<Option<Credentials>> {
    let path = path();
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    // Not `with_context` on the parse error: serde's message can quote the offending value.
    let creds = serde_json::from_str(&text)
        .map_err(|_| anyhow::anyhow!("{} is not valid JSON", path.display()))?;
    Ok(Some(creds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_auth_header_is_basic_with_the_token_username() {
        use base64::Engine;
        assert_eq!(git_auth_header(&Credentials::default()), None);
        let mut creds = Credentials::default();
        creds.bitbucket.api_token = "tok".into();
        let header = git_auth_header(&creds).unwrap();
        let encoded = header.strip_prefix("Authorization: Basic ").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
        assert_eq!(decoded, b"x-bitbucket-api-token-auth:tok");
    }
}
