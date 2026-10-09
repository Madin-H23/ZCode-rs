//! 终端字体与配色（packages/services/src/terminal/terminalProfile.ts、terminalProfileMacOs.ts）。
//!
//! 用户自定义字体优先；`terminalInheritSystemProfile` 不为 false 时依次读 Windows Terminal、
//! VS Code、iTerm2、Terminal.app、kitty、alacritty 的配置，取第一个有结果的。全部尽力而为：
//! 文件缺失、格式错误、plutil 失败都静默跳过。
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{Map, Value};

use crate::support::env::ProcessEnv;

const FONT_FAMILY_FALLBACKS: [&str; 12] = [
    "ui-monospace",
    "SFMono-Regular",
    "SF Mono",
    "Menlo",
    "Monaco",
    "Consolas",
    "Cascadia Mono",
    "JetBrains Mono",
    "MesloLGS NF",
    "Hack Nerd Font",
    "Noto Sans Mono CJK SC",
    "monospace",
];
const PLIST_READ_TIMEOUT: Duration = Duration::from_secs(2);
const PLIST_MAX_BYTES: usize = 2 * 1024 * 1024;
const ANSI_THEME_KEYS: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "brightBlack",
    "brightRed",
    "brightGreen",
    "brightYellow",
    "brightBlue",
    "brightMagenta",
    "brightCyan",
    "brightWhite",
];
const TERMINAL_APP_ANSI_KEYS: [&str; 16] = [
    "ANSIBlackColor",
    "ANSIRedColor",
    "ANSIGreenColor",
    "ANSIYellowColor",
    "ANSIBlueColor",
    "ANSIMagentaColor",
    "ANSICyanColor",
    "ANSIWhiteColor",
    "ANSIBrightBlackColor",
    "ANSIBrightRedColor",
    "ANSIBrightGreenColor",
    "ANSIBrightYellowColor",
    "ANSIBrightBlueColor",
    "ANSIBrightMagentaColor",
    "ANSIBrightCyanColor",
    "ANSIBrightWhiteColor",
];

#[derive(Debug, Clone, Default, PartialEq)]
struct Detected {
    font_family: Option<String>,
    font_size: Option<f64>,
    theme: Option<Map<String, Value>>,
}

