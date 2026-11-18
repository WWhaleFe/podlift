//! 맥 음악 폴더 정리: 파일을 태그대로 '가수/앨범/[디스크-]트랙 제목.확장자' 구조로 옮기고,
//! 앨범 아트가 없는 곡 파일에 그림을 심는다.
//!
//! - 경로 규칙은 가져오기(library.rs)와 같다. 제목 태그가 없는 파일은 옮기지 않는다.
//! - 옮기기는 두 단계다: 먼저 임시 폴더로 모두 옮긴 뒤 제자리로. 그래서 A→B, B→C 처럼 이어진
//!   이동이나 대소문자만 다른 이름도 꼬이지 않는다. 같은 이름이 이미 있으면 " (2)" 를 붙인다.
//! - 앨범 폴더의 cover.jpg 같은 그림 파일은 곡을 따라 옮긴다(곡이 여러 폴더로 갈리면 복사).
//! - 되돌리기용 기록을 ~/Library/Application Support/Podlift/organize-last.json 에 남긴다.
//! - Music 앱(또는 iTunes)이 관리하는 폴더와 아이팟은 정리하지 않는다. 옮기면 그 앱이 곡을 잃는다.
use crate::audio::{self, AudioInfo};
use crate::dedupe;
use crate::library::{self, Track};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::Tag;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// 정리하면 안 되는 폴더면 Err(이유)
pub fn guard(root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Err("폴더를 찾을 수 없습니다.".into());
    }
    for dir in root.ancestors() {
        if dir.join("iPod_Control").is_dir() {
            return Err("아이팟 안의 폴더는 정리할 수 없습니다. 아이팟 곡 정리는 '중복 정리' 탭의 아이팟을 쓰세요.".into());
        }
        if is_library_dir(dir) {
            return Err("Music 앱(또는 iTunes)이 관리하는 보관함 폴더입니다. 여기서 파일을 옮기면 Music 앱이 곡을 찾지 못합니다. Music 앱 설정의 '미디어 폴더 정리'를 쓰세요.".into());
        }
    }
    Ok(())
}

/// Music 앱·iTunes 보관함 폴더인지 (보관함 파일이 들어 있음)
fn is_library_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            n.ends_with(".musiclibrary") || n == "itunes library.itl" || n == "itunes music library.xml"
        })
    })
}

/// 폴더 안 음악 파일 (숨김 파일·Music 앱 보관함 폴더 아래는 뺀다)
pub fn collect(root: &Path, stop: &AtomicBool) -> Result<Vec<PathBuf>, String> {
    let files = dedupe::collect_files(root, stop)?;
    let mut lib_dirs: HashMap<PathBuf, bool> = HashMap::new();
    Ok(files
        .into_iter()
        .filter(|f| {
            !f.ancestors().skip(1).take_while(|d| d.starts_with(root)).any(|d| *lib_dirs.entry(d.to_path_buf()).or_insert_with(|| is_library_dir(d)))
        })
        .collect())
}

// ── 구조 정리 ───────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImageOp {
    Move(PathBuf, PathBuf),
    Copy(PathBuf, PathBuf),
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub root: PathBuf,
    pub moves: Vec<Move>,
    /// 이미 제자리인 곡 수
    pub in_place: usize,
    /// 제목 태그가 없어 그대로 두는 파일
    pub untagged: Vec<PathBuf>,
    pub images: Vec<ImageOp>,
}

fn track_of(i: &AudioInfo) -> Track {
    Track {
        src: i.path.clone(),
        size: i.size,
        title: i.title.clone(),
        artist: i.artist.clone(),
        album_artist: i.album_artist.clone(),
        album: i.album.clone(),
        track: i.track,
        disc: i.disc,
        disc_total: i.disc_total,
        compilation: i.compilation,
        tagged: i.tagged,
        protected: false,
        video: false,
        rel_dest: PathBuf::new(),
        search_key: String::new(),
    }
}

