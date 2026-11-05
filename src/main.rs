//! usclaude : affiche dans la zone de notification les limites d'usage de
//! Claude Code, les mêmes que la commande `/usage`.
//!
//! Le jeton OAuth est lu dans `~/.claude/.credentials.json`. Expiré, il est rafraîchi
//! ici comme le ferait Claude Code, qui n'y touche pas si l'on n'utilise que Claude
//! Desktop.
//!
//! Claude Code (par défaut ou `--claude`), Codex (`--codex`) et Kimi Code (`--kimi`)
//! ont chacun leur icône, leurs réglages, leur démarrage automatique et leur cache.
//! L'ancien affichage groupé reste disponible avec `--all`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use chrono::{DateTime, Datelike, Local};
use ksni::blocking::TrayMethods;
use serde_json::Value;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Endpoint non documenté, celui qu'utilise la commande `/status` de Codex.
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Endpoint de `/usage` de Kimi Code ; `KIMI_CODE_BASE_URL` peut remplacer sa base.
const KIMI_USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";
/// Rafraîchissement des jetons : mêmes adresses et identifiants que Claude Code et Codex.
const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Intervalles de rafraîchissement proposés (secondes, libellé) ; le premier par défaut.
const INTERVALS: [(u64, &str); 4] = [(90, "90 s"), (180, "3 min"), (300, "5 min"), (600, "10 min")];

/// Réponse 429 du service : sans délai indiqué, l'attente double à chaque refus jusqu'à
/// `MAX_BACKOFF` ; un délai `Retry-After` est respecté jusqu'à `MAX_RETRY_AFTER` (secondes).
const MAX_BACKOFF: u64 = 600;
const MAX_RETRY_AFTER: u64 = 3600;

/// Limites connues, dans l'ordre d'affichage : (clé, libellé français, libellé anglais).
const KNOWN: [(&str, &str, &str); 4] = [
    ("five_hour", "Session (5 h)", "Session (5h)"),
    ("seven_day", "Semaine, tous modèles", "Weekly, all models"),
    ("seven_day_opus", "Semaine, Opus", "Weekly, Opus"),
    ("seven_day_sonnet", "Semaine, Sonnet", "Weekly, Sonnet"),
];

// ---------- Langue ----------

/// Interface en français si la locale l'est, en anglais sinon. Fixée au démarrage.
static FRENCH: AtomicBool = AtomicBool::new(false);

fn french() -> bool {
    FRENCH.load(Ordering::Relaxed)
}

/// Texte dans la langue de l'interface.
fn tr(fr: &'static str, en: &'static str) -> &'static str {
    if french() { fr } else { en }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Provider {
    Claude,
    Codex,
    Kimi,
}

impl Provider {
    const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Kimi];

    fn index(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Codex => 1,
            Self::Kimi => 2,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Kimi => "Kimi Code",
        }
    }

    /// Nom du cache, également utilisé par l'instance du mode individuel correspondant.
    fn app(self) -> &'static str {
        match self {
            Self::Claude => "usclaude",
            Self::Codex => "usclaude-codex",
            Self::Kimi => "usclaude-kimi",
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Claude => "--claude",
            Self::Codex => "--codex",
            Self::Kimi => "--kimi",
        }
    }

    /// Les deux limites représentées dans la barre des tâches, dans l'ordre d'affichage.
    fn gauge_keys(self) -> (&'static str, &'static str) {
        match self {
            Self::Claude | Self::Codex => ("five_hour", "seven_day"),
            Self::Kimi => ("five_hour", "monthly"),
        }
    }

    fn color(self) -> [u8; 3] {
        match self {
            Self::Claude => [0x90, 0x90, 0x90],
            Self::Codex => [0x5c, 0x9d, 0xf0],
            Self::Kimi => [0x9c, 0x6c, 0xf0],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    All,
    Single(Provider),
}

impl Mode {
    fn providers(self) -> &'static [Provider] {
        match self {
            Self::All => &Provider::ALL,
            Self::Single(Provider::Claude) => &[Provider::Claude],
            Self::Single(Provider::Codex) => &[Provider::Codex],
            Self::Single(Provider::Kimi) => &[Provider::Kimi],
        }
    }

    /// Nom de l'instance : sert au verrou, aux réglages et au démarrage automatique.
    fn app(self) -> &'static str {
        match self {
            Self::All => "usclaude",
            Self::Single(provider) => provider.app(),
        }
    }
}

fn mode_from_flags(claude: bool, codex: bool, kimi: bool, all: bool) -> Option<Mode> {
    match (claude, codex, kimi, all) {
        // Comme à l'origine, le lancement sans option affiche Claude seul.
        (false, false, false, false) => Some(Mode::Single(Provider::Claude)),
        (true, false, false, false) => Some(Mode::Single(Provider::Claude)),
        (false, true, false, false) => Some(Mode::Single(Provider::Codex)),
        (false, false, true, false) => Some(Mode::Single(Provider::Kimi)),
        // L'ancien affichage groupé reste disponible explicitement.
        (false, false, false, true) => Some(Mode::All),
        _ => None,
    }
}

/// Français si la première variable renseignée parmi LANGUAGE, LC_ALL, LC_MESSAGES et
/// LANG (dans cet ordre) commence par « fr ».
fn locale_is_french(values: &[Option<String>]) -> bool {
    values.iter().flatten().find(|v| !v.is_empty()).is_some_and(|v| v.starts_with("fr"))
}

struct Limit {
    key: String,
    label: String,
    pct: f64,
    resets_at: Option<DateTime<Local>>,
}

struct Usage {
    plan: Option<String>,
    limits: Vec<Limit>,
    fetched: DateTime<Local>,
}

impl Usage {
    fn pct(&self, key: &str) -> Option<f64> {
        self.limits.iter().find(|l| l.key == key).map(|l| l.pct)
    }
}

// ---------- Récupération ----------

/// Le refus 429 est à part : il règle l'attente avant le prochain essai.
enum FetchError {
    /// « Trop de requêtes », avec le délai demandé par le service (`Retry-After`, secondes).
    RateLimited(Option<u64>),
    Other(String),
}

impl From<String> for FetchError {
    fn from(e: String) -> Self {
        Self::Other(e)
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::RateLimited(_) => {
                f.write_str(tr("trop de requêtes, nouvel essai plus tard", "too many requests, will retry later"))
            }
            Self::Other(e) => f.write_str(e),
        }
    }
}

fn claude_dir() -> String {
    std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| format!("{}/.claude", home()))
}

fn credentials_path() -> String {
    format!("{}/.credentials.json", claude_dir())
}

/// Renvoie (contenu brut, JSON).
fn read_json(path: &str) -> Result<(String, Value), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}{}{e}", tr(" : ", ": ")))?;
    let json = serde_json::from_str(&text).map_err(|e| format!("{path}{}{e}", tr(" : ", ": ")))?;
    Ok((text, json))
}

/// Réécrit le fichier de jetons sans passer par un état à moitié écrit (fichier
/// temporaire puis renommage), lisible par son seul propriétaire, dans la même mise en
/// forme qu'avant (sur une ligne ou indentée).
fn write_json(path: &str, old_text: &str, json: &Value) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let text = if old_text.contains('\n') {
        serde_json::to_string_pretty(json)
    } else {
        serde_json::to_string(json)
    }
    .map_err(|e| e.to_string())?;
    let tmp = format!("{path}.usclaude.tmp");
    let err = |e: std::io::Error| format!("{path}{}{e}", tr(" : ", ": "));
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(err)?;
    file.write_all(text.as_bytes()).and_then(|_| file.sync_all()).map_err(err)?;
    std::fs::rename(&tmp, path).map_err(err)
}

/// Demande de nouveaux jetons au service. Un refus (400, 401) veut dire que le jeton de
/// rafraîchissement n'est plus valable : il faut se reconnecter.
fn post_refresh(agent: &ureq::Agent, url: &str, body: &Value, relogin: &str) -> Result<Value, String> {
    let mut resp = agent
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", concat!("usclaude/", env!("CARGO_PKG_VERSION")))
        .send(body.to_string())
        .map_err(|e| format!("{}{e}", tr("rafraîchissement du jeton : ", "token refresh: ")))?;
    match resp.status().as_u16() {
        200..=299 => {}
        400 | 401 => return Err(relogin.to_owned()),
        s => return Err(format!("{} {s}", tr("rafraîchissement du jeton : réponse HTTP", "token refresh: HTTP response"))),
    }
    let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("{}{e}", tr("rafraîchissement du jeton : ", "token refresh: ")))
}

