use serde_json::json;
use std::{borrow::Borrow, collections::HashMap, sync::Arc, time::Instant};

use anyhow::{anyhow, Context as AnyhowContext, Error, Result};
use async_stream::{stream, try_stream};
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset};
use futures::{future::join_all, Stream};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use lazy_static::lazy_static;
use log::{debug, error, info};

use super::{PullLogs, PullLogsContext};
use shared::JsonValueExt;

#[derive(Clone)]
pub struct GoogleWorkspacePuller;

struct GoogResourceProps {
    resource: String,
    /// Google workspace events can be lagged from minutes to hours to days. See https://support.google.com/a/answer/7061566
    lag: chrono::Duration,
}

fn table_resource_map() -> HashMap<String, GoogResourceProps> {
    [(
        "login",
        GoogResourceProps {
            resource: "login".to_string(),
            lag: chrono::Duration::hours(2),
        },
    )]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect::<HashMap<_, _>>()
}

lazy_static! {
    static ref TABLE_RESOURCE_MAP: HashMap<String, GoogResourceProps> = table_resource_map();
}

#[async_trait]
impl PullLogs for GoogleWorkspacePuller {
    async fn pull_logs(
        self,
        client: reqwest::Client,
        ctx: &PullLogsContext,
        start_dt: DateTime<FixedOffset>,
        end_dt: DateTime<FixedOffset>,
    ) -> Result<Vec<u8>> {
        info!("Pulling Google Workspace logs....");

        let config = ctx.config();
        let cache = ctx.cache();

        let checkpoint_json = ctx.checkpoint_json.lock().await;
        let is_initial_run = checkpoint_json.is_none();

        let client_email = config.get("client_email").context("Missing client_email")?;
        let admin_email = config.get("admin_email").context("Missing admin_email")?;
        let private_key = ctx
            .get_secret_field("private_key")
            .await
            .map_err(|e| {
                error!("Error getting private key: {}", e);
                e
            })?
            .context("Missing private key")?;

        // skip early if private_key is equal <placeholder>
        if private_key == "<placeholder>" {
            return Ok(vec![]);
        }

        let access_token = {
            let mut cache = cache.lock().await;
            match cache.get("access_token") {
                Some(token) => token.to_owned(),
                None => {
                    let token =
                        get_access_token(client.clone(), client_email, admin_email, &private_key)
                            .await?;
                    cache.set("access_token", token.clone(), None);
                    token
                }
            }
        };

        let start_dt = if is_initial_run {
            start_dt - chrono::Duration::days(14)
        } else {
            start_dt
        };

        let end_time = end_dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let tables = ctx.tables_config.keys().collect::<Vec<_>>();
        if tables.is_empty() {
            return Ok(vec![]);
        }

        let table_resources = tables
            .into_iter()
            .filter_map(|t| Some((t, TABLE_RESOURCE_MAP.get(t)?)));

        let futs = table_resources
            .map(|(table, resource)| {
                let start_dt = start_dt - resource.lag;
                let start_time = start_dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();

                list_resource(
                    client.clone(),
                    &resource.resource,
                    table,
                    &access_token,
                    start_time,
                    &end_time,
                )
            })
            .collect::<Vec<_>>();

        let results = join_all(futs)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        Ok(results)
    }
}

async fn get_access_token(
    client: reqwest::Client,
    client_email: &str,
    admin_email: &str,
    private_key: &str,
) -> Result<String> {
    let now = chrono::Utc::now().timestamp();

    let claims = json!({
      "iss": client_email,
      "scope": "https://www.googleapis.com/auth/admin.reports.audit.readonly",
      "aud": "https://oauth2.googleapis.com/token",
      "iat": now,
      "exp": now + 3600,
      "sub": admin_email
    });
    let key = EncodingKey::from_rsa_pem(private_key.as_bytes())?;
    let token = encode(&Header::new(Algorithm::RS256), &claims, &key)?;

    let access_token = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &token),
        ])
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?
        .get_mut("access_token")
        .and_then(|v| v.take().into_str())
        .context("Missing access token")?;

    Ok(access_token)
}

async fn list_resource(
    client: reqwest::Client,
    resource: &str,
    table: &str,
    access_token: &str,
    start_time: String,
    end_time: &str,
) -> Result<Vec<u8>> {
    let url = format!(
        "https://admin.googleapis.com/admin/reports/v1/activity/users/all/applications/{}",
        resource
    );
    let mut first = true;
    let mut next_token: Option<String> = None;

    let mut ret: Vec<u8> = vec![];

    while first || next_token.is_some() {
        let mut qs = vec![
            ("maxResults", "1000"),
            ("prettyPrint", "false"),
            ("startTime", &start_time),
            ("endTime", end_time),
        ];
        if let Some(token) = next_token.as_ref() {
            qs.push(("pageToken", token));
        }
        let res = client
            .get(&url)
            .bearer_auth(access_token)
            .query(&qs)
            .send()
            .await?;

        let is_failure = !res.status().is_success();
        if is_failure {
            let body = res.text().await?;
            let msg = format!("Error calling url: {}, response: {}", &url, body);
            return Err(anyhow!(msg));
        }

        let mut body = res.json::<serde_json::Value>().await?;
        let items = body
            .get_mut("items")
            .and_then(|v| v.take().into_array())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.into_object())
            .peekable();

        for mut item in items {
            item.insert("_table".to_string(), table.into());
            ret.extend(serde_json::to_vec(&item)?);
            ret.push(b'\n');
        }

        next_token = body
            .get_mut("nextPageToken")
            .and_then(|v| v.take().into_str());
        first = false;
    }

    Ok(ret)
}
