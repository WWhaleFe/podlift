//! 음악 파일 한 곡의 태그·음질·앨범 아트 읽기와 음질 비교. 중복 정리와 아이팟에 넣기가 함께 쓴다.
//!
//! 같은 곡(앨범·제목이 같은 곡) 두 개 중 무엇을 남길지는 이 순서로 정한다.
//! 1. 무손실(ALAC·FLAC·WAV·AIFF)이 손실 압축(AAC·MP3)보다 낫다.
//! 2. 둘 다 무손실이면 샘플레이트 × 비트 깊이가 큰 쪽.
//! 3. 둘 다 손실 압축이면 비트레이트가 8% 넘게 높은 쪽, 비슷하면 샘플레이트가 높은 쪽.
//! 4. 여기까지 같으면 '비슷함'으로 보고 앨범 아트를 비교한다: 아트가 있는 쪽, 해상도가 큰 쪽.
//! 5. 그래도 같으면 '확인 필요'로 두고 사용자가 고른다.
use lofty::config::ParseOptions;
use lofty::file::FileType;
use lofty::picture::PictureType;
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// 앨범 아트 썸네일의 긴 변 픽셀 수
pub const THUMB_PX: u32 = 160;

#[derive(Clone, Debug, Default)]
pub struct Cover {
    pub width: u32,
    pub height: u32,
    /// 원본 이미지 바이트 수
    pub bytes: usize,
    /// THUMB_PX 이하로 줄인 RGBA 썸네일 (가로, 세로, 픽셀). 디코딩 실패면 None
    pub thumb: Option<(u32, u32, Vec<u8>)>,
}

#[derive(Clone, Debug, Default)]
pub struct AudioInfo {
    pub path: PathBuf,
    pub size: u64,
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub genre: String,
    pub composer: String,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: bool,
    /// 제목 태그가 있는지
    pub tagged: bool,
    /// "MP3", "AAC", "ALAC", "FLAC", "WAV", "AIFF", "OGG", "Opus", "기타"
    pub codec: &'static str,
    pub lossless: bool,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub bit_depth: u8,
    pub channels: u8,
    pub duration_ms: u64,
    pub cover: Option<Cover>,
}

impl AudioInfo {
    /// "AAC 256kbps · 44.1kHz" / "ALAC 16bit · 44.1kHz" 같은 한 줄 요약
    pub fn quality_label(&self) -> String {
        let khz = if self.sample_rate == 0 { String::new() } else { format!(" · {}kHz", trim_khz(self.sample_rate)) };
        if self.lossless {
            let bits = if self.bit_depth == 0 { String::new() } else { format!(" {}bit", self.bit_depth) };
            format!("{}{bits}{khz}", self.codec)
        } else {
            format!("{} {}kbps{khz}", self.codec, self.bitrate_kbps)
        }
    }
}

fn trim_khz(rate: u32) -> String {
    let k = rate as f64 / 1000.0;
    if (k.fract()).abs() < 0.05 { format!("{k:.0}") } else { format!("{k:.1}") }
}

