use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, Response},
    routing::{delete, get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, net::IpAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::RwLock;

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn yes() -> bool {
    true
}

// ---------- Modelos ----------

/// Un "servicio" = un juego de credenciales de Cloudflare (token + zona + registros).
#[derive(Serialize, Deserialize, Clone)]
struct Service {
    name: String,
    api_token: String,
    zone_id: String,
    record_ids: Vec<String>,
    #[serde(default = "yes")]
    enabled: bool,
}

#[derive(Deserialize)]
struct ServiceInput {
    name: String,
    #[serde(default)]
    api_token: String, // vacío al editar = conservar el token guardado
    zone_id: String,
    record_ids: Vec<String>,
    #[serde(default = "yes")]
    enabled: bool,
}

#[derive(Serialize)]
struct ServiceView {
    name: String,
    token_hint: String,
    zone_id: String,
    record_ids: Vec<String>,
    enabled: bool,
    status: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct DnsRecord {
    id: String,
    #[serde(rename = "type")]
    record_type: String,
    name: String,
    content: String,
    proxied: bool,
}

#[derive(Deserialize)]
struct CfResponse {
    success: bool,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
    result: Option<DnsRecord>,
}

impl CfResponse {
    fn into_record(self) -> Res<DnsRecord> {
        if !self.success {
            return Err(format!("Cloudflare: {:?}", self.errors).into());
        }
        self.result.ok_or_else(|| "Cloudflare: respuesta sin resultado".into())
    }
}

struct AppState {
    services: RwLock<Vec<Service>>,
    status: RwLock<HashMap<String, String>>,
    last_ip: RwLock<Option<String>>,
    path: PathBuf,
    client: reqwest::Client,
    /// "usuario:contraseña" para Basic Auth (None = sin autenticación, solo válido en localhost)
    auth: Option<String>,
}

// ---------- Config en disco ----------

async fn load_config(path: &PathBuf) -> Vec<Service> {
    if let Ok(text) = tokio::fs::read_to_string(path).await {
        if let Ok(list) = serde_json::from_str::<Vec<Service>>(&text) {
            return list;
        }
        eprintln!("⚠️ {} no es válido, empezando vacío", path.display());
        return vec![];
    }

    // Migración: si no hay config.json pero sí .env antiguo, crea un servicio "default"
    if let (Ok(api_token), Ok(zone_id), Ok(ids)) = (
        env::var("CLOUDFLARE_API_TOKEN"),
        env::var("ZONE_ID"),
        env::var("RECORD_IDS"),
    ) {
        let svc = Service {
            name: "default".into(),
            api_token,
            zone_id,
            record_ids: ids.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            enabled: true,
        };
        let list = vec![svc];
        let _ = save_config(path, &list).await;
        println!("ℹ️ Migrado .env -> {}", path.display());
        return list;
    }
    vec![]
}

async fn save_config(path: &PathBuf, list: &[Service]) -> Res<()> {
    let json = serde_json::to_string_pretty(list)?;
    tokio::fs::write(path, json).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(())
}

// ---------- Lógica DDNS ----------

async fn get_public_ip(client: &reqwest::Client) -> Res<String> {
    let providers = [
        "https://checkip.amazonaws.com",
        "https://api.ipify.org",
        "https://domains.google.com/checkip",
    ];
    for url in providers {
        if let Ok(res) = client.get(url).send().await {
            if let Ok(text) = res.text().await {
                let ip = text.trim().to_string();
                if ip.parse::<IpAddr>().is_ok() {
                    return Ok(ip);
                }
            }
        }
    }
    Err("No se pudo obtener la IP pública".into())
}

async fn update_service(client: &reqwest::Client, svc: &Service, ip: &str) -> Res<String> {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", svc.api_token))?);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let (mut updated, mut same, mut skipped) = (0, 0, 0);

    for id in &svc.record_ids {
        let url = format!(
            "https://api.cloudflare.com/client/v4/zones/{}/dns_records/{}",
            svc.zone_id, id
        );
        let record = client
            .get(&url)
            .headers(headers.clone())
            .send()
            .await?
            .json::<CfResponse>()
            .await?
            .into_record()?;

        // La IP pública obtenida es IPv4: solo tocamos registros A
        if record.record_type != "A" {
            skipped += 1;
            continue;
        }
        if record.content.trim() == ip {
            same += 1;
            continue;
        }

        let body = serde_json::json!({
            "type": record.record_type,
            "name": record.name,
            "content": ip,
            "ttl": if record.proxied { 1 } else { 120 },
            "proxied": record.proxied
        });
        client
            .put(&url)
            .headers(headers.clone())
            .json(&body)
            .send()
            .await?
            .json::<CfResponse>()
            .await?
            .into_record()?;
        updated += 1;
    }

    Ok(format!("✅ {updated} actualizados, {same} sin cambios, {skipped} omitidos (no A)"))
}

async fn run_all(state: &AppState) -> Res<String> {
    let ip = get_public_ip(&state.client).await?;
    let services = state.services.read().await.clone();

    for svc in services.iter().filter(|s| s.enabled) {
        let msg = match update_service(&state.client, svc, &ip).await {
            Ok(m) => m,
            Err(e) => format!("❌ {e}"),
        };
        println!("[{}] {} (IP {})", svc.name, msg, ip);
        state.status.write().await.insert(svc.name.clone(), msg);
    }
    *state.last_ip.write().await = Some(ip.clone());
    Ok(ip)
}

// ---------- Autenticación (HTTP Basic) ----------

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth_mw(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some(creds) = &s.auth {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|b| STANDARD.decode(b.trim()).ok())
            .map(|d| ct_eq(&d, creds.as_bytes()))
            .unwrap_or(false);
        if !ok {
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header(header::WWW_AUTHENTICATE, "Basic realm=\"cf-ddns\"")
                .body(Body::empty())
                .unwrap();
        }
    }
    next.run(req).await
}

// ---------- API local ----------

type ApiErr = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> ApiErr {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn list_services(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let status = s.status.read().await;
    let views: Vec<ServiceView> = s
        .services
        .read()
        .await
        .iter()
        .map(|x| ServiceView {
            name: x.name.clone(),
            token_hint: format!("••••{}", &x.api_token[x.api_token.len().saturating_sub(4)..]),
            zone_id: x.zone_id.clone(),
            record_ids: x.record_ids.clone(),
            enabled: x.enabled,
            status: status.get(&x.name).cloned().unwrap_or_else(|| "—".into()),
        })
        .collect();
    let ip = s.last_ip.read().await.clone();
    Json(serde_json::json!({ "services": views, "last_ip": ip }))
}

async fn upsert_service(
    State(s): State<Arc<AppState>>,
    Json(i): Json<ServiceInput>,
) -> Result<StatusCode, ApiErr> {
    let name = i.name.trim().to_string();
    let zone_id = i.zone_id.trim().to_string();
    let record_ids: Vec<String> = i
        .record_ids
        .iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();

    if name.is_empty() || zone_id.is_empty() || record_ids.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name, zone_id y record_ids son obligatorios".into()));
    }

    let mut list = s.services.write().await;
    let pos = list.iter().position(|x| x.name == name);

    let api_token = if i.api_token.trim().is_empty() {
        match pos {
            Some(p) => list[p].api_token.clone(),
            None => return Err((StatusCode::BAD_REQUEST, "api_token obligatorio".into())),
        }
    } else {
        i.api_token.trim().to_string()
    };

    let svc = Service { name, api_token, zone_id, record_ids, enabled: i.enabled };
    match pos {
        Some(p) => list[p] = svc,
        None => list.push(svc),
    }
    save_config(&s.path, &list).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_service(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiErr> {
    let mut list = s.services.write().await;
    list.retain(|x| x.name != name);
    s.status.write().await.remove(&name);
    save_config(&s.path, &list).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn run_now(State(s): State<Arc<AppState>>) -> Result<StatusCode, ApiErr> {
    run_all(&s).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- main ----------

#[tokio::main]
async fn main() -> Res<()> {
    dotenv::dotenv().ok();

    let path = PathBuf::from(env::var("CONFIG_PATH").unwrap_or_else(|_| "config.json".into()));
    let interval: u64 = env::var("INTERVAL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let bind = env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());

    let auth = env::var("ADMIN_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty())
        .map(|p| format!("{}:{}", env::var("ADMIN_USER").unwrap_or_else(|_| "admin".into()), p));
    let is_local = bind.starts_with("127.") || bind.starts_with("localhost") || bind.starts_with("[::1]");
    if !is_local && auth.is_none() {
        return Err("BIND no es localhost: define ADMIN_PASSWORD (y opcionalmente ADMIN_USER) en el .env".into());
    }

    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
    let state = Arc::new(AppState {
        services: RwLock::new(load_config(&path).await),
        status: RwLock::new(HashMap::new()),
        last_ip: RwLock::new(None),
        path,
        client,
        auth,
    });

    // Bucle de actualización en segundo plano
    let bg = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_all(&bg).await {
                eprintln!("❌ {e}");
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/services", get(list_services).post(upsert_service))
        .route("/api/services/:name", delete(delete_service))
        .route("/api/run", post(run_now))
        .layer(middleware::from_fn_with_state(state.clone(), auth_mw))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("🌐 Panel en http://{bind}  (cada {interval}s)");
    axum::serve(listener, app).await?;
    Ok(())
}
