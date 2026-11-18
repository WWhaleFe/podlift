//! 아이팟에 곡 넣기. iTunes·Finder 없이 파일을 iPod_Control/Music/Fxx 에 복사하고
//! iTunesDB 에 직접 등록한다.
//!
//! 순서: DB 백업 → (필요하면 변환) → 복사 → DB 에 등록 → 서명해 한 번에 쓰기.
//! - 같은 곡(앨범·제목이 같음)이 아이팟에 있으면 음질을 비교해, 넣을 곡이 분명히 나을 때만
//!   기존 곡 레코드의 파일을 바꾼다(재생 횟수·별점·재생 목록은 유지).
//! - 바꿔 넣은 옛 파일은 지우지 않고 iPod_Control/Music/.podlift-old/ 로 옮겨 두어,
//!   '되돌리기'로 마지막 작업 전 상태로 완전히 돌아갈 수 있다. 다음 작업을 시작할 때 비운다.
//! - 아이팟 클래식이 못 읽는 형식(FLAC·OGG 등)과 고해상도 무손실(48kHz·16bit 초과)은
//!   macOS 내장 afconvert 로 바꾸고 태그를 옮겨 붙인다.
//! - 앨범 아트(곡에 든 그림, 없으면 같은 폴더의 cover.jpg 등)는 그림 DB(artworkdb.rs)에 덧붙이고 곡에 연결한다.
//!   아이팟에 같은 앨범 그림이 이미 있으면 그것을 같이 쓴다. 그림을 못 넣어도 곡은 넣는다.
use crate::audio::{self, AudioInfo};
use crate::artworkdb::{self, ArtworkDb, IthmbMark};
use crate::itunesdb::{DbTrack, ITunesDb, NewTrack};
use lofty::config::WriteOptions;
use lofty::prelude::*;
use lofty::tag::TagType;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

/// 아이팟 클래식이 그대로 재생하는 확장자
const DIRECT_EXT: &[&str] = &["mp3", "m4a", "m4b", "wav", "aif", "aiff"];
/// 넣을 곡으로 받는 확장자 (나머지는 변환)
pub const INPUT_EXT: &[&str] = &["mp3", "m4a", "m4b", "m4p", "aac", "wav", "aif", "aiff", "aifc", "flac", "ogg", "opus"];

pub(crate) const OLD_DIR: &str = ".podlift-old";
pub(crate) const LAST_JOB: &str = "podlift-last-job.json";
/// 아이팟이 재생할 때 쓰는 재생 횟수 파일 (곡 목록 순서대로 한 항목씩)
pub(crate) const PLAY_COUNTS: &str = "Play Counts";

pub fn itunes_dir(mount: &Path) -> PathBuf {
    mount.join("iPod_Control").join("iTunes")
}

pub(crate) fn db_path(mount: &Path) -> PathBuf {
    itunes_dir(mount).join("iTunesDB")
}

/// 아이팟 DB 를 읽는다 (읽기 전용). 쓰기를 막아야 할 사정이 있으면 Err.
pub fn open_db(mount: &Path) -> Result<ITunesDb, String> {
    let dir = itunes_dir(mount);
    if dir.join("iTunesCDB").exists() {
        return Err("압축 DB(iTunesCDB)를 쓰는 기기입니다. 아이팟 클래식만 지원합니다".into());
    }
    let data = std::fs::read(db_path(mount)).map_err(|e| format!("iTunesDB 를 읽지 못했습니다: {e}"))?;
    let db = ITunesDb::parse(&data)?;
    if db.hashing_scheme() > 1 {
        return Err("서명 방식이 아이팟 클래식과 달라 쓸 수 없습니다 (나노 5세대 이후·터치 등)".into());
    }
    Ok(db)
}

// ── 넣을 곡 판정 ───────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// 아이팟에 없는 곡
    New,
    /// 아이팟에 있지만 이 파일이 음질이 분명히 낫다 (바꿀 곡 번호)
    Better(u32),
    /// 아이팟에 같거나 더 나은 것이 있다 (겹치는 아이팟 곡 번호)
    Exists(u32),
    /// 넣을 목록 안에 더 나은 같은 곡이 있다 (그 곡의 목록 위치)
    QueueDup(usize),
    /// 넣을 수 없음 (이유)
    Unsupported(String),
}

impl Decision {
    /// 기본으로 넣을지
    pub fn default_include(&self) -> bool {
        matches!(self, Decision::New | Decision::Better(_))
    }
}

/// 변환 방법
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Convert {
    /// 무손실 → ALAC 16bit (샘플레이트)
    Alac(u32),
    /// 손실 → AAC 256kbps
    Aac,
}

