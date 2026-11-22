use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, SystemTime};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

const API: &str = "https://hh3.gbdev.io/api";
pub const PLATFORMS: &[(&str, &str, &str)] = &[("GB", "Game Boy", "gb"), ("GBC", "Game Boy Color", "gb"), ("GBA", "Game Boy Advance", "gba")];
const CACHE_AGE: Duration = Duration::from_secs(3 * 24 * 3600);
pub const PER_PAGE: usize = 24;

#[derive(Clone, Debug)]
pub struct HubEntry {
    pub slug: String,
    pub title: String,
    pub developer: String,
    pub platform: String,
    pub typetag: String,
    pub tags: Vec<String>,
    pub year: Option<u16>,
    pub added: String,
    pub rom: Option<String>,
    pub screenshot: Option<String>,
    pub files_base: String,
}

impl HubEntry {
    pub fn playable(&self) -> bool {
        true
    }

    pub fn console(&self) -> &'static str {
        PLATFORMS.iter().find(|p| p.0 == self.platform).map(|p| p.2).unwrap_or("gb")
    }

    pub fn platform_name(&self) -> &'static str {
        PLATFORMS.iter().find(|p| p.0 == self.platform).map(|p| p.1).unwrap_or("Game Boy")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Newest,
    Title,
    YearNew,
    YearOld,
}

