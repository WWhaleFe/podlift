//! 아이팟 음악 폴더 읽기. iPod_Control/Music/Fxx 아래의 뒤섞인 이름(ABCD.m4a) 파일을 훑어
//! 태그를 읽고, 가져올 때 쓸 '가수/앨범/[디스크-]트랙 제목.확장자' 상대 경로를 정한다.
//! 아이팟은 하드디스크라 여러 스레드로 읽으면 오히려 느려지므로 스레드 하나로 차례로 읽는다.
use lofty::config::ParseOptions;
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

const AUDIO_EXT: &[&str] = &["mp3", "m4a", "m4b", "m4p", "aac", "wav", "aif", "aiff", "aifc"];
const VIDEO_EXT: &[&str] = &["m4v", "mp4", "mov"];

/// 파일 이름 한 칸의 최대 바이트 수 (macOS 한도 255 바이트, 번호·확장자 몫을 남긴다)
const NAME_MAX_BYTES: usize = 200;

#[derive(Clone, Debug)]
pub struct Track {
    pub src: PathBuf,
    pub size: u64,
    /// 태그의 제목. 없으면 아이팟 안 파일 이름(ABCD)
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: bool,
    /// 제목 태그를 읽었는지 (false 면 파일 이름만으로 저장된다)
    pub tagged: bool,
    /// iTunes Store 구매 곡(.m4p, FairPlay). 복사는 되지만 다른 기기에서는 재생되지 않는다.
    pub protected: bool,
    pub video: bool,
    /// 저장 폴더 기준 상대 경로
    pub rel_dest: PathBuf,
    /// 검색용 소문자 문자열 (제목·가수·앨범)
    pub search_key: String,
}

pub enum ScanEvent {
    /// (읽은 파일 수, 전체 파일 수)
    Progress(usize, usize),
    /// 다 읽음: 곡 목록과 음악·영상이 아니라 건너뛴 파일 수
    Done(Vec<Track>, usize),
    Failed(String),
}

pub struct Scanner {
    rx: Receiver<ScanEvent>,
    cancel: Arc<AtomicBool>,
}

impl Scanner {
    pub fn poll(&self) -> Vec<ScanEvent> {
        self.rx.try_iter().collect()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub fn start_scan(music_root: PathBuf) -> Scanner {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    std::thread::spawn(move || {
        let (files, others) = match collect_files(&music_root) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(ScanEvent::Failed(format!("음악 폴더를 읽지 못했습니다: {e}")));
                return;
            }
        };
        let total = files.len();
        let mut tracks = Vec::with_capacity(total);
        for (i, (path, size)) in files.into_iter().enumerate() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            tracks.push(read_track(path, size));
            if i % 25 == 0 || i + 1 == total {
                let _ = tx.send(ScanEvent::Progress(i + 1, total));
            }
        }
        plan_destinations(&mut tracks);
        let _ = tx.send(ScanEvent::Done(tracks, others));
    });
    Scanner { rx, cancel }
}

/// Music/Fxx/파일 두 단계만 훑는다. 맥이 FAT32 아이팟에 남기는 '._' 파일은 뺀다.
/// 돌려주는 값: ((경로, 크기) 목록, 음악·영상이 아닌 파일 수)
fn collect_files(root: &Path) -> std::io::Result<(Vec<(PathBuf, u64)>, usize)> {
    let mut files = Vec::new();
    let mut others = 0;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)?
        .flatten()
        // 숨김 폴더(.podlift-old 처럼 Podlift 가 바꿔 넣으며 치워 둔 옛 파일)는 건너뛴다
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            if media_kind(&path).is_some() {
                files.push((path, meta.len()));
            } else {
                others += 1;
            }
        }
    }
    files.sort();
    Ok((files, others))
}

/// Some(false) = 음악, Some(true) = 영상, None = 그 밖의 파일
fn media_kind(path: &Path) -> Option<bool> {
    let ext = ext_lower(path);
    if AUDIO_EXT.contains(&ext.as_str()) {
        Some(false)
    } else if VIDEO_EXT.contains(&ext.as_str()) {
        Some(true)
    } else {
        None
    }
}