/// 태그와 음질을 읽는다. with_cover 면 앨범 아트 크기와 썸네일까지 만든다(느리다).
pub fn read(path: &Path, with_cover: bool) -> AudioInfo {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut info = AudioInfo { path: path.to_path_buf(), size, title: stem, codec: "기타", ..Default::default() };
    let opts = ParseOptions::new().read_properties(true).read_cover_art(with_cover);
    let Some(file) = (|| Probe::open(path).ok()?.guess_file_type().ok()?.options(opts).read().ok())() else {
        return info;
    };
    let props = file.properties();
    info.bitrate_kbps = props.audio_bitrate().or(props.overall_bitrate()).unwrap_or(0);
    info.sample_rate = props.sample_rate().unwrap_or(0);
    info.bit_depth = props.bit_depth().unwrap_or(0);
    info.channels = props.channels().unwrap_or(0);
    info.duration_ms = props.duration().as_millis() as u64;
    (info.codec, info.lossless) = match file.file_type() {
        FileType::Mpeg => ("MP3", false),
        // MP4 안의 ALAC 은 비트 깊이가 있고 AAC 는 없다
        FileType::Mp4 if info.bit_depth > 0 => ("ALAC", true),
        FileType::Mp4 => ("AAC", false),
        FileType::Flac => ("FLAC", true),
        FileType::Wav => ("WAV", true),
        FileType::Aiff => ("AIFF", true),
        FileType::Vorbis => ("OGG", false),
        FileType::Opus => ("Opus", false),
        _ => ("기타", false),
    };
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        let s = |v: Option<std::borrow::Cow<'_, str>>| v.map(|c| c.trim().to_string()).unwrap_or_default();
        let title = s(tag.title());
        if !title.is_empty() {
            info.title = title;
            info.tagged = true;
        }
        info.artist = s(tag.artist());
        info.album = s(tag.album());
        info.genre = s(tag.genre());
        info.album_artist = tag.get_string(ItemKey::AlbumArtist).map(|v| v.trim().to_string()).unwrap_or_default();
        info.composer = tag.get_string(ItemKey::Composer).map(|v| v.trim().to_string()).unwrap_or_default();
        info.year = tag
            .get_string(ItemKey::Year)
            .or_else(|| tag.get_string(ItemKey::RecordingDate))
            .and_then(|v| v.get(..4))
            .and_then(|v| v.parse().ok());
        info.track = tag.track().filter(|&n| n > 0);
        info.track_total = tag.track_total().filter(|&n| n > 0);
        info.disc = tag.disk().filter(|&n| n > 0);
        info.disc_total = tag.disk_total().filter(|&n| n > 0);
        info.compilation = tag
            .get_string(ItemKey::FlagCompilation)
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "True"));
        if with_cover {
            let pics = tag.pictures();
            let pic = pics.iter().find(|p| p.pic_type() == PictureType::CoverFront).or_else(|| pics.first());
            info.cover = pic.map(|p| decode_cover(p.data()));
        }
    }
    info
}

/// 곡 파일 태그에 든 그림(앞표지, 없으면 첫 그림)의 바이트. 폴더의 그림 파일은 보지 않는다.
pub fn embedded_cover(path: &Path) -> Option<Vec<u8>> {
    let f = lofty::read_from_path(path).ok()?;
    let tag = f.primary_tag().or_else(|| f.first_tag())?;
    let pics = tag.pictures();
    let pic = pics.iter().find(|p| p.pic_type() == PictureType::CoverFront).or_else(|| pics.first())?;
    Some(pic.data().to_vec())
}

/// 아이팟에 넣을 앨범 아트 원본 바이트: 곡에 든 앞표지(없으면 첫 그림),
/// 그것도 없으면 같은 폴더의 cover/folder/front.jpg·png
pub fn cover_bytes(path: &Path) -> Option<Vec<u8>> {
    embedded_cover(path).or_else(|| folder_cover(path))
}

/// 곡과 같은 폴더의 cover/folder/front/albumart .jpg·png 바이트
pub fn folder_cover(path: &Path) -> Option<Vec<u8>> {
    folder_cover_files(path.parent()?).into_iter().next().and_then(|p| std::fs::read(p).ok())
}

/// 폴더 안 앨범 아트 그림 파일들 (cover → folder → front → albumart 순)
pub fn folder_cover_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    let mut out = Vec::new();
    for want in ["cover", "folder", "front", "albumart"] {
        for p in &names {
            let stem = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            let ext = p.extension().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            if stem == want && matches!(ext.as_str(), "jpg" | "jpeg" | "png") {
                out.push(p.clone());
            }
        }
    }
    out
}

fn decode_cover(data: &[u8]) -> Cover {
    let mut cover = Cover { bytes: data.len(), ..Default::default() };
    if let Ok(img) = image::load_from_memory(data) {
        cover.width = img.width();
        cover.height = img.height();
        let t = img.thumbnail(THUMB_PX, THUMB_PX).to_rgba8();
        cover.thumb = Some((t.width(), t.height(), t.into_raw()));
    }
    cover
}