/// Verrou de Claude Code pendant qu'il rafraîchit son jeton : le répertoire
/// `~/.claude.lock` (format proper-lockfile). Le prendre aussi évite que les deux
/// utilisent le même jeton de rafraîchissement, qui ne sert qu'une fois.
struct DirLock(String);

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn lock_dir(path: String) -> Option<DirLock> {
    if std::fs::create_dir(&path).is_err() {
        // Laissé par un processus tué : proper-lockfile le tient pour périmé après 10 s
        // sans mise à jour. 30 s ici, par prudence.
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|e| e > Duration::from_secs(30)));
        if !stale || std::fs::remove_dir(&path).is_err() || std::fs::create_dir(&path).is_err() {
            return None;
        }
    }
    Some(DirLock(path))
}

/// Renvoie (jeton, type d'abonnement). Le jeton expiré est rafraîchi au passage.
fn read_token(agent: &ureq::Agent) -> Result<(String, Option<String>), String> {
    let path = credentials_path();
    let (_, json) = read_json(&path)?;
    let oauth = &json["claudeAiOauth"];
    let token = oauth["accessToken"].as_str().ok_or(tr(
        "pas de connexion claude.ai (lancer claude puis /login)",
        "not signed in to claude.ai (run claude, then /login)",
    ))?;
    let plan = oauth["subscriptionType"].as_str().map(str::to_owned);
    if !claude_expired(oauth) {
        return Ok((token.to_owned(), plan));
    }
    Ok((refresh_claude(agent, &path)?, plan))
}

fn claude_expired(oauth: &Value) -> bool {
    oauth["expiresAt"].as_i64().is_some_and(|exp| exp < chrono::Utc::now().timestamp_millis())
}

fn refresh_claude(agent: &ureq::Agent, path: &str) -> Result<String, String> {
    let relogin = tr("connexion expirée : lancer claude puis /login", "sign-in expired: run claude, then /login");
    let Some(_lock) = lock_dir(format!("{}.lock", claude_dir())) else {
        return Err(tr("jeton en cours de rafraîchissement par Claude Code", "token being refreshed by Claude Code").into());
    };
    // Relu sous verrou : Claude Code a pu le rafraîchir entre-temps.
    let (text, mut json) = read_json(path)?;
    let oauth = &json["claudeAiOauth"];
    if !claude_expired(oauth)
        && let Some(token) = oauth["accessToken"].as_str()
    {
        return Ok(token.to_owned());
    }
    let refresh_token = oauth["refreshToken"].as_str().ok_or(relogin)?;
    let mut body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLAUDE_CLIENT_ID,
    });
    if let Some(scopes) = oauth["scopes"].as_array() {
        let scopes: Vec<&str> = scopes.iter().filter_map(Value::as_str).collect();
        body["scope"] = scopes.join(" ").into();
    }
    let resp = post_refresh(agent, CLAUDE_TOKEN_URL, &body, relogin)?;
    let token = apply_claude_refresh(&mut json["claudeAiOauth"], &resp, chrono::Utc::now().timestamp_millis())
        .ok_or(tr("rafraîchissement du jeton : réponse inattendue", "token refresh: unexpected response"))?;
    write_json(path, &text, &json)?;
    Ok(token)
}

/// Reporte la réponse du service dans `claudeAiOauth`, comme Claude Code : le jeton de
/// rafraîchissement ne change que s'il en est fourni un nouveau. Renvoie le jeton.
fn apply_claude_refresh(oauth: &mut Value, resp: &Value, now_ms: i64) -> Option<String> {
    let token = resp["access_token"].as_str()?;
    oauth["accessToken"] = token.into();
    if let Some(r) = resp["refresh_token"].as_str() {
        oauth["refreshToken"] = r.into();
    }
    if let Some(s) = resp["expires_in"].as_i64() {
        oauth["expiresAt"] = (now_ms + s * 1000).into();
    }
    if let Some(s) = resp["refresh_token_expires_in"].as_i64() {
        oauth["refreshTokenExpiresAt"] = (now_ms + s * 1000).into();
    }
    Some(token.to_owned())
}

/// Renvoie (jeton, identifiant du compte ChatGPT). Le jeton expiré est rafraîchi au
/// passage, comme le ferait Codex.
fn read_codex_token(agent: &ureq::Agent) -> Result<(String, String), String> {
    let dir = std::env::var("CODEX_HOME").unwrap_or_else(|_| format!("{}/.codex", home()));
    let path = format!("{dir}/auth.json");
    let (text, mut json) = read_json(&path)?;
    let tokens = &json["tokens"];
    let (Some(token), Some(account)) = (tokens["access_token"].as_str(), tokens["account_id"].as_str()) else {
        return Err(tr(
            "pas de connexion ChatGPT (lancer codex login)",
            "not signed in with ChatGPT (run codex login)",
        )
        .into());
    };
    let account = account.to_owned();
    if !jwt_exp(token).is_some_and(|exp| exp < chrono::Utc::now().timestamp()) {
        return Ok((token.to_owned(), account));
    }
    let relogin = tr("connexion expirée : lancer codex login", "sign-in expired: run codex login");
    let refresh_token = tokens["refresh_token"].as_str().ok_or(relogin)?;
    let body = serde_json::json!({
        "client_id": CODEX_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    });
    let resp = post_refresh(agent, CODEX_TOKEN_URL, &body, relogin)?;
    let token = apply_codex_refresh(&mut json, &resp, chrono::Utc::now())
        .ok_or(tr("rafraîchissement du jeton : réponse inattendue", "token refresh: unexpected response"))?;
    write_json(&path, &text, &json)?;
    Ok((token, account))
}

/// Reporte la réponse du service dans `auth.json`, comme Codex. Renvoie le jeton.
fn apply_codex_refresh(json: &mut Value, resp: &Value, now: DateTime<chrono::Utc>) -> Option<String> {
    let token = resp["access_token"].as_str()?;
    for key in ["id_token", "access_token", "refresh_token"] {
        if let Some(v) = resp[key].as_str() {
            json["tokens"][key] = v.into();
        }
    }
    json["last_refresh"] = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true).into();
    Some(token.to_owned())
}

fn kimi_dir() -> String {
    std::env::var("KIMI_CODE_HOME").unwrap_or_else(|_| format!("{}/.kimi-code", home()))
}

fn kimi_usage_url() -> String {
    std::env::var("KIMI_CODE_BASE_URL").map_or_else(
        |_| KIMI_USAGE_URL.to_owned(),
        |base| format!("{}/usages", base.trim_end_matches('/')),
    )
}

/// Seuls les jetons OAuth porteurs d'une échéance expirent. Sans `expires_at`,
/// `access_token` est traité comme une clé API longue durée.
fn kimi_token_expired(expires_at: &Value, now: i64) -> bool {
    // Marge d'une minute : ne pas lancer une requête avec un jeton sur le point d'expirer.
    expires_at.as_f64().is_some_and(|exp| exp <= now as f64 + 60.0)
}

/// Renvoie le jeton et, pour le jeton OAuth du CLI, l'identifiant stable de l'appareil.
/// Une clé `KIMI_CODE_API_KEY` est prioritaire. Les jetons OAuth de Kimi ne sont pas
/// rafraîchis ici : ils ne vivent que quelques minutes et leur rotation appartient au CLI,
/// qui relit et réécrit lui-même son fichier d'identifiants.
fn read_kimi_token() -> Result<(String, Option<String>), String> {
    if let Ok(token) = std::env::var("KIMI_CODE_API_KEY")
        && !token.trim().is_empty()
    {
        return Ok((token, None));
    }
    let dir = kimi_dir();
    let path = format!("{dir}/credentials/kimi-code.json");
    let (_, json) = read_json(&path)?;
    let token = json["access_token"].as_str().filter(|t| !t.is_empty()).ok_or(tr(
        "pas de connexion Kimi Code (lancer kimi puis /login, ou définir KIMI_CODE_API_KEY)",
        "not signed in to Kimi Code (run kimi, then /login, or set KIMI_CODE_API_KEY)",
    ))?;
    if kimi_token_expired(&json["expires_at"], chrono::Utc::now().timestamp()) {
        return Err(tr(
            "jeton Kimi Code expiré : lancer kimi pour le renouveler, ou définir KIMI_CODE_API_KEY",
            "Kimi Code token expired: run kimi to renew it, or set KIMI_CODE_API_KEY",
        )
        .into());
    }
    let device_id = std::fs::read_to_string(format!("{dir}/device_id"))
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty());
    Ok((token.to_owned(), device_id))
}