impl Detected {
    fn is_empty(&self) -> bool {
        self.font_family.is_none() && self.font_size.is_none() && self.theme.is_none()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FontProfile {
    pub font_family: String,
    pub font_size: Option<f64>,
    pub theme: Option<Map<String, Value>>,
    pub source: &'static str,
}

fn normalize(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// 自定义或探测到的字体栈后面补齐缺失的通用回退字体。
fn dedupe_stack(primary: &str) -> String {
    let mut stack: Vec<String> = primary
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    for fallback in FONT_FAMILY_FALLBACKS {
        if !stack.iter().any(|part| part == fallback) {
            stack.push(fallback.to_owned());
        }
    }
    stack.join(", ")
}

/// JS 数字转成 JSON：整数值不带小数点。
pub fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

fn home_dir(env: &ProcessEnv) -> PathBuf {
    PathBuf::from(
        env.non_empty("HOME")
            .or_else(|| env.non_empty("USERPROFILE"))
            .unwrap_or_default(),
    )
}

fn xdg_config_home(env: &ProcessEnv) -> PathBuf {
    env.non_empty("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir(env).join(".config"))
}

async fn read_text(path: &Path) -> Option<String> {
    let bytes = tokio::fs::read(path).await.ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// 去掉 `//`、`/* */` 注释（字符串内的保留）。
fn strip_json_comments(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut result = String::with_capacity(raw.len());
    let (mut in_string, mut escaped, mut index) = (false, false, 0);
    while index < chars.len() {
        let current = chars[index];
        let next = chars.get(index + 1).copied();
        if in_string {
            result.push(current);
            if escaped {
                escaped = false;
            } else if current == '\\' {
                escaped = true;
            } else if current == '"' {
                in_string = false;
            }
        } else if current == '"' {
            in_string = true;
            result.push(current);
        } else if current == '/' && next == Some('/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            result.push('\n');
        } else if current == '/' && next == Some('*') {
            index += 2;
            while index < chars.len()
                && !(chars[index] == '*' && chars.get(index + 1) == Some(&'/'))
            {
                index += 1;
            }
            index += 1;
        } else {
            result.push(current);
        }
        index += 1;
    }
    result
}

fn remove_trailing_commas(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut result = String::with_capacity(raw.len());
    let (mut in_string, mut escaped) = (false, false);
    for (index, &current) in chars.iter().enumerate() {
        if in_string {
            result.push(current);
            if escaped {
                escaped = false;
            } else if current == '\\' {
                escaped = true;
            } else if current == '"' {
                in_string = false;
            }
            continue;
        }
        if current == '"' {
            in_string = true;
        }
        if current == ',' {
            let next = chars[index + 1..].iter().find(|c| !c.is_whitespace());
            if matches!(next, Some('}' | ']')) {
                continue;
            }
        }
        result.push(current);
    }
    result
}

fn parse_jsonc(raw: &str) -> Option<Map<String, Value>> {
    let cleaned = remove_trailing_commas(&strip_json_comments(raw));
    [raw, cleaned.as_str()].into_iter().find_map(|candidate| {
        match serde_json::from_str::<Value>(candidate) {
            Ok(Value::Object(object)) => Some(object),
            _ => None,
        }
    })
}

async fn read_jsonc(path: &Path) -> Option<Map<String, Value>> {
    parse_jsonc(&read_text(path).await?)
}

/// 逐级取对象字段，末端必须是去空白后非空的字符串。
fn nested_string(value: &Value, segments: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in segments {
        current = current.as_object()?.get(*segment)?;
    }
    normalize(current.as_str())
}

async fn detect_windows_terminal(env: &ProcessEnv) -> Option<Detected> {
    let local = env
        .non_empty("LOCALAPPDATA")
        .or_else(|| env.non_empty("APPDATA"))?;
    let local = Path::new(local);
    let candidates = [
        local
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json"),
        local
            .join("Packages")
            .join("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json"),
        local
            .join("Microsoft")
            .join("Windows Terminal")
            .join("settings.json"),
    ];
    let font_of = |profile: &Value| nested_string(profile, &["font", "face"]);
    for candidate in candidates {
        let Some(parsed) = read_jsonc(&candidate).await.map(Value::Object) else {
            continue;
        };
        let list = parsed["profiles"]["list"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(default_id) = nested_string(&parsed, &["defaultProfile"]) {
            let found = list
                .iter()
                .filter(|profile| profile["guid"].as_str() == Some(default_id.as_str()))
                .find_map(font_of);
            if found.is_some() {
                return Some(Detected {
                    font_family: found,
                    ..Detected::default()
                });
            }
        }
        let defaults =
            font_of(&parsed["profiles"]["defaults"]).or_else(|| list.iter().find_map(font_of));
        if defaults.is_some() {
            return Some(Detected {
                font_family: defaults,
                ..Detected::default()
            });
        }
    }
    None
}

async fn detect_vscode(env: &ProcessEnv) -> Option<Detected> {
    let home = home_dir(env);
    let mut candidates = Vec::new();
    for base in [env.non_empty("APPDATA"), env.non_empty("XDG_CONFIG_HOME")]
        .into_iter()
        .flatten()
    {
        for product in ["Code", "Code - Insiders"] {
            candidates.push(
                Path::new(base)
                    .join(product)
                    .join("User")
                    .join("settings.json"),
            );
        }
    }
    for base in [
        home.join(".config"),
        home.join("Library").join("Application Support"),
    ] {
        for product in ["Code", "Code - Insiders"] {
            candidates.push(base.join(product).join("User").join("settings.json"));
        }
    }
    for candidate in candidates {
        if let Some(parsed) = read_jsonc(&candidate).await
            && let Some(font) =
                nested_string(&Value::Object(parsed), &["terminal.integrated.fontFamily"])
        {
            return Some(Detected {
                font_family: Some(font),
                ..Detected::default()
            });
        }
    }
    None
}

static KITTY_FONT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?mR)^\s*font_family\s+(.+)$").expect("valid regex"));

async fn detect_kitty(env: &ProcessEnv) -> Option<Detected> {
    let home = home_dir(env);
    let candidates = [
        xdg_config_home(env).join("kitty").join("kitty.conf"),
        home.join("Library")
            .join("Application Support")
            .join("kitty")
            .join("kitty.conf"),
    ];
    for candidate in candidates {
        let Some(raw) = read_text(&candidate).await else {
            continue;
        };
        let captured = KITTY_FONT
            .captures(&raw)
            .and_then(|captures| captures.get(1));
        if let Some(font) = normalize(captured.map(|matched| matched.as_str())) {
            let font = font.strip_prefix('"').unwrap_or(&font);
            let font = font.strip_suffix('"').unwrap_or(font);
            return Some(Detected {
                font_family: Some(font.to_owned()),
                ..Detected::default()
            });
        }
    }
    None
}

async fn detect_alacritty(env: &ProcessEnv) -> Option<Detected> {
    let (home, xdg) = (home_dir(env), xdg_config_home(env));
    for candidate in [
        xdg.join("alacritty").join("alacritty.toml"),
        home.join(".alacritty.toml"),
    ] {
        let Some(raw) = read_text(&candidate).await else {
            continue;
        };
        let family = raw
            .parse::<toml::Table>()
            .ok()
            .and_then(|table| normalize(table.get("font")?.get("normal")?.get("family")?.as_str()));
        if family.is_some() {
            return Some(Detected {
                font_family: family,
                ..Detected::default()
            });
        }
    }
    for name in ["alacritty.yml", "alacritty.yaml"] {
        let Some(raw) = read_text(&xdg.join("alacritty").join(name)).await else {
            continue;
        };
        let family = yaml_rust2::YamlLoader::load_from_str(&raw)
            .ok()
            .filter(|documents| documents.len() == 1)
            .and_then(|documents| normalize(documents[0]["font"]["normal"]["family"].as_str()));
        if family.is_some() {
            return Some(Detected {
                font_family: family,
                ..Detected::default()
            });
        }
    }
    None
}

/// `plutil -convert json` 读 plist；二进制格式异常或含 data/date 字段时 plutil 会失败，返回 None。
async fn read_plist(path: &Path, env: &ProcessEnv) -> Option<Map<String, Value>> {
    if !cfg!(target_os = "macos") || tokio::fs::metadata(path).await.is_err() {
        return None;
    }
    let mut command = zcode_proc::command("plutil");
    command
        .args(["-convert", "json", "-o", "-"])
        .arg(path)
        .env_clear()
        .envs(env.vars())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(PLIST_READ_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() || output.stdout.len() > PLIST_MAX_BYTES {
        return None;
    }
    match serde_json::from_slice::<Value>(&output.stdout).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

static FLOAT_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?").expect("valid regex")
});

/// `Number.parseFloat` 的前缀语义。
fn parse_float(text: &str) -> Option<f64> {
    FLOAT_PREFIX
        .find(text.trim())
        .and_then(|matched| matched.as_str().parse().ok())
}

fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => parse_float(text),
        _ => None,
    }
}

fn normalize_font_size(value: &Value) -> Option<f64> {
    number_of(value).filter(|size| size.is_finite() && (6.0..=72.0).contains(size))
}

/// JS 的 `value?.toString()`。
fn js_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        Value::Array(items) => Some(
            items
                .iter()
                .map(|item| js_string(item).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(","),
        ),
        Value::Object(_) => Some("[object Object]".to_owned()),
    }
}

static TRAILING_SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+(\d+(?:\.\d+)?)$").expect("valid regex"));

/// `Menlo-Regular 12` → `Menlo Regular`。
fn normalize_mac_font_name(value: Option<&str>) -> Option<String> {
    let normalized = normalize(value)?;
    let without_size = TRAILING_SIZE.replace(&normalized, "");
    normalize(Some(&without_size.replace('-', " ")))
}

fn mac_font_descriptor(value: &Value) -> (Option<String>, Option<f64>) {
    let raw = js_string(value);
    let size = raw
        .as_deref()
        .and_then(|raw| TRAILING_SIZE.captures(raw))
        .and_then(|captures| normalize_font_size(&Value::String(captures[1].to_owned())));
    (normalize_mac_font_name(raw.as_deref()), size)
}

/// Node `Buffer.from(text, "base64")`：忽略非法字符，兼容 URL-safe 字母，遇 `=` 结束。
fn decode_base64_lenient(text: &str) -> Vec<u8> {
    let mut bits: u32 = 0;
    let mut count = 0;
    let mut out = Vec::new();
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    out
}

static ARCHIVED_FONT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)([A-Za-z][A-Za-z0-9 ._-]*(?:Mono|Code|Nerd|Powerline|Console|Menlo|Monaco|Courier|Cascadia|Consolas|Hack|Meslo)[A-Za-z0-9 ._-]*)",
    )
    .expect("valid regex")
});

