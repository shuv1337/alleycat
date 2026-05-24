use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{0} lacks tokens.access_token or tokens.account_id")]
    MissingTokenFields(PathBuf),
}

#[derive(Debug, Deserialize)]
struct AuthFile {
    #[serde(default)]
    tokens: AuthTokens,
}

#[derive(Debug, Default, Deserialize)]
struct AuthTokens {
    access_token: Option<String>,
    account_id: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthRefreshResponse {
    pub access_token: String,
    pub chatgpt_account_id: String,
    pub chatgpt_plan_type: Option<String>,
}

impl std::fmt::Debug for AuthRefreshResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRefreshResponse")
            .field("access_token", &"<redacted>")
            .field("chatgpt_account_id", &self.chatgpt_account_id)
            .field("chatgpt_plan_type", &self.chatgpt_plan_type)
            .finish()
    }
}

pub(crate) async fn read_auth_refresh_response(
    path: &Path,
) -> Result<AuthRefreshResponse, AuthError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|source| AuthError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    let auth: AuthFile = serde_json::from_slice(&bytes).map_err(|source| AuthError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    let Some(access_token) = auth.tokens.access_token else {
        return Err(AuthError::MissingTokenFields(path.to_path_buf()));
    };
    let Some(chatgpt_account_id) = auth.tokens.account_id else {
        return Err(AuthError::MissingTokenFields(path.to_path_buf()));
    };
    Ok(AuthRefreshResponse {
        access_token,
        chatgpt_account_id,
        chatgpt_plan_type: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_chatgpt_auth_tokens_without_debug_leaking_access_token() {
        let temp = tempfile::tempdir().unwrap();
        let auth_path = temp.path().join("auth.json");
        tokio::fs::write(
            &auth_path,
            r#"{"tokens":{"access_token":"secret-token","account_id":"acct_123"}}"#,
        )
        .await
        .unwrap();

        let response = read_auth_refresh_response(&auth_path).await.unwrap();

        assert_eq!(response.chatgpt_account_id, "acct_123");
        assert_eq!(response.chatgpt_plan_type, None);
        let debug = format!("{response:?}");
        assert!(!debug.contains("secret-token"));
        assert!(debug.contains("<redacted>"));
    }

    #[tokio::test]
    async fn missing_auth_fields_are_errors_without_token_material() {
        let temp = tempfile::tempdir().unwrap();
        let auth_path = temp.path().join("auth.json");
        tokio::fs::write(&auth_path, r#"{"tokens":{"access_token":"secret-token"}}"#)
            .await
            .unwrap();

        let error = read_auth_refresh_response(&auth_path).await.unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("lacks tokens.access_token or tokens.account_id"));
        assert!(!rendered.contains("secret-token"));
    }
}
