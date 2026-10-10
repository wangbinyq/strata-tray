//! Strata 托盘: 面板上只有一个圆点, 颜色就是状态, 点开是详情.
//!
//! 状态来源: `/status` (每秒, 实时) 和 `/v1/status` (每 5 秒, 模型/GPU/内存).
//! 状态的判定和 Strata 自己的 `/metrics` 一致: busy 且还没有第一个 token 是 Reading,
//! busy 且已经在出 token 是 Generating, 其余 Idle, 连不上是 Error.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;
const UNIT: &str = "strata";
const LIVE_EVERY: Duration = Duration::from_secs(1);
const SLOW_EVERY: Duration = Duration::from_secs(5);
const TOTALS_EVERY: Duration = Duration::from_secs(30);
const KEY_ENV: &str = "STRATA_API_KEY";
const KEY_FILE: &str = ".config/strata/service.env";

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Idle,
    Reading,
    Generating,
    Error,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Idle => "Idle",
            State::Reading => "Reading",
            State::Generating => "Generating",
            State::Error => "Error",
        }
    }

    /// Strata 自己的状态色, 取自 serve/web/tokens.css 的 dark 一套
    /// (monitor 页是 data-theme="dark", 面板也是深色):
    /// generating=--st-accent, reading=--st-info, queued=--st-warn, error=--st-danger, 其余 --st-ink-muted
    fn rgb(self) -> (u8, u8, u8) {
        match self {
            State::Idle => (0x8e, 0x97, 0x9f),
            State::Reading => (0x3a, 0xa9, 0xff),
            State::Generating => (0x34, 0xd3, 0x99),
            State::Error => (0xff, 0x5a, 0x6e),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Totals {
    prompt: u64,
    reused: u64,
    output: u64,
}

#[derive(Clone, Default)]
struct Snapshot {
    state: State,
    note: Option<String>,
    live_at: Option<Instant>,
    slow_at: Option<Instant>,
    has_key: bool,
    // /status
    busy: bool,
    queued: i64,
    phase: Option<String>,
    prompt_tokens: Option<i64>,
    generated: Option<i64>,
    max_tokens: Option<i64>,
    elapsed_s: Option<f64>,
    tok_s: Option<f64>,
    tok_s_mean: Option<f64>,
    // /v1/status
    model: Option<String>,
    engine: Option<String>,
    ctx: Option<i64>,
    uptime_s: Option<i64>,
    loaded: bool,
    requests: Option<i64>,
    in_flight: Option<i64>,
    images: bool,
    last: Option<(f64, f64, f64)>,
    gpu: Option<(i64, f64, f64)>,
    ram: Option<(f64, f64)>,
    /// /metrics 的 totals: 服务器启动至今累计 (30 秒拉一次)
    totals: Option<Totals>,
}

impl Snapshot {
    fn derive_state(&mut self) {
        self.state = if self.busy {
            if self.tok_s.is_some() {
                State::Generating
            } else {
                State::Reading
            }
        } else {
            State::Idle
        };
    }

    fn fail(&mut self, why: String) {
        self.state = State::Error;
        self.note = Some(why);
        self.busy = false;
        self.queued = 0;
        self.phase = None;
        self.prompt_tokens = None;
        self.generated = None;
        self.elapsed_s = None;
        self.tok_s = None;
        self.tok_s_mean = None;
    }
}

fn api_key() -> Option<String> {
    if let Ok(k) = std::env::var(KEY_ENV) {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Some(k);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let text = std::fs::read_to_string(format!("{home}/{KEY_FILE}")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix(&format!("{KEY_ENV}=")) {
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// 服务器地址: 默认 127.0.0.1:8080, 可以用 STRATA_TRAY_HOST / STRATA_TRAY_PORT 覆盖.
fn endpoint() -> &'static (String, u16) {
    static ENDPOINT: OnceLock<(String, u16)> = OnceLock::new();
    ENDPOINT.get_or_init(|| {
        let host = std::env::var("STRATA_TRAY_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string());
        let port = std::env::var("STRATA_TRAY_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
        (host, port)
    })
}

/// 一个最小的 HTTP/1.0 客户端: 只用本机, 所以不引 HTTP 库, 也不做 TLS.
fn http_get(path: &str, key: Option<&str>) -> Result<String, String> {
    let (host, port) = endpoint();
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("地址无效: {e}"))?;
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_millis(1500))
        .map_err(|e| format!("连不上 {host}:{port} ({e})"))?;
    sock.set_read_timeout(Some(Duration::from_secs(4))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(4))).ok();

    let mut req = format!("GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nAccept: application/json\r\n");
    if let Some(k) = key {
        req.push_str(&format!("Authorization: Bearer {k}\r\n"));
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes()).map_err(|e| format!("发送请求失败 ({e})"))?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end;
    loop {
        let n = sock.read(&mut chunk).map_err(|e| format!("读取响应失败 ({e})"))?;
        if n == 0 {
            return Err("响应中断".into());
        }
        raw.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find(&raw, b"\r\n\r\n") {
            head_end = pos + 4;
            break;
        }
        if raw.len() > 1 << 20 {
            return Err("响应头过大".into());
        }
    }

    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut code = 0u16;
    let mut length: Option<usize> = None;
    for (i, line) in head.split("\r\n").enumerate() {
        if i == 0 {
            code = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        } else if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().ok();
        }
    }

    match code {
        200 => {}
        401 | 403 => return Err(format!("API key 被拒绝 (HTTP {code}), 检查 ~/{KEY_FILE}")),
        0 => return Err("响应状态行异常".into()),
        c => return Err(format!("HTTP {c}")),
    }

    let want = length.ok_or("响应没有 Content-Length")?;
    while raw.len() - head_end < want {
        let n = sock.read(&mut chunk).map_err(|e| format!("读取正文失败 ({e})"))?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    let body = &raw[head_end..(head_end + want).min(raw.len())];
    Ok(String::from_utf8_lossy(body).into_owned())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn json(body: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(body).map_err(|e| format!("JSON 解析失败 ({e})"))
}

fn poll_live(snap: &mut Snapshot, key: &str) -> Result<(), String> {
    let v = json(&http_get("/status", Some(key))?)?;
    snap.busy = v["busy"].as_bool().unwrap_or(false);
    snap.queued = v["queued"].as_i64().unwrap_or(0);
    snap.phase = v["phase"].as_str().map(str::to_string);
    snap.prompt_tokens = v["prompt_tokens"].as_i64();
    snap.generated = v["generated"].as_i64();
    snap.max_tokens = v["max_tokens"].as_i64();
    snap.elapsed_s = v["elapsed_s"].as_f64();
    // /status 只在第一个 token 到了以后才给 tokens_per_s, 所以它就是 "已经在生成" 的标记
    snap.tok_s = v["tokens_per_s"].as_f64();
    snap.tok_s_mean = v["tokens_per_s_mean"].as_f64();
    snap.live_at = Some(Instant::now());
    Ok(())
}

fn poll_slow(snap: &mut Snapshot, key: &str) -> Result<(), String> {
    let v = json(&http_get("/v1/status", Some(key))?)?;
    snap.model = v["model"].as_str().map(str::to_string);
    snap.engine = v["engine"].as_str().map(str::to_string);
    snap.ctx = v["context"]["native"].as_i64();
    snap.uptime_s = v["uptime_s"].as_i64();
    snap.loaded = v["loaded"].as_bool().unwrap_or(false);
    snap.images = v["vision"]["enabled"].as_bool().unwrap_or(false);
    snap.requests = v["activity"]["requests"].as_i64();
    snap.in_flight = v["activity"]["in_flight"].as_i64();
    if let Some(t) = v.get("last_timings").filter(|t| t.is_object()) {
        if let (Some(n), Some(ms), Some(ps)) =
            (t["predicted_n"].as_f64(), t["predicted_ms"].as_f64(), t["predicted_per_second"].as_f64())
        {
            snap.last = Some((n, ms / 1000.0, ps));
        }
    }
    if let Some(m) = v.get("machine").filter(|m| m.is_object()) {
        if let Some(u) = m["gpu"]["util_pct"].as_i64() {
            snap.gpu = Some((
                u,
                m["gpu"]["temp_c"].as_f64().unwrap_or(f64::NAN),
                m["gpu"]["power_w"].as_f64().unwrap_or(f64::NAN),
            ));
        }
        if let (Some(used), Some(total)) = (m["ram"]["used_gib"].as_f64(), m["ram"]["total_gib"].as_f64()) {
            snap.ram = Some((used, total));
        }
    }
    snap.slow_at = Some(Instant::now());
    Ok(())
}

/// /metrics 里的 totals: 服务器启动至今的累计 token 数.
fn poll_totals(snap: &mut Snapshot, key: &str) -> Result<(), String> {
    let v = json(&http_get("/metrics", Some(key))?)?;
    let t = &v["totals"];
    snap.totals = Some(Totals {
        prompt: t["prompt_tokens"].as_u64().unwrap_or(0),
        reused: t["reused"].as_u64().unwrap_or(0),
        output: t["output_tokens"].as_u64().unwrap_or(0),
    });
    Ok(())
}

/// 没有 API key 时的退路: /health 不需要认证, 但只有模型名和是否加载.
fn poll_health(snap: &mut Snapshot) -> Result<(), String> {
    let v = json(&http_get("/health", None)?)?;
    snap.model = v["model"].as_str().map(str::to_string);
    snap.ctx = v["max_context"].as_i64();
    snap.loaded = v["loaded"].as_bool().unwrap_or(false);
    snap.images = v["images"].as_bool().unwrap_or(false);
    snap.busy = false;
    snap.queued = 0;
    snap.slow_at = Some(Instant::now());
    Ok(())
}

struct Tray {
    snap: Snapshot,
    quit: bool,
}

impl Tray {
    fn info(label: String) -> MenuItem<Tray> {
        MenuItem::Standard(StandardItem { label, enabled: false, ..Default::default() })
    }

    fn action(label: &str, run: fn()) -> MenuItem<Tray> {
        MenuItem::Standard(StandardItem {
            label: label.into(),
            activate: Box::new(move |_| run()),
            ..Default::default()
        })
    }

    fn details(&self) -> Vec<String> {
        let s = &self.snap;
        let mut out = Vec::new();
        let mut head = format!("状态: {}", s.state.name());
        if let Some(p) = &s.phase {
            if s.busy {
                head.push_str(&format!("  ({p})"));
            }
        }
        out.push(head);

        if s.busy {
            if let Some(t) = s.tok_s {
                match s.tok_s_mean {
                    Some(m) => out.push(format!("{t:.1} tok/s  (均值 {m:.1})")),
                    None => out.push(format!("{t:.1} tok/s")),
                }
            } else if let Some(p) = s.prompt_tokens {
                out.push(format!("读入 prompt: {p} tokens"));
            }
            if let Some(g) = s.generated {
                let max = s.max_tokens.map(|m| format!(" / {m}")).unwrap_or_default();
                let secs = s.elapsed_s.map(|e| format!("  {e:.1} s")).unwrap_or_default();
                out.push(format!("已生成 {g}{max}{secs}"));
            }
        }
        if s.queued > 0 {
            out.push(format!("排队 {}", s.queued));
        }
        if let (Some(used), Some(ctx)) = (s.prompt_tokens, s.ctx) {
            if ctx > 0 {
                out.push(format!("上下文 {used} / {ctx}  ({:.0}%)", 100.0 * used as f64 / ctx as f64));
            }
        }
        if let Some(m) = &s.model {
            let mut line = m.clone();
            if let Some(e) = &s.engine {
                line.push_str(&format!("  (engine {e}"));
                if let Some(c) = s.ctx {
                    line.push_str(&format!(", 上下文 {c}"));
                }
                line.push_str(if s.images { ", 图像 on)" } else { ", 图像 off)" });
            }
            out.push(line);
        }
        if !s.loaded && s.note.is_none() && s.model.is_some() {
            out.push("模型未加载 (按需加载)".into());
        }
        if let Some(u) = s.uptime_s {
            let mut line = format!("运行 {}", hms(u));
            if let Some(r) = s.requests {
                line.push_str(&format!("  请求 {r}"));
            }
            if let Some(f) = s.in_flight {
                if f > 0 {
                    line.push_str(&format!("  在飞 {f}"));
                }
            }
            out.push(line);
        }
        if let Some(t) = s.totals {
            out.push(format!("tokens 总计  prompt {} · 输出 {}", count(t.prompt), count(t.output)));
            if t.prompt > 0 && t.reused > 0 {
                out.push(format!("  其中复用 {}（{:.0}%）", count(t.reused), 100.0 * t.reused as f64 / t.prompt as f64));
            }
        }
        if let Some((u, t, w)) = s.gpu {
            let mut line = format!("GPU {u}%");
            if t.is_finite() {
                line.push_str(&format!("  {t:.0}°C"));
            }
            if w.is_finite() {
                line.push_str(&format!("  {w:.0} W"));
            }
            out.push(line);
        }
        if let Some((used, total)) = s.ram {
            out.push(format!("内存 {used:.1} / {total:.1} GiB"));
        }
        if let Some((n, secs, ps)) = s.last {
            out.push(format!("上一请求 {n:.0} tok  {secs:.1} s  {ps:.1} tok/s"));
        }
        if let Some(n) = &s.note {
            out.push(n.clone());
        }
        if !s.has_key {
            out.push(format!("没有 API key (~/{KEY_FILE}), 只能看到 /health"));
        }
        out
    }
}

fn hms(secs: i64) -> String {
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

/// 大数字: 846.1K / 197.4M / 1.23B
fn count(n: u64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.2}B", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1}M", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.1}K", f / 1e3)
    } else {
        n.to_string()
    }
}

/// 一个实心圆点, 外圈比填充色暗, 浅色/深色面板上都看得见.
/// 有请求排队时, 右上角再叠一个橘色小点 (--st-warn).
/// SNI 的 IconPixmap 是 A,R,G,B (network byte order) — 和扩展的 argbToRgba 一致.
fn dot_icon(rgb: (u8, u8, u8), size: i32, queued: bool) -> Icon {
    const BADGE: (u8, u8, u8) = (0xf5, 0xb5, 0x3d);

    let s = size as f32;
    let (fr, fg, fb) = (rgb.0 as f32, rgb.1 as f32, rgb.2 as f32);
    let rim = [fr * 0.35, fg * 0.35, fb * 0.35];
    let fill = [fr, fg, fb];
    // 角标不加暗边: 16px 下它只有 6px 宽, 暗边会把它压成褐色
    let bfill = [BADGE.0 as f32, BADGE.1 as f32, BADGE.2 as f32];

    let mid = s / 2.0;
    let inner = s * 0.33;
    let outer = s * 0.42;
    let (bx, by) = (s * 0.76, s * 0.24);
    let (b_inner, b_outer) = if queued { (s * 0.18, s * 0.18) } else { (0.0, 0.0) };

    let cover = |radius: f32, d: f32| (radius - d + 0.5).clamp(0.0, 1.0);
    let mix = |under: [f32; 3], over: [f32; 3], a: f32| {
        [under[0] + (over[0] - under[0]) * a, under[1] + (over[1] - under[1]) * a, under[2] + (over[2] - under[2]) * a]
    };

    let mut data = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let d_mid = ((px - mid).powi(2) + (py - mid).powi(2)).sqrt();
            let d_badge = ((px - bx).powi(2) + (py - by).powi(2)).sqrt();
            let base_a = cover(outer, d_mid);
            let badge_a = cover(b_outer, d_badge);
            if base_a <= 0.0 && badge_a <= 0.0 {
                continue;
            }
            let base = mix(rim, fill, cover(inner, d_mid));
            let badge = mix(bfill, bfill, cover(b_inner, d_badge));
            // 角标叠在圆点上 (source-over)
            let a = badge_a + base_a * (1.0 - badge_a);
            let i = ((y * size + x) * 4) as usize;
            data[i] = (a * 255.0).round() as u8;
            for c in 0..3 {
                let v = (badge[c] * badge_a + base[c] * base_a * (1.0 - badge_a)) / a;
                data[i + 1 + c] = v.clamp(0.0, 255.0).round() as u8;
            }
        }
    }
    Icon { width: size, height: size, data }
}

fn open_web() {
    let (host, port) = endpoint();
    let _ = Command::new("xdg-open").arg(format!("http://{host}:{port}/#monitor")).spawn();
}

fn systemctl(verb: &str) {
    let _ = Command::new("systemctl").args(["--user", verb, UNIT]).spawn();
}

impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "strata".into()
    }

    fn title(&self) -> String {
        format!("Strata — {}", self.snap.state.name())
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let rgb = self.snap.state.rgb();
        let queued = self.snap.queued > 0;
        vec![dot_icon(rgb, 16, queued), dot_icon(rgb, 32, queued)]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.title(),
            description: self.details().join("\n"),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = Vec::new();
        for line in self.details() {
            items.push(Self::info(line));
        }
        items.push(MenuItem::Separator);
        items.push(Self::action("打开网页界面", open_web));
        if self.snap.state == State::Error {
            items.push(Self::action("启动服务", || systemctl("start")));
        } else {
            items.push(Self::action("重启服务", || systemctl("restart")));
            items.push(Self::action("停止服务", || systemctl("stop")));
        }
        items.push(MenuItem::Separator);
        items.push(MenuItem::Standard(StandardItem {
            label: "退出托盘".into(),
            activate: Box::new(|t| t.quit = true),
            ..Default::default()
        }));
        items
    }
}