/// Terminal.app 的 `Font` 是 NSKeyedArchiver 数据：按 latin1 解码后找像等宽字体名的片段。
fn mac_archived_font_name(value: &Value) -> Option<String> {
    let raw = match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => object.get("NS").and_then(js_string),
        _ => None,
    };
    let data = normalize(raw.as_deref())?;
    let decoded: String = decode_base64_lenient(&data)
        .into_iter()
        .map(char::from)
        .collect();
    let captures = ARCHIVED_FONT.captures(&decoded)?;
    normalize_mac_font_name(Some(&captures[1]))
}

fn color_component(value: &Value) -> Option<f64> {
    let parsed = number_of(value)?;
    if !parsed.is_finite() || parsed < 0.0 {
        return None;
    }
    if parsed <= 1.0 {
        Some(parsed)
    } else if parsed <= 255.0 {
        Some(parsed / 255.0)
    } else if parsed <= 65_535.0 {
        Some(parsed / 65_535.0)
    } else {
        None
    }
}

static HEX_COLOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^#[0-9a-f]{3}(?:[0-9a-f]{3})?(?:[0-9a-f]{2})?$").expect("valid regex")
});

fn mac_color(value: &Value) -> Option<String> {
    if let Value::String(text) = value {
        let trimmed = text.trim();
        let rgb = trimmed
            .get(..4)
            .is_some_and(|head| head.eq_ignore_ascii_case("rgb("))
            || trimmed
                .get(..5)
                .is_some_and(|head| head.eq_ignore_ascii_case("rgba("));
        return (HEX_COLOR.is_match(trimmed) || rgb).then(|| trimmed.to_owned());
    }
    let record = value.as_object()?;
    // 第一个存在的键（值为 null 也算存在）决定该分量。
    let pick = |names: &[&str]| names.iter().find_map(|name| record.get(*name));
    let red = color_component(pick(&["Red Component", "red", "Red"])?)?;
    let green = color_component(pick(&["Green Component", "green", "Green"])?)?;
    let blue = color_component(pick(&["Blue Component", "blue", "Blue"])?)?;
    let alpha = pick(&["Alpha Component", "alpha", "Alpha", "Opacity"])
        .and_then(color_component)
        .unwrap_or(1.0);
    let channel = |component: f64| (component * 255.0).round() as u8;
    let (r, g, b) = (channel(red), channel(green), channel(blue));
    if alpha < 1.0 {
        let alpha: f64 = format!("{alpha:.3}").parse().unwrap_or(alpha);
        return Some(format!("rgba({r}, {g}, {b}, {alpha})"));
    }
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

fn read_theme(
    profile: &Map<String, Value>,
    mapping: &[(&str, Vec<String>)],
) -> Option<Map<String, Value>> {
    let mut theme = Map::new();
    for (key, sources) in mapping {
        if let Some(color) = sources
            .iter()
            .find_map(|source| profile.get(source).and_then(mac_color))
        {
            theme.insert((*key).to_owned(), Value::String(color));
        }
    }
    (!theme.is_empty()).then_some(theme)
}

fn iterm2_theme(profile: &Map<String, Value>) -> Option<Map<String, Value>> {
    let mut mapping: Vec<(&str, Vec<String>)> = [
        ("foreground", "Foreground Color"),
        ("background", "Background Color"),
        ("cursor", "Cursor Color"),
        ("cursorAccent", "Cursor Text Color"),
        ("selectionBackground", "Selection Color"),
    ]
    .into_iter()
    .map(|(key, source)| (key, vec![source.to_owned()]))
    .collect();
    for (index, key) in ANSI_THEME_KEYS.iter().enumerate() {
        mapping.push((
            key,
            vec![format!("Ansi {index} Color"), format!("ANSI {index} Color")],
        ));
    }
    read_theme(profile, &mapping)
}

fn terminal_app_theme(profile: &Map<String, Value>) -> Option<Map<String, Value>> {
    let mut mapping: Vec<(&str, Vec<String>)> = [
        ("foreground", "TextColor"),
        ("background", "BackgroundColor"),
        ("cursor", "CursorColor"),
        ("selectionBackground", "SelectionColor"),
    ]
    .into_iter()
    .map(|(key, source)| (key, vec![source.to_owned()]))
    .collect();
    for (key, source) in ANSI_THEME_KEYS.iter().zip(TERMINAL_APP_ANSI_KEYS) {
        mapping.push((key, vec![source.to_owned()]));
    }
    read_theme(profile, &mapping)
}

fn iterm2_profile(plist: &Map<String, Value>) -> Option<Detected> {
    let profiles: Vec<&Value> = plist
        .get("New Bookmarks")
        .and_then(Value::as_array)
        .map(|profiles| profiles.iter().collect())
        .unwrap_or_default();
    // 默认 profile（第一个 `Default Bookmark: true`）排最前，其余保持原顺序。
    let default = profiles
        .iter()
        .position(|profile| profile.get("Default Bookmark") == Some(&Value::Bool(true)));
    let ordered = default.map(|index| profiles[index]).into_iter().chain(
        profiles
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != default)
            .map(|(_, profile)| *profile),
    );
    for profile in ordered.filter_map(Value::as_object) {
        let (font_family, font_size) = profile
            .get("Normal Font")
            .map(mac_font_descriptor)
            .unwrap_or_default();
        let detected = Detected {
            font_family,
            font_size,
            theme: iterm2_theme(profile),
        };
        if !detected.is_empty() {
            return Some(detected);
        }
    }
    None
}

