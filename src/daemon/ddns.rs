//! Dynamic DNS (4.9): runod keeps the AAAA record of the device's own
//! domain on its current global IPv6 address, through the DNS provider's
//! API — Cloudflare, Alibaba Cloud DNS or Tencent DNSPod. The settings,
//! with the provider's credentials, are in `<state>/runod/ddns.json`,
//! readable only by runod's user.

use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{digest, hmac};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const CONFIG: &str = "ddns.json";
const STATE: &str = "ddns-state.json";
/// How often runod looks at the address.
pub const EVERY: Duration = Duration::from_secs(60);
/// After a failed update, the next try waits this long.
const RETRY_SECS: i64 = 5 * 60;
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Cloudflare,
    Aliyun,
    Dnspod,
}

impl Provider {
    pub const NAMES: &[&str] = &["cloudflare", "aliyun", "dnspod"];

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.trim().to_lowercase().as_str() {
            "cloudflare" => Provider::Cloudflare,
            "aliyun" | "alidns" | "alibaba" => Provider::Aliyun,
            "dnspod" | "tencent" => Provider::Dnspod,
            _ => return None,
        })
    }

    /// What its credentials are called, in the order they are asked for.
    pub fn secrets(self) -> &'static [&'static str] {
        match self {
            Provider::Cloudflare => &["API token (Zone.DNS edit)"],
            Provider::Aliyun => &["AccessKey ID", "AccessKey secret"],
            Provider::Dnspod => &["SecretId", "SecretKey"],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub provider: Provider,
    /// The device's name, e.g. `pi.example.com`.
    pub domain: String,
    /// The registered domain holding it, e.g. `example.com`.
    pub zone: String,
    /// The credentials, in [`Provider::secrets`] order.
    pub secrets: Vec<String>,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
}

fn default_ttl() -> u32 {
    600
}

impl Config {
    pub fn new(
        provider: Provider,
        domain: &str,
        zone: Option<&str>,
        secrets: Vec<String>,
    ) -> Result<Self> {
        let domain = domain.trim().trim_end_matches('.').to_lowercase();
        let labels: Vec<&str> = domain.split('.').collect();
        if labels.len() < 2
            || labels.iter().any(|l| {
                l.is_empty()
                    || l.len() > 63
                    || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
        {
            bail!("{domain:?} is not a domain name");
        }
        let zone = match zone {
            Some(zone) => zone.trim().trim_end_matches('.').to_lowercase(),
            None => labels[labels.len() - 2..].join("."),
        };
        if domain != zone && !domain.ends_with(&format!(".{zone}")) {
            bail!("{domain} is not in {zone}");
        }
        if secrets.len() != provider.secrets().len() || secrets.iter().any(|s| s.trim().is_empty())
        {
            bail!("give {}", provider.secrets().join(" and "));
        }
        Ok(Config {
            provider,
            domain,
            zone,
            secrets: secrets.into_iter().map(|s| s.trim().to_owned()).collect(),
            ttl: default_ttl(),
        })
    }

    /// The name relative to the zone: `pi` for `pi.example.com`, `@` for the zone.
    pub fn host(&self) -> String {
        match self.domain.strip_suffix(&format!(".{}", self.zone)) {
            Some(host) => host.to_owned(),
            None => "@".to_owned(),
        }
    }
}

/// What the last updates did, for the status page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The address the record holds, as far as runod knows.
    #[serde(default)]
    pub address: Option<String>,
    /// Unix time of the last change of the record.
    #[serde(default)]
    pub updated: Option<i64>,
    /// Why the last try failed, if it did.
    #[serde(default)]
    pub error: Option<String>,
    /// Unix time of the last try.
    #[serde(default)]
    pub tried: Option<i64>,
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG)
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE)
}