impl Sort {
    pub const ALL: [Sort; 4] = [Sort::Newest, Sort::Title, Sort::YearNew, Sort::YearOld];
    pub fn label(self) -> &'static str {
        match self {
            Sort::Newest => "Recently added",
            Sort::Title => "Title A-Z",
            Sort::YearNew => "Release year, newest",
            Sort::YearOld => "Release year, oldest",
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct Filters {
    pub query: String,
    pub platform: String,
    pub kind: String,
    pub genre: String,
    pub maker: String,
    pub year: Option<u16>,
    pub sort: Sort,
}

impl Default for Filters {
    fn default() -> Self {
        Self { query: String::new(), platform: String::new(), kind: "game".into(), genre: String::new(), maker: String::new(), year: None, sort: Sort::Newest }
    }
}

/// Consoles whose free homebrew comes from itch.io's public feeds (console id, name, tags).
pub const ITCH_CONSOLES: &[(&str, &str, &[&str])] = &[
    ("a2600", "Atari 2600", &["tag-atari-2600", "tag-atari"]),
    ("a7800", "Atari 7800", &["tag-atari-7800"]),
    ("pce", "TurboGrafx-16", &["tag-pc-engine", "tag-turbografx-16"]),
    ("ws", "WonderSwan", &["tag-wonderswan"]),
    ("vb", "Virtual Boy", &["tag-virtual-boy"]),
    ("md", "Genesis", &["tag-sega-genesis", "tag-mega-drive", "tag-genesis"]),
    ("sms", "Master System", &["tag-sega-master-system", "tag-master-system"]),
    ("gg", "Game Gear", &["tag-game-gear"]),
    ("nes", "NES", &["tag-nes"]),
    ("snes", "SNES", &["tag-snes", "tag-super-nintendo"]),
    ("n64", "Nintendo 64", &["tag-n64", "tag-nintendo-64", "tag-n64brew"]),
    ("gc", "GameCube", &["tag-gamecube"]),
    ("wii", "Wii", &["tag-wii"]),
    ("ps1", "PlayStation", &["tag-ps1", "tag-psx"]),
];

/// A free game listed on itch.io (downloads happen on its page).
#[derive(Clone, Debug)]
pub struct ItchEntry {
    pub title: String,
    pub link: String,
    pub image: String,
    pub summary: String,
}

impl ItchEntry {
    pub fn key(&self) -> String {
        let h = self.link.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        format!("itch-{h:016x}")
    }
}

enum Job {
    Itch { console: String, tags: Vec<String> },
    Catalog { platform: String, refresh: bool },
    Image { slug: String, url: String },
    Install { entry: HubEntry, dir: PathBuf },
}

enum Done {
    Itch(String, Result<Vec<ItchEntry>, String>),
    Progress(String, u32, u32),
    Catalog(String, Result<Vec<HubEntry>, String>),
    Image(String, Option<ColorImage>),
    Installed(String, Result<PathBuf, String>),
}

pub struct Discover {
    tx: Sender<Job>,
    catalog_tx: Sender<Job>,
    rx: Receiver<Done>,
    pub filters: Filters,
    pub page: usize,
    catalogs: HashMap<String, Vec<HubEntry>>,
    pub loading: HashMap<String, (u32, u32)>,
    pub errors: HashMap<String, String>,
    images: HashMap<String, Option<TextureHandle>>,
    pub installing: HashMap<String, ()>,
    pub installed: Vec<(String, Result<PathBuf, String>)>,
    pub itch: HashMap<String, Result<Vec<ItchEntry>, String>>,
    itch_loading: std::collections::HashSet<String>,
}

impl Discover {
    pub fn new(cache: PathBuf) -> Self {
        let (tx, jobs) = channel::<Job>();
        let (done_tx, rx) = channel::<Done>();
        let dir = cache;
        let (catalog_tx, catalog_jobs) = channel::<Job>();
        {
            let (dir, done_tx) = (dir.clone(), done_tx.clone());
            std::thread::spawn(move || {
                while let Ok(Job::Catalog { platform, refresh }) = catalog_jobs.recv() {
                    let r = load_catalog(&dir, &platform, refresh, &|got, total| {
                        let _ = done_tx.send(Done::Progress(platform.clone(), got, total));
                    });
                    if done_tx.send(Done::Catalog(platform, r)).is_err() {
                        break;
                    }
                }
            });
        }
        std::thread::spawn(move || {
            while let Ok(job) = jobs.recv() {
                let done = match job {
                    Job::Itch { console, tags } => Done::Itch(console, fetch_itch(&tags)),
                    Job::Catalog { platform, refresh } => {
                        let r = load_catalog(&dir, &platform, refresh, &|got, total| {
                            let _ = done_tx.send(Done::Progress(platform.clone(), got, total));
                        });
                        Done::Catalog(platform, r)
                    }
                    Job::Image { slug, url } => {
                        let img = fetch_image(&dir, &slug, &url);
                        Done::Image(slug, img)
                    }
                    Job::Install { entry, dir: dest } => {
                        let slug = entry.slug.clone();
                        let r = install(&entry, &dest);
                        if let Ok(rom) = &r {
                            let cover = dir.join("homebrew-hub").join(format!("{slug}.png"));
                            let stem = rom.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                            let target = dir.join(entry.console());
                            let _ = std::fs::create_dir_all(&target);
                            let _ = std::fs::remove_file(target.join(format!("{}.none", crate::covers::thumb_name(&stem))));
                            let _ = std::fs::copy(&cover, target.join(format!("{}.png", crate::covers::thumb_name(&stem))));
                        }
                        Done::Installed(slug, r)
                    }
                };
                if done_tx.send(done).is_err() {
                    break;
                }
            }
        });
        Self {
            tx,
            catalog_tx,
            rx,
            filters: Filters::default(),
            page: 0,
            catalogs: HashMap::new(),
            loading: HashMap::new(),
            errors: HashMap::new(),
            images: HashMap::new(),
            installing: HashMap::new(),
            installed: Vec::new(),
            itch: HashMap::new(),
            itch_loading: Default::default(),
        }
    }

    /// itch.io games for a console, fetched on first request.
    pub fn itch_games(&mut self, console: &str) -> Option<&Result<Vec<ItchEntry>, String>> {
        if !self.itch.contains_key(console) && !self.itch_loading.contains(console) {
            if let Some((_, _, tags)) = ITCH_CONSOLES.iter().find(|c| c.0 == console) {
                self.itch_loading.insert(console.to_string());
                let _ = self.tx.send(Job::Itch { console: console.to_string(), tags: tags.iter().map(|t| t.to_string()).collect() });
            }
        }
        self.itch.get(console)
    }

    pub fn itch_is_loading(&self, console: &str) -> bool {
        self.itch_loading.contains(console)
    }

    pub fn itch_image(&mut self, e: &ItchEntry) -> Option<&TextureHandle> {
        let key = e.key();
        if !self.images.contains_key(&key) {
            self.images.insert(key.clone(), None);
            if e.image.to_ascii_lowercase().contains(".png") {
                let _ = self.tx.send(Job::Image { slug: key.clone(), url: e.image.clone() });
            }
        }
        self.images.get(&key).and_then(|t| t.as_ref())
    }

    fn platforms(&self) -> Vec<&'static str> {
        PLATFORMS.iter().map(|p| p.0).filter(|p| self.filters.platform.is_empty() || self.filters.platform == *p).collect()
    }

    pub fn ensure_loaded(&mut self) {
        for p in self.platforms() {
            if !self.catalogs.contains_key(p) && !self.loading.contains_key(p) && !self.errors.contains_key(p) {
                self.loading.insert(p.to_string(), (0, 0));
                let _ = self.catalog_tx.send(Job::Catalog { platform: p.to_string(), refresh: false });
            }
        }
    }

    pub fn refresh(&mut self) {
        self.errors.clear();
        for (p, _, _) in PLATFORMS {
            if !self.loading.contains_key(*p) {
                self.loading.insert(p.to_string(), (0, 0));
                let _ = self.catalog_tx.send(Job::Catalog { platform: p.to_string(), refresh: true });
            }
        }
    }

    pub fn is_loading(&self) -> bool {
        self.platforms().iter().any(|p| self.loading.contains_key(*p))
    }

    pub fn progress(&self) -> Option<f32> {
        let (got, total) = self.loading.values().fold((0, 0), |a, (g, t)| (a.0 + g, a.1 + t));
        (total > 0).then(|| got as f32 / total as f32)
    }

    fn pool(&self) -> impl Iterator<Item = &HubEntry> {
        self.platforms().into_iter().filter_map(|p| self.catalogs.get(p)).flatten()
    }

    pub fn results(&self) -> Vec<HubEntry> {
        let f = &self.filters;
        let q = f.query.trim().to_lowercase();
        let genre = f.genre.to_lowercase();
        let mut v: Vec<HubEntry> = self
            .pool()
            .filter(|e| f.kind.is_empty() || e.typetag == f.kind)
            .filter(|e| q.is_empty() || e.title.to_lowercase().contains(&q) || e.developer.to_lowercase().contains(&q))
            .filter(|e| genre.is_empty() || e.tags.iter().any(|t| t.to_lowercase() == genre))
            .filter(|e| f.maker.is_empty() || e.developer == f.maker)
            .filter(|e| f.year.is_none() || e.year == f.year)
            .cloned()
            .collect();
        match f.sort {
            Sort::Newest => v.sort_by(|a, b| b.added.cmp(&a.added)),
            Sort::Title => v.sort_by_key(|e| e.title.to_lowercase()),
            Sort::YearNew => v.sort_by(|a, b| b.year.unwrap_or(0).cmp(&a.year.unwrap_or(0)).then(a.title.cmp(&b.title))),
            Sort::YearOld => v.sort_by(|a, b| a.year.unwrap_or(9999).cmp(&b.year.unwrap_or(9999)).then(a.title.cmp(&b.title))),
        }
        v
    }

    pub fn facets(&self) -> (Vec<String>, Vec<u16>, Vec<(String, usize)>) {
        let mut genres: BTreeMap<String, (String, usize)> = BTreeMap::new();
        let mut years: Vec<u16> = Vec::new();
        let mut makers: HashMap<String, usize> = HashMap::new();
        for e in self.pool() {
            for t in &e.tags {
                let key = t.to_lowercase();
                let g = genres.entry(key).or_insert_with(|| (title_case(t), 0));
                g.1 += 1;
            }
            if let Some(y) = e.year {
                if !years.contains(&y) {
                    years.push(y);
                }
            }
            *makers.entry(e.developer.clone()).or_default() += 1;
        }
        let mut g: Vec<(String, usize)> = genres.into_values().filter(|(_, n)| *n >= 3).collect();
        g.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut genres: Vec<String> = g.into_iter().take(40).map(|(n, _)| n).collect();
        genres.sort();
        years.sort_unstable_by(|a, b| b.cmp(a));
        let mut m: Vec<(String, usize)> = makers.into_iter().collect();
        m.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        (genres, years, m)
    }

    pub fn install(&mut self, entry: &HubEntry, dir: &Path) {
        self.installing.insert(entry.slug.clone(), ());
        let _ = self.tx.send(Job::Install { entry: entry.clone(), dir: dir.to_path_buf() });
    }

    pub fn image(&mut self, e: &HubEntry) -> Option<&TextureHandle> {
        if !self.images.contains_key(&e.slug) {
            self.images.insert(e.slug.clone(), None);
            if let Some(file) = &e.screenshot {
                let url = format!("{}/{}/{}", e.files_base, enc(&e.slug), enc(file));
                let _ = self.tx.send(Job::Image { slug: e.slug.clone(), url });
            }
        }
        self.images.get(&e.slug).and_then(|t| t.as_ref())
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Vec<PathBuf> {
        let mut new = Vec::new();
        while let Ok(done) = self.rx.try_recv() {
            match done {
                Done::Itch(console, r) => {
                    self.itch_loading.remove(&console);
                    self.itch.insert(console, r);
                }
                Done::Progress(p, got, total) => {
                    self.loading.insert(p, (got, total));
                }
                Done::Catalog(p, r) => {
                    self.loading.remove(&p);
                    match r {
                        Ok(v) => {
                            self.errors.remove(&p);
                            self.catalogs.insert(p, v);
                        }
                        Err(e) => {
                            self.errors.insert(p, e);
                        }
                    }
                }
                Done::Image(slug, img) => {
                    let tex = img.map(|i| ctx.load_texture(format!("hh-{slug}"), i, TextureOptions::LINEAR));
                    self.images.insert(slug, tex);
                }
                Done::Installed(slug, r) => {
                    self.installing.remove(&slug);
                    if let Ok(p) = &r {
                        new.push(p.clone());
                    }
                    self.installed.push((slug, r));
                }
            }
            ctx.request_repaint();
        }
        if !self.loading.is_empty() || !self.installing.is_empty() || !self.itch_loading.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        new
    }
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&#039;", "'").replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">")
}

fn tag_text<'a>(item: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let a = item.find(&open)? + open.len();
    let b = item[a..].find(&close)? + a;
    Some(&item[a..b])
}

/// Free games from itch.io's RSS feeds, skipping PC/phone releases so real console homebrew remains.
fn fetch_itch(tags: &[String]) -> Result<Vec<ItchEntry>, String> {
    let mut out: Vec<ItchEntry> = Vec::new();
    let mut any_ok = false;
    for tag in tags {
        let Ok(body) = curl(&[format!("https://itch.io/games/free/{tag}.xml")]) else { continue };
        any_ok = true;
        let text = String::from_utf8_lossy(&body);
        for item in text.split("<item>").skip(1) {
            let item = item.split("</item>").next().unwrap_or("");
            let platforms = tag_text(item, "platforms").unwrap_or("");
            if ["<windows>", "<osx>", "<linux>", "<android>"].iter().any(|p| platforms.contains(p)) {
                continue;
            }
            let Some(link) = tag_text(item, "link") else { continue };
            if out.iter().any(|e| e.link == link) {
                continue;
            }
            let desc = tag_text(item, "description").unwrap_or("").trim_start_matches("<![CDATA[");
            let summary = desc.split('<').next().unwrap_or("").trim();
            out.push(ItchEntry {
                title: crate::library::clean_title(&unescape(tag_text(item, "plainTitle").or_else(|| tag_text(item, "title")).unwrap_or("Untitled"))),
                link: link.to_string(),
                image: tag_text(item, "imageurl").unwrap_or("").to_string(),
                summary: unescape(summary),
            });
        }
    }
    if !any_ok {
        return Err("Couldn't reach itch.io. Check your internet connection.".into());
    }
    Ok(out)
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn curl(urls: &[String]) -> Result<Vec<u8>, String> {
    let out = crate::quiet(crate::platform::curl())
        .args(["-sfL", "--max-time", "60"])
        .args(urls)
        .output()
        .map_err(|e| format!("Couldn't run curl: {e}"))?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err("Couldn't reach Homebrew Hub. Check your internet connection and try again.".into());
    }
    Ok(out.stdout)
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn page_url(platform: &str, page: u32) -> String {
    format!("{API}/search?platform={}&page_elements=10&page={page}", enc(platform))
}

fn load_catalog(cache: &Path, platform: &str, refresh: bool, progress: &dyn Fn(u32, u32)) -> Result<Vec<HubEntry>, String> {
    let dir = cache.join("homebrew-hub");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("catalog-{}.json", platform.to_ascii_lowercase()));
    let fresh = std::fs::metadata(&file).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|age| age < CACHE_AGE);
    let raw: Vec<serde_json::Value> = if fresh && !refresh {
        let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| e.to_string())?
    } else {
        let first: serde_json::Value = serde_json::from_slice(&curl(&[page_url(platform, 1)])?).map_err(|e| format!("Unexpected response from Homebrew Hub: {e}"))?;
        let total = first["page_total"].as_u64().unwrap_or(1) as u32;
        let mut all: Vec<serde_json::Value> = first["entries"].as_array().cloned().unwrap_or_default();
        progress(1, total);
        let mut page = 2;
        while page <= total {
            let last = (page + 15).min(total);
            let urls: Vec<String> = (page..=last).map(|p| page_url(platform, p)).collect();
            let body = curl(&urls)?;
            for v in serde_json::Deserializer::from_slice(&body).into_iter::<serde_json::Value>().flatten() {
                all.extend(v["entries"].as_array().cloned().unwrap_or_default());
            }
            progress(last, total);
            page = last + 1;
        }
        if let Ok(json) = serde_json::to_string(&all) {
            let _ = std::fs::write(&file, json);
        }
        all
    };
    let mut seen = std::collections::HashSet::new();
    Ok(raw.iter().map(|e| parse_entry(e, platform)).filter(|e| seen.insert(e.slug.clone())).collect())
}