fn terminal_app_profile(plist: &Map<String, Value>) -> Option<Detected> {
    let root = Value::Object(plist.clone());
    let names = [
        nested_string(&root, &["Startup Window Settings"]),
        nested_string(&root, &["Default Window Settings"]),
    ];
    for name in names.into_iter().flatten() {
        let Some(settings) = plist.get(&name).and_then(Value::as_object) else {
            continue;
        };
        let wrapped = Value::Object(settings.clone());
        let font = settings.get("Font").unwrap_or(&Value::Null);
        let font_family = nested_string(&wrapped, &["FontName"])
            .or_else(|| normalize_mac_font_name(nested_string(&wrapped, &["Font"]).as_deref()))
            .or_else(|| mac_archived_font_name(font));
        let font_size = settings
            .get("FontSize")
            .and_then(normalize_font_size)
            .or_else(|| mac_font_descriptor(font).1);
        let detected = Detected {
            font_family,
            font_size,
            theme: terminal_app_theme(settings),
        };
        if !detected.is_empty() {
            return Some(detected);
        }
    }
    None
}

async fn detect_system_profile(env: &ProcessEnv) -> Option<Detected> {
    let home = home_dir(env);
    if cfg!(windows)
        && let Some(found) = detect_windows_terminal(env).await
    {
        return Some(found);
    }
    if let Some(found) = detect_vscode(env).await {
        return Some(found);
    }
    if cfg!(target_os = "macos") {
        let preferences = home.join("Library").join("Preferences");
        if let Some(found) = read_plist(&preferences.join("com.googlecode.iterm2.plist"), env)
            .await
            .and_then(|plist| iterm2_profile(&plist))
        {
            return Some(found);
        }
        if let Some(found) = read_plist(&preferences.join("com.apple.Terminal.plist"), env)
            .await
            .and_then(|plist| terminal_app_profile(&plist))
        {
            return Some(found);
        }
    }
    if cfg!(windows) {
        return None;
    }
    if let Some(found) = detect_kitty(env).await {
        return Some(found);
    }
    detect_alacritty(env).await
}