/// 아이팟에 넣으려면 변환이 필요한지. Err 면 넣을 수 없음.
pub fn conversion(info: &AudioInfo) -> Result<Option<Convert>, String> {
    let ext = ext_lower(&info.path);
    if ext == "m4p" {
        return Err("iTunes Store 보호 곡(DRM)은 넣을 수 없습니다".into());
    }
    let target_rate = |r: u32| if r <= 48000 && r > 0 { r } else if r % 44100 == 0 { 44100 } else { 48000 };
    if info.lossless && (info.sample_rate > 48000 || info.bit_depth > 16) {
        return Ok(Some(Convert::Alac(target_rate(info.sample_rate))));
    }
    if DIRECT_EXT.contains(&ext.as_str()) {
        return Ok(None);
    }
    Ok(Some(if info.lossless { Convert::Alac(target_rate(info.sample_rate)) } else { Convert::Aac }))
}

/// 변환까지 마친 뒤의 음질 (비교용)
fn effective(info: &AudioInfo) -> AudioInfo {
    let mut e = info.clone();
    match conversion(info) {
        Ok(Some(Convert::Alac(rate))) => {
            e.lossless = true;
            e.codec = "ALAC";
            e.sample_rate = rate;
            e.bit_depth = 16;
        }
        Ok(Some(Convert::Aac)) => {
            e.lossless = false;
            e.codec = "AAC";
            e.bitrate_kbps = 256;
        }
        _ => {}
    }
    e
}

/// 아이팟 곡을 비교용 AudioInfo 로
fn db_as_info(t: &DbTrack) -> AudioInfo {
    AudioInfo {
        title: t.title.clone(),
        album: t.album.clone(),
        artist: t.artist.clone(),
        album_artist: t.album_artist.clone(),
        tagged: !t.title.is_empty(),
        lossless: t.lossless,
        bitrate_kbps: t.bitrate,
        sample_rate: t.sample_rate,
        bit_depth: if t.lossless { 16 } else { 0 },
        duration_ms: t.length_ms as u64,
        ..Default::default()
    }
}

/// 넣을 곡 목록마다 판정을 내린다. 같은 곡끼리는 목록 안에서 가장 나은 것 하나만 남긴다.
/// 같은 곡: 앨범+제목이 같거나, 제목+가수가 같고 길이가 10초 넘게 다르지 않음 (audio::same_song_groups).
pub fn decide(queue: &[AudioInfo], ipod: &[DbTrack]) -> Vec<Decision> {
    let ipod_infos: Vec<AudioInfo> = ipod.iter().map(db_as_info).collect();
    let mut by_album: HashMap<String, usize> = HashMap::new();
    let mut by_artist: HashMap<String, usize> = HashMap::new();
    for (i, info) in ipod_infos.iter().enumerate() {
        if let Some(k) = audio::same_song_key(info) {
            by_album.entry(k).or_insert(i);
        }
        if let Some(k) = audio::artist_title_key(info) {
            by_artist.entry(k).or_insert(i);
        }
    }
    let effective: Vec<AudioInfo> = queue.iter().map(effective).collect();
    let mut out: Vec<Decision> = queue
        .iter()
        .map(|i| match conversion(i) {
            Err(e) => Decision::Unsupported(e),
            Ok(_) => Decision::New,
        })
        .collect();
    let candidates: Vec<usize> = (0..queue.len()).filter(|&i| out[i] == Decision::New).collect();
    let refs: Vec<&AudioInfo> = candidates.iter().map(|&i| &queue[i]).collect();
    let mut groups: Vec<Vec<usize>> = audio::same_song_groups(&refs).into_iter().map(|g| g.into_iter().map(|j| candidates[j]).collect()).collect();
    // 목록 안에 짝이 없는 곡도 아이팟의 곡과는 비교한다
    let grouped: std::collections::HashSet<usize> = groups.iter().flatten().copied().collect();
    groups.extend(candidates.iter().filter(|i| !grouped.contains(i)).map(|&i| vec![i]));
    for members in groups {
        let refs: Vec<&AudioInfo> = members.iter().map(|&i| &effective[i]).collect();
        let (best_pos, _) = audio::pick_best(&refs);
        let best = members[best_pos];
        for &i in &members {
            if i != best {
                out[i] = Decision::QueueDup(best);
            }
        }
        let info = &queue[best];
        let close = |j: &usize| ipod_infos[*j].duration_ms.abs_diff(info.duration_ms) <= 10_000 || ipod_infos[*j].duration_ms == 0;
        let same_recording = |j: &usize| {
            audio::same_artist(info, &ipod_infos[*j]) || (info.duration_ms > 0 && ipod_infos[*j].duration_ms.abs_diff(info.duration_ms) <= 3_000)
        };
        let existing = audio::same_song_key(info)
            .and_then(|k| by_album.get(&k).copied())
            .filter(same_recording)
            .or_else(|| audio::artist_title_key(info).and_then(|k| by_artist.get(&k).copied()).filter(close));
        if let Some(j) = existing {
            let better = audio::compare_quality(&effective[best], &ipod_infos[j]) == Some(std::cmp::Ordering::Greater);
            out[best] = if better { Decision::Better(ipod[j].id) } else { Decision::Exists(ipod[j].id) };
        }
    }
    out
}

// ── 작업 ───────────────────────────────────────────────

