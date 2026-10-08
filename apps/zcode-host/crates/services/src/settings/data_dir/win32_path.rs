//! Node `path.win32` 的 `normalize` / `resolve` / `join`（Node 24 `lib/path.js`）逐行移植。
//!
//! 安装目录守卫（`validateDataBaseDirTarget`）在 Windows 上按这套语义比较路径；这里是纯词法计算，
//! 与平台无关，按 UTF-16 码元处理（与 JS 字符串下标一致），任何系统上都能与真实 TS 对拍。
//! `resolve` 需要的进程 cwd 与盘符当前目录（`process.env["=X:"]`）由调用方传入。

const FORWARD_SLASH: u16 = b'/' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const DOT: u16 = b'.' as u16;
const COLON: u16 = b':' as u16;

const WINDOWS_RESERVED_NAMES: [&str; 28] = [
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT\u{b9}",
    "LPT\u{b2}",
    "LPT\u{b3}",
];

type Units = Vec<u16>;

fn units(text: &str) -> Units {
    text.encode_utf16().collect()
}

fn text(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

fn concat(parts: &[&[u16]]) -> Units {
    parts.iter().flat_map(|part| part.iter().copied()).collect()
}

fn is_path_separator(code: u16) -> bool {
    code == FORWARD_SLASH || code == BACKSLASH
}

fn is_windows_device_root(code: u16) -> bool {
    (u16::from(b'A')..=u16::from(b'Z')).contains(&code)
        || (u16::from(b'a')..=u16::from(b'z')).contains(&code)
}

/// `String.prototype.indexOf(':', from)`；找不到为 -1。
fn index_of_colon(path: &[u16], from: usize) -> isize {
    path.iter()
        .skip(from)
        .position(|&code| code == COLON)
        .map_or(-1, |index| (index + from) as isize)
}

/// `String.prototype.slice(start, end)`：负数从末尾算，越界截断，end ≤ start 时为空。
fn js_slice(path: &[u16], start: isize, end: isize) -> &[u16] {
    let len = path.len() as isize;
    let clamp = |index: isize| {
        if index < 0 {
            (len + index).max(0)
        } else {
            index.min(len)
        }
    };
    let (start, end) = (clamp(start), clamp(end));
    if end <= start {
        &[]
    } else {
        &path[start as usize..end as usize]
    }
}

/// `isWindowsReservedName(path, colonIndex)`：`colonIndex` 为 -1 时 JS 的 `slice(0, -1)` 取到倒数第二个字符。
fn is_windows_reserved_name(path: &[u16], colon_index: isize) -> bool {
    let device = text(js_slice(path, 0, colon_index)).to_uppercase();
    WINDOWS_RESERVED_NAMES.contains(&device.as_str())
}

/// `normalizeString`：消去 `.` 与 `..`。
fn normalize_string(path: &[u16], allow_above_root: bool, separator: u16) -> Units {
    let mut res: Units = Vec::new();
    let mut last_segment_length: isize = 0;
    let mut last_slash: isize = -1;
    let mut dots: isize = 0;
    let mut code: u16 = 0;
    for i in 0..=path.len() {
        if i < path.len() {
            code = path[i];
        } else if is_path_separator(code) {
            break;
        } else {
            code = FORWARD_SLASH;
        }
        let i = i as isize;
        if is_path_separator(code) {
            if last_slash == i - 1 || dots == 1 {
                // NOOP
            } else if dots == 2 {
                if res.len() < 2
                    || last_segment_length != 2
                    || res[res.len() - 1] != DOT
                    || res[res.len() - 2] != DOT
                {
                    if res.len() > 2 {
                        let last_slash_index = res.len() as isize - last_segment_length - 1;
                        if last_slash_index == -1 {
                            res.clear();
                            last_segment_length = 0;
                        } else {
                            res.truncate(last_slash_index as usize);
                            let last_separator = res
                                .iter()
                                .rposition(|&unit| unit == separator)
                                .map_or(-1, |index| index as isize);
                            last_segment_length = res.len() as isize - 1 - last_separator;
                        }
                        last_slash = i;
                        dots = 0;
                        continue;
                    } else if !res.is_empty() {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i;
                        dots = 0;
                        continue;
                    }
                }
                if allow_above_root {
                    if !res.is_empty() {
                        res.push(separator);
                    }
                    res.extend([DOT, DOT]);
                    last_segment_length = 2;
                }
            } else {
                if !res.is_empty() {
                    res.push(separator);
                }
                res.extend_from_slice(&path[(last_slash + 1) as usize..i as usize]);
                last_segment_length = i - last_slash - 1;
            }
            last_slash = i;
            dots = 0;
        } else if code == DOT && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
    }
    res
}

/// UNC 根（`\\server\share`）的匹配结果。
enum UncRoot {
    /// `\\.\x`、`\\?\x` 这类设备根。
    Device { first_part_end: usize },
    /// `\\server\share`，`share_end` 是 share 段的结尾。
    Share {
        first_part: (usize, usize),
        share: (usize, usize),
    },
}

/// 以两个分隔符开头时匹配 `\\first\second`；`normalize` 与 `resolve` 共用这段逐字符扫描。
fn match_unc_root(path: &[u16]) -> Option<UncRoot> {
    let len = path.len();
    let mut j = 2;
    let mut last = j;
    while j < len && !is_path_separator(path[j]) {
        j += 1;
    }
    if !(j < len && j != last) {
        return None;
    }
    let first_part = (last, j);
    last = j;
    while j < len && is_path_separator(path[j]) {
        j += 1;
    }
    if !(j < len && j != last) {
        return None;
    }
    last = j;
    while j < len && !is_path_separator(path[j]) {
        j += 1;
    }
    if !(j == len || j != last) {
        return None;
    }
    let first = &path[first_part.0..first_part.1];
    if first == [DOT] || first == [u16::from(b'?')] {
        Some(UncRoot::Device {
            first_part_end: first_part.1,
        })
    } else {
        Some(UncRoot::Share {
            first_part,
            share: (last, j),
        })
    }
}

/// `path.win32.normalize(path)`。
pub fn normalize(path: &str) -> String {
    text(&normalize_units(&units(path)))
}

fn normalize_units(path: &[u16]) -> Units {
    let len = path.len();
    if len == 0 {
        return units(".");
    }
    let mut root_end = 0usize;
    let mut device: Option<Units> = None;
    let mut is_absolute = false;
    let code = path[0];

    if len == 1 {
        return if code == FORWARD_SLASH {
            units("\\")
        } else {
            path.to_vec()
        };
    }
    if is_path_separator(code) {
        is_absolute = true;
        if is_path_separator(path[1]) {
            match match_unc_root(path) {
                Some(UncRoot::Device { first_part_end }) => {
                    device = Some(concat(&[&units("\\\\"), &path[2..first_part_end]]));
                    root_end = 4;
                    let colon_index = index_of_colon(path, 0);
                    // `\\?\COM1:` 这类保留设备路径。
                    let possible_device = js_slice(path, 4, colon_index + 1);
                    if is_windows_reserved_name(possible_device, possible_device.len() as isize - 1)
                    {
                        device = Some(concat(&[&units("\\\\?\\"), possible_device]));
                        root_end = 4 + possible_device.len();
                    }
                }
                Some(UncRoot::Share { first_part, share }) => {
                    if share.1 == len {
                        // 只有 UNC 根，没有剩余部分。
                        return concat(&[
                            &units("\\\\"),
                            &path[first_part.0..first_part.1],
                            &units("\\"),
                            &path[share.0..],
                            &units("\\"),
                        ]);
                    }
                    device = Some(concat(&[
                        &units("\\\\"),
                        &path[first_part.0..first_part.1],
                        &units("\\"),
                        &path[share.0..share.1],
                    ]));
                    root_end = share.1;
                }
                None => {}
            }
        } else {
            root_end = 1;
        }
    } else {
        let colon_index = index_of_colon(path, 0);
        if colon_index > 0 {
            if is_windows_device_root(code) && colon_index == 1 {
                device = Some(path[0..2].to_vec());
                root_end = 2;
                if len > 2 && is_path_separator(path[2]) {
                    is_absolute = true;
                    root_end = 3;
                }
            } else if is_windows_reserved_name(path, colon_index) {
                device = Some(path[..=colon_index as usize].to_vec());
                root_end = colon_index as usize + 1;
            }
        }
    }

    let mut tail = if root_end < len {
        normalize_string(&path[root_end..], !is_absolute, BACKSLASH)
    } else {
        Vec::new()
    };
    if tail.is_empty() && !is_absolute {
        tail = units(".");
    }
    if !tail.is_empty() && is_path_separator(path[len - 1]) {
        tail.push(BACKSLASH);
    }
    let dot_tail = |tail: &[u16]| concat(&[&units(".\\"), tail]);
    if !is_absolute && device.is_none() && path.contains(&COLON) {
        // CVE-2024-36139：相对路径规范化后不能变成 Windows 会当作绝对路径的形态。
        if tail.len() >= 2 && is_windows_device_root(tail[0]) && tail[1] == COLON {
            return dot_tail(&tail);
        }
        let mut index = index_of_colon(path, 0);
        while index != -1 {
            let at = index as usize;
            if at == len - 1 || is_path_separator(path[at + 1]) {
                return dot_tail(&tail);
            }
            index = index_of_colon(path, at + 1);
        }
    }
    let colon_index = index_of_colon(path, 0);
    if is_windows_reserved_name(path, colon_index) {
        return concat(&[&units(".\\"), device.as_deref().unwrap_or_default(), &tail]);
    }
    match device {
        None if is_absolute => concat(&[&units("\\"), &tail]),
        None => tail,
        Some(device) if is_absolute => concat(&[&device, &units("\\"), &tail]),
        Some(device) => concat(&[&device, &tail]),
    }
}

/// `path.win32.resolve(...paths)`。`cwd` 是 `process.cwd()`；`drive_cwd("C:")` 是
/// `process.env["=C:"]`（Windows 每个盘符的当前目录）。
pub fn resolve(paths: &[&str], cwd: &str, drive_cwd: impl Fn(&str) -> Option<String>) -> String {
    let mut resolved_device: Units = Vec::new();
    let mut resolved_tail: Units = Vec::new();
    let mut resolved_absolute = false;

    let mut i = paths.len() as isize - 1;
    while i >= -1 {
        let index = i;
        i -= 1;
        let path: Units = if index >= 0 {
            let path = units(paths[index as usize]);
            if path.is_empty() {
                continue;
            }
            path
        } else if resolved_device.is_empty() {
            let path = units(cwd);
            let only_current = paths.len() == 1 && (paths[0].is_empty() || paths[0] == ".");
            if paths.is_empty()
                || (only_current && path.first().is_some_and(|&code| is_path_separator(code)))
            {
                // Node 在非 Windows 平台上把 cwd 的 `/` 换成 `\`；Windows 的 cwd 本来就没有 `/`。
                return text(&path).replace('/', "\\");
            }
            path
        } else {
            let device = text(&resolved_device);
            let path = drive_cwd(&format!("={device}"))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| cwd.to_owned());
            let path_units = units(&path);
            // 找到的 cwd 不在这个盘上时，退回到该盘的根目录。
            if text(js_slice(&path_units, 0, 2)).to_lowercase() != device.to_lowercase()
                && path_units.get(2) == Some(&BACKSLASH)
            {
                units(&format!("{device}\\"))
            } else {
                path_units
            }
        };

        let len = path.len();
        let mut root_end = 0usize;
        let mut device: Units = Vec::new();
        let mut is_absolute = false;
        // 只有取不到 cwd 时才会是空串；JS 的 charCodeAt(0) 此时为 NaN，不匹配任何根。
        let code = path.first().copied().unwrap_or(0);

        if len == 1 {
            if is_path_separator(code) {
                root_end = 1;
                is_absolute = true;
            }
        } else if is_path_separator(code) {
            is_absolute = true;
            if is_path_separator(path[1]) {
                match match_unc_root(&path) {
                    Some(UncRoot::Share { first_part, share }) => {
                        device = concat(&[
                            &units("\\\\"),
                            &path[first_part.0..first_part.1],
                            &units("\\"),
                            &path[share.0..share.1],
                        ]);
                        root_end = share.1;
                    }
                    Some(UncRoot::Device { first_part_end }) => {
                        device = concat(&[&units("\\\\"), &path[2..first_part_end]]);
                        root_end = 4;
                    }
                    None => {}
                }
            } else {
                root_end = 1;
            }
        } else if is_windows_device_root(code) && path[1] == COLON {
            device = path[0..2].to_vec();
            root_end = 2;
            if len > 2 && is_path_separator(path[2]) {
                is_absolute = true;
                root_end = 3;
            }
        }

        if !device.is_empty() {
            if !resolved_device.is_empty() {
                if text(&device).to_lowercase() != text(&resolved_device).to_lowercase() {
                    // 指向另一个盘，与已解析的部分无关。
                    continue;
                }
            } else {
                resolved_device = device;
            }
        }

        if resolved_absolute {
            if !resolved_device.is_empty() {
                break;
            }
        } else {
            resolved_tail = concat(&[&path[root_end..], &[BACKSLASH], &resolved_tail]);
            resolved_absolute = is_absolute;
            if is_absolute && !resolved_device.is_empty() {
                break;
            }
        }
    }

    let tail = normalize_string(&resolved_tail, !resolved_absolute, BACKSLASH);
    if resolved_absolute {
        text(&concat(&[&resolved_device, &[BACKSLASH], &tail]))
    } else {
        let joined = text(&concat(&[&resolved_device, &tail]));
        if joined.is_empty() {
            ".".to_owned()
        } else {
            joined
        }
    }
}

