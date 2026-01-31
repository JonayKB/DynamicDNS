use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Deserialize, Debug)]
struct CloudflareResponse {
    result: DnsRecord,
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

async fn get_public_ip() -> Result<String, Box<dyn std::error::Error>> {
    let providers = [
        ("Amazon", "https://checkip.amazonaws.com"),
        ("Ipify", "https://api.ipify.org"),
        ("Google", "https://domains.google.com/checkip"),
    ];

    for (_provider, url) in providers {
        if let Ok(res) = reqwest::get(url).await {
            if let Ok(text) = res.text().await {
                return Ok(text.trim().to_string());
            }
        }
    }
    Err("Cannot retrieve public IP".into())
}

async fn get_dns_record(client: &reqwest::Client, zone_id: &str, record_id: &str, headers: &HeaderMap) -> Result<DnsRecord, Box<dyn std::error::Error>> {
    let url = format!("https://api.cloudflare.com/client/v4/zones/{}/dns_records/{}", zone_id, record_id);
    let res = client.get(url).headers(headers.clone()).send().await?.json::<CloudflareResponse>().await?;
    Ok(res.result)
}

async fn update_dns(client: &reqwest::Client, zone_id: &str, ip: &str, record: DnsRecord, headers: &HeaderMap) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("https://api.cloudflare.com/client/v4/zones/{}/dns_records/{}", zone_id, record.id);
    
    let body = serde_json::json!({
        "type": record.record_type,
        "name": record.name,
        "content": ip,
        "ttl": 120,
        "proxied": record.proxied
    });

    client.put(url).headers(headers.clone()).json(&body).send().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();

    let api_token = env::var("CLOUDFLARE_API_TOKEN")?;
    let zone_id = env::var("ZONE_ID")?;
    let record_ids: Vec<String> = env::var("RECORD_IDS")?.split(',').map(|s| s.to_string()).collect();

    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", api_token))?);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let client = reqwest::Client::new();
    let public_ip = get_public_ip().await?;


    let first_record = get_dns_record(&client, &zone_id, &record_ids[0], &headers).await?;

    if first_record.content.trim() != public_ip {
        println!("⚠️ IP has changed, updating DNS...");
        
        for id in record_ids {
            let record = get_dns_record(&client, &zone_id, &id, &headers).await?;
            update_dns(&client, &zone_id, &public_ip, record, &headers).await?;
        }
        println!("✅ DNS updated!");
    } else {
        println!("ℹ️ IP is still the same. No update needed.");
    }

    Ok(())
}