/// Date d'expiration (secondes Unix) d'un jeton JWT : champ `exp` de sa partie centrale.
fn jwt_exp(token: &str) -> Option<i64> {
    let payload = base64url_decode(token.split('.').nth(1)?)?;
    serde_json::from_slice::<Value>(&payload).ok()?["exp"].as_i64()
}

/// Décodage base64 « URL » (alphabet `-_`, sans remplissage obligatoire), celui des JWT.
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn fetch(provider: Provider) -> Result<Usage, FetchError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        // Codes d'erreur traités à la main, pour lire l'en-tête Retry-After d'un 429.
        .http_status_as_error(false)
        .build()
        .into();
    let user_agent = concat!("usclaude/", env!("CARGO_PKG_VERSION"));
    // Pour Codex, l'abonnement figure dans la réponse.
    let (request, plan) = match provider {
        Provider::Codex => {
            let (token, account) = read_codex_token(&agent)?;
            let request = agent
                .get(CODEX_USAGE_URL)
                .header("Authorization", &format!("Bearer {token}"))
                .header("ChatGPT-Account-Id", &account);
            (request, None)
        }
        Provider::Kimi => {
            let (token, device_id) = read_kimi_token()?;
            let mut request = agent
                .get(&kimi_usage_url())
                .header("Authorization", &format!("Bearer {token}"))
                .header("Accept", "application/json");
            if let Some(id) = device_id {
                request = request.header("X-Msh-Device-Id", &id);
            }
            (request, None)
        }
        Provider::Claude => {
            let (token, plan) = read_token(&agent)?;
            let request = agent
                .get(USAGE_URL)
                .header("Authorization", &format!("Bearer {token}"))
                .header("anthropic-beta", "oauth-2025-04-20");
            (request, plan)
        }
    };
    let mut resp = request
        .header("User-Agent", user_agent)
        .call()
        .map_err(|e| format!("{}{e}", tr("requête : ", "request: ")))?;
    match resp.status().as_u16() {
        200..=299 => {}
        401 => {
            let msg = match provider {
                Provider::Codex => {
                    tr("jeton refusé : lancer codex pour le rafraîchir", "token rejected: run codex to refresh it")
                }
                Provider::Kimi => tr(
                    "jeton Kimi Code refusé : lancer kimi ou vérifier KIMI_CODE_API_KEY",
                    "Kimi Code token rejected: run kimi or check KIMI_CODE_API_KEY",
                ),
                Provider::Claude => {
                    tr("jeton refusé : lancer claude pour le rafraîchir", "token rejected: run claude to refresh it")
                }
            };
            return Err(msg.to_owned().into());
        }
        429 => {
            let header = resp.headers().get("retry-after").and_then(|v| v.to_str().ok());
            return Err(FetchError::RateLimited(parse_retry_after(header)));
        }
        s => return Err(format!("{} {s}", tr("réponse HTTP", "HTTP response")).into()),
    }
    let body = resp.body_mut().read_to_string().map_err(|e| format!("{}{e}", tr("réponse : ", "response: ")))?;
    let mut usage = parse(provider, &body)?;
    usage.plan = plan.or(usage.plan);
    save_cache(provider, &cache_json(&body, usage.plan.as_deref(), usage.fetched));
    Ok(usage)
}

/// `Retry-After` en secondes. La forme date HTTP, rare, est ignorée : l'attente double alors.
fn parse_retry_after(header: Option<&str>) -> Option<u64> {
    header?.trim().parse().ok()
}

// ---------- Cache de la dernière réponse ----------

fn cache_path(provider: Provider) -> String {
    let cache = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| format!("{}/.cache", home()));
    format!("{cache}/{}/last.json", provider.app())
}

fn cache_json(body: &str, plan: Option<&str>, fetched: DateTime<Local>) -> String {
    serde_json::json!({ "fetched": fetched.to_rfc3339(), "plan": plan, "body": body }).to_string()
}

/// Dernière réponse valide, réaffichée dès le démarrage avec son heure d'origine.
fn parse_cache(provider: Provider, text: &str) -> Option<Usage> {
    let json: Value = serde_json::from_str(text).ok()?;
    let mut usage = parse(provider, json["body"].as_str()?).ok()?;
    usage.fetched = DateTime::parse_from_rfc3339(json["fetched"].as_str()?).ok()?.with_timezone(&Local);
    usage.plan = json["plan"].as_str().map(str::to_owned).or(usage.plan);
    Some(usage)
}

fn load_cache(provider: Provider) -> Option<Usage> {
    parse_cache(provider, &std::fs::read_to_string(cache_path(provider)).ok()?)
}

fn save_cache(provider: Provider, json: &str) {
    let path = cache_path(provider);
    let written = std::path::Path::new(&path)
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, json));
    if let Err(e) = written {
        eprintln!("{}: {}{e}", provider.app(), tr("cache non enregistré : ", "cache not saved: "));
    }
}

fn parse(provider: Provider, body: &str) -> Result<Usage, String> {
    match provider {
        Provider::Claude => parse_claude(body),
        Provider::Codex => parse_codex(body),
        Provider::Kimi => parse_kimi(body),
    }
}

/// Nombre JSON, y compris sous forme de chaîne (format des compteurs de Kimi).
fn json_number(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(_) => v.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }?;
    n.is_finite().then_some(n)
}

fn parse_reset(v: &Value) -> Option<DateTime<Local>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Local))
}

fn kimi_ratio_limit(v: &Value, key: &str, fr: &'static str, en: &'static str) -> Option<Limit> {
    Some(Limit {
        key: key.to_owned(),
        label: tr(fr, en).to_owned(),
        pct: json_number(&v["used_ratio"])? * 100.0,
        resets_at: parse_reset(&v["reset_time"]),
    })
}

fn kimi_counts(detail: &Value, key: &str, fr: &'static str, en: &'static str) -> Option<Limit> {
    let total = json_number(&detail["limit"])?;
    let used = json_number(&detail["used"])?;
    if total <= 0.0 || used < 0.0 {
        return None;
    }
    Some(Limit {
        key: key.to_owned(),
        label: tr(fr, en).to_owned(),
        pct: used / total * 100.0,
        resets_at: parse_reset(&detail["resetTime"]),
    })
}

/// Compteur détaillé de la fenêtre de 300 minutes, plus fiable que le résumé
/// `limit_5h` chez certains comptes où celui-ci reste bloqué à zéro.
fn kimi_session_counts(quota: &Value) -> Option<Limit> {
    let item = quota["limits"].as_array()?.iter().find(|item| {
        item["window"]["duration"].as_u64() == Some(300)
            && item["window"]["timeUnit"].as_str() == Some("TIME_UNIT_MINUTE")
    })?;
    kimi_counts(&item["detail"], "five_hour", "Session (5 h)", "Session (5h)")
}

fn kimi_weekly_counts(quota: &Value) -> Option<Limit> {
    kimi_counts(&quota["usage"], "seven_day", "Semaine", "Weekly")
}

fn kimi_reconcile(ratio: Option<Limit>, counts: Option<Limit>) -> Option<Limit> {
    match (ratio, counts) {
        (Some(ratio), Some(counts)) => {
            let same_window = match (&ratio.resets_at, &counts.resets_at) {
                (Some(a), Some(b)) => a.timestamp().abs_diff(b.timestamp()) <= 2,
                _ => false,
            };
            // En cas de contradiction pour la même fenêtre, retenir la mesure la plus
            // élevée : le compteur détaillé corrige notamment un résumé resté à zéro.
            Some(if same_window && counts.pct > ratio.pct { counts } else { ratio })
        }
        (ratio, counts) => ratio.or(counts),
    }
}