fn ext_lower(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

fn read_track(src: PathBuf, size: u64) -> Track {
    let ext = ext_lower(&src);
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut t = Track {
        title: stem,
        artist: String::new(),
        album_artist: String::new(),
        album: String::new(),
        track: None,
        disc: None,
        disc_total: None,
        compilation: false,
        tagged: false,
        protected: ext == "m4p",
        video: media_kind(&src) == Some(true),
        size,
        src,
        rel_dest: PathBuf::new(),
        search_key: String::new(),
    };
    // 재생 시간·커버 이미지는 필요 없으니 읽지 않는다 (아이팟 디스크 읽기를 줄인다)
    let opts = ParseOptions::new().read_properties(false).read_cover_art(false);
    let tagged = (|| Probe::open(&t.src).ok()?.guess_file_type().ok()?.options(opts).read().ok())();
    if let Some(file) = tagged {
        if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
            let s = |v: Option<std::borrow::Cow<'_, str>>| v.map(|c| c.trim().to_string()).unwrap_or_default();
            let title = s(tag.title());
            if !title.is_empty() {
                t.title = title;
                t.tagged = true;
            }
            t.artist = s(tag.artist());
            t.album = s(tag.album());
            t.album_artist = tag.get_string(ItemKey::AlbumArtist).map(|v| v.trim().to_string()).unwrap_or_default();
            t.track = tag.track().filter(|&n| n > 0);
            t.disc = tag.disk().filter(|&n| n > 0);
            t.disc_total = tag.disk_total().filter(|&n| n > 0);
            t.compilation = tag
                .get_string(ItemKey::FlagCompilation)
                .is_some_and(|v| matches!(v.trim(), "1" | "true" | "True"));
        }
    }
    t.search_key = format!("{} {} {} {}", t.title, t.artist, t.album_artist, t.album).to_lowercase();
    t
}

/// 가수 폴더 이름: 컴필레이션은 "Compilations", 그 밖에는 앨범 가수 → 가수 → "Unknown Artist"
pub fn artist_folder(t: &Track) -> &str {
    if t.compilation {
        "Compilations"
    } else if !t.album_artist.is_empty() {
        &t.album_artist
    } else if !t.artist.is_empty() {
        &t.artist
    } else {
        "Unknown Artist"
    }
}

/// 곡을 가수·앨범·디스크·트랙 순으로 정렬하고 겹치지 않는 저장 경로를 붙인다.
/// 같은 아이팟을 다시 읽으면 같은 경로가 나오므로, 이어 가져오기 때 이미 받은 곡을 알아볼 수 있다.
pub(crate) fn plan_destinations(tracks: &mut [Track]) {
    tracks.sort_by(|a, b| {
        let key = |t: &Track| {
            (
                artist_folder(t).to_lowercase(),
                t.album.to_lowercase(),
                t.disc.unwrap_or(1),
                t.track.unwrap_or(u32::MAX),
                t.title.to_lowercase(),
            )
        };
        // 같은 자리를 두고 겹치면 이름이 짧은 파일(이미 " (2)" 가 붙은 쪽이 아닌 것)이 먼저 자리를 갖는다
        key(a).cmp(&key(b)).then_with(|| a.src.as_os_str().len().cmp(&b.src.as_os_str().len())).then_with(|| a.src.cmp(&b.src))
    });
    // macOS 기본 디스크는 대소문자를 구분하지 않으므로 소문자로 겹침을 판단한다.
    // 대소문자만 다른 가수·앨범 폴더는 처음 나온 표기 하나로 맞춘다 (따로 만들면 디스크에서는 한 폴더가 된다).
    let mut used: HashSet<String> = HashSet::new();
    let mut dir_names: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    for t in tracks.iter_mut() {
        let artist = clean(artist_folder(t));
        let album = clean(if t.album.is_empty() { "Unknown Album" } else { &t.album });
        let ext = ext_lower(&t.src);
        let multi_disc = t.disc_total.is_some_and(|n| n > 1) || t.disc.is_some_and(|n| n > 1);
        let number = match (t.track, multi_disc) {
            (Some(n), true) => format!("{}-{:02} ", t.disc.unwrap_or(1), n),
            (Some(n), false) => format!("{n:02} "),
            (None, _) => String::new(),
        };
        let base = clean(&format!("{number}{}", t.title));
        let artist_dir = dir_names.entry(artist.to_lowercase()).or_insert_with(|| PathBuf::from(&artist)).clone();
        let dir = dir_names
            .entry(format!("{}/{}", artist.to_lowercase(), album.to_lowercase()))
            .or_insert_with(|| artist_dir.join(&album))
            .clone();
        let mut n = 1;
        loop {
            let file = if n == 1 { format!("{base}.{ext}") } else { format!("{base} ({n}).{ext}") };
            let rel = dir.join(&file);
            if used.insert(rel.to_string_lossy().to_lowercase()) {
                t.rel_dest = rel;
                break;
            }
            n += 1;
        }
    }
}