/// 옮길 계획을 세운다 (파일은 건드리지 않는다)
pub fn plan(root: &Path, infos: &[AudioInfo]) -> Plan {
    let mut out = Plan { root: root.to_path_buf(), ..Default::default() };
    let mut tracks: Vec<Track> = Vec::new();
    for i in infos {
        if i.tagged {
            tracks.push(track_of(i));
        } else {
            out.untagged.push(i.path.clone());
        }
    }
    library::plan_destinations(&mut tracks);
    // 폴더별로 남는 곡이 있는지 (그림 파일을 옮길지 복사할지 정할 때)
    let mut staying: HashSet<PathBuf> = out.untagged.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
    let mut dests: BTreeMap<PathBuf, HashSet<PathBuf>> = BTreeMap::new();
    for t in &tracks {
        let to = root.join(&t.rel_dest);
        let from_dir = t.src.parent().map(Path::to_path_buf).unwrap_or_default();
        // 대소문자만 다르면 디스크에서는 같은 자리다
        if to == t.src || to.to_string_lossy().to_lowercase() == t.src.to_string_lossy().to_lowercase() {
            out.in_place += 1;
            staying.insert(from_dir);
        } else {
            dests.entry(from_dir).or_default().insert(to.parent().map(Path::to_path_buf).unwrap_or_default());
            out.moves.push(Move { from: t.src.clone(), to });
        }
    }
    for (dir, targets) in dests {
        let leaving = !staying.contains(&dir) && targets.len() == 1;
        for img in audio::folder_cover_files(&dir) {
            let Some(name) = img.file_name() else { continue };
            for t in &targets {
                if t == &dir || t.join(name).exists() {
                    continue;
                }
                let op = if leaving { ImageOp::Move(img.clone(), t.join(name)) } else { ImageOp::Copy(img.clone(), t.join(name)) };
                out.images.push(op);
            }
        }
    }
    out
}

/// 되돌리기 기록
#[derive(Serialize, Deserialize, Default)]
struct UndoLog {
    root: PathBuf,
    /// (옮긴 자리, 원래 자리)
    moves: Vec<(PathBuf, PathBuf)>,
    /// 복사해 만든 그림 파일 (되돌릴 때 지운다)
    copies: Vec<PathBuf>,
}

fn undo_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("podlift-organize-last-{}.json", std::process::id())));
    }
    dirs::config_dir().map(|d| d.join("Podlift").join("organize-last.json"))
}

#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub moved: usize,
    pub images: usize,
    pub failed: Vec<(PathBuf, String)>,
}

/// 겹치지 않는 이름: 이미 있으면 "이름 (2).확장자"
fn free_name(to: &Path) -> PathBuf {
    if !to.exists() {
        return to.to_path_buf();
    }
    let stem = to.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = to.extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
    (2..).map(|n| to.with_file_name(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap()
}

/// 계획대로 옮긴다. report(옮긴 수, 전체)
pub fn run(plan: &Plan, mut report: impl FnMut(usize, usize)) -> Outcome {
    let mut out = Outcome::default();
    let mut log = UndoLog { root: plan.root.clone(), ..Default::default() };
    let staging = plan.root.join(format!(".podlift-organize-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&staging) {
        out.failed.push((staging, e.to_string()));
        return out;
    }
    // 1) 모두 임시 폴더로
    let mut staged: Vec<(PathBuf, &Move)> = Vec::new();
    for (i, m) in plan.moves.iter().enumerate() {
        let ext = m.from.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let tmp = staging.join(format!("{i}{ext}"));
        match std::fs::rename(&m.from, &tmp) {
            Ok(()) => staged.push((tmp, m)),
            Err(e) => out.failed.push((m.from.clone(), e.to_string())),
        }
    }
    // 2) 제자리로
    let total = staged.len();
    for (n, (tmp, m)) in staged.into_iter().enumerate() {
        let placed = m.to.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| {
            let to = free_name(&m.to);
            std::fs::rename(&tmp, &to).map(|_| to)
        });
        match placed {
            Ok(to) => {
                log.moves.push((to, m.from.clone()));
                out.moved += 1;
            }
            Err(e) => {
                // 원래 자리로 돌려놓는다
                let _ = std::fs::rename(&tmp, &m.from);
                out.failed.push((m.from.clone(), e.to_string()));
            }
        }
        report(n + 1, total);
    }
    let _ = std::fs::remove_dir(&staging);
    // 3) 그림 파일
    for op in &plan.images {
        let (from, to, copy) = match op {
            ImageOp::Move(f, t) => (f, t, false),
            ImageOp::Copy(f, t) => (f, t, true),
        };
        if to.exists() || !to.parent().is_some_and(Path::is_dir) {
            continue;
        }
        let done = if copy { std::fs::copy(from, to).map(|_| ()) } else { std::fs::rename(from, to) };
        if done.is_ok() {
            out.images += 1;
            if copy {
                log.copies.push(to.clone());
            } else {
                log.moves.push((to.clone(), from.clone()));
            }
        }
    }
    // 4) 비게 된 폴더 정리, 되돌리기 기록
    let sources: Vec<PathBuf> = log.moves.iter().map(|(_, from)| from.clone()).collect();
    dedupe::prune_empty_dirs(&plan.root, &sources);
    if let Some(p) = undo_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, serde_json::to_string_pretty(&log).unwrap_or_default());
    }
    out
}