/// 같은 곡인지 판단할 열쇠: 앨범과 제목을 소문자로, 공백을 하나로 줄여 붙인다.
/// 제목 태그가 없는 곡은 열쇠가 없다(파일 이름만으로는 같은 곡이라 보기 어렵다).
pub fn same_song_key(info: &AudioInfo) -> Option<String> {
    if !info.tagged {
        return None;
    }
    Some(format!("{}\u{1f}{}", normalize(&info.album), normalize(&info.title)))
}

/// 같은 곡인지 판단할 두 번째 열쇠: 제목과 가수(없으면 앨범 가수). 앨범이 달라도
/// (싱글과 정규 앨범, 일반판과 디럭스판 등) 제목과 가수가 같으면 같은 곡으로 본다.
pub fn artist_title_key(info: &AudioInfo) -> Option<String> {
    let artist = if info.artist.trim().is_empty() { &info.album_artist } else { &info.artist };
    if !info.tagged || artist.trim().is_empty() {
        return None;
    }
    Some(format!("{}\u{1f}{}", normalize(&info.title), normalize(artist)))
}

/// 두 곡의 가수가 같은지 (가수·앨범 가수 중 하나라도, 대소문자·기호 무시, 한쪽이 다른 쪽을 품어도 같다)
pub fn same_artist(a: &AudioInfo, b: &AudioInfo) -> bool {
    let letters = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let names = |i: &AudioInfo| [letters(&i.artist), letters(&i.album_artist)].into_iter().filter(|n| !n.is_empty()).collect::<Vec<_>>();
    let (x, y) = (names(a), names(b));
    if x.is_empty() || y.is_empty() {
        return true; // 가수를 모르면 가수로는 가르지 않는다
    }
    x.iter().any(|p| y.iter().any(|q| p == q || (p.chars().count() >= 2 && q.chars().count() >= 2 && (p.contains(q.as_str()) || q.contains(p.as_str())))))
}

/// 가수 표기가 달라도(드림캐쳐/Dreamcatcher) 같은 녹음으로 볼 길이 차이 (밀리초)
const SAME_RECORDING_MS: u64 = 3_000;

/// 같은 곡끼리 묶는다(이어지면 한 묶음). 같은 곡의 조건:
/// - 앨범+제목이 같고, 가수가 같거나 가수 표기가 달라도 길이가 3초 안으로 같다
///   (다른 가수가 부른 같은 제목 곡은 묶지 않는다)
/// - 또는 앨범이 달라도 제목+가수가 같다 (길이가 많이 다르면 묶음에서 '확인 필요'가 된다)
/// 두 곡 이상인 묶음만, 각 묶음은 입력 순서대로 위치를 돌려준다.
pub fn same_song_groups(infos: &[&AudioInfo]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..infos.len()).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    fn union(p: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(p, a), find(p, b));
        if ra != rb {
            p[ra.max(rb)] = ra.min(rb);
        }
    }
    let mut by_album: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    let mut by_artist: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, info) in infos.iter().enumerate() {
        if let Some(k) = same_song_key(info) {
            let seen = by_album.entry(k).or_default();
            for &j in seen.iter() {
                let other = infos[j];
                let close = info.duration_ms > 0 && other.duration_ms > 0 && info.duration_ms.abs_diff(other.duration_ms) <= SAME_RECORDING_MS;
                if same_artist(info, other) || close {
                    union(&mut parent, i, j);
                }
            }
            seen.push(i);
        }
        if let Some(k) = artist_title_key(info) {
            match by_artist.get(&k) {
                Some(&j) => union(&mut parent, i, j),
                None => {
                    by_artist.insert(k, i);
                }
            }
        }
    }
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
    for i in 0..infos.len() {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    groups.into_values().filter(|g| g.len() >= 2).collect()
}

pub fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// 음질 비교 결과
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 음질로 분명히 갈림 (Greater = 앞쪽이 낫다)
    Quality(Ordering),
    /// 음질이 비슷해 앨범 아트로 갈림
    Cover(Ordering),
    /// 형식·음질·앨범 아트가 모두 같은 사본이라 어느 것을 남겨도 같음 (비트레이트가 조금이라도 높은 쪽을 남긴다)
    Same,
    /// 둘 다 비슷해 사람이 골라야 함 (예: 비트레이트가 비슷한 MP3 와 AAC)
    Unsure,
}

