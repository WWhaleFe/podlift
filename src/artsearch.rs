//! 앨범 아트 온라인 검색. 둘 다 무료이고 키가 필요 없다.
//! 1) MusicBrainz 음반 검색 → Cover Art Archive 앞표지 (한글·일본어 가수 이름도 별칭으로 알아본다)
//! 2) 없으면 Apple iTunes 검색 (미국·일본 스토어. 한국 스토어는 음악 결과가 없다)
//! 엉뚱한 표지를 붙이지 않도록 앨범 이름이 사실상 같고(" - Single", "(Deluxe…)" 같은 꼬리는 무시)
//! 가수도 맞을 때만 받아들인다. 받은 그림은 ~/Library/Caches/Podlift/art 에 둔다.
//! 요청은 macOS 내장 curl 로 보낸다. 보내는 것은 앨범·가수 이름뿐이다.
use sha1::{Digest, Sha1};
use std::path::PathBuf;
use std::time::Duration;

const USER_AGENT: &str = "Podlift/0.1";

/// 찾은 그림: 캐시에 저장한 파일과 어디서 찾았는지
#[derive(Clone, Debug)]
pub struct Found {
    pub file: PathBuf,
    pub from: String,
}

fn get(url: &str) -> Result<Vec<u8>, String> {
    let out = std::process::Command::new("curl")
        .args(["-sfL", "--max-time", "20", "-A", USER_AGENT, url])
        .output()
        .map_err(|e| format!("curl 실행 실패: {e}"))?;
    if !out.status.success() {
        return Err(format!("요청 실패 ({})", out.status.code().unwrap_or(-1)));
    }
    Ok(out.stdout)
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// 글자·숫자만 소문자로, 라틴 글자의 악센트는 뗀다 (Pokémon → pokemon)
fn letters(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).map(fold_accent).collect()
}

fn fold_accent(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        _ => c,
    }
}

/// 앨범 이름 비교용: " - Single"/" - EP" 와 끝의 괄호 묶음((Deluxe Edition), [Remastered] 등)을 뗀다
pub fn album_core(s: &str) -> String {
    let mut t = s.trim().to_string();
    loop {
        let lower = t.to_lowercase();
        let before = t.len();
        for suffix in [" - single", " - ep", " - the remixes"] {
            if lower.ends_with(suffix) {
                t.truncate(t.len() - suffix.len());
            }
        }
        let trimmed = t.trim_end();
        if let Some(open) = trimmed.ends_with(')').then(|| trimmed.rfind('(')).flatten().or_else(|| trimmed.ends_with(']').then(|| trimmed.rfind('[')).flatten()) {
            if open > 0 {
                t.truncate(open);
            }
        }
        t = t.trim().to_string();
        if t.len() == before {
            break;
        }
    }
    let core = letters(&t);
    if core.is_empty() { letters(s) } else { core }
}

/// 앨범 이름이 얼마나 비슷한지 0~1 (꼬리를 뗀 이름의 글자 두 개씩 겹침, 한쪽이 다른 쪽을 품으면 높게).
/// 가수까지 같으면 조금 더 높인다. '다른 앨범의 그림 고르기' 목록 순서에 쓴다.
pub fn similarity(album: &str, artist: &str, other_album: &str, other_artist: &str) -> f32 {
    let (a, b) = (album_core(album), album_core(other_album));
    let base = if a.is_empty() || b.is_empty() {
        0.0
    } else if a == b {
        1.0
    } else if a.contains(&b) || b.contains(&a) {
        0.85
    } else {
        let grams = |s: &str| -> Vec<(char, char)> {
            let c: Vec<char> = s.chars().collect();
            if c.len() < 2 { c.iter().map(|&x| (x, ' ')).collect() } else { c.windows(2).map(|w| (w[0], w[1])).collect() }
        };
        let (ga, gb) = (grams(&a), grams(&b));
        let mut rest = gb.clone();
        let mut common = 0;
        for g in &ga {
            if let Some(i) = rest.iter().position(|x| x == g) {
                rest.swap_remove(i);
                common += 1;
            }
        }
        2.0 * common as f32 / (ga.len() + gb.len()).max(1) as f32 * 0.8
    };
    let bonus = if !artist.trim().is_empty() && artist_ok(artist, &[other_artist.to_string()]) { 0.15 } else { 0.0 };
    base + bonus
}

/// 검색어(앨범·가수 아무거나)와 얼마나 맞는지: 글자가 모두 들어 있으면 1, 아니면 이름 비슷한 정도
pub fn query_score(query: &str, album: &str, artist: &str) -> f32 {
    let q = letters(query);
    if q.is_empty() {
        return 0.0;
    }
    let hay = letters(&format!("{album}{artist}"));
    if hay.contains(&q) {
        1.2
    } else {
        similarity(query, "", album, artist)
    }
}