/// `path.win32.join(...paths)`。
pub fn join(paths: &[&str]) -> String {
    let parts: Vec<Units> = paths
        .iter()
        .map(|part| units(part))
        .filter(|part| !part.is_empty())
        .collect();
    let Some(first_part) = parts.first() else {
        return ".".to_owned();
    };
    let mut joined: Units = parts.join(&BACKSLASH);

    // 拼出的路径不能以两个分隔符开头（normalize 会误当作 UNC），除非第一段本来就是 UNC 形态。
    let mut needs_replace = true;
    let mut slash_count = 0usize;
    if is_path_separator(first_part[0]) {
        slash_count += 1;
        if first_part.len() > 1 && is_path_separator(first_part[1]) {
            slash_count += 1;
            if first_part.len() > 2 {
                if is_path_separator(first_part[2]) {
                    slash_count += 1;
                } else {
                    needs_replace = false;
                }
            }
        }
    }
    if needs_replace {
        while slash_count < joined.len() && is_path_separator(joined[slash_count]) {
            slash_count += 1;
        }
        if slash_count >= 2 {
            joined = concat(&[&units("\\"), &joined[slash_count..]]);
        }
    }

    // 含保留设备名的段时跳过规范化，只把 `/` 换成 `\`。
    let mut segments: Vec<Units> = Vec::new();
    let mut part: Units = Vec::new();
    let mut index = 0;
    while index < joined.len() {
        if joined[index] == BACKSLASH {
            if !part.is_empty() {
                segments.push(std::mem::take(&mut part));
            }
            while index + 1 < joined.len() && joined[index + 1] == BACKSLASH {
                index += 1;
            }
        } else {
            part.push(joined[index]);
        }
        index += 1;
    }
    if !part.is_empty() {
        segments.push(part);
    }
    if segments.iter().any(|segment| {
        let colon_index = index_of_colon(segment, 0);
        colon_index != -1 && is_windows_reserved_name(segment, colon_index)
    }) {
        let replaced: Units = joined
            .iter()
            .map(|&code| {
                if code == FORWARD_SLASH {
                    BACKSLASH
                } else {
                    code
                }
            })
            .collect();
        return text(&replaced);
    }
    text(&normalize_units(&joined))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_slice_follows_string_prototype_slice() {
        let value = units("abcd");
        assert_eq!(text(js_slice(&value, 0, -1)), "abc");
        assert_eq!(text(js_slice(&value, 4, 0)), "");
        assert_eq!(text(js_slice(&value, -2, 10)), "cd");
    }

    #[test]
    fn reserved_name_without_colon_uses_all_but_last_unit() {
        // JS：`slice(0, -1)`，所以 "CONx" 被当成保留名 CON。
        assert!(is_windows_reserved_name(&units("CONx"), -1));
        assert!(!is_windows_reserved_name(&units("CON"), -1));
    }
}
