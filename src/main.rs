//! usclaude : affiche dans la zone de notification les limites d'usage de
//! Claude Code, les mêmes que la commande `/usage`.
//!
//! Le jeton OAuth est lu (jamais modifié) dans `~/.claude/.credentials.json`.
//! Quand il expire, c'est Claude Code qui le rafraîchit à sa prochaine utilisation.

use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use chrono::{DateTime, Datelike, Local};
use ksni::blocking::TrayMethods;
use serde_json::Value;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Intervalles de rafraîchissement proposés (secondes, libellé) ; le premier par défaut.
const INTERVALS: [(u64, &str); 4] = [(90, "90 s"), (180, "3 min"), (300, "5 min"), (600, "10 min")];

/// Limites connues, dans l'ordre d'affichage.
const KNOWN: [(&str, &str); 4] = [
    ("five_hour", "Session (5 h)"),
    ("seven_day", "Semaine, tous modèles"),
    ("seven_day_opus", "Semaine, Opus"),
    ("seven_day_sonnet", "Semaine, Sonnet"),
];

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

fn credentials_path() -> String {
    let dir = std::env::var("CLAUDE_CONFIG_DIR")
        .unwrap_or_else(|_| format!("{}/.claude", home()));
    format!("{dir}/.credentials.json")
}

/// Renvoie (jeton, type d'abonnement).
fn read_token() -> Result<(String, Option<String>), String> {
    let path = credentials_path();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{path} : {e}"))?;
    let json: Value = serde_json::from_str(&text).map_err(|e| format!("{path} : {e}"))?;
    let oauth = &json["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .ok_or("pas de connexion claude.ai (lancer claude puis /login)")?;
    if let Some(exp) = oauth["expiresAt"].as_i64()
        && exp < chrono::Utc::now().timestamp_millis()
    {
        return Err("jeton expiré : lancer claude pour le rafraîchir".into());
    }
    let plan = oauth["subscriptionType"].as_str().map(str::to_owned);
    Ok((token.to_owned(), plan))
}

fn fetch() -> Result<Usage, String> {
    let (token, plan) = read_token()?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into();
    let body = agent
        .get(USAGE_URL)
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", concat!("usclaude/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(401) => "jeton refusé : lancer claude pour le rafraîchir".into(),
            ureq::Error::StatusCode(429) => "trop de requêtes, nouvel essai plus tard".into(),
            e => format!("requête : {e}"),
        })?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("réponse : {e}"))?;
    let mut usage = parse(&body)?;
    usage.plan = plan;
    Ok(usage)
}

/// Garde toutes les entrées de la forme `{"utilization": n, "resets_at": …}`,
/// connues d'abord, puis les éventuelles nouvelles sous leur nom brut.
fn parse(body: &str) -> Result<Usage, String> {
    let json: Value = serde_json::from_str(body).map_err(|e| format!("réponse illisible : {e}"))?;
    let obj = json.as_object().ok_or("réponse inattendue")?;

    let mut keys: Vec<&str> = KNOWN.iter().map(|(k, _)| *k).collect();
    keys.extend(obj.keys().map(String::as_str).filter(|k| !KNOWN.iter().any(|(n, _)| n == k)));

    let limits: Vec<Limit> = keys
        .into_iter()
        .filter_map(|key| {
            let v = obj.get(key)?;
            let pct = v["utilization"].as_f64()?;
            let known = KNOWN.iter().find(|(k, _)| *k == key);
            // Les limites inconnues (noms de code internes) ne s'affichent que si elles comptent.
            if known.is_none() && pct == 0.0 {
                return None;
            }
            let label = known.map_or(key.to_owned(), |(_, l)| (*l).to_owned());
            let resets_at = v["resets_at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Local));
            Some(Limit { key: key.to_owned(), label, pct, resets_at })
        })
        .collect();

    if limits.is_empty() {
        return Err("aucune limite dans la réponse".into());
    }
    Ok(Usage { plan: None, limits, fetched: Local::now() })
}

// ---------- Service : démarrage automatique et redémarrage ----------

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn autostart_path() -> String {
    let config = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", home()));
    format!("{config}/autostart/usclaude.desktop")
}

fn interval_path() -> String {
    let config = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", home()));
    format!("{config}/usclaude/interval")
}

/// Intervalle enregistré s'il fait partie de la liste, sinon celui par défaut.
fn parse_interval(text: &str) -> u64 {
    text.trim()
        .parse()
        .ok()
        .filter(|s| INTERVALS.iter().any(|(v, _)| v == s))
        .unwrap_or(INTERVALS[0].0)
}

fn load_interval() -> u64 {
    parse_interval(&std::fs::read_to_string(interval_path()).unwrap_or_default())
}

fn save_interval(secs: u64) -> std::io::Result<()> {
    let path = interval_path();
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

fn set_autostart(enable: bool) -> std::io::Result<()> {
    let path = autostart_path();
    if !enable {
        return std::fs::remove_file(&path);
    }
    // Le binaire installé de préférence, pour survivre à un `cargo clean`.
    let installed = format!("{}/.local/bin/usclaude", home());
    let exec = if std::path::Path::new(&installed).exists() { installed } else { current_exe() };
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=usclaude\n\
             Comment=Limites d'usage de Claude Code dans la zone de notification\n\
             Exec={exec}\n\
             Icon=utilities-system-monitor\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n"
        ),
    )
}