pub fn compare_quality(a: &AudioInfo, b: &AudioInfo) -> Option<Ordering> {
    if a.lossless != b.lossless {
        return Some(a.lossless.cmp(&b.lossless));
    }
    if a.lossless {
        let ka = a.sample_rate as u64 * a.bit_depth.max(16) as u64;
        let kb = b.sample_rate as u64 * b.bit_depth.max(16) as u64;
        return (ka != kb).then(|| ka.cmp(&kb));
    }
    let (ra, rb) = (a.bitrate_kbps as f64, b.bitrate_kbps as f64);
    let top = ra.max(rb);
    if top > 0.0 && (ra - rb).abs() / top > 0.08 {
        return Some(a.bitrate_kbps.cmp(&b.bitrate_kbps));
    }
    (a.sample_rate != b.sample_rate).then(|| a.sample_rate.cmp(&b.sample_rate))
}

pub fn compare_cover(a: &AudioInfo, b: &AudioInfo) -> Option<Ordering> {
    let area = |i: &AudioInfo| i.cover.as_ref().map(|c| c.width as u64 * c.height as u64);
    match (area(a), area(b)) {
        (Some(_), None) => Some(Ordering::Greater),
        (None, Some(_)) => Some(Ordering::Less),
        (Some(x), Some(y)) if x != y => Some(x.cmp(&y)),
        _ => None,
    }
}

pub fn compare(a: &AudioInfo, b: &AudioInfo) -> Verdict {
    if let Some(o) = compare_quality(a, b) {
        Verdict::Quality(o)
    } else if let Some(o) = compare_cover(a, b) {
        Verdict::Cover(o)
    } else {
        Verdict::Unsure
    }
}

/// 음질 규칙과 앨범 아트로 가릴 수 없는 두 곡이 사실상 같은 사본인지:
/// 형식·무손실 여부·샘플레이트·비트 깊이가 같고 앨범 아트도 같은 크기(또는 둘 다 없음)
fn same_copy(a: &AudioInfo, b: &AudioInfo) -> bool {
    let dims = |i: &AudioInfo| i.cover.as_ref().map(|c| (c.width, c.height));
    a.codec == b.codec && a.lossless == b.lossless && a.sample_rate == b.sample_rate && a.bit_depth == b.bit_depth && dims(a) == dims(b)
}

