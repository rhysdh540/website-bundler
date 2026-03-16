use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use anyhow::{anyhow, bail, Result};
use ::base64::Engine;
use axum::http;
use base64::engine::general_purpose as base64;
use serde_json::Value as Json;

static API_URL_BASE: LazyLock<String> = LazyLock::new(|| {
    std::env::var("CF_API_URL_BASE")
        .unwrap_or_else(|_| "https://api.cloudflare.com/client/v4/".to_string())
});

const USER_AGENT: &str = "wrangler/4.73.0"; // totally!

struct Asset {
    path: PathBuf,
    name: String, // path relative to upload root
    ctype: String,
    hash: String,
}

pub async fn deploy(
    directory: PathBuf,
    project_name: &String,
    account_id: &String,
    api_token: &String,
) -> Result<()> {
    if !directory.is_dir() {
        return Err(anyhow!(format!("path {} is not a directory", directory.display())));
    }

    let start_time = std::time::Instant::now();
    let mut timer = std::time::Instant::now();
    let jwt = get_jwt(account_id, project_name, api_token).await?;
    println!("acquired upload token ({:?})", timer.elapsed());

    let max_file_count = jwt.1.get("max_file_count_allowed")
        .and_then(|v| v.as_u64()).map(|v| v as usize);

    timer = std::time::Instant::now();
    let files = gather(&directory, max_file_count)?;
    println!("found {} files ({:?})", files.len(), timer.elapsed());

    timer = std::time::Instant::now();
    let missing_count = upload_missing(&files, &jwt.0).await?;
    println!("uploaded {} missing files ({:?})", missing_count, timer.elapsed());

    let hashes = files.iter().map(|a| a.hash.clone()).collect::<Vec<_>>();
    if let Err(e) = upsert_hashes(&hashes, &jwt.0).await {
        eprintln!("warning: upsert-hashes failed: {}", e);
    }

    let deployment = create_deployment(
        &files,
        account_id,
        project_name,
        api_token,
    ).await?;

    let deployment_id = deployment.get("id").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("deployment response missing id"))?;
    let deployment_url = deployment.get("url").and_then(|v| v.as_str()).unwrap_or("(unknown)");
    println!("deployment {} created: {} (total {:?})", deployment_id, deployment_url, start_time.elapsed());

    Ok(())
}

async fn get_jwt(
    account_id: &String,
    project_name: &String,
    api_token: &String,
) -> Result<(String, Json)> {
    let jwt = make_request(
        http::Method::GET,
        &format!("accounts/{}/pages/projects/{}/upload-token", account_id, project_name),
        api_token,
        None,
        None,
    ).await?
        .ok_or_else(|| anyhow!("no result returned from upload-token"))?
        .get("jwt").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("cf api response doesn't have jwt"))?.to_string();

    let payload = jwt.split('.').nth(1).ok_or_else(|| anyhow!("invalid jwt"))?;
    let decoded = base64::STANDARD_NO_PAD.decode(payload)?;
    Ok((jwt, serde_json::from_slice(&decoded)?))
}

fn gather(directory: &PathBuf, max_file_count: Option<usize>) -> Result<Vec<Asset>> {
    let mut assets = Vec::new();
    for entry in walkdir::WalkDir::new(directory) {
        let entry = entry?;
        let path = entry.path().to_path_buf();

        if path.is_dir() || should_ignore(&path, directory) {
            continue;
        }

        assets.push(Asset {
            path: path.clone(),
            name: path.strip_prefix(directory)?.to_str().ok_or_else(|| anyhow!("invalid path"))?.replace("\\", "/"),
            ctype: mime_guess::from_path(&path).first_or_octet_stream().to_string(),
            hash: hash(&path)?,
        });
    }

    if let Some(max) = max_file_count && assets.len() > max {
        return Err(anyhow!(format!("too many files: {} (max {})", assets.len(), max)));
    }

    Ok(assets)
}

async fn upload_missing(assets: &Vec<Asset>, jwt: &String) -> Result<usize> {
    let hashes = assets.iter().map(|a| a.hash.clone()).collect::<Vec<_>>();
    let response = make_request(
        http::Method::POST,
        "pages/assets/check-missing",
        jwt,
        Some(serde_json::json!({ "hashes": hashes }).to_string()),
        Some(vec![(http::header::CONTENT_TYPE, "application/json")]),
    ).await?.ok_or_else(|| anyhow!("no result returned from check-missing"))?;

    let missing = response.as_array().ok_or_else(|| anyhow!("response from check-missing not array"))?
        .iter().map(ToString::to_string).collect::<Vec<_>>();

    let mut payload: Vec<Json> = Vec::new();
    let mut n = 0;
    for asset in assets {
        if !missing.contains(&asset.hash) {
            continue;
        }
        payload.push(serde_json::json!({
            "key": asset.hash,
            "value": base64::STANDARD.encode(std::fs::read(&asset.path)?),
            "metadata": {
                "contentType": asset.ctype,
            },
            "base64": true,
        }));
        n += 1;
    }

    make_request(
        http::Method::POST,
        "pages/assets/upload",
        jwt,
        Some(serde_json::to_string(&payload)?),
        Some(vec![(http::header::CONTENT_TYPE, "application/json")]),
    ).await?;

    Ok(n)
}