/// Verrou d'instance unique, libéré par le système à la fin du processus (même tué).
/// `wait` : attendre qu'il se libère (redémarrage) au lieu d'abandonner.
fn lock_instance(wait: bool) -> Option<std::fs::File> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    lock_file(&format!("{dir}/usclaude-{}.lock", std::env::var("USER").unwrap_or_default()), wait)
}

fn lock_file(path: &str, wait: bool) -> Option<std::fs::File> {
    let file = std::fs::File::create(path).ok()?;
    let locked = if wait { file.lock().is_ok() } else { file.try_lock().is_ok() };
    locked.then_some(file)
}

/// Relance le binaire (utile après une réinstallation) puis quitte.
/// La nouvelle instance attend que celle-ci ait libéré le verrou.
fn restart() {
    match std::process::Command::new(current_exe()).arg("--wait-lock").spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => eprintln!("usclaude : redémarrage impossible : {e}"),
    }
}

// ---------- Mise en forme ----------

fn fmt_reset(t: DateTime<Local>, now: DateTime<Local>) -> String {
    const JOURS: [&str; 7] = ["lun.", "mar.", "mer.", "jeu.", "ven.", "sam.", "dim."];
    if t.date_naive() == now.date_naive() {
        format!("à {}", t.format("%H:%M"))
    } else {
        let jour = JOURS[t.weekday().num_days_from_monday() as usize];
        format!("{jour} {} à {}", t.day(), t.format("%H:%M"))
    }
}