fn main() {
    let key = api_key();
    let mut snap = Snapshot { has_key: key.is_some(), ..Default::default() };
    let mut last_slow = Instant::now() - SLOW_EVERY;
    let mut last_totals = Instant::now() - TOTALS_EVERY;

    let tray = Tray { snap: snap.clone(), quit: false };
    let handle = match tray.spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[strata-tray] 注册托盘失败: {e}");
            std::process::exit(1);
        }
    };

    loop {
        thread::sleep(LIVE_EVERY);

        let live = match key.as_deref() {
            Some(k) => poll_live(&mut snap, k),
            None => poll_health(&mut snap),
        };
        match live {
            Ok(()) => {
                snap.derive_state();
                snap.note = None;
            }
            Err(e) => snap.fail(e),
        }

        if last_slow.elapsed() >= SLOW_EVERY && snap.state != State::Error {
            if let Some(k) = key.as_deref() {
                if let Err(e) = poll_slow(&mut snap, k) {
                    snap.note = Some(e);
                }
            }
            last_slow = Instant::now();
        }

        if last_totals.elapsed() >= TOTALS_EVERY && snap.state != State::Error {
            if let Some(k) = key.as_deref() {
                if let Err(e) = poll_totals(&mut snap, k) {
                    snap.note = Some(e);
                }
            }
            last_totals = Instant::now();
        }

        let mut quit = false;
        if handle.update(|t| {
            t.snap = snap.clone();
            quit = t.quit;
        }).is_none() {
            break; // 托盘已经被宿主关掉了
        }
        if quit {
            handle.shutdown();
            break;
        }
    }
}
