//! 폴더 안 중복 곡 찾기와 휴지통 이동. 백그라운드 스레드에서 일하고 결과를 채널로 알린다.
//! 같은 곡의 기준과 남길 곡을 고르는 규칙은 audio.rs 에 있다. 여기서는 파일을 훑어 묶고,
//! 묶음마다 audio::pick_best 를 부르고, 길이가 많이 다른 묶음에 경고를 단다.
use crate::audio::{self, AudioInfo, Verdict};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use walkdir::WalkDir;

pub(crate) const AUDIO_EXT: &[&str] = &["mp3", "m4a", "m4b", "m4p", "aac", "wav", "aif", "aiff", "aifc", "flac", "ogg", "opus"];

/// 묶음 안 곡 길이가 이만큼(밀리초) 넘게 차이 나면 다른 버전일 수 있다고 본다.
const LENGTH_MISMATCH_MS: u64 = 10_000;

/// 사용자가 중지했을 때 Failed 로 보내는 문구
pub const CANCELLED: &str = "중지했습니다.";

/// 같은 곡으로 묶인 파일들
#[derive(Clone, Debug)]
pub struct Group {
    pub key: String,
    pub album: String,
    pub title: String,
    /// 경로 순
    pub items: Vec<AudioInfo>,
    /// 남길 곡의 위치 (추천값, 사용자가 바꿀 수 있다)
    pub keep: usize,
    pub verdict: Verdict,
    /// 곡 길이 차이가 커서 다른 버전(라이브·리믹스 등)일 수 있음
    pub length_mismatch: bool,
}

impl Group {
    /// 길이의 최댓값 - 최솟값 (밀리초)
    pub fn length_spread_ms(&self) -> u64 {
        let max = self.items.iter().map(|i| i.duration_ms).max().unwrap_or(0);
        let min = self.items.iter().map(|i| i.duration_ms).min().unwrap_or(0);
        max - min
    }
}

pub enum DedupeEvent {
    /// stage 1: 태그 읽기, 2: 앨범 아트 읽기. total 이 0 이면 아직 파일을 세는 중
    Progress { stage: u8, done: usize, total: usize },
    /// 다 읽음: 묶음 목록과 훑어 본 음악 파일 수
    Done { groups: Vec<Group>, scanned_files: usize },
    Failed(String),
}

pub struct Scanner {
    rx: Receiver<DedupeEvent>,
    cancel: Arc<AtomicBool>,
}

impl Scanner {
    pub fn poll(&self) -> Vec<DedupeEvent> {
        self.rx.try_iter().collect()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub fn start_scan(root: PathBuf) -> Scanner {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    std::thread::spawn(move || {
        let result = scan(&root, &stop, |stage, done, total| {
            let _ = tx.send(DedupeEvent::Progress { stage, done, total });
        });
        let _ = tx.send(match result {
            Ok((groups, scanned_files)) => DedupeEvent::Done { groups, scanned_files },
            Err(e) => DedupeEvent::Failed(e),
        });
    });
    Scanner { rx, cancel }
}

/// 폴더를 훑어 중복 묶음을 만든다. report(단계, 읽은 수, 전체 수)
pub fn scan(root: &Path, stop: &AtomicBool, mut report: impl FnMut(u8, usize, usize)) -> Result<(Vec<Group>, usize), String> {
    if !root.is_dir() {
        return Err("폴더를 찾을 수 없습니다.".into());
    }
    report(1, 0, 0);
    let files = collect_files(root, stop)?;
    let total = files.len();

    let mut infos = Vec::with_capacity(total);
    for (i, path) in files.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        infos.push(audio::read(path, false));
        report(1, i + 1, total);
    }

    let candidates = group_candidates(infos);
    let total2: usize = candidates.iter().map(|(_, items)| items.len()).sum();
    let mut done = 0;
    report(2, 0, total2);
    let mut groups = Vec::with_capacity(candidates.len());
    for (key, items) in candidates {
        let mut full = Vec::with_capacity(items.len());
        for it in items {
            if stop.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            full.push(audio::read(&it.path, true));
            done += 1;
            report(2, done, total2);
        }
        groups.push(make_group(key, full));
    }
    sort_groups(&mut groups);
    Ok((groups, total))
}

pub(crate) fn collect_files(root: &Path, stop: &AtomicBool) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    // 숨김 파일·폴더(.으로 시작, ._ 포함)는 들어가지 않는다
    let walker = WalkDir::new(root).into_iter().filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));
    for entry in walker.flatten() {
        if stop.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = entry.path().extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if AUDIO_EXT.contains(&ext.as_str()) {
            files.push(entry.into_path());
        }
    }
    Ok(files)
}