fn should_ignore(path: &Path, root: &PathBuf) -> bool {
    if path == root {
        return false;
    }
    let file_name = path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if path.parent().is_some_and(|p| p == root) {
        // at the root level: ignore _* files (except _headers and _redirects which are uploaded separately)
        if file_name.starts_with('_') && file_name != "_headers" && file_name != "_redirects" {
            return true;
        }
    }
    if file_name.starts_with('.') {
        return true;
    }
    path.parent().is_some_and(|p| should_ignore(p, root))
}

fn hash(path: &PathBuf) -> Result<String> {
    let data = std::fs::read(path)?;
    let b64 = base64::STANDARD.encode(data);
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let hash = blake3::hash(format!("{}{}", b64, ext).as_bytes());
    Ok(hash.to_hex()[..32].to_string())
}

async fn upsert_hashes(hashes: &Vec<String>, jwt: &String) -> Result<()> {
    make_request(
        http::Method::POST,
        "pages/assets/upsert-hashes",
        jwt,
        Some(serde_json::json!({ "hashes": hashes }).to_string()),
        Some(vec![(http::header::CONTENT_TYPE, "application/json")]),
    ).await?;
    Ok(())
}

async fn create_deployment(
    assets: &Vec<Asset>,
    account_id: &String,
    project_name: &String,
    api_token: &String,
) -> Result<Json> {
    let manifest: HashMap<String, String> = assets.iter()
        .map(|a| (format!("/{}", a.name), a.hash.clone()))
        .collect();
    let manifest_json = serde_json::to_string(&manifest)?;

    let client = reqwest::Client::new();
    let url = format!("{}/accounts/{}/pages/projects/{}/deployments", *API_URL_BASE, account_id, project_name);
    let response = client.post(url)
        .header(http::header::AUTHORIZATION, format!("Bearer {}", api_token))
        .header(http::header::USER_AGENT, USER_AGENT)
        .multipart(reqwest::multipart::Form::new()
            .text("manifest", manifest_json))
        .send().await?;

    let json = serde_json::from_slice::<Json>(response.bytes().await?.as_ref())?;
    if json.get("success").and_then(|v| v.as_bool()) != Some(true) {
        let errors = json.get("errors").and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("no success and no errors"))?;
        let mut msg = "create deployment failed:".to_string();
        for error in errors {
            msg.push_str("\n- ");
            if let Some(code) = error.get("code").and_then(|v| v.as_u64()) {
                msg.push_str(&code.to_string());
                msg.push_str(": ");
            }
            if let Some(message) = error.get("message").and_then(|v| v.as_str()) {
                msg.push_str(message);
            }
        }
        bail!(msg);
    }

    Ok(json.get("result").ok_or_else(|| anyhow!("no result"))?.clone())
}

async fn make_request(
    method: http::Method,
    path: &str,
    token: &String,
    body: Option<String>,
    headers: Option<Vec<(http::header::HeaderName, &str)>>,
) -> Result<Option<Json>> {
    let client = reqwest::Client::new();
    let url = format!("{}/{}", *API_URL_BASE, path);
    let mut request = client.request(method, url)
        .header(http::header::AUTHORIZATION, format!("Bearer {}", token))
        .header(http::header::USER_AGENT, USER_AGENT); // totally!
    if let Some(headers) = headers {
        for (name, value) in headers {
            request = request.header(name, value);
        }
    }
    if let Some(body) = body {
        request = request.body(body);
    }

    let response = request.send().await?;
    let json = serde_json::from_slice::<Json>(response.bytes().await?.as_ref())?;

    if json.get("success").and_then(|v| v.as_bool()) != Some(true) {
        let errors = json.get("errors").and_then(|v| v.as_array()).ok_or_else(|| anyhow!("no success and no errors"))?;
        let mut msg = format!("request to {} failed:", path);
        for error in errors {
            msg.push_str("\n- ");
            if let Some(code) = error.get("code").and_then(|v| v.as_u64()) {
                msg.push_str(code.to_string().as_str());
                msg.push_str(": ");
            }
            if let Some(message) = error.get("message").and_then(|v| v.as_str()) {
                msg.push_str(message);
            }
        }
        msg.push_str("\n");
        bail!(msg);
    }

    Ok(json.get("result").cloned())
}