//! 곡 목록 CSV 내보내기·불러오기. 가수·그룹 이름이 한글/영문으로 달라 자동 중복 찾기가 놓친 곡을
//! AI 에게 가려 달라고 할 때 쓴다.
//!
//! 순서: 전체 곡 목록을 CSV 로 내보낸다 → AI 에게 요청문과 함께 주면 같은 곡끼리 `group` 칸에 같은 값을
//! 채운다 → 그 CSV 를 불러오면 `group` 값대로 중복 묶음을 만들어 중복 정리 화면에서 그대로 처리한다.
//!
//! 칸: id, group, keep, title, artist, album_artist, album, track, duration, quality, size_bytes, path
//! - path 는 정리할 곳(맥 폴더, 아이팟은 마운트 위치) 기준 상대 경로이며 불러올 때 곡을 찾는 열쇠다.
//! - group 이 비어 있는 곡은 중복이 아닌 것으로 본다. 한 곡만 있는 group 도 무시한다.
//! - keep 이 1(또는 y, yes, true, o, 남김)인 곡은 남길 곡으로 고른다. 비어 있으면 음질 규칙으로 추천한다.
//! - 엑셀에서 한글이 깨지지 않게 UTF-8 BOM 을 붙여 쓴다. 쉼표·따옴표·줄바꿈이 든 칸은 따옴표로 감싼다.
use crate::audio::{self, AudioInfo};
use crate::dedupe::{self, Group};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const COLUMNS: [&str; 12] = ["id", "group", "keep", "title", "artist", "album_artist", "album", "track", "duration", "quality", "size_bytes", "path"];

/// AI 에게 CSV 와 함께 줄 요청문
pub const AI_PROMPT: &str = "첨부한 CSV 는 내 음악 파일 목록입니다. 같은 곡(같은 녹음)이 여러 파일로 들어 있는 것을 찾아 주세요.

규칙:
1. 같은 곡인 행들의 group 칸에 같은 값(예: 1, 2, 3 …)을 넣어 주세요. 중복이 없는 곡은 group 칸을 비워 두세요.
2. 가수·그룹·앨범 이름이 한국어와 영어(또는 로마자, 일본어)로 다르게 적혀 있어도 같은 가수·같은 곡이면 같은 곡으로 봅니다. 예: 드림캐쳐 = Dreamcatcher, 아이즈원 = IZ*ONE, 방용국 = BANG YONGGUK.
3. 제목 뒤의 (feat. …), - Single, - EP, (Deluxe) 같은 꼬리 표기 차이는 무시해도 됩니다.
4. 라이브, 리믹스, 인스트루멘털, 다른 언어 버전(예: Japanese Ver.)처럼 다른 녹음은 같은 곡으로 묶지 마세요. duration(초) 차이가 크면 다른 버전일 가능성이 큽니다.
5. 남기고 싶은 파일이 분명하면 keep 칸에 1을 넣어도 됩니다(보통은 비워 두세요. 앱이 음질과 앨범 아트로 고릅니다).
6. 다른 칸(특히 path)은 절대 바꾸지 말고, 행을 빼거나 순서를 바꾸지 말고, 같은 칸 구성의 CSV 전체를 그대로 돌려주세요.";

/// 한 칸을 CSV 로 (필요하면 따옴표로 감싼다)
fn field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// CSV 글을 행·칸으로 나눈다 (따옴표 안의 쉼표·줄바꿈·"" 처리, BOM·빈 줄 무시)
pub fn parse(text: &str) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    cell.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                _ => cell.push(c),
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut cell)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut cell));
                if row.iter().any(|c| !c.trim().is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            _ => cell.push(c),
        }
    }
    row.push(cell);
    if row.iter().any(|c| !c.trim().is_empty()) {
        rows.push(row);
    }
    rows
}

