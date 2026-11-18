//! 아이팟에 이미 있지만 앨범 아트가 없는 곡에 그림을 채운다.
//!
//! 그림 출처: 같은 앨범의 다른 곡이 아이팟에서 쓰는 그림(새로 저장하지 않고 같이 씀) →
//! 곡 파일 태그에 든 그림 → 온라인 검색(artsearch.rs) 또는 사용자가 고른 그림 파일. 저장 방식·되돌리기는 넣기(sync.rs)와 같다.
use crate::artworkdb::{self, ArtworkDb};
use crate::audio;
use crate::itunesdb::DbTrack;
use crate::sync::{self, LastJob, LAST_JOB};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

#[derive(Clone, Debug, PartialEq)]
pub enum ArtSource {
    /// 아이팟에 이미 있는 그림 (그림 번호)
    Existing(u32),
    /// 곡 파일에 든 그림 (아이팟 안 곡 파일 경로)
    Embedded(PathBuf),
    /// 사용자가 고른 그림 파일
    Picked(PathBuf),
    /// 온라인에서 찾아 받아 둔 그림 파일
    Online(PathBuf),
}

impl ArtSource {
    pub fn label(&self) -> &'static str {
        match self {
            ArtSource::Existing(_) => "아이팟에 있는 같은 앨범 그림",
            ArtSource::Embedded(_) => "곡 파일에 든 그림",
            ArtSource::Picked(_) => "직접 고른 그림",
            ArtSource::Online(_) => "온라인에서 찾은 그림",
        }
    }

    /// 새로 저장할 그림 바이트 (이미 있는 그림이면 None)
    pub fn bytes(&self) -> Option<Vec<u8>> {
        match self {
            ArtSource::Existing(_) => None,
            ArtSource::Embedded(p) => audio::embedded_cover(p),
            ArtSource::Picked(p) | ArtSource::Online(p) => std::fs::read(p).ok(),
        }
    }
}

/// 아트가 없는 곡을 앨범별로 묶은 것
#[derive(Clone, Debug)]
pub struct Need {
    pub album: String,
    pub artist: String,
    pub track_ids: Vec<u32>,
    pub source: Option<ArtSource>,
    /// 다른 앨범의 그림을 골랐으면 그 앨범 이름
    pub borrowed: Option<String>,
}

/// 그림을 빌려줄 수 있는 앨범
#[derive(Clone, Debug)]
pub struct Donor {
    pub album: String,
    pub artist: String,
    pub source: ArtSource,
}

/// 아이팟에서 그림을 빌려줄 수 있는 앨범: 아이팟에 그림이 있는 앨범 + 곡 파일에 그림이 든 앨범(needs 결과)
pub fn donors(tracks: &[DbTrack], needs: &[Need]) -> Vec<Donor> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for t in tracks.iter().filter(|t| t.has_artwork && t.mhii_link != 0 && !t.album.is_empty()) {
        if seen.insert(sync::art_album_key(&t.album, album_artist(t))) {
            out.push(Donor { album: t.album.clone(), artist: album_artist(t).to_string(), source: ArtSource::Existing(t.mhii_link) });
        }
    }
    for n in needs.iter().filter(|n| !n.album.is_empty() && n.borrowed.is_none()) {
        if let Some(src @ (ArtSource::Embedded(_) | ArtSource::Existing(_))) = &n.source {
            if seen.insert(sync::art_album_key(&n.album, &n.artist)) {
                out.push(Donor { album: n.album.clone(), artist: n.artist.clone(), source: src.clone() });
            }
        }
    }
    out
}

fn album_artist(t: &DbTrack) -> &str {
    if t.album_artist.is_empty() { &t.artist } else { &t.album_artist }
}

/// 아트가 없는 곡을 찾아 앨범별로 묶고 그림 출처를 정한다 (곡 파일을 읽으므로 몇 초 걸릴 수 있다)
pub fn needs(mount: &Path, tracks: &[DbTrack]) -> Vec<Need> {
    let mut existing: HashMap<String, u32> = HashMap::new();
    for t in tracks.iter().filter(|t| t.has_artwork && t.mhii_link != 0 && !t.album.is_empty()) {
        existing.entry(sync::art_album_key(&t.album, album_artist(t))).or_insert(t.mhii_link);
    }
    let mut groups: BTreeMap<String, Vec<&DbTrack>> = BTreeMap::new();
    for t in tracks.iter().filter(|t| !t.has_artwork || t.mhii_link == 0) {
        // 앨범이 없는 곡은 곡마다 따로
        let key = if t.album.is_empty() { format!("\u{0}{}", t.id) } else { sync::art_album_key(&t.album, album_artist(t)) };
        groups.entry(key).or_default().push(t);
    }
    groups
        .into_iter()
        .map(|(key, ts)| {
            let source = existing.get(&key).map(|&id| ArtSource::Existing(id)).or_else(|| {
                ts.iter().filter_map(|t| t.file_path(mount)).find(|p| audio::embedded_cover(p).is_some()).map(ArtSource::Embedded)
            });
            Need { album: ts[0].album.clone(), artist: album_artist(ts[0]).to_string(), track_ids: ts.iter().map(|t| t.id).collect(), source, borrowed: None }
        })
        .collect()
}