/// 되돌릴 수 있는 마지막 정리의 폴더
pub fn last_root() -> Option<PathBuf> {
    let log: UndoLog = serde_json::from_str(&std::fs::read_to_string(undo_path()?).ok()?).ok()?;
    (!log.moves.is_empty() || !log.copies.is_empty()).then_some(log.root)
}

/// 마지막 정리를 되돌린다. 되돌린 파일 수
pub fn undo() -> Result<usize, String> {
    let path = undo_path().ok_or("기록 위치를 알 수 없습니다")?;
    let log: UndoLog = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|_| "되돌릴 정리 기록이 없습니다".to_string())?)
        .map_err(|e| format!("정리 기록이 이상합니다: {e}"))?;
    let mut back = 0;
    let mut stuck = 0;
    for (now, orig) in log.moves.iter().rev() {
        if orig.exists() || !now.exists() {
            stuck += 1;
            continue;
        }
        let ok = orig.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::rename(now, orig));
        if ok.is_ok() {
            back += 1;
        } else {
            stuck += 1;
        }
    }
    for c in &log.copies {
        let _ = std::fs::remove_file(c);
    }
    let placed: Vec<PathBuf> = log.moves.iter().map(|(now, _)| now.clone()).chain(log.copies.iter().cloned()).collect();
    dedupe::prune_empty_dirs(&log.root, &placed);
    let _ = std::fs::remove_file(&path);
    if stuck > 0 {
        return Err(format!("{back}개는 되돌렸지만 {stuck}개는 원래 자리에 다른 파일이 있거나 파일이 없어 그대로 두었습니다."));
    }
    Ok(back)
}

// ── 앨범 아트 심기 ──────────────────────────────────────

/// 아트를 넣을 수 있는 그림의 출처
#[derive(Clone, Debug, PartialEq)]
pub enum ArtSource {
    /// 같은 앨범의 다른 곡에 든 그림
    Sibling(PathBuf),
    /// 같은 폴더의 그림 파일
    FolderImage(PathBuf),
    /// 사용자가 고른 그림 파일
    Picked(PathBuf),
    /// 온라인에서 찾아 받아 둔 그림 (파일, 어디서 찾았는지)
    Online(PathBuf, String),
    /// 사용자가 고른 다른 앨범의 그림 (그 앨범 곡 파일, 앨범 이름)
    Borrowed(PathBuf, String),
}

impl ArtSource {
    pub fn label(&self) -> &'static str {
        match self {
            ArtSource::Sibling(_) => "같은 앨범 곡의 그림",
            ArtSource::FolderImage(_) => "폴더의 그림 파일",
            ArtSource::Picked(_) => "직접 고른 그림",
            ArtSource::Online(..) => "온라인에서 찾은 그림",
            ArtSource::Borrowed(..) => "고른 앨범의 그림",
        }
    }

    /// 미리 보기 캐시 열쇠로 쓰는 경로
    pub fn path(&self) -> &Path {
        match self {
            ArtSource::Sibling(p) | ArtSource::FolderImage(p) | ArtSource::Picked(p) | ArtSource::Online(p, _) | ArtSource::Borrowed(p, _) => p,
        }
    }

    pub fn bytes(&self) -> Option<Vec<u8>> {
        match self {
            ArtSource::Sibling(p) | ArtSource::Borrowed(p, _) => audio::embedded_cover(p),
            ArtSource::FolderImage(p) | ArtSource::Picked(p) | ArtSource::Online(p, _) => std::fs::read(p).ok(),
        }
    }
}