/// 곡 목록을 CSV 글로 (경로는 root 기준)
pub fn to_csv(root: &Path, infos: &[AudioInfo]) -> String {
    let mut out = String::from("\u{feff}");
    out.push_str(&COLUMNS.join(","));
    out.push('\n');
    for (i, it) in infos.iter().enumerate() {
        let rel = it.path.strip_prefix(root).unwrap_or(&it.path).to_string_lossy().into_owned();
        let cells = [
            (i + 1).to_string(),
            String::new(),
            String::new(),
            it.title.clone(),
            it.artist.clone(),
            it.album_artist.clone(),
            it.album.clone(),
            it.track.map(|n| n.to_string()).unwrap_or_default(),
            (it.duration_ms / 1000).to_string(),
            it.quality_label(),
            it.size.to_string(),
            rel,
        ];
        out.push_str(&cells.iter().map(|c| field(c)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out
}

/// 폴더(또는 아이팟 음악 폴더) 안 곡을 모두 읽는다. report(읽은 수, 전체)
pub fn read_all(dir: &Path, stop: &AtomicBool, mut report: impl FnMut(usize, usize)) -> Result<Vec<AudioInfo>, String> {
    let files = dedupe::collect_files(dir, stop)?;
    let total = files.len();
    let mut infos = Vec::with_capacity(total);
    for (i, f) in files.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return Err(dedupe::CANCELLED.into());
        }
        infos.push(audio::read(f, false));
        report(i + 1, total);
    }
    infos.sort_by(|a, b| (a.artist.to_lowercase(), a.album.to_lowercase(), a.track, a.path.clone()).cmp(&(b.artist.to_lowercase(), b.album.to_lowercase(), b.track, b.path.clone())));
    Ok(infos)
}

fn is_yes(s: &str) -> bool {
    matches!(s.trim().to_lowercase().as_str(), "1" | "y" | "yes" | "true" | "o" | "keep" | "남김" | "남기기" | "✓" | "v")
}

/// CSV 에서 읽은 묶음 하나: (group 값, [(경로, 남김 표시)])
pub type CsvGroups = Vec<(String, Vec<(PathBuf, bool)>)>;

/// 불러온 결과 알림용
#[derive(Debug, Default, PartialEq)]
pub struct ImportReport {
    pub rows: usize,
    pub groups: usize,
    /// 파일을 찾지 못한 행 (경로)
    pub missing: Vec<String>,
}

/// CSV 글에서 group 묶음을 읽는다. 칸 이름으로 찾으므로 칸 순서가 바뀌어도 된다.
pub fn read_groups(text: &str, root: &Path) -> Result<(CsvGroups, ImportReport), String> {
    let rows = parse(text);
    let Some(head) = rows.first() else { return Err("CSV 가 비어 있습니다".into()) };
    let col = |name: &str| head.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let (Some(pi), Some(gi)) = (col("path"), col("group")) else {
        return Err("CSV 에 path 칸과 group 칸이 있어야 합니다. Podlift 에서 내보낸 CSV 에 group 만 채워 주세요.".into());
    };
    let ki = col("keep");
    let mut report = ImportReport { rows: rows.len() - 1, ..Default::default() };
    let mut map: BTreeMap<String, Vec<(PathBuf, bool)>> = BTreeMap::new();
    for r in &rows[1..] {
        let group = r.get(gi).map(|s| s.trim()).unwrap_or("");
        let rel = r.get(pi).map(|s| s.trim()).unwrap_or("");
        if group.is_empty() || rel.is_empty() {
            continue;
        }
        let path = root.join(rel);
        if !path.is_file() {
            report.missing.push(rel.to_string());
            continue;
        }
        let keep = ki.and_then(|k| r.get(k)).is_some_and(|s| is_yes(s));
        map.entry(group.to_string()).or_default().push((path, keep));
    }
    let groups: CsvGroups = map.into_iter().filter(|(_, v)| v.len() >= 2).collect();
    report.groups = groups.len();
    Ok((groups, report))
}

/// CSV 묶음을 중복 정리 묶음으로: 곡을 다시 읽고(앨범 아트 포함) 음질 규칙으로 추천을 정한다.
/// 돌려주는 값: (묶음, 그 묶음에서 keep 표시된 곡 위치들)
pub fn build_groups(groups: CsvGroups, stop: &AtomicBool, mut report: impl FnMut(usize, usize)) -> Result<Vec<(Group, Vec<usize>)>, String> {
    let total: usize = groups.iter().map(|(_, v)| v.len()).sum();
    let mut done = 0;
    let mut out = Vec::new();
    for (key, items) in groups {
        let mut infos = Vec::with_capacity(items.len());
        let mut marked = Vec::new();
        for (i, (path, keep)) in items.iter().enumerate() {
            if stop.load(Ordering::Relaxed) {
                return Err(dedupe::CANCELLED.into());
            }
            infos.push(audio::read(path, true));
            if *keep {
                marked.push(i);
            }
            done += 1;
            report(done, total);
        }
        let g = dedupe::make_group(format!("csv:{key}"), infos);
        out.push((g, marked));
    }
    let mut groups: Vec<Group> = out.iter().map(|(g, _)| g.clone()).collect();
    dedupe::sort_groups(&mut groups);
    // 정렬 순서에 맞춰 keep 표시도 다시 짝짓는다
    let marks: std::collections::HashMap<String, Vec<usize>> = out.into_iter().map(|(g, m)| (g.key, m)).collect();
    Ok(groups.into_iter().map(|g| {
        let m = marks.get(&g.key).cloned().unwrap_or_default();
        (g, m)
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_roundtrip_with_quotes() {
        let rows = parse("\u{feff}a,b,c\n1,\"x, \"\"y\"\"\",\"줄\n바꿈\"\n\n2,,z\n");
        assert_eq!(rows, vec![vec!["a", "b", "c"], vec!["1", "x, \"y\"", "줄\n바꿈"], vec!["2", "", "z"]]);
        assert_eq!(field("a,b"), "\"a,b\"");
        assert_eq!(field("plain"), "plain");
    }

    #[test]
    fn export_then_import_groups() {
        let root = std::env::temp_dir().join(format!("podlift-csv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).unwrap();
        for f in ["a/드림캐쳐.mp3", "a/Dreamcatcher.m4a", "a/other.mp3"] {
            std::fs::write(root.join(f), b"x").unwrap();
        }
        let info = |p: &str, artist: &str| AudioInfo { path: root.join(p), title: "Scream".into(), artist: artist.into(), album: "Dystopia, Part 1".into(), tagged: true, ..Default::default() };
        let csv = to_csv(&root, &[info("a/드림캐쳐.mp3", "드림캐쳐"), info("a/Dreamcatcher.m4a", "Dreamcatcher"), info("a/other.mp3", "X")]);
        assert!(csv.starts_with("\u{feff}id,group,keep,"));
        assert!(csv.contains("\"Dystopia, Part 1\""));
        // AI 가 group·keep 을 채웠다고 치고 (칸 순서도 바꿔서)
        let mut rows = parse(&csv);
        rows[1][1] = "1".into();
        rows[2][1] = "1".into();
        rows[2][2] = "1".into();
        rows.push(vec!["9".into(), "2".into(), "".into(), "t".into(), "".into(), "".into(), "".into(), "".into(), "".into(), "".into(), "".into(), "a/없는파일.mp3".into()]);
        let back: String = rows.iter().map(|r| r.iter().map(|c| field(c)).collect::<Vec<_>>().join(",") + "\n").collect();
        let (groups, report) = read_groups(&back, &root).unwrap();
        assert_eq!(report, ImportReport { rows: 4, groups: 1, missing: vec!["a/없는파일.mp3".into()] });
        assert_eq!(groups[0].0, "1");
        assert_eq!(groups[0].1, vec![(root.join("a/드림캐쳐.mp3"), false), (root.join("a/Dreamcatcher.m4a"), true)]);
        assert!(read_groups("id,title\n1,x\n", &root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// 실제 곡 파일로 내보내기 → group 채우기 → 불러오기. PODLIFT_CSV_DIR=<폴더> 일 때만 돈다.
#[cfg(test)]
#[test]
fn csv_end_to_end_real_files() {
    let Ok(dir) = std::env::var("PODLIFT_CSV_DIR") else { return };
    let root = PathBuf::from(dir);
    let stop = AtomicBool::new(false);
    let infos = read_all(&root, &stop, |_, _| {}).unwrap();
    assert_eq!(infos.len(), 2);
    // 자동 기준으로는 묶이지 않는다 (가수·앨범·제목 표기가 모두 다름)
    assert!(dedupe::group_candidates(infos.clone()).is_empty());
    let csv = to_csv(&root, &infos);
    println!("{csv}");
    let mut rows = parse(&csv);
    for r in rows.iter_mut().skip(1) {
        r[1] = "A".into();
    }
    let back: String = rows.iter().map(|r| r.iter().map(|c| field(c)).collect::<Vec<_>>().join(",") + "\n").collect();
    let (groups, report) = read_groups(&back, &root).unwrap();
    assert_eq!((report.groups, report.missing.len()), (1, 0));
    let built = build_groups(groups, &stop, |_, _| {}).unwrap();
    assert_eq!(built.len(), 1);
    assert_eq!(built[0].0.items.len(), 2);
    println!("묶음 판정 {:?}, 추천 {}", built[0].0.verdict, built[0].0.items[built[0].0.keep].path.display());
}