/// `resolveTerminalFontProfile`。
pub async fn resolve(settings: &Value, env: &ProcessEnv) -> FontProfile {
    let custom = normalize(settings.get("terminalFontFamily").and_then(Value::as_str));
    let inherit = settings.get("terminalInheritSystemProfile") != Some(&Value::Bool(false));
    let detected = if inherit {
        detect_system_profile(env).await
    } else {
        None
    };
    if let Some(custom) = custom {
        let detected = detected.unwrap_or_default();
        return FontProfile {
            font_family: dedupe_stack(&custom),
            font_size: detected.font_size,
            theme: detected.theme,
            source: "custom",
        };
    }
    match detected {
        // 只配了配色或字号的系统终端也要继承，字体族缺省用 ui-monospace 开头的栈。
        Some(detected) => FontProfile {
            font_family: dedupe_stack(
                detected
                    .font_family
                    .as_deref()
                    .unwrap_or(FONT_FAMILY_FALLBACKS[0]),
            ),
            font_size: detected.font_size,
            theme: detected.theme,
            source: "system",
        },
        None => FontProfile {
            font_family: FONT_FAMILY_FALLBACKS.join(", "),
            font_size: None,
            theme: None,
            source: "fallback",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn isolated_env(home: &Path) -> ProcessEnv {
        ProcessEnv::new(
            [("HOME", home.to_str().unwrap()), ("PATH", "/usr/bin:/bin")]
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    #[tokio::test]
    async fn custom_font_wins_and_fallback_is_used_without_detections() {
        let home = tempfile::tempdir().unwrap();
        let env = isolated_env(home.path());
        let custom = resolve(&json!({"terminalFontFamily": " Fira Code, Menlo "}), &env).await;
        assert_eq!(custom.source, "custom");
        assert!(
            custom
                .font_family
                .starts_with("Fira Code, Menlo, ui-monospace, SFMono-Regular")
        );
        assert_eq!(custom.font_family.matches("Menlo").count(), 1);
        let fallback = resolve(&json!({"terminalInheritSystemProfile": true}), &env).await;
        assert_eq!(fallback.source, "fallback");
        assert_eq!(fallback.font_family, FONT_FAMILY_FALLBACKS.join(", "));
    }

    #[tokio::test]
    async fn detects_vscode_jsonc_settings_unless_disabled() {
        let home = tempfile::tempdir().unwrap();
        let settings = home.path().join(".config").join("Code").join("User");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(
            settings.join("settings.json"),
            "{\n  // comment\n  \"terminal.integrated.fontFamily\": \"JetBrains Mono\", /* x */\n}\n",
        )
        .unwrap();
        let env = isolated_env(home.path());
        let detected = resolve(&json!({}), &env).await;
        assert_eq!(detected.source, "system");
        assert!(
            detected
                .font_family
                .starts_with("JetBrains Mono, ui-monospace")
        );
        let disabled = resolve(&json!({"terminalInheritSystemProfile": false}), &env).await;
        assert_eq!(disabled.source, "fallback");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn detects_kitty_and_alacritty() {
        let home = tempfile::tempdir().unwrap();
        let env = isolated_env(home.path());
        let alacritty = home.path().join(".config").join("alacritty");
        std::fs::create_dir_all(&alacritty).unwrap();
        std::fs::write(
            alacritty.join("alacritty.yml"),
            "font:\n  normal:\n    family: Hack\n",
        )
        .unwrap();
        assert!(
            resolve(&json!({}), &env)
                .await
                .font_family
                .starts_with("Hack, ")
        );
        let kitty = home.path().join(".config").join("kitty");
        std::fs::create_dir_all(&kitty).unwrap();
        std::fs::write(
            kitty.join("kitty.conf"),
            "# x\r\n  font_family   \"Iosevka Term\"\r\n",
        )
        .unwrap();
        assert!(
            resolve(&json!({}), &env)
                .await
                .font_family
                .starts_with("Iosevka Term, ")
        );
    }

    #[test]
    fn parses_mac_profiles() {
        let plist = json!({
            "New Bookmarks": [
                {"Name": "Other", "Normal Font": "Monaco 10"},
                {
                    "Default Bookmark": true,
                    "Normal Font": "MesloLGS-NF-Regular 13",
                    "Foreground Color": {"Red Component": 1, "Green Component": 0.5, "Blue Component": 0},
                    "Background Color": {"Red Component": 0, "Green Component": 0, "Blue Component": 0, "Alpha Component": 0.5},
                    "Ansi 1 Color": "#ff0000",
                }
            ]
        });
        let detected = iterm2_profile(plist.as_object().unwrap()).unwrap();
        assert_eq!(detected.font_family.as_deref(), Some("MesloLGS NF Regular"));
        assert_eq!(detected.font_size, Some(13.0));
        assert_eq!(
            Value::Object(detected.theme.unwrap()),
            json!({"foreground": "#ff8000", "background": "rgba(0, 0, 0, 0.5)", "red": "#ff0000"})
        );

        let terminal = json!({
            "Default Window Settings": "Basic",
            "Basic": {"Font": "SF-Mono-Regular 11", "TextColor": "rgb(1,2,3)"}
        });
        let detected = terminal_app_profile(terminal.as_object().unwrap()).unwrap();
        assert_eq!(detected.font_family.as_deref(), Some("SF Mono Regular"));
        assert_eq!(detected.font_size, Some(11.0));
        // 关键字前至少要有一个字母（与 TS 正则相同），所以 "Menlo-Regular" 本身匹配不到。
        let archived = json!({"NS": "AAhTRk1vbm8tUmVndWxhcgA="});
        assert_eq!(
            mac_archived_font_name(&archived).as_deref(),
            Some("SFMono Regular")
        );
    }

    #[test]
    fn helpers_follow_js_semantics() {
        assert_eq!(parse_float(" 12.5pt"), Some(12.5));
        assert_eq!(parse_float("abc"), None);
        assert_eq!(js_number(12.0), json!(12));
        assert_eq!(js_number(12.5), json!(12.5));
        assert_eq!(decode_base64_lenient("aGk=\n"), b"hi");
        assert_eq!(
            parse_jsonc("{\"a\": [1, 2,],}"),
            json!({"a": [1, 2]}).as_object().cloned()
        );
    }
}