pub fn load(dir: &Path) -> Option<Config> {
    let text = std::fs::read_to_string(config_path(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save(dir: &Path, config: &Config) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    runo_link::identity::write_private(&config_path(dir), &serde_json::to_vec_pretty(config)?)?;
    // A new setting starts afresh.
    let _ = std::fs::remove_file(state_path(dir));
    Ok(())
}

pub fn remove(dir: &Path) -> Result<bool> {
    let _ = std::fs::remove_file(state_path(dir));
    match std::fs::remove_file(config_path(dir)) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

pub fn status(dir: &Path) -> Status {
    std::fs::read_to_string(state_path(dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_status(dir: &Path, status: &Status) {
    if let Ok(json) = serde_json::to_vec_pretty(status) {
        let _ = runo_link::identity::write_private(&state_path(dir), &json);
    }
}

/// One line of /proc/net/if_inet6: an address others can reach the device
/// at for long — global scope, not temporary, deprecated, tentative or
/// unique-local.
fn public_address(line: &str) -> Option<(String, Ipv6Addr)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [hex, _, _, scope, flags, name] = fields.as_slice() else {
        return None;
    };
    let scope = u8::from_str_radix(scope, 16).ok()?;
    let flags = u32::from_str_radix(flags, 16).ok()?;
    // IFA_F_TEMPORARY, IFA_F_DADFAILED, IFA_F_DEPRECATED, IFA_F_TENTATIVE.
    if scope != 0 || flags & (0x01 | 0x08 | 0x20 | 0x40) != 0 || hex.len() != 32 {
        return None;
    }
    let bits = u128::from_str_radix(hex, 16).ok()?;
    let address = Ipv6Addr::from(bits);
    if address.segments()[0] & 0xfe00 == 0xfc00 || address.is_loopback() {
        return None;
    }
    Some((name.to_string(), address))
}

/// The device's public IPv6 address: wired before wireless.
pub fn current_address(if_inet6: &str) -> Option<Ipv6Addr> {
    let mut found: Vec<(String, Ipv6Addr)> = if_inet6.lines().filter_map(public_address).collect();
    found.sort_by_key(|(name, _)| !name.starts_with("eth") as u8);
    found.first().map(|(_, address)| *address)
}

pub fn live_address() -> Option<Ipv6Addr> {
    current_address(&std::fs::read_to_string("/proc/net/if_inet6").ok()?)
}

/// What one round of runod's loop did.
#[derive(Debug, PartialEq, Eq)]
pub enum Round {
    /// No settings, or no public IPv6 address.
    Idle,
    Unchanged,
    Updated(Ipv6Addr),
    /// Waiting after a failure.
    Waiting,
    Failed(String),
}

/// One round: updates the record when the address changed since the last
/// update, or when the last try failed long enough ago.
pub async fn round(dir: &Path, now: i64) -> Round {
    let Some(config) = load(dir) else {
        return Round::Idle;
    };
    let Some(address) = live_address() else {
        return Round::Idle;
    };
    let mut status = status(dir);
    let text = address.to_string();
    if status.address.as_deref() == Some(text.as_str()) && status.error.is_none() {
        return Round::Unchanged;
    }
    if status.error.is_some() && status.tried.is_some_and(|t| now - t < RETRY_SECS) {
        return Round::Waiting;
    }
    status.tried = Some(now);
    match update(&config, address).await {
        Ok(()) => {
            status.address = Some(text);
            status.updated = Some(now);
            status.error = None;
            save_status(dir, &status);
            Round::Updated(address)
        }
        Err(err) => {
            let why = format!("{err:#}");
            status.error = Some(why.clone());
            save_status(dir, &status);
            Round::Failed(why)
        }
    }
}

/// Remembers that the record now holds `address`.
pub fn record(dir: &Path, address: Ipv6Addr) {
    let now = chrono::Utc::now().timestamp();
    save_status(
        dir,
        &Status {
            address: Some(address.to_string()),
            updated: Some(now),
            error: None,
            tried: Some(now),
        },
    );
}

/// Points the record at `address` now, and remembers it.
pub async fn update_now(dir: &Path, config: &Config) -> Result<Ipv6Addr> {
    let address = live_address().context(
        "this device has no public IPv6 address: the network gives none, so the device can be reached only on the local network",
    )?;
    let now = chrono::Utc::now().timestamp();
    let result = update(config, address).await;
    let status = Status {
        address: result.is_ok().then(|| address.to_string()),
        updated: result.is_ok().then_some(now),
        error: result.as_ref().err().map(|e| format!("{e:#}")),
        tried: Some(now),
    };
    save_status(dir, &status);
    result.map(|()| address)
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(TIMEOUT).build()?)
}

/// Sets the AAAA record of `config.domain` to `address`.
pub async fn update(config: &Config, address: Ipv6Addr) -> Result<()> {
    let value = address.to_string();
    match config.provider {
        Provider::Cloudflare => cloudflare(config, &value).await,
        Provider::Aliyun => aliyun(config, &value).await,
        Provider::Dnspod => dnspod(config, &value).await,
    }
    .with_context(|| format!("updating {} at {:?}", config.domain, config.provider))
}

// Cloudflare: https://developers.cloudflare.com/api/resources/dns/

const CLOUDFLARE: &str = "https://api.cloudflare.com/client/v4";

async fn cloudflare_call(
    http: &reqwest::Client,
    token: &str,
    method: reqwest::Method,
    url: String,
    body: Option<Value>,
) -> Result<Value> {
    let mut request = http.request(method, url).bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let reply: Value = request.send().await?.json().await?;
    if reply["success"] != json!(true) {
        bail!("Cloudflare: {}", reply["errors"]);
    }
    Ok(reply["result"].clone())
}

async fn cloudflare(config: &Config, value: &str) -> Result<()> {
    let http = client()?;
    let token = &config.secrets[0];
    let zones = cloudflare_call(
        &http,
        token,
        reqwest::Method::GET,
        format!("{CLOUDFLARE}/zones?name={}", config.zone),
        None,
    )
    .await?;
    let zone = zones[0]["id"]
        .as_str()
        .ok_or_else(|| anyhow!("Cloudflare has no zone {} for this token", config.zone))?;
    let records = cloudflare_call(
        &http,
        token,
        reqwest::Method::GET,
        format!(
            "{CLOUDFLARE}/zones/{zone}/dns_records?type=AAAA&name={}",
            config.domain
        ),
        None,
    )
    .await?;
    let body = json!({
        "type": "AAAA",
        "name": config.domain,
        "content": value,
        "ttl": config.ttl,
        "proxied": false,
    });
    match records[0]["id"].as_str() {
        Some(_) if records[0]["content"].as_str() == Some(value) => Ok(()),
        Some(id) => cloudflare_call(
            &http,
            token,
            reqwest::Method::PUT,
            format!("{CLOUDFLARE}/zones/{zone}/dns_records/{id}"),
            Some(body),
        )
        .await
        .map(drop),
        None => cloudflare_call(
            &http,
            token,
            reqwest::Method::POST,
            format!("{CLOUDFLARE}/zones/{zone}/dns_records"),
            Some(body),
        )
        .await
        .map(drop),
    }
}

// Alibaba Cloud DNS, RPC style with signature version 1.0:
// https://help.aliyun.com/zh/dns/api-alidns-2015-01-09-overview

const ALIYUN: &str = "https://alidns.aliyuncs.com/";

/// RFC 3986 percent-encoding, as the signature wants it.
fn percent(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The signed query string of an Alibaba Cloud RPC call.
fn aliyun_query(params: &[(&str, String)], secret: &str) -> String {
    let mut sorted: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (percent(k), percent(v)))
        .collect();
    sorted.sort();
    let canonical = sorted
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let to_sign = format!("GET&{}&{}", percent("/"), percent(&canonical));
    let key = hmac::Key::new(
        hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        format!("{secret}&").as_bytes(),
    );
    let signature =
        base64::engine::general_purpose::STANDARD.encode(hmac::sign(&key, to_sign.as_bytes()));
    format!("{canonical}&Signature={}", percent(&signature))
}

async fn aliyun_call(
    http: &reqwest::Client,
    config: &Config,
    action: &str,
    extra: &[(&str, String)],
) -> Result<Value> {
    let mut nonce = [0u8; 16];
    aws_lc_rs::rand::fill(&mut nonce).map_err(|_| anyhow!("no random numbers"))?;
    let mut params: Vec<(&str, String)> = vec![
        ("Action", action.to_owned()),
        ("Format", "JSON".into()),
        ("Version", "2015-01-09".into()),
        ("AccessKeyId", config.secrets[0].clone()),
        ("SignatureMethod", "HMAC-SHA1".into()),
        ("SignatureVersion", "1.0".into()),
        ("SignatureNonce", runo_link::hex::encode(&nonce)),
        (
            "Timestamp",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        ),
    ];
    params.extend(extra.iter().cloned());
    let query = aliyun_query(&params, &config.secrets[1]);
    let reply: Value = http
        .get(format!("{ALIYUN}?{query}"))
        .send()
        .await?
        .json()
        .await?;
    if let Some(code) = reply["Code"].as_str() {
        bail!("Alibaba Cloud DNS: {code}: {}", reply["Message"]);
    }
    Ok(reply)
}

async fn aliyun(config: &Config, value: &str) -> Result<()> {
    let http = client()?;
    let found = aliyun_call(
        &http,
        config,
        "DescribeSubDomainRecords",
        &[
            ("SubDomain", config.domain.clone()),
            ("Type", "AAAA".into()),
        ],
    )
    .await?;
    let record = &found["DomainRecords"]["Record"][0];
    let common = [
        ("RR", config.host()),
        ("Type", "AAAA".to_owned()),
        ("Value", value.to_owned()),
        ("TTL", config.ttl.to_string()),
    ];
    match record["RecordId"].as_str() {
        // Updating to the same value is an error there.
        Some(_) if record["Value"].as_str() == Some(value) => Ok(()),
        Some(id) => {
            let mut params = vec![("RecordId", id.to_owned())];
            params.extend(common);
            aliyun_call(&http, config, "UpdateDomainRecord", &params)
                .await
                .map(drop)
        }
        None => {
            let mut params = vec![("DomainName", config.zone.clone())];
            params.extend(common);
            aliyun_call(&http, config, "AddDomainRecord", &params)
                .await
                .map(drop)
        }
    }
}

// Tencent Cloud DNSPod, API 3.0 with TC3-HMAC-SHA256:
// https://cloud.tencent.com/document/api/1427/56193

const DNSPOD_HOST: &str = "dnspod.tencentcloudapi.com";

fn sha256_hex(data: &[u8]) -> String {
    runo_link::hex::encode(digest::digest(&digest::SHA256, data).as_ref())
}

fn hmac256(key: &[u8], data: &str) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes())
        .as_ref()
        .to_vec()
}

/// The TC3-HMAC-SHA256 credential scope and signature of `canonical`.
fn tc3_sign(secret_key: &str, service: &str, canonical: &str, timestamp: i64) -> (String, String) {
    let date = chrono::DateTime::from_timestamp(timestamp, 0)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string();
    let scope = format!("{date}/{service}/tc3_request");
    let to_sign = format!(
        "TC3-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let key = hmac256(format!("TC3{secret_key}").as_bytes(), &date);
    let key = hmac256(&key, service);
    let key = hmac256(&key, "tc3_request");
    (scope, runo_link::hex::encode(&hmac256(&key, &to_sign)))
}

/// The Authorization header of a Tencent Cloud API 3.0 call.
fn tc3_authorization(
    secret_id: &str,
    secret_key: &str,
    service: &str,
    host: &str,
    action: &str,
    body: &str,
    timestamp: i64,
) -> String {
    let signed_headers = "content-type;host;x-tc-action";
    let canonical = format!(
        "POST\n/\n\ncontent-type:application/json; charset=utf-8\nhost:{host}\nx-tc-action:{}\n\n{signed_headers}\n{}",
        action.to_lowercase(),
        sha256_hex(body.as_bytes())
    );
    let (scope, signature) = tc3_sign(secret_key, service, &canonical, timestamp);
    format!(
        "TC3-HMAC-SHA256 Credential={secret_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

async fn dnspod_call(
    http: &reqwest::Client,
    config: &Config,
    action: &str,
    body: Value,
) -> Result<Value> {
    let body = body.to_string();
    let timestamp = chrono::Utc::now().timestamp();
    let authorization = tc3_authorization(
        &config.secrets[0],
        &config.secrets[1],
        "dnspod",
        DNSPOD_HOST,
        action,
        &body,
        timestamp,
    );
    let reply: Value = http
        .post(format!("https://{DNSPOD_HOST}/"))
        .header("Authorization", authorization)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Host", DNSPOD_HOST)
        .header("X-TC-Action", action)
        .header("X-TC-Version", "2021-03-23")
        .header("X-TC-Timestamp", timestamp.to_string())
        .body(body)
        .send()
        .await?
        .json()
        .await?;
    let response = &reply["Response"];
    if let Some(code) = response["Error"]["Code"].as_str() {
        bail!("DNSPod: {code}: {}", response["Error"]["Message"]);
    }
    Ok(response.clone())
}

async fn dnspod(config: &Config, value: &str) -> Result<()> {
    let http = client()?;
    let found = dnspod_call(
        &http,
        config,
        "DescribeRecordList",
        json!({"Domain": config.zone, "Subdomain": config.host(), "RecordType": "AAAA"}),
    )
    .await;
    let record = match found {
        Ok(found) => found["RecordList"][0].clone(),
        Err(err) if format!("{err}").contains("NoDataOfRecord") => Value::Null,
        Err(err) => return Err(err),
    };
    let mut body = json!({
        "Domain": config.zone,
        "SubDomain": config.host(),
        "RecordType": "AAAA",
        "RecordLine": "默认",
        "Value": value,
        "TTL": config.ttl,
    });
    match record["RecordId"].as_u64() {
        Some(_) if record["Value"].as_str() == Some(value) => Ok(()),
        Some(id) => {
            body["RecordId"] = json!(id);
            dnspod_call(&http, config, "ModifyRecord", body)
                .await
                .map(drop)
        }
        None => dnspod_call(&http, config, "CreateRecord", body)
            .await
            .map(drop),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IF_INET6: &str = "\
00000000000000000000000000000001 01 80 10 80       lo
fe80000000000000d6e9b6fffe1a2b3c 02 40 20 80     eth0
fd12345678900000d6e9b6fffe1a2b3c 02 40 00 00     eth0
24098a0012345678aaaabbbbccccdddd 03 40 00 01    wlan0
24098a00123456780123456789abcdef 03 40 00 00    wlan0
24098a00123456781111222233334444 02 40 00 00     eth0
24098a00123456785555666677778888 02 40 00 20     eth0
";

    #[test]
    fn the_public_address_is_global_stable_and_wired_first() {
        assert_eq!(
            current_address(IF_INET6),
            Some("2409:8a00:1234:5678:1111:2222:3333:4444".parse().unwrap())
        );
        let wireless: String = IF_INET6
            .lines()
            .filter(|l| !l.contains("eth0"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(
            current_address(&wireless),
            Some("2409:8a00:1234:5678:123:4567:89ab:cdef".parse().unwrap())
        );
        // Link-local, unique-local, temporary and deprecated only.
        let none: String = IF_INET6.lines().take(4).map(|l| format!("{l}\n")).collect();
        assert_eq!(current_address(&none), None);
    }

    #[test]
    fn settings_check_the_domain_and_find_the_zone() {
        let config = Config::new(
            Provider::Cloudflare,
            "Pi.Example.com.",
            None,
            vec!["token".into()],
        )
        .unwrap();
        assert_eq!(config.domain, "pi.example.com");
        assert_eq!(config.zone, "example.com");
        assert_eq!(config.host(), "pi");
        let deep = Config::new(
            Provider::Aliyun,
            "pi.home.example.com.cn",
            Some("example.com.cn"),
            vec!["id".into(), "secret".into()],
        )
        .unwrap();
        assert_eq!(deep.host(), "pi.home");
        let apex =
            Config::new(Provider::Cloudflare, "example.com", None, vec!["t".into()]).unwrap();
        assert_eq!(apex.host(), "@");
        assert!(Config::new(Provider::Cloudflare, "localhost", None, vec!["t".into()]).is_err());
        assert!(
            Config::new(
                Provider::Cloudflare,
                "a_b.example.com",
                None,
                vec!["t".into()]
            )
            .is_err()
        );
        assert!(
            Config::new(
                Provider::Cloudflare,
                "pi.other.org",
                Some("example.com"),
                vec!["t".into()]
            )
            .is_err()
        );
        assert!(Config::new(Provider::Dnspod, "pi.example.com", None, vec!["id".into()]).is_err());
        assert_eq!(Provider::parse("Tencent"), Some(Provider::Dnspod));
        assert_eq!(Provider::parse("route53"), None);
    }

    #[test]
    fn settings_are_kept_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_none());
        let config = Config::new(
            Provider::Cloudflare,
            "pi.example.com",
            None,
            vec!["t".into()],
        )
        .unwrap();
        save(dir.path(), &config).unwrap();
        assert_eq!(load(dir.path()), Some(config));
        let mode = std::fs::metadata(dir.path().join(CONFIG))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(remove(dir.path()).unwrap());
        assert!(!remove(dir.path()).unwrap());
    }

    #[test]
    fn aliyun_signs_like_its_documentation() {
        // The worked example in Alibaba Cloud's "RPC signature" page.
        let params = [
            ("AccessKeyId", "testid".to_owned()),
            ("Action", "DescribeDomainRecords".to_owned()),
            ("DomainName", "example.com".to_owned()),
            ("Format", "XML".to_owned()),
            ("SignatureMethod", "HMAC-SHA1".to_owned()),
            (
                "SignatureNonce",
                "f59ed6a9-83fc-473b-9cc6-99c95df3856e".to_owned(),
            ),
            ("SignatureVersion", "1.0".to_owned()),
            ("Timestamp", "2016-03-24T16:41:54Z".to_owned()),
            ("Version", "2015-01-09".to_owned()),
        ];
        let query = aliyun_query(&params, "testsecret");
        assert!(query.starts_with("AccessKeyId=testid&Action=DescribeDomainRecords&"));
        assert!(query.contains("Timestamp=2016-03-24T16%3A41%3A54Z"));
        assert!(
            query.ends_with("&Signature=uRpHwaSEt3J%2B6KQD%2F%2FsvCh%2Fx%2BpI%3D"),
            "{query}"
        );
        assert_eq!(percent("a b*~/"), "a%20b%2A~%2F");
    }

    #[test]
    fn tencent_signs_like_its_documentation() {
        // The worked example in Tencent Cloud's "Signature v3" page, with
        // its published (masked) example key.
        let body = r#"{"Limit": 1, "Filters": [{"Values": ["\u672a\u547d\u540d"], "Name": "instance-name"}]}"#;
        let canonical = format!(
            "POST\n/\n\ncontent-type:application/json; charset=utf-8\nhost:cvm.tencentcloudapi.com\n\ncontent-type;host\n{}",
            sha256_hex(body.as_bytes())
        );
        assert_eq!(
            sha256_hex(canonical.as_bytes()),
            "5ffe6a04c0664d6b969fab9a13bdab201d63ee709638e2749d62a09ca18d7031"
        );
        let (scope, signature) = tc3_sign(
            "Gu5t9xGARNpq86cd98joQYCN3*******",
            "cvm",
            &canonical,
            1551113065,
        );
        assert_eq!(scope, "2019-02-25/cvm/tc3_request");
        assert_eq!(
            signature,
            "2230eefd229f582d8b1b891af7107b91597240707d778ab3738f756258d7652c"
        );
        let header = tc3_authorization(
            "id",
            "key",
            "dnspod",
            DNSPOD_HOST,
            "ModifyRecord",
            "{}",
            1551113065,
        );
        assert!(header.starts_with("TC3-HMAC-SHA256 Credential=id/2019-02-25/dnspod/tc3_request, SignedHeaders=content-type;host;x-tc-action, Signature="));
    }
}
