use std::{collections::BTreeSet, fs::File, io::Read, net::IpAddr};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, UtcOffset, Weekday};

use crate::util::now_ms;

pub(crate) const SCHEMA_VERSION: i64 = 1;
pub(crate) const PAYLOAD_VERSION: i64 = 1;
pub(crate) const DEFAULT_PORT: u16 = 24_923;
const MAX_EVENTS: usize = 100;
const MAX_OFFLINE_MS: i64 = 24 * 3_600_000;
const MAX_FUTURE_MS: i64 = 300_000;
pub(crate) const LEASE_MS: i64 = 45_000;
const HEARTBEAT_SECONDS: i64 = 20;
const MAX_ID: i64 = i32::MAX as i64;

type EventKey = [u8; 32];

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut value = [0u8; N];
    File::open("/dev/urandom")?.read_exact(&mut value)?;
    Ok(value)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn hash(parts: &[&[u8]]) -> EventKey {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.len().to_be_bytes());
        digest.update(part);
    }
    digest.finalize().into()
}
pub(crate) fn extension_origin(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("moz-extension://")
        .or_else(|| origin.strip_prefix("chrome-extension://"));
    rest.is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 80
            && !id.contains(['/', '?', '#', '@', ':'])
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}
pub(crate) fn loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}
fn valid_identifier(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn create_pairing_code(db: &Connection) -> Result<Value> {
    let code = hex(&random_bytes::<4>()?);
    let digest = hash(&[b"browser-pair-code-v1", code.as_bytes()]);
    let now = now_ms();
    db.execute(
        "DELETE FROM browser_pairing_codes WHERE expires_ts<? OR used_ts IS NOT NULL",
        params![now],
    )?;
    db.execute(
        "INSERT INTO browser_pairing_codes(code_hash,created_ts,expires_ts)VALUES(?,?,?)",
        params![digest.as_slice(), now, now + 300_000],
    )?;
    Ok(json!({"schemaVersion":SCHEMA_VERSION,"code":code,"expiresTimestamp":now+300_000}))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PairInput {
    schema_version: i64,
    pairing_code: String,
    client_id: String,
    display_name: String,
    browser_family: String,
    extension_version: String,
    privacy_mode: String,
    selected_media_origins: Vec<String>,
}

pub(crate) fn pair(
    db: &mut Connection,
    input: PairInput,
    origin: &str,
    peer: IpAddr,
) -> Result<Value> {
    if !loopback(peer) || !extension_origin(origin) {
        return Err(anyhow!("pairing requires loopback extension origin"));
    }
    if input.schema_version != SCHEMA_VERSION
        || !valid_identifier(&input.client_id, 8, 128)
        || input.pairing_code.len() != 8
        || !input
            .pairing_code
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || input.display_name.trim().is_empty()
        || input.display_name.chars().count() > 80
        || input.display_name.chars().any(char::is_control)
        || !valid_identifier(&input.extension_version, 1, 40)
    {
        return Err(anyhow!("invalid pairing request"));
    }
    let family = input.browser_family.to_ascii_lowercase();
    if !["firefox", "zen", "chromium", "chrome"].contains(&family.as_str()) {
        return Err(anyhow!("unsupported browser family"));
    }
    if ![
        "domains-only",
        "domains-and-titles",
        "selected-sites-media",
        "full-debug",
    ]
    .contains(&input.privacy_mode.as_str())
    {
        return Err(anyhow!("invalid privacy mode"));
    }
    let selected_media_origins = normalize_selected_origins(input.selected_media_origins.clone())?;
    let selected_media_origins_json = serde_json::to_string(&selected_media_origins)?;
    let now = now_ms();
    let bucket = now.div_euclid(60_000);
    let rate_key = hash(&[b"browser-pair-rate-v1", origin.as_bytes()]);
    db.execute("INSERT INTO browser_pairing_rate_limits(bucket_ts,rate_key,attempts)VALUES(?,?,1) ON CONFLICT(bucket_ts,rate_key)DO UPDATE SET attempts=attempts+1", params![bucket,rate_key.as_slice()])?;
    let attempts: i64 = db.query_row(
        "SELECT attempts FROM browser_pairing_rate_limits WHERE bucket_ts=? AND rate_key=?",
        params![bucket, rate_key.as_slice()],
        |row| row.get(0),
    )?;
    if attempts > 20 {
        return Err(anyhow!("pairing rate limit exceeded"));
    }
    let code_hash = hash(&[b"browser-pair-code-v1", input.pairing_code.as_bytes()]);
    let token = hex(&random_bytes::<32>()?);
    let token_hash = hash(&[b"browser-token-v1", token.as_bytes()]);
    let client_key = hash(&[b"browser-client-v1", input.client_id.as_bytes()]);
    let tx = db.transaction()?;
    let code_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM browser_pairing_codes WHERE code_hash=? AND expires_ts>=? AND used_ts IS NULL AND attempts<10",
            params![code_hash.as_slice(), now],
            |row| row.get(0),
        )
        .optional()?;
    let Some(code_id) = code_id else {
        return Err(anyhow!("invalid or expired pairing code"));
    };
    tx.execute(
        "UPDATE browser_pairing_codes SET used_ts=?,attempts=attempts+1 WHERE id=? AND used_ts IS NULL",
        params![now, code_id],
    )?;
    if let Some(existing_id) = tx
        .query_row(
            "SELECT id FROM browser_clients WHERE client_key=?",
            params![client_key.as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    {
        close_client_states(&tx, existing_id, &family, now, "client_repaired")?;
    }
    tx.execute(
        "INSERT INTO browser_clients(client_key,client_instance_id,display_name,browser_family,extension_version,secret_hash,allowed_origin,privacy_mode,selected_media_origins_json,paired_ts,last_seen_ts,last_sequence,revoked_ts)VALUES(?,?,?,?,?,?,?,?,?,?,?,0,NULL) ON CONFLICT(client_key)DO UPDATE SET client_instance_id=excluded.client_instance_id,display_name=excluded.display_name,browser_family=excluded.browser_family,extension_version=excluded.extension_version,secret_hash=excluded.secret_hash,allowed_origin=excluded.allowed_origin,privacy_mode=excluded.privacy_mode,selected_media_origins_json=excluded.selected_media_origins_json,paired_ts=excluded.paired_ts,last_seen_ts=excluded.last_seen_ts,last_sequence=0,revoked_ts=NULL",
        params![client_key.as_slice(),input.client_id,input.display_name.trim(),family,input.extension_version,token_hash.as_slice(),origin,input.privacy_mode,selected_media_origins_json,now,now],
    )?;
    tx.execute(
        "DELETE FROM browser_current_states WHERE client_id=(SELECT id FROM browser_clients WHERE client_key=?)",
        params![client_key.as_slice()],
    )?;
    tx.commit()?;
    let revision = max_policy_revision(db)?;
    Ok(
        json!({"schemaVersion":SCHEMA_VERSION,"clientId":input.client_id,"token":token,"pairedTimestamp":now,"policyRevision":revision}),
    )
}

#[derive(Clone, Debug)]
struct AuthClient {
    id: i64,
    instance_id: String,
    privacy_mode: String,
    browser_family: String,
    selected_media_origins: Vec<String>,
    last_sequence: i64,
}
fn authenticate(db: &Connection, bearer: &str, origin: &str) -> Result<AuthClient> {
    if !extension_origin(origin) || bearer.len() > 128 {
        return Err(anyhow!("invalid browser credentials"));
    }
    let digest = hash(&[b"browser-token-v1", bearer.as_bytes()]);
    db.query_row(
        "SELECT id,client_instance_id,privacy_mode,browser_family,selected_media_origins_json,last_sequence FROM browser_clients WHERE secret_hash=? AND allowed_origin=? AND revoked_ts IS NULL",
        params![digest.as_slice(), origin],
        |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,i64>(5)?)),
    )
    .optional()?
    .map(|(id,instance_id,privacy_mode,browser_family,raw,last_sequence)| -> Result<AuthClient> {Ok(AuthClient{id,instance_id,privacy_mode,browser_family,selected_media_origins:normalize_selected_origins(serde_json::from_str(&raw).map_err(|_|anyhow!("invalid persisted selected media origins"))?)?,last_sequence})})
    .transpose()?
    .ok_or_else(|| anyhow!("invalid browser credentials"))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EventType {
    Snapshot,
    WindowFocus,
    ActiveTab,
    Navigation,
    Visibility,
    MediaPlay,
    MediaPause,
    MediaEnd,
    Heartbeat,
    TabClose,
}
impl EventType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::WindowFocus => "window_focus",
            Self::ActiveTab => "active_tab",
            Self::Navigation => "navigation",
            Self::Visibility => "visibility",
            Self::MediaPlay => "media_play",
            Self::MediaPause => "media_pause",
            Self::MediaEnd => "media_end",
            Self::Heartbeat => "heartbeat",
            Self::TabClose => "tab_close",
        }
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MediaState {
    None,
    Paused,
    Playing,
    Ended,
    Seeking,
}
impl MediaState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Paused => "paused",
            Self::Playing => "playing",
            Self::Ended => "ended",
            Self::Seeking => "seeking",
        }
    }
    fn playing(&self) -> bool {
        matches!(self, Self::Playing | Self::Seeking)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserEventInput {
    payload_version: i64,
    event_key: String,
    client_sequence: i64,
    event_timestamp: i64,
    client_id: String,
    event_type: EventType,
    window_id: i64,
    tab_id: i64,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    title: Option<String>,
    active: bool,
    browser_focused: bool,
    document_visible: bool,
    audible: bool,
    media_state: MediaState,
    picture_in_picture: bool,
    #[serde(default)]
    media_position: Option<f64>,
    #[serde(default)]
    media_duration: Option<f64>,
    #[serde(default)]
    playback_rate: Option<f64>,
    private: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EventBatch {
    schema_version: i64,
    queue_health: QueueHealthInput,
    events: Vec<BrowserEventInput>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueueHealthInput {
    depth: i64,
    bytes: i64,
    dropped: i64,
    permanent_rejected: i64,
    recovery_snapshots: i64,
    #[serde(default)]
    oldest_timestamp: Option<i64>,
    #[serde(default)]
    last_successful_sync: Option<i64>,
    #[serde(default)]
    last_data_loss: Option<i64>,
}

#[derive(Clone, Debug)]
struct ValidEvent {
    source_index: usize,
    key: EventKey,
    sequence: i64,
    ts: i64,
    kind: EventType,
    window_id: i64,
    tab_id: i64,
    domain: Option<String>,
    origin: Option<String>,
    title: Option<String>,
    focused: bool,
    active: bool,
    visible: bool,
    audible: bool,
    media: MediaState,
    pip: bool,
    position: Option<f64>,
    duration: Option<f64>,
    rate: Option<f64>,
}

fn normalize_domain(value: &str) -> Result<String> {
    let value = value.trim_end_matches('.').to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 253
        || !value.is_ascii()
        || !value.contains('.')
        || value.contains(['/', '\\', ':', '?', '#', '@'])
        || value.parse::<IpAddr>().is_ok()
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(anyhow!("invalid domain"));
    }
    Ok(value)
}
fn normalize_page_origin(value: &str, domain: &str) -> Result<String> {
    let (scheme, authority) = value
        .split_once("://")
        .ok_or_else(|| anyhow!("invalid page origin"))?;
    if !matches!(scheme, "http" | "https") || authority.contains(['/', '?', '#', '@']) {
        return Err(anyhow!(
            "page origin must not contain credentials, path, query, or fragment"
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, raw)) if raw.bytes().all(|byte| byte.is_ascii_digit()) => {
            let port: u16 = raw.parse().map_err(|_| anyhow!("invalid origin port"))?;
            (host, Some(port))
        }
        _ => (authority, None),
    };
    if normalize_domain(host)? != domain {
        return Err(anyhow!("domain and origin differ"));
    }
    let default =
        (scheme == "https" && port == Some(443)) || (scheme == "http" && port == Some(80));
    Ok(if let Some(port) = port.filter(|_| !default) {
        format!("{scheme}://{domain}:{port}")
    } else {
        format!("{scheme}://{domain}")
    })
}
fn normalize_selected_origins(values: Vec<String>) -> Result<Vec<String>> {
    if values.len() > 100 {
        return Err(anyhow!("too many selected media origins"));
    }
    let mut normalized = BTreeSet::new();
    for value in values {
        if value.len() > 300 {
            return Err(anyhow!("selected media origin too long"));
        }
        let (_, authority) = value
            .split_once("://")
            .ok_or_else(|| anyhow!("invalid selected media origin"))?;
        if authority.contains(['/', '?', '#', '@']) {
            return Err(anyhow!(
                "selected media origin must not contain private URL data"
            ));
        }
        let host = authority
            .rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit()))
            .map_or(authority, |(host, _)| host);
        let domain = normalize_domain(host)?;
        let origin = normalize_page_origin(&value, &domain)?;
        if !normalized.insert(origin) {
            return Err(anyhow!("duplicate selected media origin"));
        }
    }
    Ok(normalized.into_iter().collect())
}
fn valid_number(value: Option<f64>, min: f64, max: f64) -> bool {
    value.is_none_or(|number| number.is_finite() && number >= min && number <= max)
}
fn validate_queue_health(health: &QueueHealthInput, now: i64) -> Result<()> {
    if !(0..=1000).contains(&health.depth)
        || !(0..=2_097_152).contains(&health.bytes)
        || !(0..=2_147_483_647).contains(&health.dropped)
        || !(0..=2_147_483_647).contains(&health.permanent_rejected)
        || !(0..=2_147_483_647).contains(&health.recovery_snapshots)
        || [
            health.oldest_timestamp,
            health.last_successful_sync,
            health.last_data_loss,
        ]
        .into_iter()
        .flatten()
        .any(|timestamp| timestamp < 0 || timestamp > now + MAX_FUTURE_MS)
    {
        return Err(anyhow!("invalid queue health bounds"));
    }
    Ok(())
}
fn queue_health_json(health: &QueueHealthInput, observed: i64) -> Value {
    json!({"depth":health.depth,"bytes":health.bytes,"dropped":health.dropped,"permanentRejected":health.permanent_rejected,"recoverySnapshots":health.recovery_snapshots,"oldestTimestamp":health.oldest_timestamp,"lastSuccessfulSync":health.last_successful_sync,"lastDataLoss":health.last_data_loss,"observedTimestamp":observed})
}
fn persist_queue_health(
    db: &Connection,
    client_id: i64,
    health: &QueueHealthInput,
    observed: i64,
) -> Result<()> {
    db.execute("UPDATE browser_clients SET queue_depth=?,queue_bytes=?,queue_dropped=?,queue_permanent_rejected=?,queue_recovery_snapshots=?,queue_oldest_ts=?,queue_last_success_ts=?,queue_last_data_loss_ts=?,queue_health_observed_ts=? WHERE id=?",params![health.depth,health.bytes,health.dropped,health.permanent_rejected,health.recovery_snapshots,health.oldest_timestamp,health.last_successful_sync,health.last_data_loss,observed,client_id])?;
    Ok(())
}
fn validate_event(
    event: BrowserEventInput,
    index: usize,
    client: &AuthClient,
    now: i64,
) -> Result<ValidEvent> {
    if event.payload_version != PAYLOAD_VERSION
        || event.client_id != client.instance_id
        || !valid_identifier(&event.event_key, 8, 128)
        || event.client_sequence <= 0
        || event.window_id < 0
        || event.window_id > MAX_ID
        || event.tab_id < 0
        || event.tab_id > MAX_ID
        || event.event_timestamp < now - MAX_OFFLINE_MS
        || event.event_timestamp > now + MAX_FUTURE_MS
        || event.private
    {
        return Err(anyhow!(
            "invalid event identity, bounds, timestamp, or privacy state"
        ));
    }
    let (domain, origin) = match (event.domain.as_deref(), event.origin.as_deref()) {
        (Some(domain), Some(origin)) => {
            let domain = normalize_domain(domain)?;
            let origin = normalize_page_origin(origin, &domain)?;
            (Some(domain), Some(origin))
        }
        (None, None) => (None, None),
        _ => return Err(anyhow!("domain and origin must be supplied together")),
    };
    if domain.is_none()
        && (event.audible
            || event.picture_in_picture
            || event.media_state != MediaState::None
            || event.media_position.is_some()
            || event.media_duration.is_some()
            || event.playback_rate.is_some())
    {
        return Err(anyhow!("media state requires a domain"));
    }
    let title = match event.title {
        Some(title)
            if matches!(
                client.privacy_mode.as_str(),
                "domains-and-titles" | "full-debug"
            ) =>
        {
            if title.trim().is_empty()
                || title.chars().count() > 512
                || title.chars().any(char::is_control)
            {
                return Err(anyhow!("invalid title"));
            }
            Some(title)
        }
        Some(_) => return Err(anyhow!("title forbidden by privacy mode")),
        None => None,
    };
    let media_allowed = matches!(
        client.privacy_mode.as_str(),
        "selected-sites-media" | "full-debug"
    );
    let media_present = event.media_state != MediaState::None
        || event.audible
        || event.picture_in_picture
        || event.media_position.is_some()
        || event.media_duration.is_some()
        || event.playback_rate.is_some();
    if media_present && !media_allowed {
        return Err(anyhow!("media fields forbidden by privacy mode"));
    }
    if media_present
        && client.privacy_mode == "selected-sites-media"
        && !origin
            .as_ref()
            .is_some_and(|origin| client.selected_media_origins.contains(origin))
    {
        return Err(anyhow!("media origin is not selected for this client"));
    }
    if !valid_number(event.media_position, 0.0, 864_000.0)
        || !valid_number(event.media_duration, 0.0, 864_000.0)
        || !valid_number(event.playback_rate, 0.1, 16.0)
    {
        return Err(anyhow!("invalid media numeric bounds"));
    }
    if matches!(event.media_state, MediaState::None | MediaState::Ended)
        && (event.media_position.is_some() || event.playback_rate.is_some())
    {
        return Err(anyhow!("orphan media detail"));
    }
    let event_state_valid = match event.event_type {
        EventType::MediaPlay => event.media_state.playing(),
        EventType::MediaPause => event.media_state == MediaState::Paused,
        EventType::MediaEnd => matches!(event.media_state, MediaState::Ended | MediaState::None),
        EventType::TabClose => {
            !event.active
                && !event.browser_focused
                && !event.document_visible
                && !event.audible
                && !event.picture_in_picture
                && event.media_state == MediaState::None
        }
        _ => true,
    };
    if !event_state_valid {
        return Err(anyhow!("event type and state disagree"));
    }
    Ok(ValidEvent {
        source_index: index,
        key: hash(&[b"browser-event-v1", event.event_key.as_bytes()]),
        sequence: event.client_sequence,
        ts: event.event_timestamp,
        kind: event.event_type,
        window_id: event.window_id,
        tab_id: event.tab_id,
        domain,
        origin,
        title,
        focused: event.browser_focused,
        active: event.active,
        visible: event.document_visible,
        audible: event.audible,
        media: event.media_state,
        pip: event.picture_in_picture,
        position: event.media_position,
        duration: event.media_duration,
        rate: event.playback_rate,
    })
}