/// 파일·폴더 이름 한 칸으로 쓸 수 있게 다듬는다. 경로 구분자와 다른 기기(exFAT 외장 디스크 등)에서
/// 못 쓰는 글자는 '_' 로 바꾸고, 앞의 점(숨김 파일이 됨)과 끝의 점·공백을 없앤다.
fn clean(s: &str) -> String {
    let replaced: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = replaced.trim().trim_start_matches('.').trim_end_matches(['.', ' ']);
    let mut out = String::new();
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > NAME_MAX_BYTES {
            break;
        }
        out.push(c);
    }
    let out = out.trim_end().to_string();
    if out.is_empty() { "_".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artist: &str, album: &str, n: Option<u32>) -> Track {
        Track {
            src: PathBuf::from(format!("/x/F00/{title}.m4a")),
            size: 1,
            title: title.into(),
            artist: artist.into(),
            album_artist: String::new(),
            album: album.into(),
            track: n,
            disc: None,
            disc_total: None,
            compilation: false,
            tagged: true,
            protected: false,
            video: false,
            rel_dest: PathBuf::new(),
            search_key: String::new(),
        }
    }

    #[test]
    fn clean_names() {
        assert_eq!(clean("AC/DC"), "AC_DC");
        assert_eq!(clean("..hidden"), "hidden");
        assert_eq!(clean("Live... "), "Live");
        assert_eq!(clean("   "), "_");
        let long = "가".repeat(100);
        assert!(clean(&long).len() <= NAME_MAX_BYTES);
    }

    #[test]
    fn plan_paths_and_collisions() {
        let mut v = vec![
            track("Song", "A", "Album", Some(1)),
            track("song", "a", "album", Some(1)),
            track("Intro", "", "", None),
        ];
        plan_destinations(&mut v);
        let paths: Vec<String> = v.iter().map(|t| t.rel_dest.to_string_lossy().into_owned()).collect();
        assert!(paths.contains(&"A/Album/01 Song.m4a".to_string()));
        assert!(paths.iter().any(|p| p.ends_with("01 song (2).m4a") || p.ends_with("01 Song (2).m4a")));
        assert!(paths.contains(&"Unknown Artist/Unknown Album/Intro.m4a".to_string()));
        // 대소문자만 다른 앨범은 한 폴더로
        let mut v = vec![track("One", "X", "Anime Songs", Some(1)), track("Two", "X", "ANIME SONGS", Some(2))];
        plan_destinations(&mut v);
        assert_eq!(v[0].rel_dest.parent(), v[1].rel_dest.parent());
    }

    #[test]
    fn multi_disc_and_compilation() {
        let mut a = track("X", "Someone", "Hits", Some(3));
        a.disc = Some(2);
        a.disc_total = Some(2);
        a.compilation = true;
        let mut v = vec![a];
        plan_destinations(&mut v);
        assert_eq!(v[0].rel_dest, PathBuf::from("Compilations/Hits/2-03 X.m4a"));
    }
}

/// 실제 폴더로 읽기 → 복사까지 확인한다. PODLIFT_FAKE_IPOD=<아이팟처럼 꾸민 폴더> 일 때만 돈다.
#[cfg(test)]
mod e2e {
    #[test]
    fn scan_and_copy_fake_ipod() {
        let Ok(root) = std::env::var("PODLIFT_FAKE_IPOD") else { return };
        let ipod = crate::ipod::from_folder(std::path::Path::new(&root)).expect("아이팟 폴더 인식");
        assert_eq!(ipod.model, "xB029");
        let scanner = super::start_scan(ipod.music.clone());
        let (tracks, others) = loop {
            if let Some(super::ScanEvent::Done(t, o)) = scanner.rx.recv().ok() {
                break (t, o);
            }
        };
        for t in &tracks {
            println!("{} | {} | {}", t.rel_dest.display(), t.title, t.tagged);
        }
        assert_eq!(tracks.len(), 3);
        assert_eq!(others, 1);
        let out = std::path::PathBuf::from(&root).with_file_name("fakepod-out");
        let _ = std::fs::remove_dir_all(&out);
        let jobs = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| crate::copier::Job { index: i, src: t.src.clone(), dest: out.join(&t.rel_dest), size: t.size })
            .collect();
        let copier = crate::copier::start(jobs, ipod.music.clone());
        std::thread::sleep(std::time::Duration::from_millis(500));
        let copied = copier.poll().iter().filter(|e| matches!(e, crate::copier::CopyEvent::Done(_, _, crate::copier::Outcome::Copied))).count();
        assert_eq!(copied, 3);
    }
}