/// 아트가 없는 곡이 있는 앨범 하나
#[derive(Clone, Debug)]
pub struct ArtAlbum {
    pub album: String,
    pub artist: String,
    /// 아트가 없는 곡 파일
    pub missing: Vec<PathBuf>,
    pub total: usize,
    /// 자동으로 찾은 그림 (없으면 사용자가 골라야 한다)
    pub source: Option<ArtSource>,
}

/// (곡 정보, 태그에 그림이 있는지)로 앨범을 묶어 아트가 없는 곡이 있는 앨범만 돌려준다.
/// 앨범 태그가 없는 곡은 폴더 단위로 묶는다.
pub fn plan_art(files: &[(AudioInfo, bool)]) -> Vec<ArtAlbum> {
    let mut groups: BTreeMap<(String, String), Vec<&(AudioInfo, bool)>> = BTreeMap::new();
    for f in files {
        let i = &f.0;
        let artist = if i.album_artist.is_empty() { &i.artist } else { &i.album_artist };
        let key = if i.album.trim().is_empty() {
            ("\u{0}".to_string(), i.path.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default())
        } else {
            (audio::normalize(&i.album), if i.compilation { String::new() } else { audio::normalize(artist) })
        };
        groups.entry(key).or_default().push(f);
    }
    let mut out = Vec::new();
    for (_, items) in groups {
        let missing: Vec<PathBuf> = items.iter().filter(|f| !f.1).map(|f| f.0.path.clone()).collect();
        if missing.is_empty() {
            continue;
        }
        let first = &items[0].0;
        let source = items
            .iter()
            .find(|f| f.1)
            .map(|f| ArtSource::Sibling(f.0.path.clone()))
            .or_else(|| missing.iter().find_map(|p| audio::folder_cover_files(p.parent()?).into_iter().next()).map(ArtSource::FolderImage));
        out.push(ArtAlbum {
            album: first.album.clone(),
            artist: if first.album_artist.is_empty() { first.artist.clone() } else { first.album_artist.clone() },
            total: items.len(),
            missing,
            source,
        });
    }
    out
}

/// 그림을 빌려줄 수 있는 앨범 (곡 파일에 그림이 든 앨범)
#[derive(Clone, Debug)]
pub struct Donor {
    pub album: String,
    pub artist: String,
    /// 그림이 든 곡 파일
    pub file: PathBuf,
}

/// 곡 파일에 그림이 든 앨범들 (앨범마다 한 곡)
pub fn donors(files: &[(AudioInfo, bool)]) -> Vec<Donor> {
    let mut seen: BTreeMap<(String, String), Donor> = BTreeMap::new();
    for (i, has) in files {
        if !has || i.album.trim().is_empty() {
            continue;
        }
        let artist = if i.album_artist.is_empty() { &i.artist } else { &i.album_artist };
        seen.entry((audio::normalize(&i.album), audio::normalize(artist)))
            .or_insert_with(|| Donor { album: i.album.clone(), artist: artist.clone(), file: i.path.clone() });
    }
    seen.into_values().collect()
}