fn album_ok(want: &str, got: &str) -> bool {
    let (a, b) = (album_core(want), album_core(got));
    !a.is_empty() && a == b
}

/// 가수가 맞는지: 이름(또는 별칭) 중 하나가 같거나 한쪽이 다른 쪽을 품는다. "A & B" 는 앞사람으로도 본다.
fn artist_ok(want: &str, names: &[String]) -> bool {
    if want.trim().is_empty() {
        return true;
    }
    let w = letters(want);
    let first = letters(want.split(['&', ',', '/']).next().unwrap_or(want));
    names.iter().map(|n| letters(n)).any(|n| {
        !n.is_empty() && (n == w || n == first || (n.chars().count() >= 2 && (w.contains(&n) || n.contains(&w))))
    })
}

fn cache_file(album: &str, artist: &str) -> Option<PathBuf> {
    let hash: String = Sha1::new().chain_update(album.as_bytes()).chain_update([0u8]).chain_update(artist.as_bytes()).finalize().iter().map(|b| format!("{b:02x}")).collect();
    let dir = dirs::cache_dir()?.join("Podlift").join("art");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{hash}.jpg")))
}

/// 앨범 아트를 찾는다. 못 찾으면 Ok(None). 네트워크가 안 되면 Err.
pub fn search(album: &str, artist: &str) -> Result<Option<Found>, String> {
    if album.trim().is_empty() {
        return Ok(None);
    }
    let file = cache_file(album, artist).ok_or("캐시 폴더를 만들 수 없습니다")?;
    let save = |bytes: Vec<u8>, from: String| -> Result<Option<Found>, String> {
        if image::load_from_memory(&bytes).is_err() {
            return Ok(None);
        }
        std::fs::write(&file, &bytes).map_err(|e| e.to_string())?;
        Ok(Some(Found { file: file.clone(), from }))
    };
    let mut net_error = None;
    match musicbrainz(album, artist) {
        Ok(Some((bytes, from))) => return save(bytes, from),
        Ok(None) => {}
        Err(e) => net_error = Some(e),
    }
    match itunes(album, artist) {
        Ok(Some((bytes, from))) => return save(bytes, from),
        Ok(None) => {}
        Err(e) => net_error = Some(e),
    }
    match net_error {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

/// MusicBrainz 는 초당 1번까지만 받고, 바쁘면 {"error": ...} 를 준다. 그때는 기다렸다 다시 묻는다.
fn mb_search(query: &str, limit: u32) -> Result<serde_json::Value, String> {
    let url = format!("https://musicbrainz.org/ws/2/release-group/?query={}&fmt=json&limit={limit}", enc(query));
    for attempt in 0..5 {
        std::thread::sleep(Duration::from_millis(if attempt == 0 { 1100 } else { 3000 }));
        let Ok(body) = get(&url) else { continue };
        let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("MusicBrainz 응답을 읽지 못했습니다: {e}"))?;
        if v.get("error").is_none() {
            return Ok(v);
        }
    }
    Err("MusicBrainz 가 응답하지 않습니다".into())
}

fn musicbrainz(album: &str, artist: &str) -> Result<Option<(Vec<u8>, String)>, String> {
    let clean = |s: &str| s.replace(['"', '\\'], " ");
    let mut queries = Vec::new();
    if !artist.trim().is_empty() {
        queries.push((format!("releasegroup:\"{}\" AND artist:\"{}\"", clean(album), clean(artist)), 10));
    }
    // 가수 이름이 별칭(한글 등)이라 위에서 안 걸리면 앨범으로만 찾고 별칭으로 거른다
    queries.push((format!("releasegroup:\"{}\"", clean(album)), 25));
    for (q, limit) in queries {
        let v = mb_search(&q, limit)?;
        for rg in v["release-groups"].as_array().into_iter().flatten() {
            let title = rg["title"].as_str().unwrap_or("");
            let mut names: Vec<String> = Vec::new();
            for c in rg["artist-credit"].as_array().into_iter().flatten() {
                names.extend(c["name"].as_str().map(str::to_string));
                for a in c["artist"]["aliases"].as_array().into_iter().flatten() {
                    names.extend(a["name"].as_str().map(str::to_string));
                }
            }
            if !(album_ok(album, title) && artist_ok(artist, &names)) {
                continue;
            }
            let Some(id) = rg["id"].as_str() else { continue };
            if let Ok(bytes) = get(&format!("https://coverartarchive.org/release-group/{id}/front-500")) {
                return Ok(Some((bytes, format!("MusicBrainz: {title} - {}", names.first().cloned().unwrap_or_default()))));
            }
        }
    }
    Ok(None)
}

fn itunes(album: &str, artist: &str) -> Result<Option<(Vec<u8>, String)>, String> {
    let term = format!("{artist} {album}");
    for country in ["US", "JP"] {
        let url = format!("https://itunes.apple.com/search?term={}&entity=album&limit=15&country={country}", enc(term.trim()));
        let body = get(&url)?;
        let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("iTunes 응답을 읽지 못했습니다: {e}"))?;
        for r in v["results"].as_array().into_iter().flatten() {
            let title = r["collectionName"].as_str().unwrap_or("");
            let who = r["artistName"].as_str().unwrap_or("").to_string();
            if !(album_ok(album, title) && artist_ok(artist, std::slice::from_ref(&who))) {
                continue;
            }
            let Some(small) = r["artworkUrl100"].as_str() else { continue };
            let big = small.replace("100x100bb", "600x600bb");
            if let Ok(bytes) = get(&big) {
                return Ok(Some((bytes, format!("iTunes: {title} - {who}"))));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_names_compare_without_tails() {
        assert_eq!(album_core("Missing You - Single"), "missingyou");
        assert_eq!(album_core("1989 (Deluxe Edition)"), "1989");
        assert_eq!(album_core("Kaleidoscope [Remastered] - EP"), "kaleidoscope");
        assert_eq!(album_core("(Untitled)"), "untitled");
        assert!(album_ok("2NE1 2nd Mini Album", "2NE1 2nd Mini Album"));
        assert!(!album_ok("Dance Again", "Bailar Nada Más (Dance Again - Spanish Version)"));
        assert!(artist_ok("소녀시대", &["Girls’ Generation".into(), "소녀시대".into()]));
        assert!(artist_ok("스티브 우, 두스", &["Steve Woo".into(), "스티브 우".into()]));
        assert!(!artist_ok("2NE1", &["BIGBANG".into()]));
        // 비슷한 앨범이 위로
        let s1 = similarity("1989 (Deluxe)", "Taylor Swift", "1989", "Taylor Swift");
        let s2 = similarity("1989 (Deluxe)", "Taylor Swift", "Red", "Taylor Swift");
        let s3 = similarity("언프리티 랩스타 3 1차 공연", "", "언프리티 랩스타 3 컴필레이션", "");
        assert!(s1 > s3 && s3 > s2, "{s1} {s3} {s2}");
        // 검색어는 악센트를 무시한다
        assert!(query_score("pokemon", "Nintendo Switch Pokémon Scarlet・Violet", "Various Artists") > 1.0);
    }

    /// 실제 검색. PODLIFT_NET_TEST 가 있을 때만 돈다.
    #[test]
    fn search_online() {
        if std::env::var("PODLIFT_NET_TEST").is_err() {
            return;
        }
        let found = search("2NE1 2nd Mini Album", "2NE1").unwrap().expect("2NE1 2집 미니앨범 표지");
        println!("{} {}", found.from, found.file.display());
        assert!(image::open(&found.file).is_ok());
        assert!(search("없는 앨범 이름 zzqq 12345", "없는 가수").unwrap().is_none());
    }
}

/// 여러 앨범을 차례로 찾을 때 백그라운드 스레드가 보내는 소식
pub enum BatchMsg {
    /// (찾은 수, 전체)
    Progress(usize, usize),
    /// 목록 위치와 찾은 그림
    Found(usize, Found),
    /// 끝. 네트워크 오류가 있었으면 그 내용
    Done { found: usize, error: Option<String> },
}

/// (목록 위치, 앨범, 가수)들을 차례로 찾는다. 중지하려면 받는 쪽을 버리면 된다.
pub fn start_batch(queries: Vec<(usize, String, String)>) -> std::sync::mpsc::Receiver<BatchMsg> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let total = queries.len();
        let (mut found, mut error) = (0, None);
        for (n, (idx, album, artist)) in queries.into_iter().enumerate() {
            match search(&album, &artist) {
                Ok(Some(f)) => {
                    found += 1;
                    if tx.send(BatchMsg::Found(idx, f)).is_err() {
                        return;
                    }
                }
                Ok(None) => {}
                Err(e) => error = Some(e),
            }
            if tx.send(BatchMsg::Progress(n + 1, total)).is_err() {
                return;
            }
        }
        let _ = tx.send(BatchMsg::Done { found, error });
    });
    rx
}