pub struct Item {
    /// 화면 목록의 위치
    pub index: usize,
    pub info: AudioInfo,
    /// None = 새 곡, Some(번호) = 그 곡의 파일을 바꿈
    pub replace: Option<u32>,
}

pub enum SyncEvent {
    Backup(PathBuf),
    Begin(usize),
    Done(usize, Result<(), String>),
    /// 모두 끝남: 새로 넣음, 바꿈, 앨범 아트를 붙인 곡 수, 멈춘 이유 또는 오류, 곡은 넣었지만 알릴 것
    Finished { added: usize, replaced: usize, art: usize, error: Option<String>, note: Option<String> },
}

pub struct SyncJob {
    rx: Receiver<SyncEvent>,
    cancel: Arc<AtomicBool>,
}

impl SyncJob {
    pub fn poll(&self) -> Vec<SyncEvent> {
        self.rx.try_iter().collect()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// 마지막 작업 기록. '되돌리기'가 쓴다.
#[derive(Serialize, Deserialize, Default)]
pub(crate) struct LastJob {
    /// iTunes 폴더 안 백업 파일 이름
    pub backup: String,
    /// 새로 넣은 파일 (마운트 기준 상대 경로)
    pub added: Vec<String>,
    /// (옛 파일 원래 자리, 옮겨 둔 자리) 마운트 기준 상대 경로
    pub moved_old: Vec<(String, String)>,
    /// iTunes 폴더 안 재생 횟수 파일 백업 이름 (중복 정리에서 곡을 뺐을 때)
    #[serde(default)]
    pub playcounts_backup: Option<String>,
    /// 이 작업이 쓴 곡 DB 의 SHA-1. 지금 DB 가 이것과 다르면(Finder 동기화 등) 되돌리지 않는다.
    #[serde(default)]
    pub written: Option<String>,
    /// 그림 DB 를 고쳤으면 true
    #[serde(default)]
    pub art_changed: bool,
    /// Artwork 폴더 안 그림 DB 백업 이름 (None = 작업 전에 그림 DB 가 없었음)
    #[serde(default)]
    pub art_backup: Option<String>,
    /// 덧붙이기 전 ithmb 길이
    #[serde(default)]
    pub ithmb: Vec<IthmbMark>,
}

pub fn start(mount: PathBuf, items: Vec<Item>, firewire_id: Option<Vec<u8>>) -> SyncJob {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    std::thread::spawn(move || {
        let result = run(&mount, items, firewire_id.as_deref(), &stop, &|e| {
            let _ = tx.send(e);
        });
        let finished = match result {
            Ok(o) => SyncEvent::Finished {
                added: o.added,
                replaced: o.replaced,
                art: o.art,
                error: o.stopped.then(|| "중지했습니다. 그때까지 넣은 곡은 아이팟에 등록했습니다.".to_string()),
                note: o.note,
            },
            Err(e) => SyncEvent::Finished { added: 0, replaced: 0, art: 0, error: Some(e), note: None },
        };
        let _ = tx.send(finished);
    });
    SyncJob { rx, cancel }
}

#[derive(Default)]
struct RunOutcome {
    added: usize,
    replaced: usize,
    art: usize,
    stopped: bool,
    note: Option<String>,
}

/// 넣은 곡 하나 (앨범 아트를 붙일 때 쓴다)
struct Placed {
    track_id: u32,
    info: AudioInfo,
    /// 기존 곡의 파일을 바꾼 것이면 true (그 곡에 이미 붙은 그림은 그대로 쓸 수 있다)
    replaced: bool,
}

fn run(mount: &Path, items: Vec<Item>, fwid: Option<&[u8]>, stop: &AtomicBool, send: &dyn Fn(SyncEvent)) -> Result<RunOutcome, String> {
    let fail = |e: String| Err(e);
    let mut db = match open_db(mount) {
        Ok(db) => db,
        Err(e) => return fail(e),
    };
    if db.hashing_scheme() == 1 && fwid.is_none() {
        return fail("이 아이팟의 고유 번호(FirewireGuid)를 찾지 못해 곡 DB 에 서명할 수 없습니다. 아이팟을 다시 연결해 보세요.".into());
    }

    // 1) 백업과 지난 작업 정리
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let backup_name = format!("iTunesDB.podlift-{stamp}");
    let itunes = itunes_dir(mount);
    if let Err(e) = std::fs::copy(db_path(mount), itunes.join(&backup_name)) {
        return fail(format!("DB 백업에 실패해 멈췄습니다: {e}"));
    }
    send(SyncEvent::Backup(itunes.join(&backup_name)));
    let music = mount.join("iPod_Control").join("Music");
    let old_dir = music.join(OLD_DIR);
    let _ = std::fs::remove_dir_all(&old_dir);
    let _ = std::fs::remove_file(itunes.join(LAST_JOB));

    let fdirs = music_dirs(&music);
    let tmp = std::env::temp_dir().join(format!("podlift-convert-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let mut job = LastJob { backup: backup_name, ..Default::default() };
    let mut out = RunOutcome::default();
    let mut placed: Vec<Placed> = Vec::new();
    // DB 를 쓴 뒤에 옮길 옛 파일 (원래 자리)
    let mut to_move: Vec<PathBuf> = Vec::new();
    let mut copied: Vec<PathBuf> = Vec::new();

    // 2) 곡마다 변환·복사·등록
    for item in items {
        if stop.load(Ordering::Relaxed) {
            out.stopped = true;
            break;
        }
        send(SyncEvent::Begin(item.index));
        match put_one(mount, &fdirs, &tmp, &item, &mut db) {
            Ok((dest, old, track_id)) => {
                if item.replace.is_some() {
                    to_move.extend(old);
                    out.replaced += 1;
                } else {
                    out.added += 1;
                }
                job.added.push(rel(mount, &dest));
                copied.push(dest);
                placed.push(Placed { track_id, info: item.info, replaced: item.replace.is_some() });
                send(SyncEvent::Done(item.index, Ok(())));
            }
            Err(e) => send(SyncEvent::Done(item.index, Err(e))),
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    if out.added + out.replaced == 0 {
        return Ok(out);
    }

    // 3) 앨범 아트: 그림 DB 에 덧붙이고 곡에 연결한다. 실패해도 곡은 넣는다.
    match add_artwork(mount, &stamp, &mut db, &placed, &mut job) {
        Ok(n) => out.art = n,
        Err(e) => out.note = Some(format!("앨범 아트는 넣지 못했습니다: {e}")),
    }

    // 4) DB 쓰기: 새로 만든 바이트를 다시 읽어 확인한 뒤 임시 이름으로 쓰고 바꿔 끼운다.
    let written = write_db(mount, &mut db, fwid);
    if let Ok(sha) = &written {
        job.written = Some(sha.clone());
    }
    if let Err(e) = written {
        // 등록하지 못한 파일은 지워 아이팟에 떠도는 파일을 남기지 않는다. DB 는 원본 그대로다.
        for f in &copied {
            let _ = std::fs::remove_file(f);
        }
        undo_artwork(mount, &job);
        return Err(e);
    }

    // 5) 바꿔 넣은 곡의 옛 파일을 치워 두고 작업 기록을 남긴다
    for old in to_move {
        if let Some(name) = old.file_name() {
            let _ = std::fs::create_dir_all(&old_dir);
            let parked = old_dir.join(name);
            if std::fs::rename(&old, &parked).is_ok() {
                job.moved_old.push((rel(mount, &old), rel(mount, &parked)));
            }
        }
    }
    let _ = std::fs::write(itunes.join(LAST_JOB), serde_json::to_string_pretty(&job).unwrap_or_default());
    Ok(out)
}

/// DB 쓰기: 새로 만든 바이트를 다시 읽어 확인한 뒤 임시 이름으로 쓰고 바꿔 끼운다. 쓴 바이트의 SHA-1 을 돌려준다.
pub(crate) fn write_db(mount: &Path, db: &mut ITunesDb, fwid: Option<&[u8]>) -> Result<String, String> {
    let expected = db.tracks().len();
    let bytes = db.to_bytes(fwid)?;
    let check = ITunesDb::parse(&bytes).map_err(|e| format!("새 DB 확인 실패: {e}"))?;
    if check.tracks().len() != expected {
        return Err("새 DB 의 곡 수가 맞지 않습니다".into());
    }
    let tmp_db = itunes_dir(mount).join("iTunesDB.podlift-writing");
    std::fs::write(&tmp_db, &bytes).map_err(|e| format!("DB 쓰기 실패: {e}"))?;
    if let Ok(f) = std::fs::File::open(&tmp_db) {
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp_db, db_path(mount)).map_err(|e| format!("DB 바꿔 끼우기 실패: {e}"))?;
    Ok(sha1_hex(&bytes))
}

fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    Sha1::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// 마지막 작업 뒤에 다른 프로그램(Finder·Music 동기화 등)이 곡 DB 를 바꿨는지.
/// 지문이 있으면 지문으로, 없는 옛 기록은 DB 가 기록 파일보다 나중에 바뀌었는지로 본다.
fn db_changed_since(mount: &Path, job: &LastJob) -> bool {
    match &job.written {
        Some(sha) => std::fs::read(db_path(mount)).map(|b| &sha1_hex(&b) != sha).unwrap_or(true),
        None => {
            let modified = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            match (modified(db_path(mount)), modified(itunes_dir(mount).join(LAST_JOB))) {
                (Some(db), Some(rec)) => db.duration_since(rec).is_ok_and(|d| d.as_secs() > 5),
                _ => true,
            }
        }
    }
}

/// 넣은 곡들의 앨범 아트를 그림 DB 에 넣고 곡 DB 에 연결한다. 연결한 곡 수를 돌려준다.
/// 그림 DB·ithmb 를 고쳤으면 되돌리기 정보를 job 에 남긴다.
fn add_artwork(mount: &Path, stamp: &str, db: &mut ITunesDb, placed: &[Placed], job: &mut LastJob) -> Result<usize, String> {
    // 아이팟에 이미 그림이 있는 앨범 (앨범 열쇠 → 그림 번호)
    let tracks = db.tracks();
    // 이번에 새로 넣은 곡은 아직 그림이 없으니 빼고 본다 (바꿔 넣은 곡의 기존 그림은 쓴다)
    let new_ids: std::collections::HashSet<u32> = placed.iter().filter(|p| !p.replaced).map(|p| p.track_id).collect();
    let mut existing: HashMap<String, u32> = HashMap::new();
    for t in &tracks {
        if t.has_artwork && t.mhii_link != 0 && !new_ids.contains(&t.id) && !t.album.is_empty() {
            let artist = if t.album_artist.is_empty() { &t.artist } else { &t.album_artist };
            existing.entry(art_album_key(&t.album, artist)).or_insert(t.mhii_link);
        }
    }
    let dbids: HashMap<u32, u64> = tracks.iter().map(|t| (t.id, t.dbid)).collect();

    let mut links: Vec<(u32, u32, u32)> = Vec::new(); // (곡 번호, 그림 번호, 원본 그림 크기)
    let mut art: Option<ArtworkDb> = None;
    for p in placed {
        let artist = if p.info.album_artist.is_empty() { &p.info.artist } else { &p.info.album_artist };
        let key = art_album_key(&p.info.album, artist);
        let cover = audio::cover_bytes(&p.info.path);
        if !p.info.album.is_empty() {
            if let Some(&image_id) = existing.get(&key) {
                links.push((p.track_id, image_id, cover.as_ref().map_or(0, |c| c.len() as u32)));
                continue;
            }
        }
        let Some(cover) = cover else { continue };
        if art.is_none() {
            art = Some(ArtworkDb::open(mount)?);
        }
        let image_id = art.as_mut().unwrap().add(&cover, &key, dbids.get(&p.track_id).copied().unwrap_or(0))?;
        links.push((p.track_id, image_id, cover.len() as u32));
    }

    if let Some(mut art) = art.filter(|a| a.has_pending()) {
        // 그림 DB 백업 후 덧붙이기
        let art_db = artworkdb::db_path(mount);
        if art_db.is_file() {
            let name = format!("ArtworkDB.podlift-{stamp}");
            std::fs::copy(&art_db, artworkdb::artwork_dir(mount).join(&name)).map_err(|e| format!("ArtworkDB 백업 실패: {e}"))?;
            job.art_backup = Some(name);
        }
        job.ithmb = art.commit(mount)?;
        job.art_changed = true;
    }
    let mut n = 0;
    for (track_id, image_id, size) in links {
        if db.set_artwork(track_id, image_id, size).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// 그림 열쇠: 앨범 + 앨범 가수(없으면 가수), 소문자·공백 정리
pub(crate) fn art_album_key(album: &str, artist: &str) -> String {
    format!("{}\u{1f}{}", audio::normalize(album), audio::normalize(artist))
}

/// 이번 작업에서 고친 그림 DB·ithmb 를 작업 전으로 돌린다
pub(crate) fn undo_artwork(mount: &Path, job: &LastJob) {
    if !job.art_changed {
        return;
    }
    artworkdb::rollback_ithmb(mount, &job.ithmb);
    let art_db = artworkdb::db_path(mount);
    match &job.art_backup {
        Some(name) => {
            let _ = std::fs::copy(artworkdb::artwork_dir(mount).join(name), &art_db);
        }
        None => {
            let _ = std::fs::remove_file(&art_db);
        }
    }
}

/// 곡 하나를 변환·복사·등록한다. 돌려주는 값: (아이팟에 쓴 파일, 바꿔 넣었으면 옛 파일, 곡 번호)
fn put_one(mount: &Path, fdirs: &[PathBuf], tmp: &Path, item: &Item, db: &mut ITunesDb) -> Result<(PathBuf, Option<PathBuf>, u32), String> {
    // 변환
    let src = &item.info.path;
    let conv = conversion(&item.info)?;
    let (file, ext) = match conv {
        None => (src.clone(), ext_lower(src)),
        Some(c) => (convert(src, tmp, c, item.index)?, "m4a".to_string()),
    };
    let info = audio::read(&file, false);

    // 복사: 무작위 Fxx 폴더에 무작위 4글자 이름
    let mut rng = rand::thread_rng();
    let dir = &fdirs[rng.gen_range(0..fdirs.len())];
    let dest = loop {
        let name: String = (0..4).map(|_| (b'A' + rng.gen_range(0..26)) as char).collect();
        let p = dir.join(format!("{name}.{ext}"));
        if !p.exists() {
            break p;
        }
    };
    let part = dest.with_extension(format!("{ext}.podlift-part"));
    let copy = std::fs::copy(&file, &part).and_then(|_| std::fs::rename(&part, &dest));
    if conv.is_some() {
        let _ = std::fs::remove_file(&file);
    }
    if let Err(e) = copy {
        let _ = std::fs::remove_file(&part);
        return Err(format!("복사 실패: {e}"));
    }

    let t = NewTrack {
        location: format!(":{}", rel(mount, &dest).replace('/', ":")),
        ext: ext.clone(),
        alac: info.codec == "ALAC",
        // 태그는 원본에서 (변환한 파일은 태그를 옮겨 붙였지만 원본이 가장 정확하다)
        title: item.info.title.clone(),
        artist: item.info.artist.clone(),
        album: item.info.album.clone(),
        album_artist: item.info.album_artist.clone(),
        genre: item.info.genre.clone(),
        composer: item.info.composer.clone(),
        track_nr: item.info.track.unwrap_or(0),
        track_total: item.info.track_total.unwrap_or(0),
        cd_nr: item.info.disc.unwrap_or(0),
        cd_total: item.info.disc_total.unwrap_or(0),
        year: item.info.year.unwrap_or(0),
        compilation: item.info.compilation,
        size: std::fs::metadata(&dest).map(|m| m.len() as u32).unwrap_or(info.size as u32),
        length_ms: info.duration_ms as u32,
        bitrate: info.bitrate_kbps,
        sample_rate: info.sample_rate,
    };
    let registered = match item.replace {
        None => db.add_track(&t).map(|id| (None, id)),
        Some(id) => {
            let old = db.tracks().into_iter().find(|x| x.id == id).and_then(|x| x.file_path(mount));
            db.replace_file(id, &t).map(|_| (old, id))
        }
    };
    match registered {
        Ok((old, id)) => Ok((dest, old, id)),
        Err(e) => {
            let _ = std::fs::remove_file(&dest);
            Err(e)
        }
    }
}

/// afconvert 로 바꾸고 원본 태그(앨범 아트 포함)를 MP4 태그로 옮겨 붙인다.
fn convert(src: &Path, tmp: &Path, how: Convert, n: usize) -> Result<PathBuf, String> {
    let out = tmp.join(format!("{n}.m4a"));
    let _ = std::fs::remove_file(&out);
    let mut cmd = std::process::Command::new("afconvert");
    cmd.arg("-f").arg("m4af");
    match how {
        // '/1' = 16bit 원본 표시 (kAppleLosslessFormatFlag_16BitSourceData)
        Convert::Alac(rate) => cmd.arg("-d").arg(format!("alac@{rate}/1")),
        Convert::Aac => cmd.args(["-d", "aac", "-b", "256000", "-s", "3"]),
    };
    let res = cmd.arg(src).arg(&out).output().map_err(|e| format!("afconvert 실행 실패: {e}"))?;
    if !res.status.success() || !out.exists() {
        return Err(format!("변환 실패: {}", String::from_utf8_lossy(&res.stderr).trim()));
    }
    // 태그 옮기기 (실패해도 곡은 넣는다: DB 에는 원본 태그로 등록된다)
    let tag = lofty::read_from_path(src).ok().and_then(|f| f.primary_tag().or_else(|| f.first_tag()).cloned());
    if let Some(mut tag) = tag {
        tag.re_map(TagType::Mp4Ilst);
        let _ = tag.save_to_path(&out, WriteOptions::default());
    }
    Ok(out)
}

/// Music 아래의 Fxx 폴더들 (없으면 F00 을 만든다)
fn music_dirs(music: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(music)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().to_ascii_uppercase();
                    n.len() == 3 && n.starts_with('F') && n[1..].chars().all(|c| c.is_ascii_digit())
                })
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    if dirs.is_empty() {
        let f = music.join("F00");
        let _ = std::fs::create_dir_all(&f);
        dirs.push(f);
    }
    dirs.sort();
    dirs
}

pub(crate) fn rel(mount: &Path, p: &Path) -> String {
    p.strip_prefix(mount).unwrap_or(p).to_string_lossy().into_owned()
}

fn ext_lower(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

// ── 되돌리기 ──────────────────────────────────────────

/// 되돌릴 수 있는 마지막 작업이 있으면 그 백업 파일 이름
pub fn last_backup(mount: &Path) -> Option<String> {
    let text = std::fs::read_to_string(itunes_dir(mount).join(LAST_JOB)).ok()?;
    let job: LastJob = serde_json::from_str(&text).ok()?;
    (itunes_dir(mount).join(&job.backup).is_file() && !db_changed_since(mount, &job)).then_some(job.backup)
}

/// 마지막 작업 전으로 되돌린다: DB 를 백업으로 바꾸고, 새로 넣은 파일을 지우고, 치워 둔 옛 파일을 제자리로.
pub fn restore_last(mount: &Path) -> Result<usize, String> {
    let itunes = itunes_dir(mount);
    let text = std::fs::read_to_string(itunes.join(LAST_JOB)).map_err(|_| "되돌릴 작업 기록이 없습니다".to_string())?;
    let job: LastJob = serde_json::from_str(&text).map_err(|e| format!("작업 기록이 이상합니다: {e}"))?;
    if db_changed_since(mount, &job) {
        return Err("마지막 작업 뒤에 Finder·Music 동기화 등이 곡 DB 를 바꿔서 되돌리면 곡이 엉킵니다. 되돌리지 않았습니다.".into());
    }
    let backup = itunes.join(&job.backup);
    // 백업이 정상인지 먼저 확인
    let data = std::fs::read(&backup).map_err(|e| format!("백업을 읽지 못했습니다: {e}"))?;
    ITunesDb::parse(&data).map_err(|e| format!("백업 DB 가 이상합니다: {e}"))?;
    std::fs::copy(&backup, db_path(mount)).map_err(|e| format!("DB 되돌리기 실패: {e}"))?;
    for f in &job.added {
        let _ = std::fs::remove_file(mount.join(f));
    }
    let mut stuck = 0;
    for (orig, parked) in &job.moved_old {
        if let Some(parent) = mount.join(orig).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::rename(mount.join(parked), mount.join(orig)).is_err() && mount.join(parked).exists() {
            stuck += 1;
        }
    }
    if let Some(name) = &job.playcounts_backup {
        let _ = std::fs::copy(itunes.join(name), itunes.join(PLAY_COUNTS));
    }
    undo_artwork(mount, &job);
    let _ = std::fs::remove_file(itunes.join(LAST_JOB));
    if stuck > 0 {
        // 치워 둔 폴더를 지우면 그 파일이 사라지므로 남겨 둔다
        return Err(format!("DB 는 되돌렸지만 파일 {stuck}개를 제자리로 옮기지 못했습니다. iPod_Control/Music/{OLD_DIR} 에 남겨 두었습니다."));
    }
    let _ = std::fs::remove_dir_all(mount.join("iPod_Control").join("Music").join(OLD_DIR));
    Ok(job.added.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(path: &str, album: &str, title: &str, lossless: bool, kbps: u32) -> AudioInfo {
        AudioInfo {
            path: PathBuf::from(path),
            album: album.into(),
            title: title.into(),
            tagged: true,
            lossless,
            codec: if lossless { "ALAC" } else { "AAC" },
            bitrate_kbps: kbps,
            sample_rate: 44100,
            bit_depth: if lossless { 16 } else { 0 },
            ..Default::default()
        }
    }

    #[test]
    fn decisions() {
        let ipod = vec![DbTrack { id: 60, title: "Song".into(), album: "Album".into(), bitrate: 128, sample_rate: 44100, ..Default::default() }];
        let queue = vec![
            info("/a/new.mp3", "Other", "Fresh", false, 320),
            info("/a/better.m4a", "Album", "Song", true, 900),
            info("/a/worse.m4a", "Album", "Song", false, 256),
            info("/a/drm.m4p", "X", "Y", false, 256),
        ];
        let d = decide(&queue, &ipod);
        assert_eq!(d[0], Decision::New);
        // 앨범이 달라도 제목+가수가 같고 길이가 비슷하면 이미 있는 곡
        let ipod2 = vec![DbTrack { id: 61, title: "Hit".into(), album: "Single".into(), artist: "X".into(), bitrate: 256, sample_rate: 44100, length_ms: 200_000, ..Default::default() }];
        let mut same = info("/a/album.m4a", "Full Album", "Hit", false, 256);
        same.artist = "X".into();
        same.duration_ms = 201_000;
        let mut live = same.clone();
        live.path = PathBuf::from("/a/live.m4a");
        live.album = "Live".into();
        live.duration_ms = 260_000;
        assert_eq!(decide(&[same], &ipod2), vec![Decision::Exists(61)]);
        assert_eq!(decide(&[live], &ipod2), vec![Decision::New]);
        assert_eq!(d[1], Decision::Better(60));
        assert_eq!(d[2], Decision::QueueDup(1));
        assert!(matches!(d[3], Decision::Unsupported(_)));
    }

    #[test]
    fn hires_converts_and_counts_as_16bit() {
        let mut hi = info("/a/x.flac", "A", "B", true, 3000);
        hi.codec = "FLAC";
        hi.sample_rate = 96000;
        hi.bit_depth = 24;
        assert_eq!(conversion(&hi), Ok(Some(Convert::Alac(48000))));
        hi.sample_rate = 88200;
        assert_eq!(conversion(&hi), Ok(Some(Convert::Alac(44100))));
        // 아이팟에 이미 16bit/44.1 ALAC 이 있으면 하이레즈를 넣어도 나아지지 않는다
        let ipod = vec![DbTrack { id: 70, title: "B".into(), album: "A".into(), lossless: true, sample_rate: 44100, ..Default::default() }];
        assert_eq!(decide(&[hi], &ipod), vec![Decision::Exists(70)]);
    }
}

/// 가짜 아이팟에 넣기 → 더 좋은 음질로 바꾸기 → 되돌리기 전체 흐름.
/// PODLIFT_SYNC_TEST=<빈 작업 폴더>, PODLIFT_SYNC_SRC=<곡 파일이 든 폴더: low.mp3, hires.flac> 일 때만 돈다.
#[cfg(test)]
#[test]
fn sync_end_to_end() {
    let (Ok(work), Ok(src)) = (std::env::var("PODLIFT_SYNC_TEST"), std::env::var("PODLIFT_SYNC_SRC")) else { return };
    let mount = PathBuf::from(work);
    let _ = std::fs::remove_dir_all(&mount);
    std::fs::create_dir_all(itunes_dir(&mount)).unwrap();
    std::fs::create_dir_all(mount.join("iPod_Control/Music/F00")).unwrap();
    std::fs::create_dir_all(mount.join("iPod_Control/Device")).unwrap();
    std::fs::write(mount.join("iPod_Control/Device/SysInfo"), "ModelNumStr: xB029\nFirewireGuid: 0x000A27001A2B3C4D\n").unwrap();
    std::fs::write(db_path(&mount), crate::itunesdb::tests::empty_db(true)).unwrap();
    let fwid = crate::ipod::firewire_id(&mount).and_then(|s| crate::itunesdb::parse_firewire_id(&s));
    assert!(fwid.is_some());

    let wait = |job: SyncJob| loop {
        for e in job.poll() {
            match e {
                SyncEvent::Done(i, Err(msg)) => panic!("{i}번 실패: {msg}"),
                SyncEvent::Finished { added, replaced, art, error, note } => {
                    assert_eq!(note, None);
                    return (added, replaced, art, error);
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let src = PathBuf::from(src);
    // 곡에 그림이 없으니 같은 폴더의 cover.png 를 앨범 아트로 쓴다
    if !src.join("cover.png").exists() {
        image::RgbImage::from_pixel(600, 600, image::Rgb([200, 30, 30])).save(src.join("cover.png")).unwrap();
    }
    let low = audio::read(&src.join("low.mp3"), false);
    let hires = audio::read(&src.join("hires.flac"), false);

    // 1) 저음질 mp3 를 새 곡으로
    let db = open_db(&mount).unwrap();
    assert_eq!(decide(std::slice::from_ref(&low), &db.tracks()), vec![Decision::New]);
    let (a, r, art, e) = wait(start(mount.clone(), vec![Item { index: 0, info: low.clone(), replace: None }], fwid.clone()));
    assert_eq!((a, r, art, e), (1, 0, 1, None));
    let tracks = open_db(&mount).unwrap().tracks();
    assert_eq!(tracks.len(), 1);
    let first = tracks[0].clone();
    assert!(first.has_artwork && first.mhii_link == 100, "그림 연결: {} {}", first.has_artwork, first.mhii_link);
    assert!(artworkdb::db_path(&mount).is_file());
    let f1060 = artworkdb::artwork_dir(&mount).join("F1060_1.ithmb");
    assert_eq!(std::fs::metadata(&f1060).unwrap().len(), 320 * 320 * 2);
    assert!(first.file_path(&mount).unwrap().is_file());
    println!("넣음: {} | {} | {}kbps", first.title, first.location, first.bitrate);

    // 2) 같은 곡의 하이레즈 FLAC → ALAC 16bit 로 바꿔 넣기
    let d = decide(std::slice::from_ref(&hires), &tracks);
    assert_eq!(d, vec![Decision::Better(first.id)]);
    let (a, r, art, e) = wait(start(mount.clone(), vec![Item { index: 0, info: hires, replace: Some(first.id) }], fwid.clone()));
    // 같은 앨범 그림이 이미 있으니 새 그림 없이 같은 번호를 쓴다
    assert_eq!((a, r, art, e), (0, 1, 1, None));
    assert_eq!(std::fs::metadata(&f1060).unwrap().len(), 320 * 320 * 2);
    let after = open_db(&mount).unwrap().tracks();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, first.id);
    assert_eq!(after[0].mhii_link, 100);
    assert!(after[0].lossless, "ALAC 으로 바뀌어야 함: {}", after[0].filetype);
    let new_file = after[0].file_path(&mount).unwrap();
    assert!(new_file.is_file());
    assert!(!first.file_path(&mount).unwrap().exists(), "옛 파일은 치워져야 함");
    let converted = audio::read(&new_file, false);
    println!("바꿈: {} | {} | {}", after[0].location, converted.quality_label(), converted.title);
    assert_eq!((converted.codec, converted.bit_depth, converted.sample_rate), ("ALAC", 16, 48000));
    assert_eq!(converted.title, "하이레즈 곡", "태그가 옮겨 붙어야 함");

    // 3) 되돌리기: mp3 가 다시 제자리에, ALAC 파일은 지워짐
    assert!(last_backup(&mount).is_some());
    restore_last(&mount).unwrap();
    let back = open_db(&mount).unwrap().tracks();
    assert_eq!(back[0].location, first.location);
    assert!(first.file_path(&mount).unwrap().is_file());
    assert!(!new_file.exists());
    println!("되돌림: {}", back[0].location);
}