/// Réponse Kimi Code : limites de 5 h, hebdomadaire et mensuelle. `limit_month_total`
/// est la limite mensuelle bloquante ; `limit_month_code` n'en est qu'une composante.
fn parse_kimi(body: &str) -> Result<Usage, String> {
    let json: Value =
        serde_json::from_str(body).map_err(|e| format!("{}{e}", tr("réponse illisible : ", "unreadable response: ")))?;
    // Certains passages par la passerelle enveloppent la réponse dans `data.quota`.
    let quota = if json["data"]["quota"].is_object() { &json["data"]["quota"] } else { &json };
    let usages = &quota["usages"];
    let session = kimi_reconcile(
        kimi_ratio_limit(&usages["limit_5h"], "five_hour", "Session (5 h)", "Session (5h)"),
        kimi_session_counts(quota),
    );
    let weekly = kimi_reconcile(
        kimi_ratio_limit(&usages["limit_7d"], "seven_day", "Semaine", "Weekly"),
        kimi_weekly_counts(quota),
    );
    let monthly = kimi_ratio_limit(&usages["limit_month_total"], "monthly", "Mois", "Monthly");
    let limits: Vec<Limit> = [session, weekly, monthly].into_iter().flatten().collect();
    if limits.is_empty() {
        return Err(tr("aucune limite dans la réponse", "no limits in the response").into());
    }
    Ok(Usage { plan: None, limits, fetched: Local::now() })
}

/// Réponse Codex : `rate_limit.primary_window` (5 h) et `secondary_window` (semaine),
/// rangées sous les clés de Claude pour partager l'icône.
fn parse_codex(body: &str) -> Result<Usage, String> {
    let json: Value =
        serde_json::from_str(body).map_err(|e| format!("{}{e}", tr("réponse illisible : ", "unreadable response: ")))?;
    let limits: Vec<Limit> = [("five_hour", "primary_window"), ("seven_day", "secondary_window")]
        .into_iter()
        .filter_map(|(key, field)| {
            let w = &json["rate_limit"][field];
            let pct = w["used_percent"].as_f64()?;
            let hours = w["limit_window_seconds"].as_u64().unwrap_or(0) / 3600;
            let label = match hours {
                168 => tr("Semaine", "Weekly").to_owned(),
                h if french() => format!("Session ({h} h)"),
                h => format!("Session ({h}h)"),
            };
            let resets_at = w["reset_at"]
                .as_i64()
                .and_then(|s| DateTime::from_timestamp(s, 0))
                .map(|d| d.with_timezone(&Local));
            Some(Limit { key: key.to_owned(), label, pct, resets_at })
        })
        .collect();
    if limits.is_empty() {
        return Err(tr("aucune limite dans la réponse", "no limits in the response").into());
    }
    let plan = json["plan_type"].as_str().map(str::to_owned);
    Ok(Usage { plan, limits, fetched: Local::now() })
}

/// Garde toutes les entrées de la forme `{"utilization": n, "resets_at": …}`,
/// connues d'abord, puis les éventuelles nouvelles sous leur nom brut.
fn parse_claude(body: &str) -> Result<Usage, String> {
    let json: Value =
        serde_json::from_str(body).map_err(|e| format!("{}{e}", tr("réponse illisible : ", "unreadable response: ")))?;
    let obj = json.as_object().ok_or(tr("réponse inattendue", "unexpected response"))?;

    let mut keys: Vec<&str> = KNOWN.iter().map(|(k, _, _)| *k).collect();
    keys.extend(obj.keys().map(String::as_str).filter(|k| !KNOWN.iter().any(|(n, _, _)| n == k)));

    let limits: Vec<Limit> = keys
        .into_iter()
        .filter_map(|key| {
            let v = obj.get(key)?;
            let pct = v["utilization"].as_f64()?;
            let known = KNOWN.iter().find(|(k, _, _)| *k == key);
            // Les limites inconnues (noms de code internes) ne s'affichent que si elles comptent.
            if known.is_none() && pct == 0.0 {
                return None;
            }
            let label = known.map_or(key.to_owned(), |&(_, fr, en)| tr(fr, en).to_owned());
            let resets_at = v["resets_at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Local));
            Some(Limit { key: key.to_owned(), label, pct, resets_at })
        })
        .collect();

    if limits.is_empty() {
        return Err(tr("aucune limite dans la réponse", "no limits in the response").into());
    }
    Ok(Usage { plan: None, limits, fetched: Local::now() })
}

// ---------- Service : démarrage automatique et redémarrage ----------

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn autostart_path(mode: Mode) -> String {
    let config = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", home()));
    format!("{config}/autostart/{}.desktop", mode.app())
}

fn interval_path(mode: Mode) -> String {
    let config = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", home()));
    format!("{config}/{}/interval", mode.app())
}

/// Intervalle enregistré s'il fait partie de la liste, sinon celui par défaut.
fn parse_interval(text: &str) -> u64 {
    text.trim()
        .parse()
        .ok()
        .filter(|s| INTERVALS.iter().any(|(v, _)| v == s))
        .unwrap_or(INTERVALS[0].0)
}

fn load_interval(mode: Mode) -> u64 {
    parse_interval(&std::fs::read_to_string(interval_path(mode)).unwrap_or_default())
}

fn save_interval(mode: Mode, secs: u64) -> std::io::Result<()> {
    let path = interval_path(mode);
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{secs}\n"))
}

/// Chemin du binaire en cours. Après une réinstallation, Linux le suffixe de « (deleted) ».
fn current_exe() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.to_string_lossy().trim_end_matches(" (deleted)").to_owned()
}

/// Actif si le fichier existe et n'a pas été désactivé par XFCE (`Hidden=true`).
fn autostart_enabled(desktop: Option<&str>) -> bool {
    desktop.is_some_and(|d| !d.lines().any(|l| l.trim() == "Hidden=true"))
}

fn set_autostart(mode: Mode, enable: bool) -> std::io::Result<()> {
    let path = autostart_path(mode);
    if !enable {
        return std::fs::remove_file(&path);
    }
    // Le binaire installé de préférence, pour survivre à un `cargo clean`.
    let installed = format!("{}/.local/bin/usclaude", home());
    let mut exec = if std::path::Path::new(&installed).exists() { installed } else { current_exe() };
    let comment = match mode {
        Mode::All => tr(
            "Limites d'usage de Claude Code, Codex et Kimi Code dans la zone de notification",
            "Claude Code, Codex and Kimi Code usage limits in the system tray",
        ),
        Mode::Single(Provider::Claude) => {
            tr("Limites d'usage de Claude Code dans la zone de notification", "Claude Code usage limits in the system tray")
        }
        Mode::Single(Provider::Codex) => {
            tr("Limites d'usage de Codex dans la zone de notification", "Codex usage limits in the system tray")
        }
        Mode::Single(Provider::Kimi) => tr(
            "Limites d'usage de Kimi Code dans la zone de notification",
            "Kimi Code usage limits in the system tray",
        ),
    };
    if let Mode::Single(provider) = mode {
        exec += " ";
        exec += provider.flag();
    }
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = mode.app();
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={name}\n\
             Comment={comment}\n\
             Exec={exec}\n\
             Icon=utilities-system-monitor\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n"
        ),
    )
}

/// Verrou d'instance unique, libéré par le système à la fin du processus (même tué).
/// `wait` : attendre qu'il se libère (redémarrage) au lieu d'abandonner.
fn lock_instance(mode: Mode, wait: bool) -> Option<std::fs::File> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    lock_file(&format!("{dir}/{}-{}.lock", mode.app(), std::env::var("USER").unwrap_or_default()), wait)
}

fn lock_file(path: &str, wait: bool) -> Option<std::fs::File> {
    let file = std::fs::File::create(path).ok()?;
    let locked = if wait { file.lock().is_ok() } else { file.try_lock().is_ok() };
    locked.then_some(file)
}

/// Relance le binaire (utile après une réinstallation) puis quitte.
/// La nouvelle instance attend que celle-ci ait libéré le verrou.
fn restart(mode: Mode) {
    let mut cmd = std::process::Command::new(current_exe());
    cmd.arg("--wait-lock");
    if let Mode::Single(provider) = mode {
        cmd.arg(provider.flag());
    }
    match cmd.spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => eprintln!("{}: {}{e}", mode.app(), tr("redémarrage impossible : ", "restart failed: ")),
    }
}

