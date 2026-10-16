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

/// Set by Nitely's own git calls (`playbook::git_remote`) to the grant it also passes the helper,
/// so `nitely git-credential` answers git during those calls and nobody else.
pub const GRANT_ENV: &str = "NITELY_CREDENTIAL_GRANT";

/// Whether the helper was started by one of Nitely's own git calls: its environment carries the
/// same grant as its `--grant` argument. This keeps the token out of anyone's output who runs the
/// helper by hand; a process that can read the credentials file could of course read it directly.
pub fn granted(arg: Option<&str>, env: Option<&str>) -> bool {
    matches!((arg, env), (Some(a), Some(e)) if a.len() >= 32 && a == e)
}

/// What `nitely git-credential get` tells git for a request (git's credential protocol: `key=value`
/// lines). Only an HTTPS request for bitbucket.org is answered, with the API token under Bitbucket's
/// fixed username for tokens over Git (the account email works only for the REST API).
pub fn git_answer(request: &str, creds: &Credentials) -> Option<String> {
    let field = |k: &str| request.lines().find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix('=')));
    let host = field("host")?;
    let https = field("protocol").is_none_or(|p| p == "https");
    if !https || !host.eq_ignore_ascii_case("bitbucket.org") || creds.bitbucket.api_token.is_empty() {
        return None;
    }
    Some(format!("username=x-bitbucket-api-token-auth\npassword={}\n", creds.bitbucket.api_token))
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
    fn answers_git_only_for_bitbucket_over_https() {
        let mut creds = Credentials::default();
        creds.bitbucket.api_token = "tok".into();
        let ask = |req: &str| git_answer(req, &creds);
        assert_eq!(ask("protocol=https\nhost=bitbucket.org\n").as_deref(), Some("username=x-bitbucket-api-token-auth\npassword=tok\n"));
        assert_eq!(ask("host=Bitbucket.org").as_deref(), Some("username=x-bitbucket-api-token-auth\npassword=tok\n"));
        assert_eq!(ask("protocol=https\nhost=github.com\n"), None);
        assert_eq!(ask("protocol=http\nhost=bitbucket.org\n"), None);
        assert_eq!(ask("protocol=https\nhost=bitbucket.org.evil.com\n"), None);
        assert_eq!(git_answer("protocol=https\nhost=bitbucket.org\n", &Credentials::default()), None);
    }

    #[test]
    fn only_a_matching_grant_lets_the_helper_answer() {
        let g = "0123456789abcdef0123456789abcdef";
        assert!(granted(Some(g), Some(g)));
        assert!(!granted(None, Some(g)));
        assert!(!granted(Some(g), None));
        assert!(!granted(Some(g), Some("0123456789abcdef0123456789abcdee")));
        assert!(!granted(Some("x"), Some("x")), "too short to be a grant");
    }
}