pub struct Job {
    rx: Receiver<Result<usize, String>>,
}

impl Job {
    pub fn poll(&self) -> Option<Result<usize, String>> {
        self.rx.try_recv().ok()
    }
}

/// 백그라운드에서 그림을 채운다. work: (곡 번호들, 그림 출처)
pub fn start(mount: PathBuf, work: Vec<(Vec<u32>, ArtSource)>, fwid: Option<Vec<u8>>) -> Job {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run(&mount, &work, fwid.as_deref()));
    });
    Job { rx }
}

/// 그림을 붙인 곡 수를 돌려준다
pub fn run(mount: &Path, work: &[(Vec<u32>, ArtSource)], fwid: Option<&[u8]>) -> Result<usize, String> {
    let mut db = sync::open_db(mount)?;
    if db.hashing_scheme() == 1 && fwid.is_none() {
        return Err("이 아이팟의 고유 번호(FirewireGuid)를 찾지 못해 곡 DB 에 서명할 수 없습니다. 아이팟을 다시 연결해 보세요.".into());
    }
    let tracks = db.tracks();
    let by_id: HashMap<u32, &DbTrack> = tracks.iter().map(|t| (t.id, t)).collect();
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let itunes = sync::itunes_dir(mount);
    let backup = format!("iTunesDB.podlift-{stamp}");
    std::fs::copy(sync::db_path(mount), itunes.join(&backup)).map_err(|e| format!("DB 백업에 실패해 멈췄습니다: {e}"))?;
    let _ = std::fs::remove_file(itunes.join(LAST_JOB));
    let mut job = LastJob { backup, ..Default::default() };

    // 그림 DB 에 새 그림을 덧붙이고 (곡, 그림 번호, 원본 크기) 연결 목록을 만든다
    let mut links: Vec<(u32, u32, u32)> = Vec::new();
    let mut art: Option<ArtworkDb> = None;
    for (ids, source) in work {
        let ids: Vec<u32> = ids.iter().copied().filter(|id| by_id.contains_key(id)).collect();
        let Some(first) = ids.first().and_then(|id| by_id.get(id)) else { continue };
        let (image_id, size) = match source {
            ArtSource::Existing(id) => (*id, 1),
            _ => {
                let bytes = source.bytes().ok_or("그림을 읽지 못했습니다")?;
                if art.is_none() {
                    art = Some(ArtworkDb::open(mount)?);
                }
                let key = sync::art_album_key(&first.album, album_artist(first));
                (art.as_mut().unwrap().add(&bytes, &key, first.dbid)?, bytes.len() as u32)
            }
        };
        links.extend(ids.iter().map(|&id| (id, image_id, size)));
    }
    if let Some(mut a) = art.filter(|a| a.has_pending()) {
        let art_db = artworkdb::db_path(mount);
        if art_db.is_file() {
            let name = format!("ArtworkDB.podlift-{stamp}");
            std::fs::copy(&art_db, artworkdb::artwork_dir(mount).join(&name)).map_err(|e| format!("ArtworkDB 백업 실패: {e}"))?;
            job.art_backup = Some(name);
        }
        job.ithmb = a.commit(mount)?;
        job.art_changed = true;
    }
    let mut n = 0;
    for (track_id, image_id, size) in links {
        if db.set_artwork(track_id, image_id, size).is_ok() {
            n += 1;
        }
    }
    match sync::write_db(mount, &mut db, fwid) {
        Ok(sha) => job.written = Some(sha),
        Err(e) => {
            sync::undo_artwork(mount, &job);
            return Err(e);
        }
    }
    let _ = std::fs::write(itunes.join(LAST_JOB), serde_json::to_string_pretty(&job).unwrap_or_default());
    Ok(n)
}

/// '한 번에 채우기' 진행 소식
pub enum AutoMsg {
    /// 지금 하는 일 (화면에 그대로 보여 준다)
    Stage(String),
    Done(Result<AutoOutcome, String>),
}

#[derive(Debug, Default)]
pub struct AutoOutcome {
    /// 그림을 붙인 곡 수
    pub filled: usize,
    /// 그림을 채운 앨범 수 (곡 파일·아이팟 그림 / 온라인)
    pub from_file: usize,
    pub online: usize,
    /// 그림을 찾지 못한 앨범 ("앨범 - 가수")
    pub missing: Vec<String>,
}

pub struct AutoJob {
    rx: Receiver<AutoMsg>,
}

impl AutoJob {
    pub fn poll(&self) -> Vec<AutoMsg> {
        self.rx.try_iter().collect()
    }
}