fn parse_entry(e: &serde_json::Value, platform: &str) -> HubEntry {
    let files = e["files"].as_array().cloned().unwrap_or_default();
    let rom = files
        .iter()
        .find(|f| f["default"].as_bool() == Some(true))
        .or_else(|| files.iter().find(|f| f["playable"].as_bool() == Some(true)))
        .and_then(|f| f["filename"].as_str())
        .map(str::to_string);
    let year = e["date"].as_str().and_then(|d| d.get(..4)).and_then(|y| y.parse::<u16>().ok()).filter(|y| (1985..=2100).contains(y));
    let tags = e["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str())
                .filter(|t| !t.contains(':') && !t.to_lowercase().contains("gbcompo") && !t.to_lowercase().starts_with("gameboy"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    HubEntry {
        slug: e["slug"].as_str().unwrap_or_default().to_string(),
        title: crate::library::clean_title(e["title"].as_str().unwrap_or("Untitled").trim()),
        developer: e["developer"].as_str().unwrap_or("Unknown developer").trim().to_string(),
        platform: e["platform"].as_str().unwrap_or(platform).to_string(),
        typetag: e["typetag"].as_str().unwrap_or("game").to_string(),
        tags,
        year,
        added: e["firstadded_date"].as_str().unwrap_or("").to_string(),
        rom,
        screenshot: pick_image(&e["screenshots"]),
        files_base: files_base(e["baserepo"].as_str()),
    }
}

fn files_base(baserepo: Option<&str>) -> String {
    let repo = baserepo.and_then(|r| r.strip_prefix("https://github.com/")).unwrap_or("gbdev/database").trim_end_matches('/');
    format!("https://raw.githubusercontent.com/{repo}/master/entries")
}

fn pick_image(v: &serde_json::Value) -> Option<String> {
    let names: Vec<&str> = v.as_array()?.iter().filter_map(|s| s.as_str()).collect();
    names
        .iter()
        .find(|n| n.eq_ignore_ascii_case("cover.png"))
        .or_else(|| names.iter().find(|n| n.to_ascii_lowercase().ends_with(".png")))
        .map(|s| s.to_string())
}

fn fetch_image(cache: &Path, slug: &str, url: &str) -> Option<ColorImage> {
    let dir = cache.join("homebrew-hub");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{slug}.png"));
    if !path.exists() {
        let ok = crate::quiet(crate::platform::curl()).args(["-sfL", "--max-time", "20", "-o"]).arg(&path).arg(url).status().is_ok_and(|s| s.success());
        if !ok {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    }
    crate::covers::decode(&path)
}

fn install(e: &HubEntry, dir: &Path) -> Result<PathBuf, String> {
    let file = e.rom.as_ref().ok_or("This entry has no playable ROM file.")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let safe: String = e.title.chars().map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    let ext = Path::new(file).extension().and_then(|x| x.to_str()).unwrap_or("gb");
    let dest = dir.join(format!("{} ({}).{ext}", safe.trim(), e.developer.trim()));
    let url = format!("{}/{}/{}", e.files_base, enc(&e.slug), enc(file));
    let ok = crate::quiet(crate::platform::curl()).args(["-sfL", "--max-time", "60", "-o"]).arg(&dest).arg(&url).status().is_ok_and(|s| s.success());
    if !ok || std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) < 0x150 {
        let _ = std::fs::remove_file(&dest);
        return Err(format!("Download of {} failed. Try again later.", e.title));
    }
    Ok(dest)
}

pub struct Source {
    pub name: &'static str,
    pub url: &'static str,
    pub what: &'static str,
}

pub const SOURCES: &[(&str, &[Source])] = &[
    (
        "Free games",
        &[
            Source { name: "RomsFun", url: "https://romsfun.com", what: "Game file archive covering many consoles." },
            Source { name: "Vimm's Vault", url: "https://vimm.net/vault", what: "Long-running archive of console games and manuals." },
            Source { name: "RomsGames", url: "https://www.romsgames.net", what: "Game file archive with emulator downloads." },
            Source { name: "RomsRetro", url: "https://romsretro.com", what: "Archive focused on retro consoles." },
            Source { name: "R-Roms", url: "https://r-roms.gitlab.io", what: "Index of game collections hosted elsewhere." },
            Source { name: "Emuparadise", url: "https://www.emuparadise.me/roms-isos-games.php", what: "Classic emulation site with game listings." },
            Source { name: "xenia-emulator", url: "https://xenia-emulator.com/xbox-360-roms", what: "Xbox 360 game listings." },
            Source { name: "Ultimate ROM Collection (Internet Archive)", url: "https://archive.org/download/ultimate-rom-collection", what: "Large multi-console collection on the Internet Archive." },
            Source { name: "Romspedia", url: "https://www.romspedia.com", what: "Game file archive covering many consoles." },
            Source { name: "WoWroms", url: "https://wowroms.com", what: "Large game archive with a page per game and download links for most consoles, including Game Gear, Master System, Genesis, NES, SNES, Game Boy, GBA, N64, PlayStation and more. Games usually arrive as .zip or .7z files, which this app opens directly: add the file and it appears under the right console." },
            Source { name: "Homebrew Hub", url: "https://hh.gbdev.io", what: "Over a thousand free Game Boy, Game Boy Color and GBA games, shared by their creators. Browse them all under Free games." },
            Source { name: "itch.io: NES games", url: "https://itch.io/games/tag-nes", what: "New indie games made for the NES by today's developers; many are free or pay-what-you-want." },
            Source { name: "itch.io: Game Boy games", url: "https://itch.io/games/tag-gameboy", what: "Indie Game Boy releases, including GB Studio games." },
            Source { name: "NESdev community", url: "https://www.nesdev.org", what: "The NES homebrew community: forums, competitions and freely released games." },
            Source { name: "itch.io: Game Boy Advance games", url: "https://itch.io/games/tag-gba", what: "Indie GBA games, including GBA Jam entries. They run on our built-in GBA core." },
            Source { name: "itch.io: Nintendo 64 games", url: "https://itch.io/games/tag-n64", what: "New N64 homebrew, including N64brew Game Jam entries (.z64). Plays on our early N64 core." },
            Source { name: "N64brew community", url: "https://n64brew.dev/wiki/Main_Page", what: "N64 homebrew developers: game jams, demos and tools, all freely released." },
            Source { name: "itch.io: GameCube games", url: "https://itch.io/games/tag-gamecube", what: "GameCube homebrew (.dol). Text and 2D homebrew runs today; 3D arrives with our GX graphics core." },
            Source { name: "GC-Forever community", url: "https://www.gc-forever.com/wiki/index.php?title=Main_Page", what: "GameCube homebrew wiki and forum: apps, games and guides." },
            Source { name: "F-Droid (Android)", url: "https://f-droid.org", what: "Free and open-source Android apps. Install it inside an Android phone or tablet from the Phones & Tablets tab." },
        ],
    ),
    (
        "Back up games you own",
        &[
            Source { name: "GBxCart RW", url: "https://www.gbxcart.com", what: "USB reader that dumps Game Boy / GBA cartridges and their save files to your PC." },
            Source { name: "Sanni Cart Reader", url: "https://github.com/sanni/cartreader", what: "Open-source hardware that dumps NES, SNES, N64, Game Boy and many more cartridges." },
            Source { name: "Redump disc guides", url: "http://wiki.redump.org/index.php?title=Dumping_Guides", what: "How to make exact copies of your PlayStation, PS2 and GameCube discs with a PC drive." },
            Source { name: "PS2 BIOS guide (PCSX2)", url: "https://pcsx2.net/docs/setup/bios/", what: "How to dump the BIOS from your own PlayStation 2, required by PS2 emulation." },
        ],
    ),
    (
        "Dump games from your own console",
        &[
            Source { name: "Original Xbox: files xemu needs", url: "https://xemu.app/docs/required-files/", what: "Official xemu guide to dumping the BIOS, MCPX ROM and hard disk from your own Xbox (softmodded consoles can FTP them off)." },
            Source { name: "Xbox 360: modding (RGH/RGH3)", url: "https://consolemods.org/wiki/Xbox_360:RGH", what: "ConsoleMods guide to RGH, the hardware mod that lets your own 360 run homebrew and copy your games to a PC." },
            Source { name: "Xbox 360: ripping games", url: "https://github.com/xenia-project/xenia/wiki/Quickstart", what: "The Xenia project's quickstart, including how to rip your own discs and installed games." },
            Source { name: "Xbox One/Series: Developer Mode", url: "https://learn.microsoft.com/en-us/windows/uwp/xbox-apps/devkit-activation", what: "Microsoft's official Developer Mode runs your own apps and homebrew on the console. It does not unlock retail games for copying." },
            Source { name: "PS3: dumping games", url: "https://wiki.rpcs3.net/index.php?title=Help%3ADumping_PlayStation_3_games", what: "RPCS3's guide: dump discs with a compatible Blu-ray drive, or from a PS3 running custom firmware/HEN." },
            Source { name: "PS4: dumping with GoldHEN", url: "https://shadps4.net/quickstart/", what: "shadPS4's quickstart: a jailbroken PS4 (GoldHEN) dumps your games and system files over FTP." },
            Source { name: "GameCube/Wii: CleanRip", url: "https://dolphin-emu.org/docs/guides/ripping-games/", what: "Dolphin's guide to ripping your discs with CleanRip on a Wii running the Homebrew Channel." },
            Source { name: "PS2: BIOS and discs", url: "https://pcsx2.net/docs/setup/bios/", what: "Dump your PS2 BIOS with homebrew; PS2 discs rip with any PC DVD drive (see the Redump guides above)." },
        ],
    ),
    (
        "Name your files so artwork is found",
        &[
            Source { name: "No-Intro DAT-o-MATIC", url: "https://datomatic.no-intro.org", what: "Checksums and standard names for cartridge games. Correct names let the app find box art." },
            Source { name: "Redump database", url: "http://redump.org", what: "Checksums and standard names for disc-based games." },
        ],
    ),
];

pub struct Guide {
    pub title: &'static str,
    pub summary: &'static str,
    pub steps: &'static [&'static str],
}

pub const GUIDES: &[Guide] = &[
    Guide {
        title: "Free games for every console",
        summary: "Homebrew made by independent developers, free to download and share.",
        steps: &[
            "Open Discover and pick a console. Game Boy, Game Boy Color and Game Boy Advance games come from Homebrew Hub and add to your library in one click.",
            "The other console buttons (Atari, Master System, Game Gear, Genesis, TurboGrafx-16, WonderSwan, Virtual Boy, NES, SNES, Nintendo 64, GameCube, Wii, PlayStation) list free homebrew from itch.io. Open a game's page, download it, then use Add files.",
            "Many developers also release their games for free on their own websites; the download page tells you which console and file type it is.",
            "Some classic games have been released for free by their rights holders. Only download a commercial game if the people who own it have made it free.",
        ],
    },
    Guide {
        title: "Add your games",
        summary: "Add files once; the library keeps them organised by console.",
        steps: &[
            "Open Games and pick a console on the left, then click Add files. Games are copied into the app's own games folder, so moving or deleting the originals is safe.",
            "Or click Choose game folder and select a folder of your files (subfolders are scanned too).",
            "Nintendo: NES (.nes, .fds), SNES (.sfc, .smc), Virtual Boy (.vb), Nintendo 64 (.z64, .n64, .v64), Game Boy and Color (.gb, .gbc), Game Boy Advance (.gba), GameCube (.iso, .gcm, .dol).",
            "Sega: Master System (.sms), Game Gear (.gg), Genesis / Mega Drive (.md, .gen, .smd, .bin).",
            "Sony: PlayStation (.cue + .bin, .iso) with your BIOS.",
            "Others: Atari 2600 (.a26, .bin), Atari 7800 (.a78), TurboGrafx-16 (.pce), WonderSwan and Color (.ws, .wsc).",
            ".zip files work for cartridge games. Keep the standard No-Intro or Redump file name (for example \"Sonic the Hedgehog (USA, Europe)\") so box art is found.",
        ],
    },
    Guide {
        title: "Back up your own cartridges",
        summary: "Make a legal copy of a cartridge you own, including its save.",
        steps: &[
            "Use a cartridge reader: a Sanni Cart Reader (open source) covers NES, SNES, N64, Game Boy, GBA, Genesis, Master System, Game Gear, TurboGrafx-16, WonderSwan, Virtual Boy and Atari with the right adapters. GBxCart RW is a simple option for Game Boy and GBA.",
            "Insert the cartridge and use the reader's software to save the game to your PC.",
            "Also back up the save (.sav / .srm) and give it the same name as the game (Game.md + Game.srm) to carry on where you left off.",
            "Put the files in your games folder, or use Add files.",
        ],
    },
    Guide {
        title: "Back up your own discs",
        summary: "PlayStation, PS2, Saturn, Sega CD, Dreamcast and GameCube games.",
        steps: &[
            "PlayStation, PS2, Saturn, Sega CD and 3DO discs: follow the Redump dumping guide with a PC CD/DVD drive; save as .bin + .cue (or .iso for PS2).",
            "Dreamcast: GD-ROMs can't be read by PC drives; dump them on your own Dreamcast with an SD card adapter and a dumping tool.",
            "GameCube and Wii: on a Wii with the Homebrew Channel, CleanRip copies your discs to USB as .iso.",
            "Got a .zip or .7z of .bin/.cue files? Under PlayStation (or GameCube/Wii), click Convert disc to turn it into a single .iso.",
        ],
    },
    Guide {
        title: "Copy games from newer consoles you own",
        summary: "Handhelds and modern consoles, using official Developer Mode or homebrew.",
        steps: &[
            "Nintendo Switch: with homebrew on your console, nxdumptool saves your cartridges and installed games (.xci / .nsp); Lockpick_RCM saves your prod.keys.",
            "Nintendo 3DS and DS: GodMode9 dumps cartridges and installed titles.",
            "PSP: homebrew such as a UMD dumper copies discs to the memory stick as .iso; PS Vita: VitaShell copies games.",
            "PS3: HEN or custom firmware dumps discs and PSN games; PS4: GoldHEN adds an FTP server for your games.",
            "Original Xbox: a softmod adds an FTP server for your games, BIOS and hard drive. Xbox 360: an RGH-modded console can copy discs and installed games.",
            "Xbox One / Series: Developer Mode is official but retail games stay encrypted; they can't be copied.",
            "Modding can void warranties and may be restricted where you live; only copy games you own.",
        ],
    },
    Guide {
        title: "Xbox, Xbox 360, Xbox One and Series X|S",
        summary: "What each Xbox can open today, and what's coming.",
        steps: &[
            "Original Xbox: add your disc images (.iso / .xiso) or a game's default.xbe. Add your console's flash BIOS and MCPX boot ROM in Settings > BIOS files; the app never downloads them.",
            "Xbox 360: add discs (.iso) or games extracted to folders (default.xex). The CPU and game loader are built; graphics is next.",
            "Xbox One and Series X|S: store and disc games are encrypted with keys that never leave the console, so no PC can run them. You can open a game decrypted on your own Xbox (choose its MicrosoftGame.config or appxmanifest.xml), and the app shows what it needs.",
            "Beware of websites and GitHub pages offering a ready-made \"Xbox One emulator\" download: none exist, and those downloads are often malware.",
        ],
    },
    Guide {
        title: "BIOS files and keys",
        summary: "What each console needs, and where to add it.",
        steps: &[
            "Most consoles here need nothing extra: NES, SNES, Game Boy, GBA, N64, GameCube, Master System, Game Gear, Genesis, TurboGrafx-16, WonderSwan, Virtual Boy and Atari start on their own.",
            "PlayStation: a BIOS dumped from your console (for example SCPH1001.BIN, 512 KB). Famicom Disk System: disksys.rom (8 KB).",
            "Wii: the common key (16 bytes) from your console, saved by BootMii's key dump. Switch: prod.keys from Lockpick_RCM.",
            "Original Xbox: the flash BIOS (256 KB or 1 MB) and MCPX boot ROM (512 bytes), both dumped from your own Xbox.",
            "Add them in Settings > BIOS files. Each console can hold several (for example different regions); pick the one it uses.",
            "These files belong to the console makers, so the app can't download them; Find on this PC looks in your Downloads folder for dumps you already have.",
        ],
    },
    Guide {
        title: "File types and conversion",
        summary: "Archives, disc images and what to do with them.",
        steps: &[
            "Cartridge games inside a .zip open directly; no need to extract.",
            "PlayStation games are best as .bin + .cue (keeps CD music); a single .iso also works but drops music tracks.",
            "Games downloaded as a data .iso containing .bin/.cue files are unpacked automatically the first time you play.",
            "Convert disc (PlayStation, GameCube, Wii) turns .zip/.7z archives and BIN/CUE into .iso, merges multi-file CUE sheets, or rebuilds BIN/CUE from an .iso.",
        ],
    },
    Guide {
        title: "iPhone and iPad apps",
        summary: "What the iPhone & iPad section can open today.",
        steps: &[
            "Open Phones & Tablets and click Open app under iPhone or iPad, then choose an .ipa file.",
            "The app reads the package and tells you the app's name, version, the iOS version it needs, its processor type, and whether it is encrypted.",
            "App Store apps are encrypted to the device that downloaded them; only decrypted copies of apps you own can ever run. Apple doesn't allow copying iOS itself, so the core recreates the system from scratch.",
            "Playing apps is in progress: classic iPhone OS 2-4 era apps (32-bit ARM) come first.",
        ],
    },
    Guide {
        title: "Get games from your own discs and consoles",
        summary: "What works, what needs special hardware, and what no tool can do. Only for games you own.",
        steps: &[
            "Open the Game Dump Helper that comes in the app's folder. \"gamedump check <file>\" says what a dump is and if it's complete; \"gamedump hash <file>\" gives SHA-1 and CRC-32 to compare with redump.org.",
            "PlayStation 1 and 2: an ordinary PC disc drive reads them. For PlayStation 1 make a raw .bin plus .cue (ImgBurn, or the tools Redump lists); for PlayStation 2 \"gamedump rip E: game.iso\" copies the data disc.",
            "Original Xbox and Xbox 360: retail discs hide their game data from normal drives, so they need a disc drive with special firmware. Redump documents the accepted methods; this tool doesn't do that part.",
            "GameCube and Wii: use a homebrew disc dumper on your own Wii, and keep the full image (trimmed or compressed ones can break games). The Helper warns when an image is smaller than a full disc.",
            "Xbox One, Series X|S, PlayStation 4 and 5: store and disc games are encrypted for their console and can't be copied off it. Microsoft's Developer Mode runs developer packages, and folders of already decrypted games can be opened here. This app contains no keys and no console exploits.",
        ],
    },
    Guide {
        title: "PlayStation 5 and 4 games",
        summary: "Only games decrypted on your own console can be opened, and running them is still being built.",
        steps: &[
            "Store and disc games are locked to the console they came from, and no PC can unlock them. Nothing here, or in any emulator, gets around that.",
            "If you own a jailbroken console, you can dump a game you own to a folder. Add that folder (or its eboot.bin) and the app reads it: the title, the program's segments, and which PlayStation system functions the game calls.",
            "Playing needs our own versions of those system libraries and a graphics backend for the console's GPU. That work is in progress; the reader is the first stage.",
        ],
    },
    Guide {
        title: "Play in your browser",
        summary: "The website's Use online tab runs games without installing anything.",
        steps: &[
            "Open the download site and click Use online, then drop a game onto the page.",
            "NES, SNES, Game Boy / Color, Game Boy Advance, Genesis, Master System, Game Gear, TurboGrafx-16, Atari 2600 / 7800, WonderSwan and Virtual Boy games play online, on phones and tablets too (on-screen controls); PlayStation, Nintendo 64, GameCube and newer plays in the desktop app.",
            "Saves are kept in your browser. Files never leave your computer.",
        ],
    },
    Guide {
        title: "Android phones and tablets",
        summary: "Coming with our own 64-bit PC core.",
        steps: &[
            "Android runs on a virtual PC. Everything in this app uses its own cores, so Android phones and tablets arrive once our PC core runs 64-bit systems.",
            "Until then, Phones & Tablets can open iPhone and iPad apps (.ipa) to show what's inside them.",
        ],
    },
    Guide {
        title: "Controllers and controls",
        summary: "Xbox, PlayStation and Switch Pro controllers work out of the box.",
        steps: &[
            "Plug in or pair your controller; the header shows it as connected.",
            "In a game, click Controls to remap keys or controller buttons. Press a button to see it light up.",
            "Choose This game only to give one game its own layout.",
            "Turbo A and Turbo B repeat the button while held.",
        ],
    },
    Guide {
        title: "No sound or crackling?",
        summary: "Quick checks when a game is silent or crackles.",
        steps: &[
            "Open Settings and check that Audio is not muted and the volume is up.",
            "Sound plays through Windows' default output device; change it in the Windows sound settings, then restart the app.",
            "Crackling usually means the game can't keep full speed: close other heavy programs, turn off Smooth motion, or try Frame skip 1.",
            "Audio is skipped while fast-forwarding and while paused.",
        ],
    },
    Guide {
        title: "Install Arch Linux in a VM",
        summary: "A full 64-bit Linux desktop in a window.",
        steps: &[
            "Open Virtual Machines and click the Arch Linux template.",
            "Click Download Arch Linux ISO; it is verified against the official checksum automatically.",
            "Press Start, choose Arch Linux install medium, then type archinstall and follow the prompts (disk: /dev/vda).",
            "When it finishes, power off, click Detach ISO and start the VM again.",
        ],
    },
];