/// 같은 곡끼리 묶어(앨범+제목 또는 제목+가수, audio::same_song_groups) 2개 이상인 묶음만 돌려준다.
/// 열쇠가 없는 곡은 뺀다. 묶음 안은 경로 순, 묶음 열쇠는 첫 곡의 경로(묶음마다 다르다).
pub fn group_candidates(infos: Vec<AudioInfo>) -> Vec<(String, Vec<AudioInfo>)> {
    let refs: Vec<&AudioInfo> = infos.iter().collect();
    let groups = audio::same_song_groups(&refs);
    let mut slots: Vec<Option<AudioInfo>> = infos.into_iter().map(Some).collect();
    let mut out: BTreeMap<String, Vec<AudioInfo>> = BTreeMap::new();
    for g in groups {
        let mut items: Vec<AudioInfo> = g.iter().filter_map(|&i| slots[i].take()).collect();
        items.sort_by(|a, b| a.path.cmp(&b.path));
        out.insert(items[0].path.to_string_lossy().into_owned(), items);
    }
    out.into_iter().collect()
}

/// 묶음 하나에서 남길 곡과 판정을 정한다. items 는 2개 이상이어야 한다.
pub fn make_group(key: String, items: Vec<AudioInfo>) -> Group {
    let refs: Vec<&AudioInfo> = items.iter().collect();
    let (keep, mut verdict) = audio::pick_best(&refs);
    let max = items.iter().map(|i| i.duration_ms).max().unwrap_or(0);
    let min = items.iter().map(|i| i.duration_ms).min().unwrap_or(0);
    let length_mismatch = max - min > LENGTH_MISMATCH_MS;
    if length_mismatch {
        verdict = Verdict::Unsure;
    }
    Group { key, album: items[keep].album.clone(), title: items[keep].title.clone(), items, keep, verdict, length_mismatch }
}

/// 앨범, 제목 순 (대소문자 무시)
pub fn sort_groups(groups: &mut [Group]) {
    groups.sort_by_cached_key(|g| (g.album.to_lowercase(), g.title.to_lowercase()));
}

pub enum TrashEvent {
    Progress { done: usize, total: usize },
    Moved(PathBuf),
    Failed(PathBuf, String),
    Finished,
}

pub struct Trasher {
    rx: Receiver<TrashEvent>,
}

impl Trasher {
    pub fn poll(&self) -> Vec<TrashEvent> {
        self.rx.try_iter().collect()
    }
}

/// 파일을 하나씩 휴지통으로 옮긴다. 끝나면 비게 된 폴더(root 자체는 제외)를 지운다.
pub fn start_trash(paths: Vec<PathBuf>, root: PathBuf) -> Trasher {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let total = paths.len();
        let mut moved = Vec::new();
        for (i, path) in paths.into_iter().enumerate() {
            match trash::delete(&path) {
                Ok(()) => {
                    let _ = tx.send(TrashEvent::Moved(path.clone()));
                    moved.push(path);
                }
                Err(e) => {
                    let _ = tx.send(TrashEvent::Failed(path, e.to_string()));
                }
            }
            let _ = tx.send(TrashEvent::Progress { done: i + 1, total });
        }
        prune_empty_dirs(&root, &moved);
        let _ = tx.send(TrashEvent::Finished);
    });
    Trasher { rx }
}