/// Attente après un nouveau refus 429. Le délai `Retry-After` du service s'il y en a un
/// (jamais sous l'intervalle choisi, au plus `MAX_RETRY_AFTER`) ; sinon le double de
/// l'attente précédente (ou de l'intervalle au premier refus), plafonné à `MAX_BACKOFF`.
fn next_backoff(current: Option<u64>, interval: u64, retry_after: Option<u64>) -> u64 {
    match retry_after {
        Some(s) => s.clamp(interval, MAX_RETRY_AFTER),
        None => (current.unwrap_or(interval) * 2).min(MAX_BACKOFF.max(interval)),
    }
}

// ---------- Mise en forme ----------

/// « à 15:11 » le jour même, « mer. 16 à 19:59 » sinon (« at 15:11 », « Wed 16 at 19:59 »).
fn fmt_reset(t: DateTime<Local>, now: DateTime<Local>) -> String {
    const JOURS: [&str; 7] = ["lun.", "mar.", "mer.", "jeu.", "ven.", "sam.", "dim."];
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let at = tr("à", "at");
    if t.date_naive() == now.date_naive() {
        format!("{at} {}", t.format("%H:%M"))
    } else {
        let i = t.weekday().num_days_from_monday() as usize;
        let day = if french() { JOURS[i] } else { DAYS[i] };
        format!("{day} {} {at} {}", t.day(), t.format("%H:%M"))
    }
}

fn fmt_limit(l: &Limit, now: DateTime<Local>) -> String {
    let mut s = if french() {
        format!("{} : {:.0} %", l.label, l.pct)
    } else {
        format!("{}: {:.0}%", l.label, l.pct)
    };
    if let Some(t) = l.resets_at {
        s += &format!(" — {} {}", tr("reset", "resets"), fmt_reset(t, now));
    }
    s
}

// ---------- Icône ----------

fn level_color(pct: f64) -> [u8; 3] {
    if pct < 50.0 {
        [0x4c, 0xaf, 0x50] // vert
    } else if pct < 80.0 {
        [0xff, 0x98, 0x00] // orange
    } else {
        [0xf4, 0x43, 0x36] // rouge
    }
}

const ICON_BASE_SIZE: usize = 32;
const ICON_SIZES: [usize; 4] = [32, 48, 64, 128];

/// Place un pixel de la grille de référence 32×32, agrandi en bloc net dans la
/// résolution demandée. Barres et séparateurs gardent ainsi leurs proportions.
fn put_pixel(px: &mut [u8], size: usize, x: usize, y: usize, [r, g, b]: [u8; 3]) {
    let x0 = x * size / ICON_BASE_SIZE;
    let x1 = (x + 1) * size / ICON_BASE_SIZE;
    let y0 = y * size / ICON_BASE_SIZE;
    let y1 = (y + 1) * size / ICON_BASE_SIZE;
    for py in y0..y1 {
        for x in x0..x1 {
            let i = (py * size + x) * 4;
            px[i..i + 4].copy_from_slice(&[0xff, r, g, b]);
        }
    }
}

fn draw_cross(px: &mut [u8], size: usize, color: [u8; 3]) {
    for i in 8..24 {
        put_pixel(px, size, i, i, color);
        put_pixel(px, size, i, 31 - i, color);
    }
}

fn draw_gauge(px: &mut [u8], size: usize, x0: usize, w: usize, pct: f64, outline: [u8; 3]) {
    let fill = ((pct.clamp(0.0, 100.0) / 100.0) * 26.0).round() as usize; // intérieur : y 4..30
    let color = level_color(pct);
    for y in 2..ICON_BASE_SIZE - 1 {
        for x in x0..x0 + w {
            let border = y == 2 || y == ICON_BASE_SIZE - 2 || x == x0 || x == x0 + w - 1;
            if border {
                put_pixel(px, size, x, y, outline);
            } else if y >= ICON_BASE_SIZE - 2 - fill && y > 3 {
                put_pixel(px, size, x, y, color);
            }
        }
    }
}

/// Deux grandes jauges verticales correspondant aux deux clés choisies pour le service.
fn draw_icon(size: usize, provider: Provider, first: Option<f64>, second: Option<f64>) -> ksni::Icon {
    let mut px = vec![0u8; size * size * 4]; // ARGB, transparent
    if first.is_none() && second.is_none() {
        draw_cross(&mut px, size, provider.color());
        return ksni::Icon { width: size as i32, height: size as i32, data: px };
    }
    // Toujours dessiner les deux contours : une limite absente de la réponse laisse
    // sa jauge vide au lieu de faire disparaître complètement la barre.
    for (x0, pct) in [(3, first), (18, second)] {
        draw_gauge(&mut px, size, x0, 11, pct.unwrap_or(0.0), provider.color());
    }
    ksni::Icon { width: size as i32, height: size as i32, data: px }
}

/// Six mini-jauges, deux par service dans l'ordre Claude, Codex, Kimi, avec un
/// séparateur vertical entre les services. La croix ne remplace l'ensemble que si
/// aucun service n'a encore fourni de données.
fn draw_all_icon(size: usize, states: &[ProviderState; 3]) -> ksni::Icon {
    let mut px = vec![0u8; size * size * 4];
    if states.iter().all(|state| state.usage.is_none()) {
        draw_cross(&mut px, size, Provider::Claude.color());
        return ksni::Icon { width: size as i32, height: size as i32, data: px };
    }
    for provider in Provider::ALL {
        let (first, second) = icon_levels(provider, &states[provider.index()]);
        let x0 = 1 + provider.index() * 11;
        for (x, pct) in [(x0, first), (x0 + 5, second)] {
            draw_gauge(&mut px, size, x, 4, pct.unwrap_or(0.0), provider.color());
        }
    }
    // Traits verticaux dans les espaces entre les trois groupes de deux jauges.
    for x in [10, 21] {
        for y in 3..29 {
            put_pixel(&mut px, size, x, y, [0x60, 0x60, 0x60]);
        }
    }
    ksni::Icon { width: size as i32, height: size as i32, data: px }
}

// ---------- Zone de notification ----------

#[derive(Default)]
struct ProviderState {
    usage: Option<Usage>,
    /// Une erreur n'efface pas les valeurs antérieures : les deux restent affichés.
    error: Option<String>,
    /// Prochain essai quand le service refuse (429) : l'attente est alors allongée.
    next_retry: Option<DateTime<Local>>,
}

fn icon_levels(provider: Provider, state: &ProviderState) -> (Option<f64>, Option<f64>) {
    let usage = state.usage.as_ref();
    let (first, second) = provider.gauge_keys();
    (usage.and_then(|u| u.pct(first)), usage.and_then(|u| u.pct(second)))
}

struct UsageTray {
    mode: Mode,
    states: [ProviderState; 3],
    refresh: Sender<()>,
    interval: u64,
}

impl UsageTray {
    fn new(mode: Mode, refresh: Sender<()>) -> Self {
        let states = std::array::from_fn(|index| {
            let provider = Provider::ALL[index];
            ProviderState {
                usage: mode.providers().contains(&provider).then(|| load_cache(provider)).flatten(),
                ..Default::default()
            }
        });
        Self { mode, states, refresh, interval: load_interval(mode) }
    }
}

fn state_lines(state: &ProviderState, now: DateTime<Local>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(usage) = &state.usage {
        if let Some(plan) = &usage.plan {
            lines.push(format!("{}{plan}", tr("Abonnement : ", "Plan: ")));
        }
        lines.extend(usage.limits.iter().map(|l| fmt_limit(l, now)));
        lines.push(format!("{} {}", tr("Mis à jour", "Updated"), fmt_reset(usage.fetched, now)));
    } else if state.error.is_none() {
        lines.push(tr("Chargement…", "Loading…").into());
    }
    if let Some(error) = &state.error {
        lines.push(format!("{}{error}", tr("Erreur : ", "Error: ")));
    }
    if let Some(t) = state.next_retry {
        let label = tr("Service saturé : nouvel essai à", "Rate-limited: next try at");
        lines.push(format!("{label} {}", t.format("%H:%M")));
    }
    lines
}