#[derive(Clone, Debug)]
struct CurrentState {
    window_id: i64,
    tab_id: i64,
    episode_key: Vec<u8>,
    started: i64,
    observed: i64,
    expires: i64,
    domain: Option<String>,
    origin: Option<String>,
    title: Option<String>,
    focused: bool,
    active: bool,
    visible: bool,
    audible: bool,
    media: String,
    pip: bool,
}
impl CurrentState {
    fn matches(&self, event: &ValidEvent) -> bool {
        self.domain == event.domain
            && self.origin == event.origin
            && self.title == event.title
            && self.focused == event.focused
            && self.active == event.active
            && self.visible == event.visible
            && self.audible == event.audible
            && self.media == event.media.as_str()
            && self.pip == event.pip
    }
}
fn load_current(
    db: &Connection,
    client_id: i64,
    window_id: i64,
    tab_id: i64,
) -> Result<Option<CurrentState>> {
    Ok(db.query_row(
        "SELECT window_id,tab_id,episode_key,state_started_ts,last_observed_ts,expires_ts,domain,origin,title,browser_focused,tab_active,document_visible,audible,media_state,picture_in_picture FROM browser_current_states WHERE client_id=? AND window_id=? AND tab_id=?",
        params![client_id,window_id,tab_id],
        |row| Ok(CurrentState{window_id:row.get(0)?,tab_id:row.get(1)?,episode_key:row.get(2)?,started:row.get(3)?,observed:row.get(4)?,expires:row.get(5)?,domain:row.get(6)?,origin:row.get(7)?,title:row.get(8)?,focused:row.get::<_,i64>(9)?!=0,active:row.get::<_,i64>(10)?!=0,visible:row.get::<_,i64>(11)?!=0,audible:row.get::<_,i64>(12)?!=0,media:row.get(13)?,pip:row.get::<_,i64>(14)?!=0}),
    ).optional()?)
}
fn load_client_states(db: &Connection, client_id: i64) -> Result<Vec<CurrentState>> {
    let mut stmt = db.prepare("SELECT window_id,tab_id,episode_key,state_started_ts,last_observed_ts,expires_ts,domain,origin,title,browser_focused,tab_active,document_visible,audible,media_state,picture_in_picture FROM browser_current_states WHERE client_id=? ORDER BY window_id,tab_id")?;
    let rows = stmt
        .query_map(params![client_id], |row| {
            Ok(CurrentState {
                window_id: row.get(0)?,
                tab_id: row.get(1)?,
                episode_key: row.get(2)?,
                started: row.get(3)?,
                observed: row.get(4)?,
                expires: row.get(5)?,
                domain: row.get(6)?,
                origin: row.get(7)?,
                title: row.get(8)?,
                focused: row.get::<_, i64>(9)? != 0,
                active: row.get::<_, i64>(10)? != 0,
                visible: row.get::<_, i64>(11)? != 0,
                audible: row.get::<_, i64>(12)? != 0,
                media: row.get(13)?,
                pip: row.get::<_, i64>(14)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn browser_app_matches(family: &str, app: &str) -> bool {
    let app = app.to_ascii_lowercase();
    match family {
        "firefox" => app.contains("firefox"),
        "zen" => app.contains("zen") || app.contains("firefox"),
        "chromium" | "chrome" => app.contains("chrom") || app.contains("brave"),
        _ => false,
    }
}
fn merged_coverage(mut ranges: Vec<(i64, i64)>) -> i64 {
    ranges.sort_unstable();
    let mut total = 0;
    let mut current: Option<(i64, i64)> = None;
    for (start, end) in ranges.into_iter().filter(|(start, end)| end > start) {
        match current {
            Some((left, right)) if start <= right => current = Some((left, right.max(end))),
            Some((left, right)) => {
                total += right - left;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((start, end)) = current {
        total += end - start;
    }
    total
}
pub(crate) fn reconcile_os(
    db: &Connection,
    family: &str,
    start: i64,
    end: i64,
) -> Result<(&'static str, Option<i64>)> {
    let mut stmt = db.prepare("SELECT id,start_ts,end_ts,focused_app FROM focus_context_intervals WHERE end_ts>? AND start_ts<? ORDER BY start_ts,id LIMIT 512")?;
    let rows = stmt
        .query_map(params![start, end], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(("unknown", None));
    }
    let mut matching = Vec::new();
    let mut contradictory = Vec::new();
    let mut context_id = None;
    for (id, row_start, row_end, app) in rows {
        let range = (row_start.max(start), row_end.min(end));
        if browser_app_matches(family, &app) {
            matching.push(range);
            context_id.get_or_insert(id);
        } else if !app.is_empty() {
            contradictory.push(range);
        }
    }
    let duration = end - start;
    let matched = merged_coverage(matching);
    let contradicted = merged_coverage(contradictory);
    Ok((
        if matched >= duration && contradicted == 0 {
            "verified"
        } else if contradicted >= duration && matched == 0 {
            "contradicted"
        } else if matched == 0 && contradicted == 0 {
            "unknown"
        } else {
            "mixed"
        },
        context_id,
    ))
}
fn confidence(os: &str) -> f64 {
    match os {
        "verified" => 0.98,
        "contradicted" => 0.95,
        "mixed" => 0.65,
        _ => 0.5,
    }
}

fn compact_attention(
    db: &Connection,
    client_id: i64,
    family: &str,
    state: &CurrentState,
    requested_end: i64,
    reason: &str,
) -> Result<usize> {
    let end = requested_end
        .min(state.expires)
        .min(state.observed.saturating_add(LEASE_MS));
    if end <= state.started {
        return Ok(0);
    }
    let service = if let Some(domain) = state.domain.as_deref() {
        let key = hash(&[b"service", domain.as_bytes()]);
        db.execute("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts)VALUES('service',?,?,?,?) ON CONFLICT(kind,canonical_key)DO UPDATE SET last_seen_ts=MAX(last_seen_ts,excluded.last_seen_ts)",params![key.as_slice(),domain,state.started,end])?;
        Some(db.query_row(
            "SELECT id FROM context_entities WHERE kind='service' AND canonical_key=?",
            params![key.as_slice()],
            |row| row.get::<_, i64>(0),
        )?)
    } else {
        None
    };
    let mut start = state.started;
    let mut inserted = 0;
    while start < end {
        let segment_end = end.min(crate::context::next_local_midnight_ms(start));
        let (os, context_id) = reconcile_os(db, family, start, segment_end)?;
        db.execute("INSERT INTO browser_attention_intervals(client_id,window_id,tab_id,start_ts,end_ts,episode_key,domain,origin,service_entity_id,browser_focused,tab_active,document_visible,media_playing,audible,picture_in_picture,os_focus_verified,os_focus_state,os_context_interval_id,confidence,closed_reason)VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![client_id,state.window_id,state.tab_id,start,segment_end,state.episode_key,state.domain,state.origin,service,state.focused as i64,state.active as i64,state.visible as i64,matches!(state.media.as_str(),"playing"|"seeking")as i64,state.audible as i64,state.pip as i64,(os=="verified")as i64,os,context_id,confidence(os),reason])?;
        inserted += 1;
        start = segment_end;
    }
    Ok(inserted)
}
fn close_client_states(
    db: &Connection,
    client_id: i64,
    family: &str,
    now: i64,
    reason: &str,
) -> Result<()> {
    for state in load_client_states(db, client_id)? {
        compact_attention(db, client_id, family, &state, now, reason)?;
    }
    db.execute(
        "DELETE FROM browser_current_states WHERE client_id=?",
        params![client_id],
    )?;
    Ok(())
}

pub(crate) fn ingest(
    db: &mut Connection,
    bearer: &str,
    origin: &str,
    peer: IpAddr,
    batch: EventBatch,
) -> Result<Value> {
    if !loopback(peer) {
        return Err(anyhow!("browser ingestion is loopback only"));
    }
    let client = authenticate(db, bearer, origin)?;
    if batch.schema_version != SCHEMA_VERSION
        || batch.events.is_empty()
        || batch.events.len() > MAX_EVENTS
    {
        return Err(anyhow!("invalid event batch"));
    }
    if client.privacy_mode == "off" {
        return Err(anyhow!("browser integration disabled"));
    }
    let now = now_ms();
    validate_queue_health(&batch.queue_health, now)?;
    let queue_health = batch.queue_health.clone();
    let bucket = now.div_euclid(60_000);
    db.execute("INSERT INTO browser_request_rate_limits(client_id,bucket_ts,requests)VALUES(?,?,1) ON CONFLICT(client_id,bucket_ts)DO UPDATE SET requests=requests+1",params![client.id,bucket])?;
    let requests: i64 = db.query_row(
        "SELECT requests FROM browser_request_rate_limits WHERE client_id=? AND bucket_ts=?",
        params![client.id, bucket],
        |row| row.get(0),
    )?;
    if requests > 120 {
        return Err(anyhow!("browser request rate limit exceeded"));
    }
    let mut valid = Vec::new();
    let mut rejected = Vec::new();
    let mut previous_sequence = 0;
    let mut previous_ts = i64::MIN;
    for (index, event) in batch.events.into_iter().enumerate() {
        let sequence = event.client_sequence;
        let timestamp = event.event_timestamp;
        if sequence <= previous_sequence || timestamp < previous_ts {
            rejected.push(json!({"index":index,"code":"unordered_batch","reason":"events must be strictly ordered","permanent":true}));
            continue;
        }
        previous_sequence = sequence;
        previous_ts = timestamp;
        match validate_event(event,index,&client,now){Ok(event)=>valid.push(event),Err(error)=>rejected.push(json!({"index":index,"code":"invalid_event","reason":error.to_string(),"permanent":true}))}
    }
    let tx = db.transaction()?;
    let mut accepted = 0usize;
    let mut duplicates = 0usize;
    let mut heartbeats = 0usize;
    let mut recoveries = 0usize;
    let mut transitions = 0usize;
    let mut last_sequence = client.last_sequence;
    let mut last_domain = None;
    for event in valid {
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM browser_event_receipts WHERE client_id=? AND event_key=?)",
            params![client.id, event.key.as_slice()],
            |row| row.get(0),
        )?;
        if duplicate {
            duplicates += 1;
            continue;
        }
        if event.sequence <= last_sequence {
            rejected.push(json!({"index":event.source_index,"code":"sequence_replay","reason":"sequence was already processed","permanent":true}));
            continue;
        }
        let prior = load_current(&tx, client.id, event.window_id, event.tab_id)?;
        let stale = prior.as_ref().is_some_and(|state| event.ts > state.expires);
        if let Some(state) = &prior {
            if stale || !state.matches(&event) || event.kind == EventType::TabClose {
                compact_attention(
                    &tx,
                    client.id,
                    &client.browser_family,
                    state,
                    event.ts,
                    if stale {
                        "lease_stale"
                    } else if event.kind == EventType::TabClose {
                        "tab_close"
                    } else {
                        "state_changed"
                    },
                )?;
            }
        }
        let changed = prior
            .as_ref()
            .is_none_or(|state| stale || !state.matches(&event));
        tx.execute("INSERT INTO browser_event_receipts(client_id,event_key,client_sequence,received_ts)VALUES(?,?,?,?)",params![client.id,event.key.as_slice(),event.sequence,now])?;
        if changed || event.kind == EventType::TabClose {
            let persisted_kind = if matches!(event.kind, EventType::Heartbeat | EventType::Snapshot)
            {
                recoveries += 1;
                "recovery"
            } else {
                event.kind.as_str()
            };
            tx.execute("INSERT INTO browser_events(client_id,event_key,client_sequence,window_id,tab_id,event_ts,received_ts,event_type,domain,origin,title,browser_focused,tab_active,document_visible,audible,media_state,picture_in_picture,media_position,media_duration,playback_rate,private,payload_version)VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,0,?)",params![client.id,event.key.as_slice(),event.sequence,event.window_id,event.tab_id,event.ts,now,persisted_kind,event.domain,event.origin,event.title,event.focused as i64,event.active as i64,event.visible as i64,event.audible as i64,event.media.as_str(),event.pip as i64,event.position,event.duration,event.rate,PAYLOAD_VERSION])?;
            transitions += 1;
        } else if event.kind == EventType::Heartbeat {
            heartbeats += 1;
        }
        if event.kind == EventType::TabClose {
            tx.execute(
                "DELETE FROM browser_current_states WHERE client_id=? AND window_id=? AND tab_id=?",
                params![client.id, event.window_id, event.tab_id],
            )?;
        } else {
            let started = if changed {
                event.ts
            } else {
                prior.as_ref().unwrap().started
            };
            let episode = if let Some(state) = prior.as_ref().filter(|state| {
                state.domain == event.domain
                    && matches!(state.media.as_str(), "playing" | "seeking")
                    && event.media.playing()
            }) {
                state.episode_key.clone()
            } else {
                hash(&[
                    b"browser-episode-v1",
                    &client.id.to_be_bytes(),
                    &event.window_id.to_be_bytes(),
                    &event.tab_id.to_be_bytes(),
                    &event.sequence.to_be_bytes(),
                ])
                .to_vec()
            };
            let lease = hash(&[
                b"browser-state-v1",
                &client.id.to_be_bytes(),
                &event.window_id.to_be_bytes(),
                &event.tab_id.to_be_bytes(),
                &event.sequence.to_be_bytes(),
            ]);
            tx.execute("INSERT INTO browser_current_states(client_id,window_id,tab_id,lease_key,last_event_key,episode_key,client_sequence,state_started_ts,last_observed_ts,expires_ts,domain,origin,title,browser_focused,tab_active,document_visible,audible,media_state,picture_in_picture,private,payload_version,heartbeat_upserts)VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,0,?,?) ON CONFLICT(client_id,window_id,tab_id)DO UPDATE SET lease_key=excluded.lease_key,last_event_key=excluded.last_event_key,episode_key=excluded.episode_key,client_sequence=excluded.client_sequence,state_started_ts=excluded.state_started_ts,last_observed_ts=excluded.last_observed_ts,expires_ts=excluded.expires_ts,domain=excluded.domain,origin=excluded.origin,title=excluded.title,browser_focused=excluded.browser_focused,tab_active=excluded.tab_active,document_visible=excluded.document_visible,audible=excluded.audible,media_state=excluded.media_state,picture_in_picture=excluded.picture_in_picture,payload_version=excluded.payload_version,heartbeat_upserts=browser_current_states.heartbeat_upserts+excluded.heartbeat_upserts",params![client.id,event.window_id,event.tab_id,lease.as_slice(),event.key.as_slice(),episode,event.sequence,started,event.ts,event.ts+LEASE_MS,event.domain,event.origin,event.title,event.focused as i64,event.active as i64,event.visible as i64,event.audible as i64,event.media.as_str(),event.pip as i64,PAYLOAD_VERSION,(event.kind==EventType::Heartbeat&&!changed)as i64])?;
        }
        last_sequence = event.sequence;
        last_domain = event.domain.clone();
        accepted += 1;
    }
    tx.execute(
        "UPDATE browser_clients SET last_seen_ts=?,last_sequence=? WHERE id=?",
        params![now, last_sequence, client.id],
    )?;
    persist_queue_health(&tx, client.id, &queue_health, now)?;
    tx.execute("UPDATE browser_ingestion_metrics SET accepted_events=accepted_events+?,duplicate_events=duplicate_events+?,rejected_events=rejected_events+?,heartbeat_upserts=heartbeat_upserts+?,persisted_transitions=persisted_transitions+?,recovery_transitions=recovery_transitions+?,last_ingest_ts=? WHERE id=1",params![accepted as i64,duplicates as i64,rejected.len()as i64,heartbeats as i64,transitions as i64,recoveries as i64,now])?;
    tx.execute("INSERT INTO source_health(source,last_success_ts,success_count,detail_json)VALUES('browser',?,1,?) ON CONFLICT(source)DO UPDATE SET last_success_ts=excluded.last_success_ts,success_count=success_count+1,detail_json=excluded.detail_json,last_error_category=NULL",params![now,json!({"accepted":accepted,"duplicates":duplicates,"rejected":rejected.len(),"heartbeatUpserts":heartbeats,"queueHealth":queue_health_json(&queue_health,now)}).to_string()])?;
    tx.commit()?;
    let revision = max_policy_revision(db)?;
    let policy_decision = last_domain
        .as_deref()
        .and_then(|domain| evaluate_policies(db, client.id, domain, now).ok())
        .and_then(|(_, decisions)| strongest_decision(&decisions).cloned());
    Ok(
        json!({"schemaVersion":SCHEMA_VERSION,"accepted":accepted,"duplicates":duplicates,"heartbeatUpserts":heartbeats,"recoveredTransitions":recoveries,"rejected":rejected,"serverTimestamp":now,"policyRevision":revision,"policyDecision":policy_decision,"queueHealth":queue_health_json(&queue_health,now)}),
    )
}

pub(crate) fn expire_stale_states(db: &Connection, now: i64, limit: usize) -> Result<usize> {
    let mut stmt=db.prepare("SELECT s.client_id,c.browser_family,s.window_id,s.tab_id,s.episode_key,s.state_started_ts,s.last_observed_ts,s.expires_ts,s.domain,s.origin,s.title,s.browser_focused,s.tab_active,s.document_visible,s.audible,s.media_state,s.picture_in_picture FROM browser_current_states s JOIN browser_clients c ON c.id=s.client_id WHERE s.expires_ts<? ORDER BY s.expires_ts LIMIT ?")?;
    let rows = stmt
        .query_map(params![now, limit.clamp(1, 500) as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                CurrentState {
                    window_id: row.get(2)?,
                    tab_id: row.get(3)?,
                    episode_key: row.get(4)?,
                    started: row.get(5)?,
                    observed: row.get(6)?,
                    expires: row.get(7)?,
                    domain: row.get(8)?,
                    origin: row.get(9)?,
                    title: row.get(10)?,
                    focused: row.get::<_, i64>(11)? != 0,
                    active: row.get::<_, i64>(12)? != 0,
                    visible: row.get::<_, i64>(13)? != 0,
                    audible: row.get::<_, i64>(14)? != 0,
                    media: row.get(15)?,
                    pip: row.get::<_, i64>(16)? != 0,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (client_id, family, state) in &rows {
        compact_attention(db, *client_id, family, state, state.expires, "lease_stale")?;
        db.execute("DELETE FROM browser_current_states WHERE client_id=? AND window_id=? AND tab_id=? AND expires_ts=?",params![client_id,state.window_id,state.tab_id,state.expires])?;
    }
    Ok(rows.len())
}

fn max_policy_revision(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT COALESCE(MAX(revision),0) FROM attention_policies",
        [],
        |row| row.get(0),
    )?)
}
fn load_queue_health(db: &Connection, client_id: i64) -> Result<(QueueHealthInput, i64)> {
    db.query_row("SELECT queue_depth,queue_bytes,queue_dropped,queue_permanent_rejected,queue_recovery_snapshots,queue_oldest_ts,queue_last_success_ts,queue_last_data_loss_ts,COALESCE(queue_health_observed_ts,0) FROM browser_clients WHERE id=?",params![client_id],|row|Ok((QueueHealthInput{depth:row.get(0)?,bytes:row.get(1)?,dropped:row.get(2)?,permanent_rejected:row.get(3)?,recovery_snapshots:row.get(4)?,oldest_timestamp:row.get(5)?,last_successful_sync:row.get(6)?,last_data_loss:row.get(7)?},row.get(8)?))).map_err(Into::into)
}
pub(crate) fn client_config(
    db: &Connection,
    bearer: &str,
    origin: &str,
    peer: IpAddr,
) -> Result<Value> {
    if !loopback(peer) {
        return Err(anyhow!("loopback only"));
    }
    let client = authenticate(db, bearer, origin)?;
    let (queue, observed) = load_queue_health(db, client.id)?;
    Ok(
        json!({"schemaVersion":SCHEMA_VERSION,"clientId":client.instance_id,"privacyMode":client.privacy_mode,"selectedMediaOrigins":client.selected_media_origins,"queueHealth":queue_health_json(&queue,observed),"policyRevision":max_policy_revision(db)?,"heartbeatSeconds":HEARTBEAT_SECONDS,"leaseSeconds":LEASE_MS/1000,"maxBatchEvents":MAX_EVENTS,"defaultPort":DEFAULT_PORT}),
    )
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ClientConfigInput {
    schema_version: i64,
    client_id: String,
    privacy_mode: String,
    selected_media_origins: Vec<String>,
    queue_health: QueueHealthInput,
}
pub(crate) fn update_client_config(
    db: &Connection,
    bearer: &str,
    origin: &str,
    peer: IpAddr,
    input: ClientConfigInput,
) -> Result<Value> {
    if !loopback(peer) {
        return Err(anyhow!("loopback only"));
    }
    let client = authenticate(db, bearer, origin)?;
    let now = now_ms();
    if input.schema_version != SCHEMA_VERSION
        || input.client_id != client.instance_id
        || ![
            "off",
            "domains-only",
            "domains-and-titles",
            "selected-sites-media",
            "full-debug",
        ]
        .contains(&input.privacy_mode.as_str())
    {
        return Err(anyhow!("invalid client config"));
    }
    validate_queue_health(&input.queue_health, now)?;
    let selected = normalize_selected_origins(input.selected_media_origins)?;
    let selected_json = serde_json::to_string(&selected)?;
    db.execute(
        "UPDATE browser_clients SET privacy_mode=?,selected_media_origins_json=? WHERE id=?",
        params![input.privacy_mode, selected_json, client.id],
    )?;
    persist_queue_health(db, client.id, &input.queue_health, now)?;
    db.execute("INSERT INTO source_health(source,last_success_ts,success_count,detail_json)VALUES('browser',?,1,?) ON CONFLICT(source)DO UPDATE SET last_success_ts=excluded.last_success_ts,success_count=success_count+1,detail_json=excluded.detail_json,last_error_category=NULL",params![now,json!({"configSync":true,"queueHealth":queue_health_json(&input.queue_health,now)}).to_string()])?;
    client_config(db, bearer, origin, peer)
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicyScope {
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    excluded_domains: Vec<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicySchedule {
    #[serde(default = "all_weekdays")]
    weekdays: Vec<u8>,
    #[serde(default)]
    start_minute: u16,
    #[serde(default = "end_of_day")]
    end_minute: u16,
}
fn all_weekdays() -> Vec<u8> {
    vec![1, 2, 3, 4, 5, 6, 7]
}
fn end_of_day() -> u16 {
    1440
}
impl Default for PolicySchedule {
    fn default() -> Self {
        Self {
            weekdays: all_weekdays(),
            start_minute: 0,
            end_minute: 1440,
        }
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicyGrace {
    #[serde(default)]
    grace_seconds: i64,
    #[serde(default = "default_snooze")]
    snooze_seconds: i64,
    #[serde(default = "default_snoozes")]
    max_daily_snoozes: i64,
    #[serde(default = "default_session_gap")]
    session_gap_seconds: i64,
    #[serde(default = "default_rolling")]
    rolling_seconds: i64,
}
fn default_snooze() -> i64 {
    300
}
fn default_snoozes() -> i64 {
    2
}
fn default_session_gap() -> i64 {
    60
}
fn default_rolling() -> i64 {
    3600
}
impl Default for PolicyGrace {
    fn default() -> Self {
        Self {
            grace_seconds: 0,
            snooze_seconds: 300,
            max_daily_snoozes: 2,
            session_gap_seconds: 60,
            rolling_seconds: 3600,
        }
    }
}
#[derive(Clone, Debug)]
struct PolicyRow {
    id: i64,
    name: String,
    scope: PolicyScope,
    metric: String,
    window: String,
    budget: i64,
    warning: Option<i64>,
    enforcement: String,
    schedule: PolicySchedule,
    grace: PolicyGrace,
    revision: i64,
}
fn schedule_active(schedule: &PolicySchedule, now: i64) -> bool {
    if schedule.start_minute > 1440 || schedule.end_minute > 1440 {
        return false;
    }
    let utc = OffsetDateTime::from_unix_timestamp_nanos(now as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let local = utc.to_offset(UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC));
    let weekday = match local.weekday() {
        Weekday::Monday => 1,
        Weekday::Tuesday => 2,
        Weekday::Wednesday => 3,
        Weekday::Thursday => 4,
        Weekday::Friday => 5,
        Weekday::Saturday => 6,
        Weekday::Sunday => 7,
    };
    let selected = |day| schedule.weekdays.is_empty() || schedule.weekdays.contains(&day);
    let minute = local.hour() as u16 * 60 + local.minute() as u16;
    let (start, end) = (schedule.start_minute, schedule.end_minute);
    if start == end || (start == 0 && end == 1440) {
        return selected(weekday);
    }
    if start < end {
        return selected(weekday) && minute >= start && minute < end;
    }
    if minute >= start {
        return selected(weekday);
    }
    minute < end && selected(if weekday == 1 { 7 } else { weekday - 1 })
}
fn clip_spans_to_schedule(spans: Vec<(i64, i64)>, schedule: &PolicySchedule) -> Vec<(i64, i64)> {
    let mut clipped = Vec::new();
    for (start, end) in merge_spans(spans) {
        let mut cursor = start;
        while cursor < end {
            let next = end.min((cursor.div_euclid(60_000) + 1) * 60_000);
            if schedule_active(schedule, cursor) {
                clipped.push((cursor, next));
            }
            cursor = next;
        }
    }
    merge_spans(clipped)
}
fn domain_matches(domain: &str, suffix: &str) -> bool {
    domain == suffix
        || domain
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.'))
}
pub(crate) fn metric_counts(
    metric: &str,
    focused: bool,
    active: bool,
    visible: bool,
    audible: bool,
    playing: bool,
    os: &str,
) -> bool {
    match metric {
        "tab_active_seconds" => active,
        "browser_focused_seconds" => focused,
        "document_visible_seconds" => visible,
        "media_playback_seconds" => playing,
        "audible_playback_seconds" => playing && audible,
        "focused_playback_seconds" => {
            focused && active && visible && playing && os != "contradicted"
        }
        "verified_focused_playback_seconds" => {
            focused && active && visible && playing && os == "verified"
        }
        "background_playback_seconds" => {
            playing && (!focused || !active || !visible || os == "contradicted")
        }
        _ => false,
    }
}
fn merge_spans(mut spans: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    spans.sort_unstable();
    let mut merged = Vec::new();
    for (start, end) in spans.into_iter().filter(|(start, end)| end > start) {
        if let Some((_, right)) = merged.last_mut() {
            if start <= *right {
                *right = (*right).max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    merged
}
fn union_seconds(spans: Vec<(i64, i64)>) -> i64 {
    merge_spans(spans)
        .into_iter()
        .map(|(start, end)| end - start)
        .sum::<i64>()
        / 1000
}
fn continuous_session_spans(spans: Vec<(i64, i64)>, now: i64, gap_ms: i64) -> Vec<(i64, i64)> {
    let merged = merge_spans(spans);
    let Some(&(_, last_end)) = merged.last() else {
        return Vec::new();
    };
    if now - last_end > gap_ms {
        return Vec::new();
    }
    let mut start_index = merged.len() - 1;
    while start_index > 0 && merged[start_index].0 - merged[start_index - 1].1 <= gap_ms {
        start_index -= 1
    }
    merged[start_index..].to_vec()
}
fn metric_spans(
    db: &Connection,
    client_id: i64,
    domain: &str,
    metric: &str,
    from: i64,
    to: i64,
) -> Result<Vec<(i64, i64)>> {
    let mut spans = Vec::new();
    let mut stmt=db.prepare("SELECT start_ts,end_ts,domain,browser_focused,tab_active,document_visible,audible,media_playing,os_focus_state FROM browser_attention_intervals WHERE client_id=? AND end_ts>? AND start_ts<?")?;
    for row in stmt.query_map(params![client_id, from, to], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)? != 0,
            row.get::<_, i64>(4)? != 0,
            row.get::<_, i64>(5)? != 0,
            row.get::<_, i64>(6)? != 0,
            row.get::<_, i64>(7)? != 0,
            row.get::<_, String>(8)?,
        ))
    })? {
        let (start, end, row_domain, focused, active, visible, audible, playing, os) = row?;
        if row_domain
            .as_deref()
            .is_some_and(|value| domain_matches(value, domain))
            && metric_counts(metric, focused, active, visible, audible, playing, &os)
        {
            spans.push((start.max(from), end.min(to)));
        }
    }
    let family: String = db.query_row(
        "SELECT browser_family FROM browser_clients WHERE id=?",
        params![client_id],
        |row| row.get(0),
    )?;
    for state in load_client_states(db, client_id)? {
        let start = state.started.max(from);
        let end = state.expires.min(state.observed + LEASE_MS).min(to);
        if end > start
            && state
                .domain
                .as_deref()
                .is_some_and(|value| domain_matches(value, domain))
        {
            let (os, _) = reconcile_os(db, &family, start, end)?;
            if metric_counts(
                metric,
                state.focused,
                state.active,
                state.visible,
                state.audible,
                matches!(state.media.as_str(), "playing" | "seeking"),
                os,
            ) {
                spans.push((start, end));
            }
        }
    }
    Ok(spans)
}
fn load_policies(db: &Connection) -> Result<Vec<PolicyRow>> {
    let mut stmt=db.prepare("SELECT id,name,scope_json,metric,window_kind,budget_seconds,warning_seconds,enforcement,schedule_json,grace_json,revision FROM attention_policies WHERE enabled=1 ORDER BY id LIMIT 256")?;
    let raw = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(
            |(
                id,
                name,
                scope,
                metric,
                window,
                budget,
                warning,
                enforcement,
                schedule,
                grace,
                revision,
            )| {
                Ok(PolicyRow {
                    id,
                    name,
                    scope: serde_json::from_str(&scope)
                        .map_err(|_| anyhow!("invalid policy scope"))?,
                    metric,
                    window,
                    budget,
                    warning,
                    enforcement,
                    schedule: serde_json::from_str(&schedule)
                        .map_err(|_| anyhow!("invalid policy schedule"))?,
                    grace: serde_json::from_str(&grace)
                        .map_err(|_| anyhow!("invalid policy grace"))?,
                    revision,
                })
            },
        )
        .collect()
}
fn policy_json(policy: &PolicyRow) -> Value {
    json!({"id":policy.id,"name":policy.name,"revision":policy.revision,"enabled":true,"scope":{"domains":policy.scope.domains,"excludedDomains":policy.scope.excluded_domains},"metric":policy.metric,"windowKind":policy.window,"budgetSeconds":policy.budget,"warningSeconds":policy.warning,"enforcement":policy.enforcement,"schedule":{"weekdays":policy.schedule.weekdays,"startMinute":policy.schedule.start_minute,"endMinute":policy.schedule.end_minute},"grace":{"graceSeconds":policy.grace.grace_seconds,"snoozeSeconds":policy.grace.snooze_seconds,"maxDailySnoozes":policy.grace.max_daily_snoozes,"sessionGapSeconds":policy.grace.session_gap_seconds,"rollingSeconds":policy.grace.rolling_seconds}})
}
fn evaluate_policies(
    db: &Connection,
    client_id: i64,
    domain: &str,
    now: i64,
) -> Result<(Vec<Value>, Vec<Value>)> {
    let domain = normalize_domain(domain)?;
    let policies = load_policies(db)?;
    let mut serialized = Vec::new();
    let mut decisions = Vec::new();
    for policy in policies {
        serialized.push(policy_json(&policy));
        let domains = policy
            .scope
            .domains
            .iter()
            .map(|raw| normalize_domain(raw))
            .collect::<Result<Vec<_>>>()?;
        let excluded = policy
            .scope
            .excluded_domains
            .iter()
            .map(|raw| normalize_domain(raw))
            .collect::<Result<Vec<_>>>()?;
        let Some(scope) = domains
            .iter()
            .filter(|suffix| domain_matches(&domain, suffix))
            .max_by_key(|suffix| suffix.len())
        else {
            continue;
        };
        if excluded
            .iter()
            .any(|suffix| domain_matches(&domain, suffix))
        {
            continue;
        }
        let from = match policy.window.as_str() {
            "calendar-day" => crate::context::local_midnight_ms(now),
            "rolling" => now - policy.grace.rolling_seconds.clamp(1, 604800) * 1000,
            "session" => now - 86_400_000,
            _ => continue,
        };
        let mut spans = clip_spans_to_schedule(
            metric_spans(db, client_id, scope, &policy.metric, from, now)?,
            &policy.schedule,
        );
        if policy.window == "session" {
            spans = continuous_session_spans(
                spans,
                now,
                policy.grace.session_gap_seconds.clamp(1, 3600) * 1000,
            );
        }
        let used = union_seconds(spans);
        let active_schedule = schedule_active(&policy.schedule, now);
        let override_action:Option<String>=db.query_row("SELECT action FROM policy_overrides WHERE policy_id=? AND (client_id IS NULL OR client_id=?) AND start_ts<=? AND end_ts>? ORDER BY end_ts DESC LIMIT 1",params![policy.id,client_id,now,now],|row|row.get(0)).optional()?;
        let effective_budget = policy
            .budget
            .saturating_add(policy.grace.grace_seconds.max(0));
        let remaining = (effective_budget - used).max(0);
        let (action, reason) = if !active_schedule {
            ("allow", "Outside policy schedule")
        } else if override_action.is_some() {
            ("allow", "Policy temporarily overridden")
        } else if policy.enforcement == "observe" {
            ("observe", "Observe-only policy")
        } else if used >= effective_budget {
            (policy.enforcement.as_str(), "Attention budget exhausted")
        } else if policy.warning.is_some_and(|warning| remaining <= warning) {
            ("warn", "Attention budget nearly exhausted")
        } else {
            ("allow", "Within attention budget")
        };
        decisions.push(json!({"policyId":policy.id,"revision":policy.revision,"enforcement":policy.enforcement,"action":action,"usedSeconds":used,"budgetSeconds":policy.budget,"remainingSeconds":remaining,"reason":reason,"degraded":false}));
    }
    Ok((serialized, decisions))
}
fn strongest_decision(decisions: &[Value]) -> Option<&Value> {
    fn rank(action: &str) -> i32 {
        match action {
            "hard-block" => 4,
            "soft-block" => 3,
            "warn" => 2,
            "observe" => 1,
            _ => 0,
        }
    }
    decisions.iter().max_by(|left, right| {
        rank(left["action"].as_str().unwrap_or("allow"))
            .cmp(&rank(right["action"].as_str().unwrap_or("allow")))
            .then_with(|| {
                let l = left["remainingSeconds"].as_i64().unwrap_or(i64::MAX);
                let r = right["remainingSeconds"].as_i64().unwrap_or(i64::MAX);
                r.cmp(&l)
            })
    })
}

pub(crate) fn policy_state(
    db: &Connection,
    bearer: &str,
    origin: &str,
    peer: IpAddr,
    domain: Option<&str>,
) -> Result<Value> {
    if !loopback(peer) {
        return Err(anyhow!("loopback only"));
    }
    let client = authenticate(db, bearer, origin)?;
    let now = now_ms();
    let (policies, decisions) = if let Some(domain) = domain {
        evaluate_policies(db, client.id, domain, now)?
    } else {
        (
            load_policies(db)?.iter().map(policy_json).collect(),
            Vec::new(),
        )
    };
    Ok(
        json!({"schemaVersion":SCHEMA_VERSION,"revision":max_policy_revision(db)?,"serverTimestamp":now,"policies":policies,"decisions":decisions}),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InterventionInput {
    schema_version: i64,
    policy_id: i64,
    action: String,
    decision_revision: i64,
    event_timestamp: i64,
    #[serde(default)]
    remaining_seconds: Option<i64>,
}
pub(crate) fn record_intervention(
    db: &Connection,
    bearer: &str,
    origin: &str,
    peer: IpAddr,
    input: InterventionInput,
) -> Result<Value> {
    if !loopback(peer) {
        return Err(anyhow!("loopback only"));
    }
    let client = authenticate(db, bearer, origin)?;
    if input.schema_version != SCHEMA_VERSION
        || ![
            "warn",
            "soft-block",
            "hard-block",
            "continue",
            "snooze",
            "override",
        ]
        .contains(&input.action.as_str())
    {
        return Err(anyhow!("invalid intervention action"));
    }
    let now = now_ms();
    if (input.event_timestamp - now).abs() > MAX_FUTURE_MS {
        return Err(anyhow!("invalid intervention timestamp"));
    }
    let (policy_revision, grace_json): (i64, String) = db
        .query_row(
            "SELECT revision,grace_json FROM attention_policies WHERE id=? AND enabled=1",
            params![input.policy_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| anyhow!("unknown policy"))?;
    if policy_revision != input.decision_revision {
        return Err(anyhow!("stale policy decision"));
    }
    let grace: PolicyGrace =
        serde_json::from_str(&grace_json).map_err(|_| anyhow!("invalid policy grace"))?;
    if matches!(input.action.as_str(), "warn" | "soft-block" | "hard-block") {
        let mut authorized = false;
        for domain in load_client_states(db, client.id)?
            .into_iter()
            .filter_map(|state| state.domain)
        {
            let (_, decisions) = evaluate_policies(db, client.id, &domain, now)?;
            authorized |= decisions.iter().any(|decision| {
                decision["policyId"].as_i64() == Some(input.policy_id)
                    && decision["revision"].as_i64() == Some(input.decision_revision)
                    && decision["action"].as_str() == Some(input.action.as_str())
            });
        }
        if !authorized {
            return Err(anyhow!(
                "reported application does not match current server decision"
            ));
        }
    }
    if matches!(input.action.as_str(), "continue" | "snooze" | "override") {
        if input.action == "snooze" {
            let day = crate::context::local_midnight_ms(now);
            let used:i64=db.query_row("SELECT COUNT(*) FROM intervention_events WHERE policy_id=? AND client_id=? AND action='snooze' AND event_ts>=?",params![input.policy_id,client.id,day],|row|row.get(0))?;
            if used >= grace.max_daily_snoozes {
                return Err(anyhow!("daily snooze limit reached"));
            }
        }
        let seconds = match input.action.as_str() {
            "continue" => grace.grace_seconds.max(1),
            "snooze" => grace.snooze_seconds.max(1),
            "override" => 3600,
            _ => 0,
        };
        db.execute("INSERT INTO policy_overrides(policy_id,client_id,action,start_ts,end_ts,created_ts)VALUES(?,?,?,?,?,?)",params![input.policy_id,client.id,input.action,now,now+seconds*1000,now])?;
    }
    let duplicate:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM intervention_events WHERE policy_id=? AND client_id=? AND action=? AND decision_revision=? AND event_ts BETWEEN ? AND ?)",params![input.policy_id,client.id,input.action,input.decision_revision,input.event_timestamp-1000,input.event_timestamp+1000],|row|row.get(0))?;
    if !duplicate {
        db.execute("INSERT INTO intervention_events(policy_id,client_id,event_ts,action,remaining_seconds,decision_revision)VALUES(?,?,?,?,?,?)",params![input.policy_id,client.id,input.event_timestamp,input.action,input.remaining_seconds,input.decision_revision])?;
    }
    Ok(json!({"schemaVersion":SCHEMA_VERSION,"accepted":true,"duplicate":duplicate}))
}

pub(crate) fn revoke_client(
    db: &Connection,
    bearer: &str,
    origin: &str,
    peer: IpAddr,
) -> Result<Value> {
    if !loopback(peer) {
        return Err(anyhow!("loopback only"));
    }
    let client = authenticate(db, bearer, origin)?;
    close_client_states(
        db,
        client.id,
        &client.browser_family,
        now_ms(),
        "client_revoked",
    )?;
    db.execute(
        "UPDATE browser_clients SET revoked_ts=? WHERE id=?",
        params![now_ms(), client.id],
    )?;
    Ok(json!({"schemaVersion":SCHEMA_VERSION,"revoked":true}))
}
pub(crate) fn clients(db: &Connection) -> Result<Value> {
    let mut stmt=db.prepare("SELECT id,client_instance_id,display_name,browser_family,extension_version,allowed_origin,privacy_mode,selected_media_origins_json,paired_ts,last_seen_ts,queue_depth,queue_bytes,queue_dropped,queue_permanent_rejected,queue_recovery_snapshots,queue_oldest_ts,queue_last_success_ts,queue_last_data_loss_ts,queue_health_observed_ts,revoked_ts FROM browser_clients ORDER BY id")?;
    let raw = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<i64>>(9)?,
                QueueHealthInput {
                    depth: row.get(10)?,
                    bytes: row.get(11)?,
                    dropped: row.get(12)?,
                    permanent_rejected: row.get(13)?,
                    recovery_snapshots: row.get(14)?,
                    oldest_timestamp: row.get(15)?,
                    last_successful_sync: row.get(16)?,
                    last_data_loss: row.get(17)?,
                },
                row.get::<_, Option<i64>>(18)?.unwrap_or(0),
                row.get::<_, Option<i64>>(19)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let rows=raw.into_iter().map(|(id,client_id,display,family,version,allowed,mode,selected,paired,last_seen,queue,observed,revoked)|Ok(json!({"id":id,"clientId":client_id,"displayName":display,"browserFamily":family,"extensionVersion":version,"allowedOrigin":allowed,"privacyMode":mode,"selectedMediaOrigins":serde_json::from_str::<Vec<String>>(&selected)?,"pairedTimestamp":paired,"lastSeenTimestamp":last_seen,"queueHealth":queue_health_json(&queue,observed),"revokedTimestamp":revoked}))).collect::<Result<Vec<_>>>()?;
    Ok(json!({"schemaVersion":SCHEMA_VERSION,"clients":rows}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;
    fn db() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        init_db(&mut db).unwrap();
        db
    }
    fn queue(now: i64) -> QueueHealthInput {
        QueueHealthInput {
            depth: 0,
            bytes: 2,
            dropped: 0,
            permanent_rejected: 0,
            recovery_snapshots: 0,
            oldest_timestamp: None,
            last_successful_sync: Some(now),
            last_data_loss: None,
        }
    }
    fn pair_fixture(db: &mut Connection, mode: &str) -> (String, String) {
        let code = create_pairing_code(db).unwrap()["code"]
            .as_str()
            .unwrap()
            .to_string();
        let input = PairInput {
            schema_version: 1,
            pairing_code: code,
            client_id: "11111111-1111-4111-8111-111111111111".into(),
            display_name: "fixture".into(),
            browser_family: "firefox".into(),
            extension_version: "1.0.0".into(),
            privacy_mode: mode.into(),
            selected_media_origins: vec!["https://youtube.com".into(), "https://twitch.tv".into()],
        };
        let response = pair(
            db,
            input,
            "moz-extension://fixture",
            "127.0.0.1".parse().unwrap(),
        )
        .unwrap();
        (
            response["token"].as_str().unwrap().into(),
            response["clientId"].as_str().unwrap().into(),
        )
    }
    fn event(
        client: &str,
        key: &str,
        seq: i64,
        ts: i64,
        window: i64,
        tab: i64,
        kind: EventType,
        domain: &str,
    ) -> BrowserEventInput {
        BrowserEventInput {
            payload_version: 1,
            event_key: key.into(),
            client_sequence: seq,
            event_timestamp: ts,
            client_id: client.into(),
            event_type: kind,
            window_id: window,
            tab_id: tab,
            domain: Some(domain.into()),
            origin: Some(format!("https://{domain}")),
            title: None,
            active: true,
            browser_focused: true,
            document_visible: true,
            audible: false,
            media_state: MediaState::None,
            picture_in_picture: false,
            media_position: None,
            media_duration: None,
            playback_rate: None,
            private: false,
        }
    }
    #[test]
    fn shared_contract_fixtures_deserialize_strictly() {
        let pair: PairInput =
            serde_json::from_str(include_str!("../contracts/browser/v1/pair-request.json"))
                .unwrap();
        assert_eq!(pair.schema_version, 1);
        let batch: EventBatch =
            serde_json::from_str(include_str!("../contracts/browser/v1/event-batch.json")).unwrap();
        assert_eq!(batch.events[0].window_id, 4);
        let intervention: InterventionInput = serde_json::from_str(include_str!(
            "../contracts/browser/v1/intervention-request.json"
        ))
        .unwrap();
        assert_eq!(intervention.action, "soft-block");
        let config: ClientConfigInput =
            serde_json::from_str(include_str!("../contracts/browser/v1/config-request.json"))
                .unwrap();
        assert_eq!(config.queue_health.depth, 2);
        let policy: crate::activity::PolicyInput =
            serde_json::from_str(include_str!("../contracts/browser/v1/policy-create.json"))
                .unwrap();
        assert_eq!(policy.metric, "focused_playback_seconds");
        let mut hostile: Value =
            serde_json::from_str(include_str!("../contracts/browser/v1/event-batch.json")).unwrap();
        hostile["events"][0]["url"] = json!("https://example.com/private");
        assert!(serde_json::from_value::<EventBatch>(hostile).is_err());
        let mut hostile_policy: Value =
            serde_json::from_str(include_str!("../contracts/browser/v1/policy-create.json"))
                .unwrap();
        hostile_policy["freeTextReason"] = json!("private");
        assert!(serde_json::from_value::<crate::activity::PolicyInput>(hostile_policy).is_err());
    }
    #[test]
    fn hostile_privacy_and_numeric_payloads_are_rejected() {
        let mut db = db();
        let (token, client) = pair_fixture(&mut db, "domains-only");
        assert!(client_config(
            &db,
            "",
            "moz-extension://fixture",
            "127.0.0.1".parse().unwrap()
        )
        .is_err());
        let now = now_ms();
        let mut media = event(
            &client,
            "event-media",
            1,
            now,
            1,
            1,
            EventType::Snapshot,
            "example.com",
        );
        media.media_state = MediaState::Playing;
        let response = ingest(
            &mut db,
            &token,
            "moz-extension://fixture",
            "127.0.0.1".parse().unwrap(),
            EventBatch {
                schema_version: 1,
                queue_health: queue(now),
                events: vec![media],
            },
        )
        .unwrap();
        assert_eq!(response["rejected"][0]["permanent"], true);
        let raw = r#"{"schemaVersion":1,"events":[],"metadata":{"secret":true}}"#;
        assert!(serde_json::from_str::<EventBatch>(raw).is_err());
    }
    #[test]
    fn queue_health_bounds_are_enforced_before_ingestion() {
        let mut db = db();
        let (token, client) = pair_fixture(&mut db, "domains-only");
        let now = now_ms();
        let mut health = queue(now);
        health.depth = 1001;
        let result = ingest(
            &mut db,
            &token,
            "moz-extension://fixture",
            "127.0.0.1".parse().unwrap(),
            EventBatch {
                schema_version: 1,
                queue_health: health,
                events: vec![event(
                    &client,
                    "queue-event",
                    1,
                    now,
                    1,
                    1,
                    EventType::Snapshot,
                    "example.com",
                )],
            },
        );
        assert!(result.unwrap_err().to_string().contains("queue health"));
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM browser_event_receipts", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn selected_media_is_rejected_outside_persisted_origin_allowlist() {
        let mut db = db();
        let (token, client) = pair_fixture(&mut db, "selected-sites-media");
        let now = now_ms();
        let mut allowed = event(
            &client,
            "allowed-media",
            1,
            now,
            1,
            1,
            EventType::MediaPlay,
            "youtube.com",
        );
        allowed.media_state = MediaState::Playing;
        let accepted = ingest(
            &mut db,
            &token,
            "moz-extension://fixture",
            "127.0.0.1".parse().unwrap(),
            EventBatch {
                schema_version: 1,
                queue_health: queue(now),
                events: vec![allowed],
            },
        )
        .unwrap();
        assert_eq!(accepted["accepted"], 1);
        let mut hostile = event(
            &client,
            "hostile-media",
            2,
            now + 1,
            1,
            2,
            EventType::MediaPlay,
            "example.com",
        );
        hostile.media_state = MediaState::Playing;
        let rejected = ingest(
            &mut db,
            &token,
            "moz-extension://fixture",
            "127.0.0.1".parse().unwrap(),
            EventBatch {
                schema_version: 1,
                queue_health: queue(now + 1),
                events: vec![hostile],
            },
        )
        .unwrap();
        assert_eq!(rejected["accepted"], 0);
        assert!(rejected["rejected"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not selected"));
    }
    #[test]
    fn multiple_tabs_windows_pip_close_and_same_batch_gap_are_independent() {
        let mut db = db();
        let (token, client) = pair_fixture(&mut db, "selected-sites-media");
        let now = now_ms();
        let mut first = event(
            &client,
            "event-0001",
            1,
            now,
            1,
            1,
            EventType::Snapshot,
            "youtube.com",
        );
        first.media_state = MediaState::Playing;
        let mut second = event(
            &client,
            "event-0002",
            2,
            now + 1000,
            2,
            2,
            EventType::Snapshot,
            "twitch.tv",
        );
        second.active = false;
        second.browser_focused = false;
        second.document_visible = false;
        second.media_state = MediaState::Playing;
        second.picture_in_picture = true;
        let mut late = event(
            &client,
            "event-0003",
            3,
            now + LEASE_MS + 60_000,
            1,
            1,
            EventType::Navigation,
            "example.com",
        );
        late.active = true;
        let response = ingest(
            &mut db,
            &token,
            "moz-extension://fixture",
            "127.0.0.1".parse().unwrap(),
            EventBatch {
                schema_version: 1,
                queue_health: queue(now),
                events: vec![first, second, late],
            },
        )
        .unwrap();
        assert_eq!(response["accepted"], 3);
        let states: i64 = db
            .query_row("SELECT COUNT(*) FROM browser_current_states", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(states, 2);
        let capped: i64 = db
            .query_row(
                "SELECT MAX(end_ts-start_ts) FROM browser_attention_intervals WHERE tab_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(capped, LEASE_MS);
        let mut close = event(
            &client,
            "event-0004",
            4,
            now + LEASE_MS + 61_000,
            2,
            2,
            EventType::TabClose,
            "twitch.tv",
        );
        close.active = false;
        close.browser_focused = false;
        close.document_visible = false;
        close.media_state = MediaState::None;
        ingest(
            &mut db,
            &token,
            "moz-extension://fixture",
            "127.0.0.1".parse().unwrap(),
            EventBatch {
                schema_version: 1,
                queue_health: queue(now),
                events: vec![close],
            },
        )
        .unwrap();
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM browser_current_states WHERE window_id=2 AND tab_id=2",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
    #[test]
    fn os_verification_requires_full_overlap_and_reports_mixed() {
        let db = db();
        let start = now_ms() - 10_000;
        db.execute("INSERT INTO focus_context_intervals(start_ts,end_ts,last_observed_ts,context_key,extractor_version,focused_app,ambiguity_json,closed_reason)VALUES(?,?,?,?,1,'firefox','{}','test')",params![start,start+5_000,start+5_000,vec![1u8;32]]).unwrap();
        db.execute("INSERT INTO focus_context_intervals(start_ts,end_ts,last_observed_ts,context_key,extractor_version,focused_app,ambiguity_json,closed_reason)VALUES(?,?,?,?,1,'terminal','{}','test')",params![start+5_000,start+10_000,start+10_000,vec![2u8;32]]).unwrap();
        assert_eq!(
            reconcile_os(&db, "firefox", start, start + 10_000)
                .unwrap()
                .0,
            "mixed"
        );
        assert_eq!(
            reconcile_os(&db, "firefox", start, start + 5_000)
                .unwrap()
                .0,
            "verified"
        );
    }
    #[test]
    fn schedule_clipping_handles_day_and_overnight_boundaries() {
        let midnight = crate::context::local_midnight_ms(now_ms());
        let end = crate::context::next_local_midnight_ms(midnight);
        let day = PolicySchedule {
            weekdays: all_weekdays(),
            start_minute: 60,
            end_minute: 120,
        };
        assert_eq!(
            union_seconds(clip_spans_to_schedule(vec![(midnight, end)], &day)),
            3600
        );
        let overnight = PolicySchedule {
            weekdays: all_weekdays(),
            start_minute: 23 * 60,
            end_minute: 60,
        };
        assert_eq!(
            union_seconds(clip_spans_to_schedule(vec![(midnight, end)], &overnight)),
            7200
        );
    }
    #[test]
    fn overnight_schedule_attributes_after_midnight_to_previous_weekday() {
        let mut sunday = crate::context::local_midnight_ms(now_ms());
        while crate::context::local_date(sunday).weekday() != Weekday::Sunday {
            sunday = crate::context::next_local_midnight_ms(sunday);
        }
        let monday = crate::context::next_local_midnight_ms(sunday);
        let tuesday = crate::context::next_local_midnight_ms(monday);
        let schedule = |weekdays| PolicySchedule {
            weekdays,
            start_minute: 23 * 60,
            end_minute: 60,
        };
        assert!(schedule_active(
            &schedule(vec![1]),
            monday + 23 * 3_600_000 + 30 * 60_000
        ));
        assert!(schedule_active(&schedule(vec![1]), tuesday + 30 * 60_000));
        assert!(!schedule_active(&schedule(vec![2]), tuesday + 30 * 60_000));
        assert!(!schedule_active(&schedule(vec![1]), tuesday + 60 * 60_000));
        assert!(schedule_active(&schedule(vec![7]), monday + 30 * 60_000));
        assert!(!schedule_active(&schedule(vec![1]), monday + 30 * 60_000));
    }
    #[test]
    fn evaluate_policies_counts_only_scheduled_history() {
        let mut db = db();
        let (_token, _client) = pair_fixture(&mut db, "domains-only");
        let client_id: i64 = db
            .query_row("SELECT id FROM browser_clients", [], |row| row.get(0))
            .unwrap();
        let midnight = crate::context::local_midnight_ms(now_ms());
        let now = midnight + 3 * 3_600_000;
        db.execute("INSERT INTO browser_attention_intervals(client_id,window_id,tab_id,start_ts,end_ts,episode_key,domain,origin,browser_focused,tab_active,document_visible,media_playing,audible,picture_in_picture,os_focus_verified,os_focus_state,confidence,closed_reason)VALUES(1,1,1,?,?,?,?,?,1,1,1,1,0,0,1,'verified',1.0,'test')",params![midnight,now,vec![7u8;32],"youtube.com","https://youtube.com"]).unwrap();
        db.execute("INSERT INTO attention_policies(name,scope_json,metric,window_kind,budget_seconds,warning_seconds,enforcement,schedule_json,grace_json,revision,created_ts,updated_ts)VALUES('scheduled',?,'media_playback_seconds','rolling',7200,300,'warn',?,?,1,?,?)",params![json!({"domains":["youtube.com"],"excludedDomains":[]}).to_string(),json!({"weekdays":[1,2,3,4,5,6,7],"startMinute":60,"endMinute":120}).to_string(),json!({"graceSeconds":0,"snoozeSeconds":300,"maxDailySnoozes":2,"sessionGapSeconds":60,"rollingSeconds":14400}).to_string(),now,now]).unwrap();
        let (_, decisions) = evaluate_policies(&db, client_id, "youtube.com", now).unwrap();
        assert_eq!(decisions[0]["usedSeconds"], 3600);
        assert_eq!(decisions[0]["action"], "allow");
    }
    #[test]
    fn overlapping_tabs_are_union_counted_and_policy_modes_serialize() {
        let mut db = db();
        let (_token, _client) = pair_fixture(&mut db, "domains-only");
        let client_id: i64 = db
            .query_row("SELECT id FROM browser_clients", [], |row| row.get(0))
            .unwrap();
        let now = now_ms();
        for (tab, start, end) in [(1, now - 60_000, now), (2, now - 30_000, now)] {
            db.execute("INSERT INTO browser_attention_intervals(client_id,window_id,tab_id,start_ts,end_ts,episode_key,domain,origin,browser_focused,tab_active,document_visible,media_playing,audible,picture_in_picture,os_focus_verified,os_focus_state,confidence,closed_reason)VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![client_id,1,tab,start,end,vec![tab as u8;32],"youtube.com","https://youtube.com",1,1,1,1,0,0,1,"verified",1.0,"test"]).unwrap();
        }
        assert_eq!(
            union_seconds(
                metric_spans(
                    &db,
                    client_id,
                    "youtube.com",
                    "focused_playback_seconds",
                    now - 120_000,
                    now
                )
                .unwrap()
            ),
            60
        );
        for (enforcement, id) in [
            ("observe", 1),
            ("warn", 2),
            ("soft-block", 3),
            ("hard-block", 4),
        ] {
            db.execute("INSERT INTO attention_policies(name,scope_json,metric,window_kind,budget_seconds,warning_seconds,enforcement,schedule_json,grace_json,revision,created_ts,updated_ts)VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",params![enforcement,json!({"domains":["youtube.com"],"excludedDomains":[]}).to_string(),"focused_playback_seconds","rolling",if id==1{120}else{30},10,enforcement,json!({"weekdays":[1,2,3,4,5,6,7],"startMinute":0,"endMinute":1440}).to_string(),json!({"graceSeconds":0,"snoozeSeconds":300,"maxDailySnoozes":2,"sessionGapSeconds":60,"rollingSeconds":3600}).to_string(),id,now,now]).unwrap();
        }
        let (_, decisions) = evaluate_policies(&db, client_id, "youtube.com", now).unwrap();
        assert_eq!(decisions.len(), 4);
        assert!(decisions
            .iter()
            .any(|decision| decision["action"] == "soft-block"));
        assert!(decisions
            .iter()
            .any(|decision| decision["action"] == "hard-block"));
    }
}