/// 옮긴 파일들이 있던 폴더가 비었으면 위로 올라가며 지운다. root 와 그 위는 건드리지 않는다.
/// .DS_Store 만 남은 폴더도 비었다고 본다. remove_dir 은 비어 있을 때만 성공하므로 안전하다.
pub fn prune_empty_dirs(root: &Path, files: &[PathBuf]) -> usize {
    let parents: HashSet<&Path> = files.iter().filter_map(|f| f.parent()).collect();
    let mut removed = 0;
    for start in parents {
        let mut dir = start.to_path_buf();
        while dir != root && dir.starts_with(root) {
            let only_junk = std::fs::read_dir(&dir)
                .map(|rd| rd.flatten().all(|e| e.file_name() == ".DS_Store"))
                .unwrap_or(false);
            if only_junk {
                let _ = std::fs::remove_file(dir.join(".DS_Store"));
            }
            if std::fs::remove_dir(&dir).is_err() {
                break;
            }
            removed += 1;
            if !dir.pop() {
                break;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Cover;
    use std::cmp::Ordering as Ord;

    fn song(path: &str, album: &str, title: &str, codec: &'static str, lossless: bool, kbps: u32, secs: u64) -> AudioInfo {
        AudioInfo {
            path: PathBuf::from(path),
            album: album.into(),
            title: title.into(),
            tagged: true,
            codec,
            lossless,
            bitrate_kbps: kbps,
            sample_rate: 44100,
            bit_depth: if lossless { 16 } else { 0 },
            duration_ms: secs * 1000,
            ..Default::default()
        }
    }

    fn with_cover(mut i: AudioInfo, side: u32) -> AudioInfo {
        i.cover = Some(Cover { width: side, height: side, bytes: 1000, thumb: None });
        i
    }

    #[test]
    fn groups_by_normalized_key_and_drops_singles_and_untagged() {
        let mut untagged_a = song("/m/u1.mp3", "A", "T", "MP3", false, 128, 200);
        let mut untagged_b = song("/m/u2.mp3", "A", "T", "MP3", false, 128, 200);
        untagged_a.tagged = false;
        untagged_b.tagged = false;
        let infos = vec![
            song("/m/b.m4a", "Album X", "Song  A", "AAC", false, 256, 200),
            song("/m/a.mp3", "album x", "song a", "MP3", false, 128, 200),
            song("/m/c.m4a", "Album X", "Song B", "AAC", false, 256, 180),
            song("/m/d.m4a", "Album Y", "Song A", "AAC", false, 256, 200),
            untagged_a,
            untagged_b,
        ];
        let groups = group_candidates(infos);
        assert_eq!(groups.len(), 1);
        let paths: Vec<_> = groups[0].1.iter().map(|i| i.path.to_str().unwrap()).collect();
        assert_eq!(paths, ["/m/a.mp3", "/m/b.m4a"]);
    }

    #[test]
    fn keeps_better_quality() {
        let items = vec![
            song("/m/a.mp3", "X", "S", "MP3", false, 128, 200),
            song("/m/b.m4a", "X", "S", "ALAC", true, 900, 200),
            song("/m/c.m4a", "X", "S", "AAC", false, 256, 200),
        ];
        let g = make_group("k".into(), items);
        assert_eq!(g.keep, 1);
        assert_eq!(g.verdict, Verdict::Quality(Ord::Greater));
        assert!(!g.length_mismatch);
    }

    #[test]
    fn cover_decides_when_quality_ties() {
        let items = vec![
            song("/m/a.m4a", "X", "S", "AAC", false, 256, 200),
            with_cover(song("/m/b.m4a", "X", "S", "AAC", false, 256, 200), 1400),
        ];
        let g = make_group("k".into(), items);
        assert_eq!(g.keep, 1);
        assert_eq!(g.verdict, Verdict::Cover(Ord::Greater));
    }

    #[test]
    fn identical_is_same() {
        let items = vec![song("/m/a.m4a", "X", "S", "AAC", false, 256, 200), song("/m/b.m4a", "X", "S", "AAC", false, 256, 200)];
        assert_eq!(make_group("k".into(), items).verdict, Verdict::Same);
        let items = vec![song("/m/a.mp3", "X", "S", "MP3", false, 256, 200), song("/m/b.m4a", "X", "S", "AAC", false, 256, 200)];
        assert_eq!(make_group("k".into(), items).verdict, Verdict::Unsure);
    }

    #[test]
    fn length_mismatch_forces_unsure_but_still_recommends() {
        // 10초 차이는 경고하지 않고, 10초를 넘으면 경고한다
        let ok = vec![song("/m/a.mp3", "X", "S", "MP3", false, 128, 200), song("/m/b.m4a", "X", "S", "AAC", false, 256, 210)];
        let g = make_group("k".into(), ok);
        assert!(!g.length_mismatch);
        assert_eq!(g.verdict, Verdict::Quality(Ord::Greater));

        let live = vec![song("/m/a.mp3", "X", "S", "MP3", false, 128, 200), song("/m/b.m4a", "X", "S", "AAC", false, 256, 245)];
        let g = make_group("k".into(), live);
        assert!(g.length_mismatch);
        assert_eq!(g.verdict, Verdict::Unsure);
        assert_eq!(g.keep, 1);
        assert_eq!(g.length_spread_ms(), 45_000);
    }

    #[test]
    fn groups_sorted_by_album_then_title() {
        let mk = |a: &str, t: &str| make_group(format!("{a}{t}"), vec![song("/m/1.mp3", a, t, "MP3", false, 128, 1), song("/m/2.mp3", a, t, "MP3", false, 128, 1)]);
        let mut gs = vec![mk("B", "a"), mk("a", "z"), mk("a", "b")];
        sort_groups(&mut gs);
        let names: Vec<_> = gs.iter().map(|g| format!("{}/{}", g.album, g.title)).collect();
        assert_eq!(names, ["a/b", "a/z", "B/a"]);
    }

    #[test]
    fn prune_removes_only_empty_dirs() {
        let root = std::env::temp_dir().join(format!("podlift-dedupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let gone = root.join("a/b");
        let kept = root.join("c");
        std::fs::create_dir_all(&gone).unwrap();
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::write(gone.join(".DS_Store"), b"x").unwrap();
        std::fs::write(kept.join("other.mp3"), b"x").unwrap();
        let moved = vec![gone.join("x.mp3"), kept.join("y.mp3"), root.join("z.mp3")];
        assert_eq!(prune_empty_dirs(&root, &moved), 2);
        assert!(!root.join("a").exists());
        assert!(kept.join("other.mp3").is_file());
        assert!(root.is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// PODLIFT_DEDUPE_DIR 가 있을 때만 도는 실제 파일 시험.
    /// 폴더에는 앨범 "Album X" 의 "Song A"(mp3 128k, aac 256k, alac 가 섞임)와
    /// "Song B"(aac 256k 둘, 하나에만 큰 커버)가 있다고 가정한다.
    #[test]
    fn scan_real_folder() {
        let Ok(dir) = std::env::var("PODLIFT_DEDUPE_DIR") else { return };
        let stop = AtomicBool::new(false);
        let (groups, scanned) = scan(Path::new(&dir), &stop, |_, _, _| {}).unwrap();
        println!("scanned {scanned} files, {} groups", groups.len());
        for g in &groups {
            println!("{} / {}  keep={} verdict={:?} mismatch={}", g.album, g.title, g.keep, g.verdict, g.length_mismatch);
            for (i, it) in g.items.iter().enumerate() {
                let cover = it.cover.as_ref().map(|c| format!("{}x{}", c.width, c.height)).unwrap_or("-".into());
                println!("  {} {} {} {}ms cover={cover}", if i == g.keep { "*" } else { " " }, it.path.display(), it.quality_label(), it.duration_ms);
            }
            let refs: Vec<&AudioInfo> = g.items.iter().collect();
            assert_eq!(audio::pick_best(&refs).0, g.keep);
        }
        let a = groups.iter().find(|g| g.title == "Song A").expect("Song A 묶음");
        assert_eq!(a.items[a.keep].codec, "ALAC");
        assert_eq!(a.verdict, Verdict::Quality(Ord::Greater));
        let b = groups.iter().find(|g| g.title == "Song B").expect("Song B 묶음");
        assert!(b.items[b.keep].cover.is_some());
        assert_eq!(b.verdict, Verdict::Cover(Ord::Greater));
    }
}