fn tooltip_description(mode: Mode, states: &[ProviderState; 3], now: DateTime<Local>) -> String {
    mode.providers()
        .iter()
        .map(|&provider| {
            let mut lines = Vec::new();
            if mode == Mode::All {
                lines.push(provider.name().to_owned());
            }
            lines.extend(state_lines(&states[provider.index()], now));
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl ksni::Tray for UsageTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        self.mode.app().into()
    }

    fn title(&self) -> String {
        match self.mode {
            Mode::All => "Claude Code, Codex, Kimi Code — usage".into(),
            Mode::Single(provider) => format!("{} — usage", provider.name()),
        }
    }

    // Pas de zone de notification (bureau non compatible, ou panneau pas encore prêt à
    // l'ouverture de session) : on patiente, ksni affiche l'icône dès qu'elle apparaît.
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        eprintln!(
            "{}: {}",
            self.mode.app(),
            tr(
                "aucune zone de notification compatible StatusNotifierItem pour l'instant, \
                 l'icône apparaîtra dès qu'elle sera disponible.",
                "no StatusNotifierItem-compatible system tray yet, \
                 the icon will appear as soon as one is available."
            )
        );
        true
    }

    fn watcher_online(&self) {
        eprintln!("{}: {}", self.mode.app(), tr("zone de notification trouvée.", "system tray found."));
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        ICON_SIZES
            .iter()
            .map(|&size| match self.mode {
                Mode::All => draw_all_icon(size, &self.states),
                Mode::Single(provider) => {
                    let (first, second) = icon_levels(provider, &self.states[provider.index()]);
                    draw_icon(size, provider, first, second)
                }
            })
            .collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let title = match self.mode {
            Mode::All => "Claude Code, Codex, Kimi Code — usage",
            Mode::Single(Provider::Claude) => "Claude Code — /usage",
            Mode::Single(Provider::Kimi) => "Kimi Code — /usage",
            Mode::Single(Provider::Codex) => "Codex — /status",
        };
        ksni::ToolTip {
            title: title.into(),
            description: tooltip_description(self.mode, &self.states, Local::now()),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{CheckmarkItem, MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};
        let info = |label: String| -> MenuItem<Self> {
            StandardItem { label, enabled: false, ..Default::default() }.into()
        };
        let now = Local::now();

        let mode = self.mode;
        let mut items = Vec::new();
        for (position, &provider) in mode.providers().iter().enumerate() {
            if position > 0 {
                items.push(MenuItem::Separator);
            }
            items.push(info(provider.name().into()));
            items.extend(state_lines(&self.states[provider.index()], now).into_iter().map(&info));
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: tr("Actualiser", "Refresh").into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.refresh.send(());
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            SubMenu {
                label: tr("Réglages", "Settings").into(),
                icon_name: "preferences-system".into(),
                submenu: vec![
                    info(tr("Rafraîchir toutes les :", "Refresh every:").into()),
                    RadioGroup {
                        selected: INTERVALS.iter().position(|(s, _)| *s == self.interval).unwrap_or(0),
                        select: Box::new(move |t: &mut Self, i| {
                            t.interval = INTERVALS[i].0;
                            if let Err(e) = save_interval(mode, t.interval) {
                                eprintln!("{}: {}{e}", mode.app(), tr("réglage non enregistré : ", "setting not saved: "));
                            }
                            // Réveille la boucle pour appliquer le nouvel intervalle tout de suite.
                            let _ = t.refresh.send(());
                        }),
                        options: INTERVALS
                            .iter()
                            .map(|(_, label)| RadioItem { label: (*label).into(), ..Default::default() })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        let autostart = autostart_enabled(std::fs::read_to_string(autostart_path(mode)).ok().as_deref());
        items.push(
            CheckmarkItem {
                label: tr("Lancer à l'ouverture de session", "Start at login").into(),
                checked: autostart,
                activate: Box::new(move |_: &mut Self| {
                    if let Err(e) = set_autostart(mode, !autostart) {
                        eprintln!("{}: {}{e}", mode.app(), tr("démarrage automatique : ", "autostart: "));
                    }
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: tr("Redémarrer", "Restart").into(),
                icon_name: "system-reboot".into(),
                activate: Box::new(move |_| restart(mode)),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: tr("Quitter", "Quit").into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// Affiche tous les services demandés, même si certains échouent. Renvoie leur succès global.
fn print_usage(mode: Mode) -> bool {
    let mut success = true;
    for (position, &provider) in mode.providers().iter().enumerate() {
        if mode == Mode::All {
            if position > 0 {
                println!();
            }
            println!("{}", provider.name());
        }
        match fetch(provider) {
            Ok(usage) => usage.limits.iter().for_each(|l| println!("{}", fmt_limit(l, Local::now()))),
            Err(e) => {
                success = false;
                eprintln!("{}{e}", tr("Erreur : ", "Error: "));
            }
        }
    }
    success
}

fn main() {
    let locale = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"].map(|v| std::env::var(v).ok());
    FRENCH.store(locale_is_french(&locale), Ordering::Relaxed);
    let args: Vec<String> = std::env::args().collect();
    let has = |flag| args.iter().any(|arg| arg == flag);
    let Some(mode) = mode_from_flags(has("--claude"), has("--codex"), has("--kimi"), has("--all")) else {
        eprintln!(
            "{}",
            tr(
                "--claude, --codex, --kimi et --all sont mutuellement incompatibles.",
                "--claude, --codex, --kimi and --all are mutually exclusive."
            )
        );
        std::process::exit(2);
    };

    // `usclaude --print` : affiche une fois dans le terminal (diagnostic).
    if has("--print") {
        if !print_usage(mode) {
            std::process::exit(1);
        }
        return;
    }

    let Some(_lock) = lock_instance(mode, has("--wait-lock")) else {
        eprintln!("{} {}", mode.app(), tr("tourne déjà.", "is already running."));
        return;
    };

    let (tx, rx) = mpsc::channel();
    let tray = UsageTray::new(mode, tx);
    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("{}: {}{e}", mode.app(), tr("impossible de créer l'icône : ", "cannot create the tray icon: "));
            std::process::exit(1);
        }
    };

    let mut backoff = [None; 3];
    let mut next_fetch: [Option<DateTime<Local>>; 3] = [None; 3];
    loop {
        let now = Local::now();
        let mut results = Vec::new();
        for &provider in mode.providers() {
            if next_fetch[provider.index()].is_some_and(|t| t > now) {
                continue;
            }
            results.push((provider, fetch(provider)));
        }
        let interval = handle.update(|t| t.interval).unwrap_or(INTERVALS[0].0);
        for (provider, result) in &results {
            let index = provider.index();
            backoff[index] = match result {
                Err(FetchError::RateLimited(after)) => Some(next_backoff(backoff[index], interval, *after)),
                _ => None,
            };
            next_fetch[index] = backoff[index].map(|s| now + Duration::from_secs(s));
        }
        let wait_started = Local::now();
        let secs = mode
            .providers()
            .iter()
            .map(|provider| {
                next_fetch[provider.index()].map_or(interval, |t| {
                    t.signed_duration_since(wait_started).num_seconds().max(1) as u64
                })
            })
            .min()
            .unwrap_or(interval);

        handle.update(|t| {
            for (provider, result) in results {
                let state = &mut t.states[provider.index()];
                state.next_retry = next_fetch[provider.index()];
                match result {
                    Ok(usage) => {
                        state.usage = Some(usage);
                        state.error = None;
                    }
                    // Une erreur (réseau, 429, jeton) n'efface pas les dernières valeurs.
                    Err(e) => {
                        eprintln!("{}: {}: {e}", mode.app(), provider.name());
                        state.error = Some(e.to_string());
                    }
                }
            }
        });
        // Attend l'échéance, un clic sur « Actualiser » ou un changement de réglage.
        if rx.recv_timeout(Duration::from_secs(secs)).is_ok() {
            next_fetch = [None; 3];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "five_hour": {"utilization": 37.0, "resets_at": "2026-09-10T16:59:59.943648+00:00"},
        "seven_day": {"utilization": 62.5, "resets_at": "2026-09-14T08:00:00+00:00"},
        "seven_day_oauth_apps": null,
        "seven_day_opus": {"utilization": 0.0, "resets_at": null},
        "a_new_limit": {"utilization": 5, "resets_at": null},
        "nimbus_quill": {"utilization": 0.0, "resets_at": null},
        "extra_usage": {"is_enabled": false}
    }"#;

    fn sample_usage(provider: Provider) -> Usage {
        match provider {
            Provider::Claude => parse_claude(SAMPLE).unwrap(),
            Provider::Codex => parse_codex(
                r#"{"plan_type":"plus","rate_limit":{
                    "primary_window":{"used_percent":20,"limit_window_seconds":18000},
                    "secondary_window":{"used_percent":40,"limit_window_seconds":604800}
                }}"#,
            )
            .unwrap(),
            Provider::Kimi => parse_kimi(
                r#"{"usages":{
                    "limit_5h":{"used_ratio":0.2},
                    "limit_7d":{"used_ratio":0.3},
                    "limit_month_total":{"used_ratio":0.4}
                }}"#,
            )
            .unwrap(),
        }
    }

    #[test]
    fn individual_mode_flags_are_exclusive() {
        assert_eq!(mode_from_flags(false, false, false, false), Some(Mode::Single(Provider::Claude)));
        assert_eq!(mode_from_flags(true, false, false, false), Some(Mode::Single(Provider::Claude)));
        assert_eq!(mode_from_flags(false, true, false, false), Some(Mode::Single(Provider::Codex)));
        assert_eq!(mode_from_flags(false, false, true, false), Some(Mode::Single(Provider::Kimi)));
        assert_eq!(mode_from_flags(false, false, false, true), Some(Mode::All));
        assert_eq!(mode_from_flags(true, true, false, false), None);
        assert_eq!(mode_from_flags(true, false, true, false), None);
        assert_eq!(mode_from_flags(false, true, true, false), None);
        assert_eq!(mode_from_flags(true, true, true, false), None);
        assert_eq!(mode_from_flags(false, false, true, true), None);
        assert_eq!(Mode::Single(Provider::Claude).app(), "usclaude");
        assert_eq!(Mode::All.providers(), Provider::ALL);
    }

    #[test]
    fn parse_keeps_known_order_then_new_limits() {
        let u = parse(Provider::Claude, SAMPLE).unwrap();
        let keys: Vec<_> = u.limits.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, ["five_hour", "seven_day", "seven_day_opus", "a_new_limit"]);
        assert_eq!(u.pct("seven_day"), Some(62.5));
        assert!(u.limits[0].resets_at.is_some());
        assert!(u.limits[2].resets_at.is_none());
    }

    #[test]
    fn parse_codex_windows() {
        let body = r#"{"plan_type": "plus", "rate_limit": {
            "primary_window": {"used_percent": 100, "limit_window_seconds": 18000, "reset_at": 1789058810},
            "secondary_window": {"used_percent": 37, "limit_window_seconds": 604800, "reset_at": 1789468946}
        }}"#;
        let u = parse_codex(body).unwrap();
        assert_eq!(u.plan.as_deref(), Some("plus"));
        assert_eq!(u.pct("five_hour"), Some(100.0));
        assert_eq!(u.pct("seven_day"), Some(37.0));
        assert_eq!(u.limits[0].label, "Session (5h)");
        assert_eq!(u.limits[1].label, "Weekly");
        assert!(u.limits[1].resets_at.is_some());
        assert!(parse_codex(r#"{"rate_limit": null}"#).is_err());
    }

    #[test]
    fn parse_kimi_windows_and_detailed_counts() {
        let body = r#"{
            "usage": {"limit": "100", "used": "29", "resetTime": "2026-10-02T12:58:51Z"},
            "limits": [{"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                "detail": {"limit": "100", "used": "88", "remaining": "12",
                    "resetTime": "2026-10-01T01:58:51.471957Z"}}],
            "usages": {
                "limit_5h": {"used_ratio": 0, "reset_time": "2026-10-01T01:58:50Z"},
                "limit_7d": {"used_ratio": 0, "reset_time": "2026-10-02T12:58:50Z"},
                "limit_month_total": {"used_ratio": 0.406, "reset_time": "2026-11-01T00:00:00Z"},
                "limit_month_code": {"used_ratio": 0.25, "reset_time": "2026-11-01T00:00:00Z"}
            }
        }"#;
        let u = parse_kimi(body).unwrap();
        let keys: Vec<_> = u.limits.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, ["five_hour", "seven_day", "monthly"]);
        assert_eq!(u.pct("five_hour"), Some(88.0), "le compteur détaillé corrige le résumé à zéro");
        assert!((u.pct("seven_day").unwrap() - 29.0).abs() < 1e-10, "le compteur hebdomadaire est retenu");
        assert_eq!(u.pct("monthly"), Some(40.6));
        assert_eq!(u.limits[1].label, "Weekly");
        assert_eq!(u.limits[2].label, "Monthly");
        assert!(u.limits[1].resets_at.is_some());
        assert!(u.limits[2].resets_at.is_some());
    }

    #[test]
    fn parse_kimi_ratio_session_and_envelope() {
        let body = r#"{"code": 0, "data": {"kind": "ok", "quota": {"usages": {
            "limit_5h": {"used_ratio": "0.25", "reset_time": "2026-10-01T01:58:50Z"},
            "limit_7d": {"used_ratio": 0.1, "reset_time": "2026-10-02T12:58:50Z"},
            "limit_month_total": {"used_ratio": 0.4, "reset_time": "2026-11-01T00:00:00Z"}
        }}}}"#;
        let u = parse_kimi(body).unwrap();
        assert_eq!(u.pct("five_hour"), Some(25.0));
        assert_eq!(u.pct("seven_day"), Some(10.0));
        assert_eq!(u.pct("monthly"), Some(40.0));
        assert!(parse_kimi(r#"{"usages":{}}"#).is_err());
    }

    #[test]
    fn kimi_api_key_does_not_require_expiry() {
        assert!(!kimi_token_expired(&Value::Null, 1_000), "clé API sans échéance");
        assert!(!kimi_token_expired(&serde_json::json!(1_061), 1_000));
        assert!(kimi_token_expired(&serde_json::json!(1_060), 1_000), "marge d'une minute");
        assert!(kimi_token_expired(&serde_json::json!(999), 1_000));
    }

    #[test]
    fn codex_token_expiry() {
        // Partie centrale : {"exp":123} en base64 URL, sans remplissage.
        assert_eq!(jwt_exp("en-tete.eyJleHAiOjEyM30.signature"), Some(123));
        assert_eq!(jwt_exp("pas-un-jwt"), None);
        assert_eq!(jwt_exp("a.!!!.b"), None);
        assert_eq!(base64url_decode("-_8").unwrap(), [0xfb, 0xff]);
    }

    #[test]
    fn claude_refresh_keeps_other_fields() {
        let mut oauth = serde_json::json!({
            "accessToken": "ancien", "refreshToken": "r1", "expiresAt": 1,
            "scopes": ["user:inference"], "subscriptionType": "pro"
        });
        let resp = serde_json::json!({"access_token": "nouveau", "expires_in": 28800});
        assert_eq!(apply_claude_refresh(&mut oauth, &resp, 1000).as_deref(), Some("nouveau"));
        assert_eq!(oauth["expiresAt"], 28_801_000);
        assert_eq!(oauth["refreshToken"], "r1", "gardé faute de nouveau");
        assert_eq!(oauth["subscriptionType"], "pro");
        let resp = serde_json::json!({"access_token": "a", "refresh_token": "r2"});
        apply_claude_refresh(&mut oauth, &resp, 0);
        assert_eq!(oauth["refreshToken"], "r2");
        assert!(apply_claude_refresh(&mut oauth, &serde_json::json!({"error": "x"}), 0).is_none());
    }

    #[test]
    fn codex_refresh_updates_tokens() {
        let mut json = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {"id_token": "i1", "access_token": "a1", "refresh_token": "r1", "account_id": "acc"},
            "last_refresh": "2026-09-01T00:00:00Z"
        });
        let resp = serde_json::json!({"id_token": "i2", "access_token": "a2", "refresh_token": "r2"});
        let now = DateTime::parse_from_rfc3339("2026-09-29T20:00:00Z").unwrap().to_utc();
        assert_eq!(apply_codex_refresh(&mut json, &resp, now).as_deref(), Some("a2"));
        assert_eq!(json["tokens"]["refresh_token"], "r2");
        assert_eq!(json["tokens"]["account_id"], "acc");
        assert_eq!(json["last_refresh"], "2026-09-29T20:00:00.000000000Z");
    }

    #[test]
    fn dir_lock_is_exclusive() {
        let path = std::env::temp_dir().join(format!("usclaude-test-{}.lock-dir", std::process::id()));
        let path = path.to_str().unwrap().to_owned();
        let first = lock_dir(path.clone());
        assert!(first.is_some());
        assert!(lock_dir(path.clone()).is_none(), "déjà pris");
        drop(first);
        assert!(lock_dir(path).is_some(), "libéré à la fin");
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse(Provider::Claude, "{}").is_err());
        assert!(parse(Provider::Claude, "pas du json").is_err());
    }

    // Les tests ne passent pas par main() : l'interface y reste en anglais, quelle que
    // soit la langue de la machine qui les lance.
    #[test]
    fn reset_format() {
        let now: DateTime<Local> = DateTime::parse_from_rfc3339("2026-09-10T10:00:00+02:00").unwrap().into();
        let same_day = now + chrono::Duration::hours(3);
        let later = now + chrono::Duration::days(2);
        assert_eq!(fmt_reset(same_day, now), format!("at {}", same_day.format("%H:%M")));
        assert!(fmt_reset(later, now).starts_with("Sat 12 at"));
    }

    #[test]
    fn locale_detection() {
        let s = |v: &str| Some(v.to_owned());
        assert!(locale_is_french(&[None, None, None, s("fr_FR.UTF-8")]));
        assert!(!locale_is_french(&[None, s("en_US.UTF-8"), None, s("fr_FR.UTF-8")]), "LC_ALL prime sur LANG");
        assert!(locale_is_french(&[s("fr:en"), None, None, s("en_US.UTF-8")]), "LANGUAGE en premier");
        assert!(!locale_is_french(&[s(""), None, None, s("C")]), "variable vide ignorée");
        assert!(!locale_is_french(&[None, None, None, None]));
    }

    #[test]
    fn autostart_state() {
        assert!(!autostart_enabled(None));
        assert!(autostart_enabled(Some("[Desktop Entry]\nExec=usclaude\n")));
        assert!(!autostart_enabled(Some("[Desktop Entry]\nHidden=true\n")));
    }

    #[test]
    fn interval_setting() {
        assert_eq!(parse_interval("300\n"), 300);
        assert_eq!(parse_interval(""), 90);
        assert_eq!(parse_interval("42"), 90, "valeur hors liste refusée");
        assert_eq!(parse_interval("abc"), 90);
    }

    #[test]
    fn backoff_doubles_up_to_cap() {
        assert_eq!(next_backoff(None, 90, None), 180);
        assert_eq!(next_backoff(Some(180), 90, None), 360);
        assert_eq!(next_backoff(Some(360), 90, None), 600, "plafond de 10 min");
        assert_eq!(next_backoff(Some(600), 90, None), 600);
        assert_eq!(next_backoff(None, 600, None), 600, "jamais plus court que l'intervalle choisi");
    }

    #[test]
    fn backoff_follows_retry_after() {
        assert_eq!(next_backoff(Some(600), 90, Some(120)), 120, "délai du service prioritaire");
        assert_eq!(next_backoff(None, 90, Some(10)), 90, "jamais sous l'intervalle choisi");
        assert_eq!(next_backoff(None, 90, Some(99_999)), 3600, "plafond d'une heure");
        assert_eq!(parse_retry_after(Some(" 120 ")), Some(120));
        assert_eq!(parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")), None);
        assert_eq!(parse_retry_after(None), None);
    }

    #[test]
    fn cache_round_trip() {
        let fetched: DateTime<Local> = DateTime::parse_from_rfc3339("2026-09-09T15:11:00+02:00").unwrap().into();
        let u = parse_cache(Provider::Claude, &cache_json(SAMPLE, Some("max"), fetched)).unwrap();
        assert_eq!(u.fetched, fetched, "heure d'origine conservée");
        assert_eq!(u.plan.as_deref(), Some("max"));
        assert_eq!(u.pct("seven_day"), Some(62.5));
        assert!(parse_cache(Provider::Claude, "{}").is_none());
        assert!(parse_cache(Provider::Claude, "pas du json").is_none());
    }

    #[test]
    fn single_instance_lock() {
        let path = std::env::temp_dir().join(format!("usclaude-test-{}.lock", std::process::id()));
        let path = path.to_str().unwrap();
        let first = lock_file(path, false);
        assert!(first.is_some());
        assert!(lock_file(path, false).is_none(), "deuxième instance refusée");
        drop(first);
        assert!(lock_file(path, false).is_some(), "verrou libéré à la fermeture");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn icon_sizes() {
        for size in ICON_SIZES {
            let i = draw_icon(size, Provider::Claude, Some(100.0), Some(0.0));
            assert_eq!((i.width, i.height), (size as i32, size as i32));
            assert_eq!(i.data.len(), size * size * 4);
            assert_eq!(draw_icon(size, Provider::Claude, None, None).data.len(), size * size * 4);
        }
    }

    #[test]
    fn missing_limit_keeps_its_empty_gauge() {
        let icon = draw_icon(32, Provider::Kimi, None, Some(40.0));
        for x in [3, 18] {
            let offset = (2 * 32 + x) * 4;
            assert_eq!(icon.data[offset..offset + 4], [0xff, 0x9c, 0x6c, 0xf0], "contour x={x}");
        }
        assert_eq!(icon.data[(29 * 32 + 4) * 4], 0, "la jauge absente reste vide");
    }

    #[test]
    fn kimi_session_gauge_reflects_its_percentage() {
        let icon = draw_icon(32, Provider::Kimi, Some(52.0), Some(0.0));
        // 52 % de la hauteur intérieure donnent 14 pixels remplis à partir de y=16.
        let filled = (16 * 32 + 4) * 4;
        assert_eq!(icon.data[filled..filled + 4], [0xff, 0xff, 0x98, 0x00]);
        assert_eq!(icon.data[(15 * 32 + 4) * 4], 0, "au-dessus du niveau de 52 %");
    }

    #[test]
    fn all_icon_draws_two_gauges_per_provider_at_every_size() {
        let states = std::array::from_fn(|index| ProviderState {
            usage: Some(sample_usage(Provider::ALL[index])),
            ..Default::default()
        });
        let expected = [(37.0, 62.5), (20.0, 40.0), (20.0, 40.0)];
        for size in ICON_SIZES {
            let scaled = |v| v * size / ICON_BASE_SIZE;
            let icon = draw_all_icon(size, &states);
            assert_eq!(icon.data.len(), size * size * 4);
            for provider in Provider::ALL {
                let (first, second) = icon_levels(provider, &states[provider.index()]);
                assert_eq!((first, second), (Some(expected[provider.index()].0), Some(expected[provider.index()].1)));
                let x0 = 1 + provider.index() * 11;
                for x in [x0, x0 + 5] {
                    let offset = (scaled(2) * size + scaled(x)) * 4;
                    let [r, g, b] = provider.color();
                    assert_eq!(icon.data[offset..offset + 4], [0xff, r, g, b], "{} x={x}", provider.name());
                }
            }
            for x in [10, 21] {
                let offset = (scaled(16) * size + scaled(x)) * 4;
                assert_eq!(icon.data[offset..offset + 4], [0xff, 0x60, 0x60, 0x60], "séparateur x={x}");
            }
        }

        // Une donnée chez un seul service suffit à supprimer la croix centrale.
        let mut one: [ProviderState; 3] = Default::default();
        one[Provider::Kimi.index()].usage = Some(sample_usage(Provider::Kimi));
        let icon = draw_all_icon(32, &one);
        assert_eq!(icon.data[(16 * 32 + 16) * 4], 0);
        let offset = (2 * 32 + 1) * 4;
        assert_eq!(icon.data[offset..offset + 4], [0xff, 0x90, 0x90, 0x90], "jauge Claude vide");
        assert_eq!(draw_all_icon(32, &Default::default()).data[(16 * 32 + 16) * 4], 0xff);
    }

    #[test]
    fn all_tooltip_keeps_sections_values_and_errors() {
        let mut states: [ProviderState; 3] = Default::default();
        states[Provider::Claude.index()].usage = Some(sample_usage(Provider::Claude));
        states[Provider::Claude.index()].error = Some("stale Claude".into());
        states[Provider::Codex.index()].error = Some("Codex unavailable".into());
        let description = tooltip_description(Mode::All, &states, Local::now());
        let claude = description.find("Claude Code").unwrap();
        let codex = description.find("Codex\n").unwrap();
        let kimi = description.find("Kimi Code").unwrap();
        assert!(claude < codex && codex < kimi);
        assert!(description.contains("Session (5h): 37%"));
        assert!(description.contains("Updated"));
        assert!(description.contains("Error: stale Claude"));
        assert!(description.contains("Error: Codex unavailable"));
        assert!(description.ends_with("Kimi Code\nLoading…"));
    }
}