/// 아트 없는 곡을 한 번에 채운다: 같은 앨범의 아이팟 그림 → 곡 파일 속 그림 → 온라인 검색(online 이면).
/// 찾지 못한 앨범은 건너뛰고 알려 준다.
pub fn start_auto(mount: PathBuf, fwid: Option<Vec<u8>>, online: bool) -> AutoJob {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let send = |m: AutoMsg| {
            let _ = tx.send(m);
        };
        let result = (|| {
            send(AutoMsg::Stage("아이팟 곡 DB 를 읽는 중…".into()));
            let tracks = sync::open_db(&mount)?.tracks();
            send(AutoMsg::Stage("아트 없는 곡의 파일에서 그림을 찾는 중… (곡이 많으면 몇 분 걸립니다)".into()));
            let needs = needs(&mount, &tracks);
            let mut out = AutoOutcome::default();
            let mut work: Vec<(Vec<u32>, ArtSource)> = Vec::new();
            let lacking: Vec<&Need> = needs.iter().filter(|n| n.source.is_none()).collect();
            for n in needs.iter().filter(|n| n.source.is_some()) {
                out.from_file += 1;
                work.push((n.track_ids.clone(), n.source.clone().unwrap()));
            }
            for (i, n) in lacking.iter().enumerate() {
                let found = if online && !n.album.trim().is_empty() {
                    send(AutoMsg::Stage(format!("그림이 없는 앨범을 온라인에서 찾는 중… {} / {}", i + 1, lacking.len())));
                    crate::artsearch::search(&n.album, &n.artist).ok().flatten()
                } else {
                    None
                };
                match found {
                    Some(f) => {
                        out.online += 1;
                        work.push((n.track_ids.clone(), ArtSource::Online(f.file)));
                    }
                    None => out.missing.push(format!("{} - {}", if n.album.is_empty() { "(앨범 없음)" } else { &n.album }, n.artist)),
                }
            }
            if work.is_empty() {
                return Ok(out);
            }
            send(AutoMsg::Stage(format!("앨범 {}개의 그림을 아이팟에 저장하는 중… 아이팟을 빼지 마세요.", work.len())));
            out.filled = run(&mount, &work, fwid.as_deref())?;
            Ok(out)
        })();
        send(AutoMsg::Done(result));
    });
    AutoJob { rx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::itunesdb::{tests::empty_db, ITunesDb, NewTrack};

    fn png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(200, 200, image::Rgb([10, 200, 10]))).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn fill_then_restore() {
        let mount = std::env::temp_dir().join(format!("podlift-art-fill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&mount);
        std::fs::create_dir_all(mount.join("iPod_Control/iTunes")).unwrap();
        std::fs::create_dir_all(mount.join("iPod_Control/Device")).unwrap();
        std::fs::write(mount.join("iPod_Control/Device/SysInfo"), "ModelNumStr: xB029\n").unwrap();
        let mut db = ITunesDb::parse(&empty_db(false)).unwrap();
        let song = |t: &str, album: &str| NewTrack { location: format!(":iPod_Control:Music:F00:{t}.mp3"), ext: "mp3".into(), title: t.into(), album: album.into(), artist: "A".into(), ..Default::default() };
        let a = db.add_track(&song("one", "Album")).unwrap();
        let b = db.add_track(&song("two", "Album")).unwrap();
        let c = db.add_track(&song("solo", "")).unwrap();
        sync::write_db(&mount, &mut db, None).unwrap();
        let before = std::fs::read(sync::db_path(&mount)).unwrap();

        let tracks = sync::open_db(&mount).unwrap().tracks();
        let n = needs(&mount, &tracks);
        assert_eq!(n.len(), 2); // Album 묶음, 앨범 없는 곡
        assert!(n.iter().all(|x| x.source.is_none())); // 파일이 없으니 그림도 없다

        let pic = mount.join("cover.png");
        std::fs::write(&pic, png()).unwrap();
        let filled = run(&mount, &[(vec![a, b], ArtSource::Picked(pic.clone())), (vec![c], ArtSource::Picked(pic))], None).unwrap();
        assert_eq!(filled, 3);
        let after = sync::open_db(&mount).unwrap().tracks();
        assert!(after.iter().all(|t| t.has_artwork && t.mhii_link != 0));
        assert_eq!(after[0].mhii_link, after[1].mhii_link, "같은 앨범은 그림 하나를 같이 쓴다");
        assert!(needs(&mount, &after).is_empty());

        sync::restore_last(&mount).unwrap();
        assert_eq!(std::fs::read(sync::db_path(&mount)).unwrap(), before);

        // 한 번에 채우기: 곡 파일이 없으니 그림을 못 찾고(온라인 끔) 건너뛴 앨범으로 알려 준다
        let job = start_auto(mount.clone(), None, false);
        let out = loop {
            if let Some(AutoMsg::Done(r)) = job.poll().into_iter().find(|m| matches!(m, AutoMsg::Done(_))) {
                break r.unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!((out.filled, out.from_file, out.missing.len()), (0, 0, 2));
        assert!(!artworkdb::db_path(&mount).exists());
        let _ = std::fs::remove_dir_all(&mount);
    }
}