/// 곡 파일 태그에 앞표지 그림을 넣는다 (기존 앞표지는 바꾼다). 다른 태그는 그대로 둔다.
pub fn embed(path: &Path, image: &[u8]) -> Result<(), String> {
    let mime = if image.starts_with(b"\x89PNG") {
        MimeType::Png
    } else if image.starts_with(&[0xFF, 0xD8]) {
        MimeType::Jpeg
    } else {
        return Err("JPEG·PNG 그림만 넣을 수 있습니다".into());
    };
    let mut file = lofty::read_from_path(path).map_err(|e| format!("파일을 읽지 못했습니다: {e}"))?;
    if file.primary_tag_mut().is_none() {
        let typ = file.primary_tag_type();
        file.insert_tag(Tag::new(typ));
    }
    let tag = file.primary_tag_mut().ok_or("태그를 만들 수 없는 형식입니다")?;
    tag.remove_picture_type(PictureType::CoverFront);
    tag.push_picture(Picture::unchecked(image.to_vec()).pic_type(PictureType::CoverFront).mime_type(mime).build());
    file.save_to_path(path, WriteOptions::default()).map_err(|e| format!("저장하지 못했습니다: {e}"))
}

/// 아트를 넣은 파일 기록 (되돌리기용). 넣기 전에는 모두 앞표지가 없던 파일이다.
#[derive(Serialize, Deserialize, Default)]
struct ArtLog {
    files: Vec<PathBuf>,
}

fn art_log_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("podlift-art-last-{}.json", std::process::id())));
    }
    dirs::config_dir().map(|d| d.join("Podlift").join("art-embed-last.json"))
}

/// 아트를 넣은 파일을 기록한다 (마지막 한 번만 되돌릴 수 있다)
pub fn record_embedded(files: &[PathBuf]) {
    let Some(p) = art_log_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, serde_json::to_string_pretty(&ArtLog { files: files.to_vec() }).unwrap_or_default());
}

/// 되돌릴 수 있는 마지막 아트 넣기의 파일 수
pub fn last_embedded() -> Option<usize> {
    let log: ArtLog = serde_json::from_str(&std::fs::read_to_string(art_log_path()?).ok()?).ok()?;
    (!log.files.is_empty()).then_some(log.files.len())
}