fn fmt_limit(l: &Limit, now: DateTime<Local>) -> String {
    let mut s = format!("{} : {:.0} %", l.label, l.pct);
    if let Some(t) = l.resets_at {
        s += &format!(" — reset {}", fmt_reset(t, now));
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

/// Deux jauges verticales : session à gauche, semaine à droite.
/// Une croix grise quand il n'y a pas de données.
fn draw_icon(session: Option<f64>, week: Option<f64>) -> ksni::Icon {
    const S: usize = 32;
    let mut px = vec![0u8; S * S * 4]; // ARGB, transparent
    let mut put = |x: usize, y: usize, [r, g, b]: [u8; 3]| {
        let i = (y * S + x) * 4;
        px[i..i + 4].copy_from_slice(&[0xff, r, g, b]);
    };
    let grey = [0x90, 0x90, 0x90];

    let (Some(session), Some(week)) = (session, week) else {
        for i in 8..24 {
            put(i, i, grey);
            put(i, 31 - i, grey);
        }
        return ksni::Icon { width: S as i32, height: S as i32, data: px };
    };

    for (x0, pct) in [(3, session), (18, week)] {
        let w = 11;
        let fill = ((pct.clamp(0.0, 100.0) / 100.0) * 26.0).round() as usize; // intérieur : y 4..30
        let color = level_color(pct);
        for y in 2..S - 1 {
            for x in x0..x0 + w {
                let border = y == 2 || y == S - 2 || x == x0 || x == x0 + w - 1;
                if border {
                    put(x, y, grey);
                } else if y >= S - 2 - fill && y > 3 {
                    put(x, y, color);
                }
            }
        }
    }
    ksni::Icon { width: S as i32, height: S as i32, data: px }
}

// ---------- Zone de notification ----------

struct UsageTray {
    state: Option<Result<Usage, String>>,
    refresh: Sender<()>,
    interval: u64,
}

impl ksni::Tray for UsageTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }

    fn title(&self) -> String {
        "Claude — usage".into()
    }

    // Pas de zone de notification (bureau non compatible, ou panneau pas encore prêt à
    // l'ouverture de session) : on patiente, ksni affiche l'icône dès qu'elle apparaît.
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        eprintln!(
            "usclaude : aucune zone de notification compatible StatusNotifierItem pour l'instant, \
             l'icône apparaîtra dès qu'elle sera disponible."
        );
        true
    }

    fn watcher_online(&self) {
        eprintln!("usclaude : zone de notification trouvée.");
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let usage = self.state.as_ref().and_then(|s| s.as_ref().ok());
        vec![draw_icon(
            usage.and_then(|u| u.pct("five_hour")),
            usage.and_then(|u| u.pct("seven_day")),
        )]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let now = Local::now();
        let description = match &self.state {
            None => "Chargement…".into(),
            Some(Err(e)) => e.clone(),
            Some(Ok(u)) => u.limits.iter().map(|l| fmt_limit(l, now)).collect::<Vec<_>>().join("\n"),
        };
        ksni::ToolTip { title: "Claude Code — /usage".into(), description, ..Default::default() }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{CheckmarkItem, MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};
        let info = |label: String| -> MenuItem<Self> {
            StandardItem { label, enabled: false, ..Default::default() }.into()
        };
        let now = Local::now();

        let mut items = Vec::new();
        match &self.state {
            None => items.push(info("Chargement…".into())),
            Some(Err(e)) => items.push(info(format!("Erreur : {e}"))),
            Some(Ok(u)) => {
                if let Some(plan) = &u.plan {
                    items.push(info(format!("Abonnement : {plan}")));
                    items.push(MenuItem::Separator);
                }
                items.extend(u.limits.iter().map(|l| info(fmt_limit(l, now))));
                items.push(MenuItem::Separator);
                items.push(info(format!("Mis à jour à {}", u.fetched.format("%H:%M"))));
            }
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Actualiser".into(),
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
                label: "Réglages".into(),
                icon_name: "preferences-system".into(),
                submenu: vec![
                    info("Rafraîchir toutes les :".into()),
                    RadioGroup {
                        selected: INTERVALS.iter().position(|(s, _)| *s == self.interval).unwrap_or(0),
                        select: Box::new(|t: &mut Self, i| {
                            t.interval = INTERVALS[i].0;
                            if let Err(e) = save_interval(t.interval) {
                                eprintln!("usclaude : réglage non enregistré : {e}");
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
        let autostart = autostart_enabled(std::fs::read_to_string(autostart_path()).ok().as_deref());
        items.push(
            CheckmarkItem {
                label: "Lancer à l'ouverture de session".into(),
                checked: autostart,
                activate: Box::new(move |_: &mut Self| {
                    if let Err(e) = set_autostart(!autostart) {
                        eprintln!("usclaude : démarrage automatique : {e}");
                    }
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Redémarrer".into(),
                icon_name: "system-reboot".into(),
                activate: Box::new(|_| restart()),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quitter".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

fn main() {
    // `usclaude --print` : affiche une fois dans le terminal (diagnostic).
    if std::env::args().any(|a| a == "--print") {
        match fetch() {
            Ok(u) => u.limits.iter().for_each(|l| println!("{}", fmt_limit(l, Local::now()))),
            Err(e) => {
                eprintln!("Erreur : {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let wait = std::env::args().any(|a| a == "--wait-lock");
    let Some(_lock) = lock_instance(wait) else {
        eprintln!("usclaude tourne déjà.");
        return;
    };

    let (tx, rx) = mpsc::channel();
    let tray = UsageTray { state: None, refresh: tx, interval: load_interval() };
    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("usclaude : impossible de créer l'icône : {e}");
            std::process::exit(1);
        }
    };

    loop {
        let result = fetch();
        // Une erreur passagère (réseau, 429) ne doit pas effacer les dernières valeurs.
        handle.update(|t| match (&t.state, result) {
            (Some(Ok(_)), Err(e)) => eprintln!("usclaude : {e}"),
            (_, r) => t.state = Some(r),
        });
        // Attend l'échéance, un clic sur « Actualiser » ou un changement de réglage.
        let secs = handle.update(|t| t.interval).unwrap_or(INTERVALS[0].0);
        let _ = rx.recv_timeout(Duration::from_secs(secs));
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

    #[test]
    fn parse_keeps_known_order_then_new_limits() {
        let u = parse(SAMPLE).unwrap();
        let keys: Vec<_> = u.limits.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, ["five_hour", "seven_day", "seven_day_opus", "a_new_limit"]);
        assert_eq!(u.pct("seven_day"), Some(62.5));
        assert!(u.limits[0].resets_at.is_some());
        assert!(u.limits[2].resets_at.is_none());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse("{}").is_err());
        assert!(parse("pas du json").is_err());
    }

    #[test]
    fn reset_format() {
        let now: DateTime<Local> = DateTime::parse_from_rfc3339("2026-09-10T10:00:00+02:00").unwrap().into();
        let same_day = now + chrono::Duration::hours(3);
        let later = now + chrono::Duration::days(2);
        assert_eq!(fmt_reset(same_day, now), format!("à {}", same_day.format("%H:%M")));
        assert!(fmt_reset(later, now).starts_with("sam. 12 à"));
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
    fn icon_size() {
        let i = draw_icon(Some(100.0), Some(0.0));
        assert_eq!(i.data.len(), 32 * 32 * 4);
        assert_eq!(draw_icon(None, None).data.len(), 32 * 32 * 4);
    }
}