/// 여러 곡 중 가장 나은 것의 위치와, 그 판단이 무엇으로 갈렸는지.
/// 1등과 2등 사이의 판단을 돌려준다 (Unsure 면 사람이 골라야 한다).
pub fn pick_best(items: &[&AudioInfo]) -> (usize, Verdict) {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&x, &y| {
        let (a, b) = (items[x], items[y]);
        compare_quality(b, a).or_else(|| compare_cover(b, a)).unwrap_or(Ordering::Equal)
    });
    let best = order[0];
    let verdict = match order.get(1) {
        Some(&second) => compare(items[best], items[second]),
        None => Verdict::Quality(Ordering::Greater),
    };
    if verdict == Verdict::Unsure {
        // 1등과 비긴 곡들이 모두 같은 사본이면 비트레이트(그다음 파일 크기)가 큰 쪽을 남긴다
        let tied: Vec<usize> = order.iter().copied().take_while(|&i| compare(items[best], items[i]) == Verdict::Unsure || i == best).collect();
        if tied.iter().all(|&i| same_copy(items[best], items[i])) {
            let pick = tied.iter().copied().max_by_key(|&i| (items[i].bitrate_kbps, items[i].size, std::cmp::Reverse(i))).unwrap_or(best);
            return (pick, Verdict::Same);
        }
    }
    (best, verdict)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(codec: &'static str, lossless: bool, kbps: u32, rate: u32, cover: Option<(u32, u32)>) -> AudioInfo {
        AudioInfo {
            codec,
            lossless,
            bitrate_kbps: kbps,
            sample_rate: rate,
            bit_depth: if lossless { 16 } else { 0 },
            cover: cover.map(|(w, h)| Cover { width: w, height: h, ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn lossless_beats_lossy() {
        let a = info("ALAC", true, 900, 44100, None);
        let b = info("AAC", false, 320, 48000, Some((1000, 1000)));
        assert_eq!(compare(&a, &b), Verdict::Quality(Ordering::Greater));
    }

    #[test]
    fn bitrate_decides_then_cover_then_unsure() {
        let lo = info("MP3", false, 128, 44100, Some((600, 600)));
        let hi = info("AAC", false, 256, 44100, None);
        assert_eq!(compare(&hi, &lo), Verdict::Quality(Ordering::Greater));

        let a = info("AAC", false, 256, 44100, Some((600, 600)));
        let b = info("AAC", false, 260, 44100, Some((1400, 1400)));
        assert_eq!(compare(&b, &a), Verdict::Cover(Ordering::Greater));

        let c = info("AAC", false, 256, 44100, Some((600, 600)));
        assert_eq!(compare(&a, &c), Verdict::Unsure);
    }

    #[test]
    fn identical_copies_are_same_not_unsure() {
        let a = info("AAC", false, 263, 44100, Some((1200, 1200)));
        let b = info("AAC", false, 266, 44100, Some((1200, 1200)));
        let mp3 = info("MP3", false, 256, 44100, Some((600, 600)));
        let (best, v) = pick_best(&[&a, &mp3, &b]);
        assert_eq!((best, v), (2, Verdict::Same));
        // 형식이 다르면 (MP3 256 과 AAC 263, 아트 같음) 사람이 고른다
        let aac = info("AAC", false, 263, 44100, Some((600, 600)));
        assert_eq!(pick_best(&[&mp3, &aac]).1, Verdict::Unsure);
    }

    #[test]
    fn groups_by_album_title_or_artist_title() {
        let mk = |album: &str, title: &str, artist: &str| AudioInfo { album: album.into(), title: title.into(), artist: artist.into(), tagged: true, ..Default::default() };
        let v = [
            mk("1989", "Style", "Taylor Swift"),
            mk("1989 (Deluxe)", "Style", "Taylor Swift"), // 앨범 다름, 제목+가수 같음
            mk("1989", "style", "테일러 스위프트"),      // 앨범+제목 같음
            mk("Other", "Style", "Someone"),             // 제목만 같음 → 따로
            mk("Single", "Alone", ""),                   // 가수 없음, 짝 없음
        ];
        let refs: Vec<&AudioInfo> = v.iter().collect();
        // 2번은 가수 표기가 다르지만 길이(둘 다 0 = 모름)로는 가릴 수 없어 묶이지 않는다
        assert_eq!(same_song_groups(&refs), vec![vec![0, 1]]);

        // 같은 앨범·제목, 다른 가수: 길이가 같으면(표기만 다름) 묶고, 다르면(다른 곡) 따로
        let mut a = mk("Heaven - Single", "Heaven", "TAEYEON");
        let mut b = mk("Heaven - Single", "HEAVEN", "에일리");
        a.duration_ms = 152_000;
        b.duration_ms = 211_000;
        assert!(same_song_groups(&[&a, &b]).is_empty());
        let mut c = mk("11111101", "Melodie", "IDIOTAPE");
        let mut d = mk("11111101", "Melodie", "이디오테잎");
        c.duration_ms = 301_000;
        d.duration_ms = 300_200;
        assert_eq!(same_song_groups(&[&c, &d]), vec![vec![0, 1]]);
    }

    #[test]
    fn best_of_three() {
        let a = info("MP3", false, 128, 44100, None);
        let b = info("AAC", false, 256, 44100, None);
        let c = info("AAC", false, 256, 44100, Some((500, 500)));
        let (best, v) = pick_best(&[&a, &b, &c]);
        assert_eq!(best, 2);
        assert_eq!(v, Verdict::Cover(Ordering::Greater));
    }
}