/// 마지막 아트 넣기를 되돌린다: 그때 넣은 앞표지를 뺀다. 되돌린 파일 수
pub fn undo_embedded() -> Result<usize, String> {
    let path = art_log_path().ok_or("기록 위치를 알 수 없습니다")?;
    let log: ArtLog = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|_| "되돌릴 기록이 없습니다".to_string())?).map_err(|e| e.to_string())?;
    let mut done = 0;
    let mut failed = Vec::new();
    for f in &log.files {
        let result = (|| -> Result<(), String> {
            let mut file = lofty::read_from_path(f).map_err(|e| e.to_string())?;
            if let Some(tag) = file.primary_tag_mut() {
                tag.remove_picture_type(PictureType::CoverFront);
            }
            file.save_to_path(f, WriteOptions::default()).map_err(|e| e.to_string())
        })();
        match result {
            Ok(()) => done += 1,
            Err(e) => failed.push(format!("{}: {e}", f.display())),
        }
    }
    let _ = std::fs::remove_file(&path);
    match failed.first() {
        Some(first) => Err(format!("{done}개는 되돌렸지만 {}개는 실패했습니다 ({first})", failed.len())),
        None => Ok(done),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(path: &Path, title: &str, artist: &str, album: &str, n: u32) -> AudioInfo {
        AudioInfo { path: path.to_path_buf(), title: title.into(), artist: artist.into(), album: album.into(), track: Some(n), tagged: true, ..Default::default() }
    }

    #[test]
    fn organize_and_undo() {
        let root = std::env::temp_dir().join(format!("podlift-org-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("mess/sub")).unwrap();
        let a = root.join("mess/x.mp3");
        let b = root.join("mess/sub/y.mp3");
        let c = root.join("mess/notag.mp3");
        for p in [&a, &b, &c] {
            std::fs::write(p, p.to_string_lossy().as_bytes()).unwrap();
        }
        std::fs::write(root.join("mess/sub/cover.jpg"), b"img").unwrap();
        let mut untagged = info(&c, "notag", "", "", 0);
        untagged.tagged = false;
        let infos = vec![info(&a, "Song A", "Artist", "Album", 1), info(&b, "Song B", "Artist", "Album", 2), untagged];
        let p = plan(&root, &infos);
        assert_eq!(p.moves.len(), 2);
        assert_eq!(p.untagged, vec![c.clone()]);
        // sub 폴더는 곡이 모두 떠나므로 그림을 옮긴다
        assert_eq!(p.images, vec![ImageOp::Move(root.join("mess/sub/cover.jpg"), root.join("Artist/Album/cover.jpg"))]);

        let out = run(&p, |_, _| {});
        assert_eq!((out.moved, out.images, out.failed.len()), (2, 1, 0));
        assert!(root.join("Artist/Album/01 Song A.mp3").exists());
        assert!(root.join("Artist/Album/02 Song B.mp3").exists());
        assert!(root.join("Artist/Album/cover.jpg").exists());
        assert!(!root.join("mess/sub").exists(), "빈 폴더는 지운다");
        assert!(c.exists());

        // 다시 계획하면 모두 제자리
        let again = vec![
            info(&root.join("Artist/Album/01 Song A.mp3"), "Song A", "Artist", "Album", 1),
            info(&root.join("Artist/Album/02 Song B.mp3"), "Song B", "Artist", "Album", 2),
        ];
        let p2 = plan(&root, &again);
        assert_eq!((p2.moves.len(), p2.in_place), (0, 2));

        assert_eq!(undo().unwrap(), 3);
        assert!(a.exists() && b.exists() && root.join("mess/sub/cover.jpg").exists());
        assert!(!root.join("Artist").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 실제 MP3 에 그림 심기. PODLIFT_SYNC_SRC=<low.mp3 가 든 폴더> 일 때만 돈다.
    #[test]
    fn embed_into_real_file() {
        let Ok(src) = std::env::var("PODLIFT_SYNC_SRC") else { return };
        let dir = std::env::temp_dir().join(format!("podlift-embed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.mp3");
        std::fs::copy(Path::new(&src).join("low.mp3"), &f).unwrap();
        let before = audio::read(&f, false);
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(300, 300, image::Rgb([200, 30, 30]))).write_to(&mut png, image::ImageFormat::Png).unwrap();
        embed(&f, png.get_ref()).unwrap();
        let after = audio::read(&f, true);
        assert_eq!(after.cover.as_ref().map(|c| (c.width, c.height)), Some((300, 300)));
        assert_eq!((after.title.clone(), after.artist.clone(), after.duration_ms / 1000), (before.title.clone(), before.artist.clone(), before.duration_ms / 1000));
        // 되돌리면 앞표지가 빠진다
        record_embedded(std::slice::from_ref(&f));
        assert_eq!(last_embedded(), Some(1));
        assert_eq!(undo_embedded().unwrap(), 1);
        assert!(audio::embedded_cover(&f).is_none());
        assert_eq!(audio::read(&f, false).title, before.title);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbered_copy_does_not_swap_names() {
        let root = Path::new("/m");
        let infos = vec![
            info(&root.join("A/B/Song (2).m4a"), "Song", "A", "B", 0),
            info(&root.join("A/B/Song.m4a"), "Song", "A", "B", 0),
        ];
        let mut infos = infos;
        for i in &mut infos {
            i.track = None;
        }
        let p = plan(root, &infos);
        assert_eq!((p.moves.len(), p.in_place), (0, 2));
    }

    #[test]
    fn guard_refuses_music_library() {
        let root = std::env::temp_dir().join(format!("podlift-guard-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Music Library.musiclibrary")).unwrap();
        std::fs::create_dir_all(root.join("Media/Music")).unwrap();
        assert!(guard(&root.join("Media/Music")).is_err());
        assert!(guard(&std::env::temp_dir()).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn art_plan_prefers_sibling_then_folder() {
        let dir = std::env::temp_dir().join(format!("podlift-artplan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("folder.jpg"), b"x").unwrap();
        let f = |n: &str, album: &str, has: bool| (info(&dir.join(n), n, "A", album, 1), has);
        let albums = plan_art(&[f("1.mp3", "One", true), f("2.mp3", "One", false), f("3.mp3", "Two", false), f("4.mp3", "Full", true)]);
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].album, "One");
        assert_eq!(albums[0].source, Some(ArtSource::Sibling(dir.join("1.mp3"))));
        assert_eq!(albums[1].source, Some(ArtSource::FolderImage(dir.join("folder.jpg"))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
