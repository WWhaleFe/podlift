//! 아이팟 곡 DB(iPod_Control/iTunes/iTunesDB) 읽기·고치기·쓰기. 아이팟 클래식(hash58 서명) 기준.
//! 형식은 libgpod(itdb_itunesdb.c, itdb_hash58.c)를 따랐다.
//!
//! 파일은 "mh??" 청크의 나무다. 모든 청크는 [태그 4][머리 길이 4][세 번째 칸 4]로 시작하고,
//! 세 번째 칸은 목록 청크(mhlt·mhlp·mhla·mhli)면 자식 수, 그 밖에는 머리+본문 전체 길이다.
//!   mhbd(머리) → mhsd(구역, 0x0C = 종류) × N
//!     종류 1: mhlt → mhit(곡) → mhod(문자열 등)
//!     종류 2: mhlp → mhyp(재생 목록) → mhod…, mhip(곡 참조) → mhod 100(위치)
//!     종류 3: 종류 2 와 같은 꼴(팟캐스트 묶음판)
//!     종류 4: mhla → mhia(앨범) → mhod 200/201
//!     종류 8: mhli → mhii(가수) → mhod 300
//!
//! 안전을 위해 DB 전체를 새로 만들지 않고, 읽은 나무를 그대로 두고 필요한 곳만 고친다.
//! 열 때 "읽은 그대로 다시 쓰면 원본과 한 바이트도 다르지 않은지"를 확인해, 다르면 쓰기를 막는다.
//! 곡은 언제나 목록 끝에 덧붙이므로 기존 곡의 위치(재생 횟수 파일·On-The-Go 목록이 쓰는 번호)가 바뀌지 않는다.
//! 곡을 뺄 때(중복 정리)는 뺀 위치를 돌려주어 재생 횟수 파일도 같이 고친다.
use sha1::{Digest, Sha1};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum Node {
    Chunk(Chunk),
    /// 청크로 해석되지 않는 바이트 (모르는 형식의 본문). 그대로 다시 쓴다.
    Raw(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Chunk {
    /// 태그·길이 칸을 포함한 머리 바이트 전체. 길이 칸은 쓸 때 다시 채운다.
    pub header: Vec<u8>,
    pub body: Body,
}

#[derive(Clone, Debug)]
pub enum Body {
    /// 머리 뒤에 자식 청크가 이어진다 (세 번째 칸 = 전체 길이)
    Children(Vec<Node>),
    /// 목록 청크: 세 번째 칸 = 자식 수
    List(Vec<Node>),
    /// mhod: 머리 뒤 본문 바이트
    Leaf(Vec<u8>),
}

/// 목록 청크 (mhlf 는 그림 DB(ArtworkDB)의 그림 형식 목록)
const LIST_TAGS: [&[u8; 4]; 5] = [b"mhlt", b"mhlp", b"mhla", b"mhli", b"mhlf"];

// mhod 종류
pub const MHOD_TITLE: u32 = 1;
pub const MHOD_PATH: u32 = 2;
pub const MHOD_ALBUM: u32 = 3;
pub const MHOD_ARTIST: u32 = 4;
pub const MHOD_GENRE: u32 = 5;
pub const MHOD_FILETYPE: u32 = 6;
pub const MHOD_COMPOSER: u32 = 12;
pub const MHOD_ALBUMARTIST: u32 = 22;
pub const MHOD_SORT_ARTIST: u32 = 23;
pub const MHOD_SORT_TITLE: u32 = 27;
pub const MHOD_SORT_ALBUM: u32 = 28;
pub const MHOD_SORT_COMPOSER: u32 = 30;
const MHOD_LIBINDEX: u32 = 52;
const MHOD_JUMPTABLE: u32 = 53;
const MHOD_PLAYLIST_POS: u32 = 100;
const MHOD_ALBUM_ALBUM: u32 = 200;
const MHOD_ALBUM_ARTIST: u32 = 201;
const MHOD_ARTIST_MHII: u32 = 300;

/// 1904-01-01 과 1970-01-01 사이의 초
const MAC_EPOCH_OFFSET: i64 = 2_082_844_800;

pub(crate) fn rd32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).unwrap_or(0)
}

pub(crate) fn wr32(b: &mut [u8], at: usize, v: u32) {
    if let Some(s) = b.get_mut(at..at + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

pub(crate) fn wr16(b: &mut [u8], at: usize, v: u16) {
    if let Some(s) = b.get_mut(at..at + 2) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

pub(crate) fn wr64(b: &mut [u8], at: usize, v: u64) {
    if let Some(s) = b.get_mut(at..at + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

impl Chunk {
    pub fn tag(&self) -> &[u8] {
        &self.header[..4]
    }

    pub fn is(&self, tag: &[u8; 4]) -> bool {
        self.tag() == tag
    }

    pub fn u32_at(&self, at: usize) -> u32 {
        rd32(&self.header, at)
    }

    pub fn set_u32(&mut self, at: usize, v: u32) {
        wr32(&mut self.header, at, v);
    }

    pub(crate) fn new(tag: &[u8; 4], header_len: usize, body: Body) -> Self {
        let mut header = vec![0u8; header_len];
        header[..4].copy_from_slice(tag);
        wr32(&mut header, 4, header_len as u32);
        Chunk { header, body }
    }

    pub fn children(&self) -> &[Node] {
        match &self.body {
            Body::Children(c) | Body::List(c) => c,
            Body::Leaf(_) => &[],
        }
    }

    pub fn children_mut(&mut self) -> Option<&mut Vec<Node>> {
        match &mut self.body {
            Body::Children(c) | Body::List(c) => Some(c),
            Body::Leaf(_) => None,
        }
    }

    pub fn chunks(&self) -> impl Iterator<Item = &Chunk> {
        self.children().iter().filter_map(|n| match n {
            Node::Chunk(c) => Some(c),
            Node::Raw(_) => None,
        })
    }

    /// mhod 의 종류 (mhod 가 아니면 0)
    pub fn mhod_type(&self) -> u32 {
        if self.is(b"mhod") { self.u32_at(12) } else { 0 }
    }

    /// 문자열 mhod 의 글자 (UTF-16LE 또는 UTF-8)
    pub fn mhod_string(&self) -> Option<String> {
        let Body::Leaf(body) = &self.body else { return None };
        if body.len() < 16 {
            return None;
        }
        let enc = rd32(body, 0);
        let len = rd32(body, 4) as usize;
        let bytes = body.get(16..16 + len)?;
        Some(if enc == 2 {
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        })
    }

    /// 자식 mhod 중 종류가 같은 첫 문자열
    pub fn string(&self, typ: u32) -> String {
        self.chunks().find(|c| c.mhod_type() == typ).and_then(|c| c.mhod_string()).unwrap_or_default()
    }
}

/// 문자열 mhod 를 만든다 (UTF-16LE)
pub fn string_mhod(typ: u32, s: &str) -> Node {
    let utf16: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut body = vec![0u8; 16];
    wr32(&mut body, 0, 1);
    wr32(&mut body, 4, utf16.len() as u32);
    wr32(&mut body, 8, 1);
    body.extend_from_slice(&utf16);
    let mut c = Chunk::new(b"mhod", 24, Body::Leaf(body));
    c.set_u32(12, typ);
    Node::Chunk(c)
}

fn mhod_with_body(typ: u32, body: Vec<u8>) -> Node {
    let mut c = Chunk::new(b"mhod", 24, Body::Leaf(body));
    c.set_u32(12, typ);
    Node::Chunk(c)
}

// ── 읽기·쓰기 ──────────────────────────────────────────

fn parse_chunk(data: &[u8], pos: usize) -> Result<(Chunk, usize), String> {
    let tag = data.get(pos..pos + 4).ok_or("청크가 파일 끝을 넘습니다")?;
    if &tag[..2] != b"mh" {
        return Err(format!("{pos:#x} 위치에 청크가 없습니다"));
    }
    let hlen = rd32(data, pos + 4) as usize;
    let third = rd32(data, pos + 8) as usize;
    if hlen < 12 || pos + hlen > data.len() {
        return Err(format!("{pos:#x} 위치 청크의 머리 길이가 이상합니다"));
    }
    let header = data[pos..pos + hlen].to_vec();
    let tag4: [u8; 4] = tag.try_into().unwrap();
    if LIST_TAGS.contains(&&tag4) {
        let mut children = Vec::with_capacity(third);
        let mut at = pos + hlen;
        for _ in 0..third {
            let (c, end) = parse_chunk(data, at)?;
            children.push(Node::Chunk(c));
            at = end;
        }
        return Ok((Chunk { header, body: Body::List(children) }, at));
    }
    if third < hlen || pos + third > data.len() {
        return Err(format!("{pos:#x} 위치 청크의 전체 길이가 이상합니다"));
    }
    let end = pos + third;
    if &tag4 == b"mhod" {
        return Ok((Chunk { header, body: Body::Leaf(data[pos + hlen..end].to_vec()) }, end));
    }
    // 자식 청크를 차례로 읽고, 해석 못 하는 나머지는 날바이트로 둔다.
    let mut children = Vec::new();
    let mut at = pos + hlen;
    while at < end {
        match parse_chunk(&data[..end], at) {
            Ok((c, next)) => {
                children.push(Node::Chunk(c));
                at = next;
            }
            Err(_) => {
                children.push(Node::Raw(data[at..end].to_vec()));
                at = end;
            }
        }
    }
    Ok((Chunk { header, body: Body::Children(children) }, end))
}

pub(crate) fn write_node(node: &Node, out: &mut Vec<u8>) {
    match node {
        Node::Raw(b) => out.extend_from_slice(b),
        Node::Chunk(c) => {
            let start = out.len();
            out.extend_from_slice(&c.header);
            match &c.body {
                Body::Leaf(b) => {
                    out.extend_from_slice(b);
                    let total = (out.len() - start) as u32;
                    wr32(out, start + 8, total);
                }
                Body::Children(ch) => {
                    ch.iter().for_each(|n| write_node(n, out));
                    let total = (out.len() - start) as u32;
                    wr32(out, start + 8, total);
                }
                Body::List(ch) => {
                    ch.iter().for_each(|n| write_node(n, out));
                    wr32(out, start + 8, ch.len() as u32);
                }
            }
        }
    }
}

/// 바이트 전체를 청크 나무 하나로 읽고, 다시 쓴 결과가 원본과 한 바이트도 다르지 않은지 확인한다.
/// 곡 DB(mhbd)와 그림 DB(mhfd)가 함께 쓴다.
pub(crate) fn parse_tree(data: &[u8], root_tag: &[u8; 4]) -> Result<Chunk, String> {
    if data.get(..4) != Some(&root_tag[..]) {
        return Err(format!("{} 머리가 없습니다", String::from_utf8_lossy(root_tag)));
    }
    let (root, end) = parse_chunk(data, 0)?;
    if end != data.len() {
        return Err(format!("끝에 해석하지 못한 {} 바이트가 있습니다", data.len() - end));
    }
    let mut again = Vec::with_capacity(data.len());
    write_node(&Node::Chunk(root.clone()), &mut again);
    if again != data {
        let at = again.iter().zip(data).position(|(a, b)| a != b).unwrap_or(again.len().min(data.len()));
        return Err(format!("그대로 다시 쓸 수 없습니다 ({at:#x} 위치부터 다름). 안전을 위해 쓰지 않습니다"));
    }
    Ok(root)
}

// ── 곡 정보 ────────────────────────────────────────────

/// DB 에 든 곡 한 개 (화면·중복 판단용)
#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // 앨범 가수·파일 형식·크기는 아직 화면에 쓰지 않지만 DB 에서 함께 읽어 둔다
pub struct DbTrack {
    pub id: u32,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub genre: String,
    pub composer: String,
    /// ":iPod_Control:Music:F01:ABCD.mp3"
    pub location: String,
    pub filetype: String,
    pub size: u32,
    pub length_ms: u32,
    pub bitrate: u32,
    pub sample_rate: u32,
    pub track_nr: u32,
    pub cd_nr: u32,
    pub lossless: bool,
    /// 곡 고유 번호(64비트). 그림 DB 의 mhii 가 이것으로 곡을 가리킨다.
    pub dbid: u64,
    /// 연결된 그림 번호 (mhit 0x160, 없으면 0)
    pub mhii_link: u32,
    pub has_artwork: bool,
    pub sort_title: String,
    pub sort_artist: String,
    pub sort_album: String,
    pub sort_composer: String,
}

impl DbTrack {
    fn from_mhit(c: &Chunk) -> Self {
        let marker = c.u32_at(0x18).to_be_bytes();
        let filetype = c.string(MHOD_FILETYPE);
        let lossless = filetype.to_lowercase().contains("lossless")
            || matches!(&marker, b"WAV " | b"AIF " | b"AIFF");
        DbTrack {
            id: c.u32_at(0x10),
            title: c.string(MHOD_TITLE),
            artist: c.string(MHOD_ARTIST),
            album: c.string(MHOD_ALBUM),
            album_artist: c.string(MHOD_ALBUMARTIST),
            genre: c.string(MHOD_GENRE),
            composer: c.string(MHOD_COMPOSER),
            location: c.string(MHOD_PATH),
            filetype,
            size: c.u32_at(0x24),
            length_ms: c.u32_at(0x28),
            track_nr: c.u32_at(0x2C),
            bitrate: c.u32_at(0x38),
            sample_rate: c.u32_at(0x3C) >> 16,
            cd_nr: c.u32_at(0x5C),
            lossless,
            dbid: c.header.get(0x70..0x78).map(|s| u64::from_le_bytes(s.try_into().unwrap())).unwrap_or(0),
            mhii_link: if c.header.len() >= 0x164 { c.u32_at(0x160) } else { 0 },
            has_artwork: c.header.get(0xA4) == Some(&1),
            sort_title: c.string(MHOD_SORT_TITLE),
            sort_artist: c.string(MHOD_SORT_ARTIST),
            sort_album: c.string(MHOD_SORT_ALBUM),
            sort_composer: c.string(MHOD_SORT_COMPOSER),
        }
    }

    /// 아이팟 안의 실제 파일 경로 (마운트 위치 기준)
    pub fn file_path(&self, mount: &std::path::Path) -> Option<std::path::PathBuf> {
        let rel = self.location.trim_start_matches(':');
        if rel.is_empty() {
            return None;
        }
        Some(rel.split(':').fold(mount.to_path_buf(), |p, part| p.join(part)))
    }
}

/// 새로 넣을 곡의 정보
#[derive(Clone, Debug, Default)]
pub struct NewTrack {
    /// ":iPod_Control:Music:F12:ABCD.mp3"
    pub location: String,
    /// 소문자 확장자 (mp3, m4a, wav, aif…)
    pub ext: String,
    /// ALAC 이면 true (m4a 안이 무손실)
    pub alac: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub genre: String,
    pub composer: String,
    pub track_nr: u32,
    pub track_total: u32,
    pub cd_nr: u32,
    pub cd_total: u32,
    pub year: u32,
    pub compilation: bool,
    pub size: u32,
    pub length_ms: u32,
    pub bitrate: u32,
    pub sample_rate: u32,
}

impl NewTrack {
    /// mhod 6 에 넣는 설명 (iTunes 가 쓰는 문구)
    fn filetype_desc(&self) -> &'static str {
        match self.ext.as_str() {
            "mp3" => "MPEG audio file",
            "m4a" if self.alac => "Apple Lossless audio file",
            "m4a" | "m4b" | "aac" => "AAC audio file",
            "wav" => "WAV audio file",
            "aif" | "aiff" | "aifc" => "AIFF audio file",
            _ => "Audio file",
        }
    }

    /// 확장자로 만든 4글자 표지 ('MP3 ', 'M4A ')
    fn marker(&self) -> u32 {
        let mut m = [b' '; 4];
        for (i, b) in self.ext.bytes().take(4).enumerate() {
            m[i] = b.to_ascii_uppercase();
        }
        u32::from_be_bytes(m)
    }

    /// 파일 형식에 따라 정해진 칸들(표지, type1/2, unk126, unk144, 크기·길이·음질)을 mhit 머리에 쓴다.
    fn write_file_fields(&self, h: &mut [u8]) {
        let mp3 = self.ext == "mp3";
        let lossy_mp4 = matches!(self.ext.as_str(), "m4a" | "m4b" | "aac") && !self.alac;
        wr32(h, 0x18, self.marker());
        h[0x1C] = 0; // type1: 0 = CBR MP3 / AAC
        h[0x1D] = u8::from(mp3); // type2: 1 = MP3
        wr32(h, 0x24, self.size);
        wr32(h, 0x28, self.length_ms);
        wr32(h, 0x38, self.bitrate);
        wr32(h, 0x3C, self.sample_rate << 16);
        wr32(h, 0x4C, 0); // soundcheck: 새 파일 기준으로 아이팟이 다시 계산하지 않으므로 0
        if h.len() >= 0x80 {
            wr16(h, 0x7E, if mp3 || lossy_mp4 { 0xffff } else { 0 });
        }
        if h.len() >= 0x8C {
            h[0x88..0x8C].copy_from_slice(&(self.sample_rate as f32).to_le_bytes());
        }
        if h.len() >= 0x92 {
            wr16(h, 0x90, if mp3 { 0x000c } else if lossy_mp4 { 0x0033 } else { 0 });
        }
        if h.len() >= 0xCC {
            // 무간격 재생 정보는 원래 파일 것이라 지운다
            wr32(h, 0xB8, 0);
            wr64(h, 0xBC, 0);
            wr32(h, 0xC8, 0);
        }
        if h.len() >= 0xFC {
            wr32(h, 0xF8, 0);
        }
        if h.len() >= 0x130 {
            wr32(h, 0x12C, self.size);
        }
    }
}

/// 1904년 기준 현지 시각 (아이팟 DB 의 시각 표기)
pub fn mac_time_now() -> u32 {
    let now = chrono::Local::now();
    let local = now.timestamp() + now.offset().local_minus_utc() as i64;
    (local + MAC_EPOCH_OFFSET) as u32
}

// ── DB ──────────────────────────────────────────────────

pub struct ITunesDb {
    root: Chunk,
    /// 처음 읽었을 때 기본 재생 목록(mhsd 2·3)의 곡 번호 순서. 색인을 고칠 때 기존 순서를 읽는 데 쓴다.
    orig_masters: HashMap<u32, Vec<u32>>,
}

/// 앨범 묶음 열쇠 (앨범, 앨범 가수 없으면 가수) 소문자
fn album_key(album: &str, artist: &str) -> (String, String) {
    (album.to_lowercase(), artist.to_lowercase())
}

impl ITunesDb {
    /// 바이트를 읽어 나무를 만들고, 다시 쓴 결과가 원본과 같은지 확인한다.
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        let root = parse_tree(data, b"mhbd").map_err(|e| format!("iTunesDB: {e}"))?;
        let mut db = ITunesDb { root, orig_masters: HashMap::new() };
        for typ in [2, 3] {
            if let Some(ids) = db.master_ids(typ) {
                db.orig_masters.insert(typ, ids);
            }
        }
        Ok(db)
    }

    /// 서명 방식 (0 = 없음, 1 = hash58, 2 = hash72, 4 = hashAB)
    pub fn hashing_scheme(&self) -> u16 {
        let h = &self.root.header;
        let scheme = h.get(0x30..0x32).map(|s| u16::from_le_bytes([s[0], s[1]])).unwrap_or(0);
        let has58 = h.get(0x58..0x6C).is_some_and(|s| s.iter().any(|&b| b != 0));
        if scheme == 0 && has58 { 1 } else { scheme }
    }

    fn mhsd(&self, typ: u32) -> Option<&Chunk> {
        self.root.chunks().find(|c| c.is(b"mhsd") && c.u32_at(12) == typ)
    }

    fn mhsd_list_mut(&mut self, typ: u32) -> Option<&mut Vec<Node>> {
        let mhsd = self.root.children_mut()?.iter_mut().find_map(|n| match n {
            Node::Chunk(c) if c.is(b"mhsd") && c.u32_at(12) == typ => Some(c),
            _ => None,
        })?;
        let list = mhsd.children_mut()?.iter_mut().find_map(|n| match n {
            Node::Chunk(c) if matches!(c.body, Body::List(_)) => Some(c),
            _ => None,
        })?;
        list.children_mut()
    }

    fn mhits(&self) -> impl Iterator<Item = &Chunk> {
        self.mhsd(1)
            .and_then(|s| s.chunks().find(|c| c.is(b"mhlt")))
            .into_iter()
            .flat_map(|l| l.chunks())
            .filter(|c| c.is(b"mhit"))
    }

    pub fn tracks(&self) -> Vec<DbTrack> {
        self.mhits().map(DbTrack::from_mhit).collect()
    }

    /// 곡 하나를 목록 끝에 덧붙이고, 기본 재생 목록(전체 곡)에도 넣는다. 새 곡 번호를 돌려준다.
    pub fn add_track(&mut self, t: &NewTrack) -> Result<u32, String> {
        let id = self.mhits().map(|c| c.u32_at(0x10)).max().unwrap_or(0).max(51) + 1;
        let album_artist = if t.album_artist.is_empty() { &t.artist } else { &t.album_artist };
        let album_id = if t.album.is_empty() { 0 } else { self.album_id(&t.album, album_artist) };
        let artist_id = if t.artist.is_empty() { 0 } else { self.artist_id(&t.artist) };
        let id_0x24 = self.root.header.get(0x24..0x2C).map(|s| u64::from_le_bytes(s.try_into().unwrap())).unwrap_or(0);
        let now = mac_time_now();
        let dbid: u64 = rand::random();

        let mut h = vec![0u8; 0x248];
        h[..4].copy_from_slice(b"mhit");
        wr32(&mut h, 4, 0x248);
        wr32(&mut h, 0x10, id);
        wr32(&mut h, 0x14, 1); // 보이기
        h[0x1E] = u8::from(t.compilation);
        wr32(&mut h, 0x20, now); // 수정 시각
        wr32(&mut h, 0x2C, t.track_nr);
        wr32(&mut h, 0x30, t.track_total);
        wr32(&mut h, 0x34, t.year);
        wr32(&mut h, 0x5C, t.cd_nr);
        wr32(&mut h, 0x60, t.cd_total);
        wr32(&mut h, 0x68, now); // 추가한 시각
        wr64(&mut h, 0x70, dbid);
        h[0xA4] = 0x02; // 앨범 아트 없음
        wr64(&mut h, 0xA8, dbid);
        h[0xB2] = 0x01; // '안 들은 곡' 점 표시 안 함
        wr32(&mut h, 0xD0, 1); // 오디오
        wr32(&mut h, 0x120, album_id);
        wr64(&mut h, 0x124, id_0x24);
        wr64(&mut h, 0x134, 0x808080808080);
        wr32(&mut h, 0x168, 1);
        wr32(&mut h, 0x1E0, artist_id);
        t.write_file_fields(&mut h);

        let mut mhods = vec![string_mhod(MHOD_TITLE, &t.title), string_mhod(MHOD_PATH, &t.location)];
        for (typ, v) in [
            (MHOD_ALBUM, &t.album),
            (MHOD_ARTIST, &t.artist),
            (MHOD_GENRE, &t.genre),
            (MHOD_COMPOSER, &t.composer),
            (MHOD_ALBUMARTIST, &t.album_artist),
        ] {
            if !v.is_empty() {
                mhods.push(string_mhod(typ, v));
            }
        }
        mhods.push(string_mhod(MHOD_FILETYPE, t.filetype_desc()));
        wr32(&mut h, 0x0C, mhods.len() as u32);
        let mhit = Chunk { header: h, body: Body::Children(mhods) };

        self.mhsd_list_mut(1).ok_or("곡 목록(mhsd 1)이 없습니다")?.push(Node::Chunk(mhit));
        for typ in [2, 3] {
            if let Some(mpl) = self.master_playlist_mut(typ) {
                append_mhip(mpl, id);
            }
        }
        Ok(id)
    }

    /// 기존 곡의 파일만 새 파일로 바꾼다. 번호·재생 횟수·별점·재생 목록은 그대로 둔다.
    pub fn replace_file(&mut self, id: u32, t: &NewTrack) -> Result<(), String> {
        let list = self.mhsd_list_mut(1).ok_or("곡 목록(mhsd 1)이 없습니다")?;
        let mhit = list
            .iter_mut()
            .find_map(|n| match n {
                Node::Chunk(c) if c.is(b"mhit") && c.u32_at(0x10) == id => Some(c),
                _ => None,
            })
            .ok_or(format!("{id}번 곡을 찾지 못했습니다"))?;
        t.write_file_fields(&mut mhit.header);
        wr32(&mut mhit.header, 0x20, mac_time_now());
        let children = mhit.children_mut().ok_or("곡 레코드 형식이 이상합니다")?;
        for (typ, value) in [(MHOD_PATH, t.location.as_str()), (MHOD_FILETYPE, t.filetype_desc())] {
            let fresh = string_mhod(typ, value);
            match children.iter_mut().find(|n| matches!(n, Node::Chunk(c) if c.mhod_type() == typ)) {
                Some(slot) => *slot = fresh,
                None => children.push(fresh),
            }
        }
        let count = children.iter().filter(|n| matches!(n, Node::Chunk(c) if c.is(b"mhod"))).count();
        wr32(&mut mhit.header, 0x0C, count as u32);
        Ok(())
    }

    /// 곡에 그림 DB 의 그림을 연결한다. image_id 가 0 이면 연결을 지운다.
    /// art_size 는 원본 그림 파일의 바이트 수 (iTunes 처럼 기록만 한다).
    pub fn set_artwork(&mut self, track_id: u32, image_id: u32, art_size: u32) -> Result<(), String> {
        let list = self.mhsd_list_mut(1).ok_or("곡 목록(mhsd 1)이 없습니다")?;
        let mhit = list
            .iter_mut()
            .find_map(|n| match n {
                Node::Chunk(c) if c.is(b"mhit") && c.u32_at(0x10) == track_id => Some(c),
                _ => None,
            })
            .ok_or(format!("{track_id}번 곡을 찾지 못했습니다"))?;
        let h = &mut mhit.header;
        if h.len() < 0x164 {
            return Err("곡 레코드가 짧아 그림을 연결할 수 없습니다 (오래된 DB 형식)".into());
        }
        let on = image_id != 0;
        wr16(h, 0x7C, u16::from(on)); // 그림 수
        wr32(h, 0x80, if on { art_size } else { 0 });
        h[0xA4] = if on { 0x01 } else { 0x02 };
        wr32(h, 0x160, image_id);
        Ok(())
    }

    /// 앨범 목록(mhsd 4)에서 앨범 번호를 찾고, 없으면 새로 만든다. 앨범 목록이 없는 DB 면 0.
    fn album_id(&mut self, album: &str, artist: &str) -> u32 {
        let Some(list) = self.mhsd_list_mut(4) else { return 0 };
        let key = album_key(album, artist);
        let mut max = 0;
        for n in list.iter() {
            if let Node::Chunk(c) = n {
                if c.is(b"mhia") {
                    max = max.max(c.u32_at(0x10));
                    if album_key(&c.string(MHOD_ALBUM_ALBUM), &c.string(MHOD_ALBUM_ARTIST)) == key {
                        return c.u32_at(0x10);
                    }
                }
            }
        }
        let id = max + 1;
        let mut c = Chunk::new(b"mhia", 88, Body::Children(vec![string_mhod(MHOD_ALBUM_ALBUM, album), string_mhod(MHOD_ALBUM_ARTIST, artist)]));
        c.set_u32(0x0C, 2);
        c.set_u32(0x10, id);
        wr64(&mut c.header, 0x14, rand::random());
        c.set_u32(0x1C, 2);
        list.push(Node::Chunk(c));
        id
    }

    /// 가수 목록(mhsd 8)에서 가수 번호를 찾고, 없으면 새로 만든다. 가수 목록이 없는 DB 면 0.
    fn artist_id(&mut self, artist: &str) -> u32 {
        let Some(list) = self.mhsd_list_mut(8) else { return 0 };
        let key = artist.to_lowercase();
        let mut max = 0;
        for n in list.iter() {
            if let Node::Chunk(c) = n {
                if c.is(b"mhii") {
                    max = max.max(c.u32_at(0x10));
                    if c.string(MHOD_ARTIST_MHII).to_lowercase() == key {
                        return c.u32_at(0x10);
                    }
                }
            }
        }
        let id = max + 1;
        let mut c = Chunk::new(b"mhii", 80, Body::Children(vec![string_mhod(MHOD_ARTIST_MHII, artist)]));
        c.set_u32(0x0C, 1);
        c.set_u32(0x10, id);
        wr64(&mut c.header, 0x14, rand::random());
        c.set_u32(0x1C, 2);
        list.push(Node::Chunk(c));
        id
    }

    /// 기본 재생 목록(전체 곡, mhyp 0x14 = 1)
    fn master_playlist_mut(&mut self, mhsd_type: u32) -> Option<&mut Chunk> {
        self.mhsd_list_mut(mhsd_type)?.iter_mut().find_map(|n| match n {
            Node::Chunk(c) if c.is(b"mhyp") && c.header.get(0x14) == Some(&1) => Some(c),
            _ => None,
        })
    }

    /// 기본 재생 목록의 곡 번호 (mhip 순서)
    fn master_ids(&self, mhsd_type: u32) -> Option<Vec<u32>> {
        let mpl = self.mhsd(mhsd_type)?.chunks().next()?.chunks().find(|c| c.is(b"mhyp") && c.header.get(0x14) == Some(&1))?;
        Some(mpl.chunks().filter(|c| c.is(b"mhip")).map(|c| c.u32_at(0x18)).collect())
    }

    /// 곡을 DB 에서 뺀다. pairs 는 (뺄 곡, 대신 남길 곡) 번호 쌍. 남길 곡이 None 이면(같은 곡을 모두 지움)
    /// 재생 목록에서도 그냥 빠진다. 남길 곡이 있으면 재생 목록의 뺄 곡 자리를 남길 곡으로 바꾸고(이미 있으면 지운다),
    /// 재생·건너뛴 횟수는 남길 곡에 더하고 별점·마지막 재생 시각은 큰 쪽을 남긴다. 곡이 하나도 남지 않은 앨범·가수 항목은 지운다.
    /// 돌려주는 값: 곡 목록(mhlt)에서 뺀 곡들의 원래 위치 (재생 횟수 파일 정리용, 오름차순)
    pub fn remove_tracks(&mut self, pairs: &[(u32, Option<u32>)]) -> Result<Vec<usize>, String> {
        let replace: HashMap<u32, Option<u32>> = pairs.iter().copied().collect();
        let existing: std::collections::HashSet<u32> = self.mhits().map(|c| c.u32_at(0x10)).collect();
        for (&gone, &keep) in &replace {
            if !existing.contains(&gone) || keep.is_some_and(|k| !existing.contains(&k)) {
                return Err(format!("{gone}번 곡 또는 남길 곡이 DB 에 없습니다"));
            }
            if let Some(k) = keep {
                if replace.contains_key(&k) || gone == k {
                    return Err(format!("{k}번 곡은 남길 곡이면서 뺄 곡입니다"));
                }
            }
        }

        // 1) 재생 기록을 남길 곡으로 합치고 곡 레코드를 뺀다
        let list = self.mhsd_list_mut(1).ok_or("곡 목록(mhsd 1)이 없습니다")?;
        let mut stats: HashMap<u32, Vec<[u32; 6]>> = HashMap::new();
        let mut removed_pos = Vec::new();
        for (pos, n) in list.iter().enumerate() {
            if let Node::Chunk(c) = n {
                let id = c.u32_at(0x10);
                if let Some(&keep) = replace.get(&id) {
                    removed_pos.push(pos);
                    if let (Some(keep), true) = (keep, c.header.len() >= 0xA4) {
                        stats.entry(keep).or_default().push(play_stats(&c.header));
                    }
                }
            }
        }
        list.retain(|n| !matches!(n, Node::Chunk(c) if c.is(b"mhit") && replace.contains_key(&c.u32_at(0x10))));
        for n in list.iter_mut() {
            let Node::Chunk(c) = n else { continue };
            let Some(more) = stats.get(&c.u32_at(0x10)) else { continue };
            let mut s = play_stats(&c.header);
            for m in more {
                s[0] = s[0].saturating_add(m[0]); // 재생 횟수
                s[1] = s[1].saturating_add(m[1]); // 재생 횟수(마지막 동기화 뒤)
                s[2] = s[2].max(m[2]); // 마지막 재생 시각
                s[3] = s[3].saturating_add(m[3]); // 건너뛴 횟수
                s[4] = s[4].max(m[4]); // 마지막으로 건너뛴 시각
                s[5] = s[5].max(m[5]); // 별점
            }
            set_play_stats(&mut c.header, s);
        }

        // 2) 재생 목록 (전체 곡 목록·팟캐스트판·스마트 목록 구역 모두)
        for typ in [2, 3, 5] {
            let Some(playlists) = self.mhsd_list_mut(typ) else { continue };
            for n in playlists.iter_mut() {
                let Node::Chunk(pl) = n else { continue };
                if !pl.is(b"mhyp") {
                    continue;
                }
                let master = pl.header.get(0x14) == Some(&1);
                let Some(children) = pl.children_mut() else { continue };
                let mut present: std::collections::HashSet<u32> =
                    children.iter().filter_map(|n| match n { Node::Chunk(c) if c.is(b"mhip") => Some(c.u32_at(0x18)), _ => None }).collect();
                children.retain_mut(|n| {
                    let Node::Chunk(c) = n else { return true };
                    if !c.is(b"mhip") {
                        return true;
                    }
                    let Some(&keep) = replace.get(&c.u32_at(0x18)) else { return true };
                    let Some(keep) = keep else { return false };
                    if master || present.contains(&keep) {
                        return false;
                    }
                    c.set_u32(0x18, keep);
                    present.insert(keep);
                    true
                });
                let count = children.iter().filter(|n| matches!(n, Node::Chunk(c) if c.is(b"mhip"))).count() as u32;
                pl.set_u32(0x10, count);
            }
        }

        // 3) 곡이 남지 않은 앨범(mhsd 4, mhit 0x120)·가수(mhsd 8, mhit 0x1E0) 항목
        let field = |c: &Chunk, at: usize| (c.header.len() >= at + 4).then(|| c.u32_at(at));
        let albums: std::collections::HashSet<u32> = self.mhits().filter_map(|c| field(c, 0x120)).collect();
        let artists: std::collections::HashSet<u32> = self.mhits().filter_map(|c| field(c, 0x1E0)).collect();
        for (typ, tag, used) in [(4, b"mhia", &albums), (8, b"mhii", &artists)] {
            if let Some(list) = self.mhsd_list_mut(typ) {
                list.retain(|n| !matches!(n, Node::Chunk(c) if c.is(tag) && !used.contains(&c.u32_at(0x10))));
            }
        }
        Ok(removed_pos)
    }

    /// 기본 재생 목록의 정렬 색인(mhod 52)과 글자 건너뛰기 표(mhod 53)를 고친다.
    /// 아이팟은 이것으로 '노래·가수·앨범·장르·작곡가' 메뉴를 그린다.
    /// 곡 구성이 처음 읽었을 때와 같으면 손대지 않는다. 이미 있는 색인(iTunes·Finder 가 만든 것)은
    /// 그 순서를 그대로 두고 뺀 곡만 지우고 새 곡만 알맞은 자리에 끼운다(Apple 정렬은 한자를 한글 읽기로
    /// 놓는 등 똑같이 흉내 내기 어렵다). 색인이 없거나 읽을 수 없으면 처음부터 만든다.
    fn rebuild_indexes(&mut self) {
        let keys: HashMap<u32, SortKey> = self.tracks().iter().map(|t| (t.id, SortKey::of(t))).collect();
        for typ in [2, 3] {
            let Some(members) = self.master_ids(typ) else { continue };
            let orig = self.orig_masters.get(&typ).cloned();
            if orig.as_ref() == Some(&members) {
                continue;
            }
            // 고친 뒤의 색인은 지금 곡 구성을 기준으로 한다 (to_bytes 를 다시 불러도 같은 결과)
            self.orig_masters.insert(typ, members.clone());
            let Some(mpl) = self.master_playlist_mut(typ) else { continue };
            if let Some(orig) = orig {
                if update_indexes(mpl, &orig, &members, &keys) {
                    continue;
                }
            }
            let ordered: Vec<(u32, &SortKey)> = members.iter().filter_map(|id| keys.get(id).map(|k| (*id, k))).collect();
            let fresh = index_mhods(&ordered);
            let Some(children) = mpl.children_mut() else { continue };
            let mut mhods: Vec<Node> = Vec::new();
            let mut rest: Vec<Node> = Vec::new();
            for n in children.drain(..) {
                match &n {
                    Node::Chunk(c) if matches!(c.mhod_type(), MHOD_LIBINDEX | MHOD_JUMPTABLE) => {}
                    Node::Chunk(c) if c.is(b"mhod") => mhods.push(n),
                    _ => rest.push(n),
                }
            }
            if !members.is_empty() {
                mhods.extend(fresh);
            }
            let mhod_count = mhods.len() as u32;
            let mhip_count = rest.iter().filter(|n| matches!(n, Node::Chunk(c) if c.is(b"mhip"))).count() as u32;
            children.extend(mhods);
            children.extend(rest);
            mpl.set_u32(0x0C, mhod_count);
            mpl.set_u32(0x10, mhip_count);
        }
    }

    /// 색인을 다시 만들고 바이트로 쓴다. 서명이 필요한 DB 면 firewire_id 로 hash58 을 넣는다.
    pub fn to_bytes(&mut self, firewire_id: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let scheme = self.hashing_scheme();
        if scheme > 1 {
            return Err(format!("이 아이팟은 지원하지 않는 서명 방식({scheme})을 씁니다. 아이팟 클래식만 지원합니다"));
        }
        self.rebuild_indexes();
        let mut out = Vec::new();
        write_node(&Node::Chunk(self.root.clone()), &mut out);
        if scheme == 1 {
            let fwid = firewire_id.ok_or("이 아이팟의 고유 번호(FirewireGuid)를 찾지 못해 서명할 수 없습니다")?;
            write_hash58(&mut out, fwid);
        }
        Ok(out)
    }
}

/// 기본 재생 목록 끝에 곡 참조(mhip + 위치 mhod 100)를 덧붙인다.
fn append_mhip(mpl: &mut Chunk, track_id: u32) {
    let Some(children) = mpl.children_mut() else { return };
    let pos = children.iter().filter(|n| matches!(n, Node::Chunk(c) if c.is(b"mhip"))).count() as u32;
    let mut body = vec![0u8; 20];
    wr32(&mut body, 0, pos);
    let mut mhip = Chunk::new(b"mhip", 76, Body::Children(vec![mhod_with_body(MHOD_PLAYLIST_POS, body)]));
    mhip.set_u32(0x0C, 1);
    mhip.set_u32(0x18, track_id);
    children.push(Node::Chunk(mhip));
    mpl.set_u32(0x10, pos + 1);
}

/// 곡 레코드의 재생 기록: [재생 횟수, 재생 횟수 2, 마지막 재생, 건너뛴 횟수, 마지막 건너뜀, 별점]
/// (libgpod: 0x50, 0x54, 0x58, 0x9C, 0xA0, 0x1F)
fn play_stats(h: &[u8]) -> [u32; 6] {
    [rd32(h, 0x50), rd32(h, 0x54), rd32(h, 0x58), rd32(h, 0x9C), rd32(h, 0xA0), h[0x1F] as u32]
}

fn set_play_stats(h: &mut [u8], s: [u32; 6]) {
    wr32(h, 0x50, s[0]);
    wr32(h, 0x54, s[1]);
    wr32(h, 0x58, s[2]);
    wr32(h, 0x9C, s[3]);
    wr32(h, 0xA0, s[4]);
    h[0x1F] = s[5].min(100) as u8;
}

// ── 정렬 색인 ───────────────────────────────────────────

/// 정렬용 글자열. 한국어 맥의 iTunes·Finder 가 만든 색인과 맞춰 보니:
/// 문장 부호·공백은 건너뛰고 대소문자를 무시하며, 첫 글자의 문자 종류 순서는
/// 한글·한자 → 라틴 글자 → 가나 → 숫자·기타였다. (종류 순위, 접은 글자)
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SortStr(u8, String);

impl SortStr {
    /// 정렬용 태그가 있으면 그것, 없으면 원래 값
    fn new(sort: &str, value: &str) -> Self {
        let s = if sort.is_empty() { value } else { sort };
        // 앞쪽 문장 부호·공백만 건너뛴다 (가운데 것까지 빼면 Apple 순서와 더 어긋났다)
        let folded: String = s.trim_start_matches(|c: char| !c.is_alphanumeric()).chars().flat_map(char::to_lowercase).map(fold_accent).collect();
        let rank = match folded.chars().next() {
            None => 3,
            Some(c) if c.is_numeric() => 3,
            Some(c) if is_hangul_or_han(c) => 0,
            Some(c) if is_kana(c) => 2,
            Some(_) => 1,
        };
        SortStr(rank, folded)
    }

    /// 건너뛰기 표의 글자. Apple 색인처럼 한글·한자는 'A' 묶음 앞쪽에, 가나·숫자는 '0' 묶음에 넣는다.
    fn letter(&self) -> u16 {
        match (self.0, self.1.chars().next()) {
            (0, _) => b'A' as u16,
            (1, Some(c)) => {
                let up = c.to_uppercase().next().unwrap_or(c);
                let mut buf = [0u16; 2];
                up.encode_utf16(&mut buf)[0]
            }
            _ => b'0' as u16,
        }
    }
}

/// 라틴 글자의 악센트를 뗀다 (é → e). 자주 쓰는 Latin-1 범위만.
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

fn is_hangul_or_han(c: char) -> bool {
    matches!(c as u32, 0xAC00..=0xD7A3 | 0x1100..=0x11FF | 0x3130..=0x318F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FFFF)
}

fn is_kana(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F)
}

/// 곡 하나의 정렬 열쇠들
struct SortKey {
    title: SortStr,
    artist: SortStr,
    album: SortStr,
    album_artist: SortStr,
    genre: SortStr,
    composer: SortStr,
    cd: u32,
    track: u32,
}

impl SortKey {
    fn of(t: &DbTrack) -> Self {
        let album_artist = if t.album_artist.is_empty() { SortStr::new(&t.sort_artist, &t.artist) } else { SortStr::new("", &t.album_artist) };
        SortKey {
            title: SortStr::new(&t.sort_title, &t.title),
            artist: SortStr::new(&t.sort_artist, &t.artist),
            album: SortStr::new(&t.sort_album, &t.album),
            album_artist,
            genre: SortStr::new("", &t.genre),
            composer: SortStr::new(&t.sort_composer, &t.composer),
            cd: t.cd_nr,
            track: t.track_nr,
        }
    }

    /// 정렬 종류별로 건너뛰기 글자를 고를 칸
    fn primary(&self, sort_type: u32) -> &SortStr {
        match sort_type {
            0x04 => &self.album,
            0x05 => &self.artist,
            0x07 => &self.genre,
            0x12 => &self.composer,
            0x23 | 0x24 => &self.album_artist,
            _ => &self.title,
        }
    }

    /// 정렬 종류별 순서. 0x03 제목, 0x04 앨범, 0x05 가수, 0x07 장르, 0x12 작곡가 (libgpod),
    /// 0x1D·0x1E·0x1F (TV 프로그램·시즌·회, 음악은 제목 순과 같다), 0x23·0x24 (앨범 가수 쪽으로 보인다)
    fn cmp(&self, other: &Self, sort_type: u32) -> std::cmp::Ordering {
        let in_album = || {
            self.album.cmp(&other.album).then(self.cd.cmp(&other.cd)).then(self.track.cmp(&other.track)).then_with(|| self.title.cmp(&other.title))
        };
        match sort_type {
            0x04 => in_album(),
            0x05 => self.artist.cmp(&other.artist).then_with(in_album),
            0x07 => self.genre.cmp(&other.genre).then_with(|| self.artist.cmp(&other.artist)).then_with(in_album),
            0x12 => self.composer.cmp(&other.composer).then_with(in_album),
            0x23 | 0x24 => self.album_artist.cmp(&other.album_artist).then_with(in_album),
            _ => self.title.cmp(&other.title),
        }
    }
}

/// 기존 색인을 고친다: 처음 순서에서 빠진 곡을 지우고 새 곡을 비교 규칙으로 끼운다.
/// 기존 색인을 읽을 수 없으면 false (처음부터 만든다).
fn update_indexes(mpl: &mut Chunk, orig: &[u32], members: &[u32], keys: &HashMap<u32, SortKey>) -> bool {
    let Some(children) = mpl.children_mut() else { return false };
    let leaf = |n: &Node, typ: u32| match n {
        Node::Chunk(c) if c.is(b"mhod") && c.mhod_type() == typ => match &c.body {
            Body::Leaf(b) => Some(b.clone()),
            _ => None,
        },
        _ => None,
    };
    // 정렬 종류 → 기존 건너뛰기 표 (글자, 시작, 개수)
    let mut tables: HashMap<u32, Vec<(u16, u32, u32)>> = HashMap::new();
    let mut indexes = 0;
    for n in children.iter() {
        if let Some(b) = leaf(n, MHOD_JUMPTABLE) {
            let Some((sort, entries)) = parse_jumptable(&b) else { return false };
            tables.insert(sort, entries);
        }
        if let Some(b) = leaf(n, MHOD_LIBINDEX) {
            let Some((_, positions)) = parse_libindex(&b) else { return false };
            if positions.iter().any(|&p| p as usize >= orig.len()) || positions.len() != orig.len() {
                return false;
            }
            indexes += 1;
        }
    }
    if indexes == 0 {
        return false;
    }
    let new_pos: HashMap<u32, u32> = members.iter().enumerate().map(|(i, id)| (*id, i as u32)).collect();
    let before: std::collections::HashSet<u32> = orig.iter().copied().collect();
    let added: Vec<u32> = members.iter().copied().filter(|id| !before.contains(id)).collect();
    if added.iter().any(|id| !keys.contains_key(id)) {
        return false;
    }

    let mut fresh_tables: HashMap<u32, Vec<(u16, u32, u32)>> = HashMap::new();
    for n in children.iter_mut() {
        let Node::Chunk(c) = n else { continue };
        if !(c.is(b"mhod") && c.mhod_type() == MHOD_LIBINDEX) {
            continue;
        }
        let Body::Leaf(body) = &mut c.body else { continue };
        let (sort, positions) = parse_libindex(body).unwrap();
        let table = tables.get(&sort);
        // 건너뛰기 표의 시작·개수는 정렬된 목록 안의 순번이다
        let letter_at = |i: u32| table.and_then(|t| t.iter().find(|(_, s, n)| i >= *s && i < s + n).map(|e| e.0));
        // (곡 번호, 건너뛰기 글자)
        let mut order: Vec<(u32, Option<u16>)> = positions
            .iter()
            .enumerate()
            .map(|(i, &p)| (orig[p as usize], letter_at(i as u32)))
            .filter(|(id, _)| new_pos.contains_key(id))
            .collect();
        for id in &added {
            let key = &keys[id];
            let at = order.partition_point(|(o, _)| keys.get(o).is_none_or(|k| k.cmp(key, sort) != std::cmp::Ordering::Greater));
            let own = key.primary(sort).letter();
            let prev = at.checked_sub(1).and_then(|i| order[i].1);
            let next = order.get(at).and_then(|e| e.1);
            let letter = if Some(own) == prev || Some(own) == next { own } else { prev.or(next).unwrap_or(own) };
            order.insert(at, (*id, table.map(|_| letter)));
        }
        let mut fresh = body[..48].to_vec();
        wr32(&mut fresh, 4, order.len() as u32);
        let mut runs: Vec<(u16, u32, u32)> = Vec::new();
        for (i, (id, letter)) in order.iter().enumerate() {
            fresh.extend_from_slice(&new_pos[id].to_le_bytes());
            if let Some(l) = letter {
                match runs.last_mut() {
                    Some(last) if last.0 == *l => last.2 += 1,
                    _ => runs.push((*l, i as u32, 1)),
                }
            }
        }
        *body = fresh;
        if table.is_some() {
            fresh_tables.insert(sort, runs);
        }
    }
    for n in children.iter_mut() {
        let Node::Chunk(c) = n else { continue };
        if !(c.is(b"mhod") && c.mhod_type() == MHOD_JUMPTABLE) {
            continue;
        }
        let Body::Leaf(body) = &mut c.body else { continue };
        let sort = rd32(body, 0);
        let Some(runs) = fresh_tables.get(&sort) else { continue };
        let mut fresh = body[..16].to_vec();
        wr32(&mut fresh, 4, runs.len() as u32);
        for (letter, start, count) in runs {
            fresh.extend_from_slice(&letter.to_le_bytes());
            fresh.extend_from_slice(&0u16.to_le_bytes());
            fresh.extend_from_slice(&start.to_le_bytes());
            fresh.extend_from_slice(&count.to_le_bytes());
        }
        *body = fresh;
    }
    true
}

/// mhod 52 본문: [정렬 종류][개수][0 × 40][위치 × 개수]
fn parse_libindex(b: &[u8]) -> Option<(u32, Vec<u32>)> {
    let n = *b.get(4..8).map(|s| u32::from_le_bytes(s.try_into().unwrap())).as_ref()? as usize;
    if b.len() < 48 + n * 4 {
        return None;
    }
    Some((rd32(b, 0), (0..n).map(|i| rd32(b, 48 + i * 4)).collect()))
}

/// mhod 53 본문: [정렬 종류][개수][0 × 8][(글자 2, 0 2, 시작 4, 개수 4) × 개수]
fn parse_jumptable(b: &[u8]) -> Option<(u32, Vec<(u16, u32, u32)>)> {
    let n = *b.get(4..8).map(|s| u32::from_le_bytes(s.try_into().unwrap())).as_ref()? as usize;
    if b.len() < 16 + n * 12 {
        return None;
    }
    Some((rd32(b, 0), (0..n).map(|i| (u16::from_le_bytes([b[16 + i * 12], b[17 + i * 12]]), rd32(b, 20 + i * 12), rd32(b, 24 + i * 12))).collect()))
}

/// 색인을 처음부터 만든다: mhod 52·53 다섯 쌍 (제목, 가수, 앨범, 장르, 작곡가 순 - libgpod 와 같은 순서)
fn index_mhods(members: &[(u32, &SortKey)]) -> Vec<Node> {
    let mut out = Vec::new();
    for sort_type in [0x03, 0x05, 0x04, 0x07, 0x12] {
        let mut order: Vec<usize> = (0..members.len()).collect();
        order.sort_by(|&a, &b| members[a].1.cmp(members[b].1, sort_type));
        let mut body52 = vec![0u8; 48];
        wr32(&mut body52, 0, sort_type);
        wr32(&mut body52, 4, order.len() as u32);
        // 건너뛰기 표: (글자, 시작, 개수)
        let mut jumps: Vec<(u16, u32, u32)> = Vec::new();
        for (pos, &i) in order.iter().enumerate() {
            body52.extend_from_slice(&(i as u32).to_le_bytes());
            let letter = members[i].1.primary(sort_type).letter();
            match jumps.last_mut() {
                Some(last) if last.0 == letter => last.2 += 1,
                _ => jumps.push((letter, pos as u32, 1)),
            }
        }
        out.push(mhod_with_body(MHOD_LIBINDEX, body52));
        let mut body53 = vec![0u8; 16];
        wr32(&mut body53, 0, sort_type);
        wr32(&mut body53, 4, jumps.len() as u32);
        for (letter, start, count) in jumps {
            body53.extend_from_slice(&letter.to_le_bytes());
            body53.extend_from_slice(&0u16.to_le_bytes());
            body53.extend_from_slice(&start.to_le_bytes());
            body53.extend_from_slice(&count.to_le_bytes());
        }
        out.push(mhod_with_body(MHOD_JUMPTABLE, body53));
    }
    out
}

// ── hash58 서명 (itdb_hash58.c) ─────────────────────────

const TABLE1: [u8; 256] = [
    0x63, 0x7C, 0x77, 0x7B, 0xF2, 0x6B, 0x6F, 0xC5, 0x30, 0x01, 0x67, 0x2B, 0xFE, 0xD7, 0xAB, 0x76,
    0xCA, 0x82, 0xC9, 0x7D, 0xFA, 0x59, 0x47, 0xF0, 0xAD, 0xD4, 0xA2, 0xAF, 0x9C, 0xA4, 0x72, 0xC0,
    0xB7, 0xFD, 0x93, 0x26, 0x36, 0x3F, 0xF7, 0xCC, 0x34, 0xA5, 0xE5, 0xF1, 0x71, 0xD8, 0x31, 0x15,
    0x04, 0xC7, 0x23, 0xC3, 0x18, 0x96, 0x05, 0x9A, 0x07, 0x12, 0x80, 0xE2, 0xEB, 0x27, 0xB2, 0x75,
    0x09, 0x83, 0x2C, 0x1A, 0x1B, 0x6E, 0x5A, 0xA0, 0x52, 0x3B, 0xD6, 0xB3, 0x29, 0xE3, 0x2F, 0x84,
    0x53, 0xD1, 0x00, 0xED, 0x20, 0xFC, 0xB1, 0x5B, 0x6A, 0xCB, 0xBE, 0x39, 0x4A, 0x4C, 0x58, 0xCF,
    0xD0, 0xEF, 0xAA, 0xFB, 0x43, 0x4D, 0x33, 0x85, 0x45, 0xF9, 0x02, 0x7F, 0x50, 0x3C, 0x9F, 0xA8,
    0x51, 0xA3, 0x40, 0x8F, 0x92, 0x9D, 0x38, 0xF5, 0xBC, 0xB6, 0xDA, 0x21, 0x10, 0xFF, 0xF3, 0xD2,
    0xCD, 0x0C, 0x13, 0xEC, 0x5F, 0x97, 0x44, 0x17, 0xC4, 0xA7, 0x7E, 0x3D, 0x64, 0x5D, 0x19, 0x73,
    0x60, 0x81, 0x4F, 0xDC, 0x22, 0x2A, 0x90, 0x88, 0x46, 0xEE, 0xB8, 0x14, 0xDE, 0x5E, 0x0B, 0xDB,
    0xE0, 0x32, 0x3A, 0x0A, 0x49, 0x06, 0x24, 0x5C, 0xC2, 0xD3, 0xAC, 0x62, 0x91, 0x95, 0xE4, 0x79,
    0xE7, 0xC8, 0x37, 0x6D, 0x8D, 0xD5, 0x4E, 0xA9, 0x6C, 0x56, 0xF4, 0xEA, 0x65, 0x7A, 0xAE, 0x08,
    0xBA, 0x78, 0x25, 0x2E, 0x1C, 0xA6, 0xB4, 0xC6, 0xE8, 0xDD, 0x74, 0x1F, 0x4B, 0xBD, 0x8B, 0x8A,
    0x70, 0x3E, 0xB5, 0x66, 0x48, 0x03, 0xF6, 0x0E, 0x61, 0x35, 0x57, 0xB9, 0x86, 0xC1, 0x1D, 0x9E,
    0xE1, 0xF8, 0x98, 0x11, 0x69, 0xD9, 0x8E, 0x94, 0x9B, 0x1E, 0x87, 0xE9, 0xCE, 0x55, 0x28, 0xDF,
    0x8C, 0xA1, 0x89, 0x0D, 0xBF, 0xE6, 0x42, 0x68, 0x41, 0x99, 0x2D, 0x0F, 0xB0, 0x54, 0xBB, 0x16,
];

const TABLE2: [u8; 256] = [
    0x52, 0x09, 0x6A, 0xD5, 0x30, 0x36, 0xA5, 0x38, 0xBF, 0x40, 0xA3, 0x9E, 0x81, 0xF3, 0xD7, 0xFB,
    0x7C, 0xE3, 0x39, 0x82, 0x9B, 0x2F, 0xFF, 0x87, 0x34, 0x8E, 0x43, 0x44, 0xC4, 0xDE, 0xE9, 0xCB,
    0x54, 0x7B, 0x94, 0x32, 0xA6, 0xC2, 0x23, 0x3D, 0xEE, 0x4C, 0x95, 0x0B, 0x42, 0xFA, 0xC3, 0x4E,
    0x08, 0x2E, 0xA1, 0x66, 0x28, 0xD9, 0x24, 0xB2, 0x76, 0x5B, 0xA2, 0x49, 0x6D, 0x8B, 0xD1, 0x25,
    0x72, 0xF8, 0xF6, 0x64, 0x86, 0x68, 0x98, 0x16, 0xD4, 0xA4, 0x5C, 0xCC, 0x5D, 0x65, 0xB6, 0x92,
    0x6C, 0x70, 0x48, 0x50, 0xFD, 0xED, 0xB9, 0xDA, 0x5E, 0x15, 0x46, 0x57, 0xA7, 0x8D, 0x9D, 0x84,
    0x90, 0xD8, 0xAB, 0x00, 0x8C, 0xBC, 0xD3, 0x0A, 0xF7, 0xE4, 0x58, 0x05, 0xB8, 0xB3, 0x45, 0x06,
    0xD0, 0x2C, 0x1E, 0x8F, 0xCA, 0x3F, 0x0F, 0x02, 0xC1, 0xAF, 0xBD, 0x03, 0x01, 0x13, 0x8A, 0x6B,
    0x3A, 0x91, 0x11, 0x41, 0x4F, 0x67, 0xDC, 0xEA, 0x97, 0xF2, 0xCF, 0xCE, 0xF0, 0xB4, 0xE6, 0x73,
    0x96, 0xAC, 0x74, 0x22, 0xE7, 0xAD, 0x35, 0x85, 0xE2, 0xF9, 0x37, 0xE8, 0x1C, 0x75, 0xDF, 0x6E,
    0x47, 0xF1, 0x1A, 0x71, 0x1D, 0x29, 0xC5, 0x89, 0x6F, 0xB7, 0x62, 0x0E, 0xAA, 0x18, 0xBE, 0x1B,
    0xFC, 0x56, 0x3E, 0x4B, 0xC6, 0xD2, 0x79, 0x20, 0x9A, 0xDB, 0xC0, 0xFE, 0x78, 0xCD, 0x5A, 0xF4,
    0x1F, 0xDD, 0xA8, 0x33, 0x88, 0x07, 0xC7, 0x31, 0xB1, 0x12, 0x10, 0x59, 0x27, 0x80, 0xEC, 0x5F,
    0x60, 0x51, 0x7F, 0xA9, 0x19, 0xB5, 0x4A, 0x0D, 0x2D, 0xE5, 0x7A, 0x9F, 0x93, 0xC9, 0x9C, 0xEF,
    0xA0, 0xE0, 0x3B, 0x4D, 0xAE, 0x2A, 0xF5, 0xB0, 0xC8, 0xEB, 0xBB, 0x3C, 0x83, 0x53, 0x99, 0x61,
    0x17, 0x2B, 0x04, 0x7E, 0xBA, 0x77, 0xD6, 0x26, 0xE1, 0x69, 0x14, 0x63, 0x55, 0x21, 0x0C, 0x7D,
];

const FIXED: [u8; 18] = [0x67, 0x23, 0xFE, 0x30, 0x45, 0x33, 0xF8, 0x90, 0x99, 0x21, 0x07, 0xC1, 0xD0, 0x12, 0xB2, 0xA1, 0x07, 0x81];

fn lcm(a: u32, b: u32) -> u32 {
    if a == 0 || b == 0 {
        return 1;
    }
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    a * b / x
}

/// "0x000A27001A2B3C4D" 또는 "000A27001A2B3C4D" → 바이트
pub fn parse_firewire_id(s: &str) -> Option<Vec<u8>> {
    let hex = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if hex.len() < 16 || hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

fn hash58_key(fwid: &[u8]) -> [u8; 64] {
    let mut y = [0u8; 16];
    for i in 0..4 {
        let l = lcm(fwid[i * 2] as u32, fwid[i * 2 + 1] as u32);
        let (hi, lo) = (((l >> 8) & 0xff) as usize, (l & 0xff) as usize);
        y[i * 4] = TABLE1[hi];
        y[i * 4 + 1] = TABLE2[hi];
        y[i * 4 + 2] = TABLE1[lo];
        y[i * 4 + 3] = TABLE2[lo];
    }
    let digest = Sha1::new().chain_update(FIXED).chain_update(y).finalize();
    let mut key = [0u8; 64];
    key[..20].copy_from_slice(&digest);
    key
}

/// DB 바이트에 hash58 서명을 넣는다. db_id(0x18)·0x32·hash58(0x58) 칸을 0으로 두고 계산한 뒤 되돌린다.
pub fn write_hash58(db: &mut [u8], fwid: &[u8]) {
    let backup_id: Vec<u8> = db[0x18..0x20].to_vec();
    let backup_32: Vec<u8> = db[0x32..0x46].to_vec();
    db[0x18..0x20].fill(0);
    db[0x32..0x46].fill(0);
    db[0x58..0x6C].fill(0);
    wr16(db, 0x30, 1);

    let mut key = hash58_key(fwid);
    key.iter_mut().for_each(|b| *b ^= 0x36);
    let inner = Sha1::new().chain_update(key).chain_update(&*db).finalize();
    key.iter_mut().for_each(|b| *b ^= 0x36 ^ 0x5c);
    let outer = Sha1::new().chain_update(key).chain_update(inner).finalize();

    db[0x58..0x6C].copy_from_slice(&outer);
    db[0x18..0x20].copy_from_slice(&backup_id);
    db[0x32..0x46].copy_from_slice(&backup_32);
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// libgpod 가 만드는 것과 같은 꼴의 빈 DB (mhsd 1·3·2·4·8). 테스트에서만 쓴다.
    pub fn empty_db(hash: bool) -> Vec<u8> {
        let mut mhbd = Chunk::new(b"mhbd", 0xF4, Body::Children(Vec::new()));
        mhbd.set_u32(0x0C, 1);
        mhbd.set_u32(0x10, 0x30);
        wr64(&mut mhbd.header, 0x18, 0x1122334455667788);
        if hash {
            wr16(&mut mhbd.header, 0x30, 1);
        }
        let mhsd = |typ: u32, list: Chunk| {
            let mut c = Chunk::new(b"mhsd", 96, Body::Children(vec![Node::Chunk(list)]));
            c.set_u32(12, typ);
            Node::Chunk(c)
        };
        let mpl = || {
            let mut c = Chunk::new(b"mhyp", 108, Body::Children(vec![string_mhod(MHOD_TITLE, "iPod")]));
            c.header[0x14] = 1;
            c.set_u32(0x0C, 1);
            Node::Chunk(c)
        };
        let children = vec![
            mhsd(1, Chunk::new(b"mhlt", 92, Body::List(vec![]))),
            mhsd(3, Chunk::new(b"mhlp", 92, Body::List(vec![mpl()]))),
            mhsd(2, Chunk::new(b"mhlp", 92, Body::List(vec![mpl()]))),
            mhsd(4, Chunk::new(b"mhla", 92, Body::List(vec![]))),
            mhsd(8, Chunk::new(b"mhli", 92, Body::List(vec![]))),
        ];
        mhbd.set_u32(0x14, children.len() as u32);
        mhbd.body = Body::Children(children);
        let mut out = Vec::new();
        write_node(&Node::Chunk(mhbd), &mut out);
        out
    }

    fn song(title: &str, artist: &str, album: &str, n: u32) -> NewTrack {
        NewTrack {
            location: format!(":iPod_Control:Music:F00:{title}.mp3"),
            ext: "mp3".into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            track_nr: n,
            size: 1000,
            length_ms: 200_000,
            bitrate: 320,
            sample_rate: 44100,
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_add_and_reparse() {
        let data = empty_db(false);
        let mut db = ITunesDb::parse(&data).expect("빈 DB 읽기");
        db.add_track(&song("Zebra", "B", "Album", 2)).unwrap();
        db.add_track(&song("apple", "A", "Album", 1)).unwrap();
        db.add_track(&song("밤편지", "아이유", "Palette", 3)).unwrap();
        let bytes = db.to_bytes(None).unwrap();

        // 다시 읽으면 원본과 같아야 하고(되쓰기 확인), 곡·앨범·가수가 들어 있어야 한다
        let again = ITunesDb::parse(&bytes).expect("쓴 DB 다시 읽기");
        let tracks = again.tracks();
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].id, 52);
        assert_eq!(tracks[2].title, "밤편지");
        assert_eq!(tracks[2].location, ":iPod_Control:Music:F00:밤편지.mp3");
        assert_eq!(again.mhsd_list_len(4), 3); // (Album, B), (Album, A), (Palette, 아이유)
        assert_eq!(again.mhsd_list_len(8), 3);

        // 기본 재생 목록: mhip 3개, 색인 mhod 10개 + 제목 1개
        for typ in [2, 3] {
            let mpl = again.mpl(typ).unwrap();
            assert_eq!(mpl.u32_at(0x10), 3);
            assert_eq!(mpl.u32_at(0x0C), 11);
            // 제목 순 색인 (한글 먼저): 밤편지(2번째) → apple(1번째) → Zebra(0번째)
            let idx = mpl.chunks().find(|c| c.mhod_type() == MHOD_LIBINDEX).unwrap();
            let Body::Leaf(b) = &idx.body else { panic!() };
            assert_eq!(rd32(b, 0), 0x03);
            assert_eq!([rd32(b, 48), rd32(b, 52), rd32(b, 56)], [2, 1, 0]);
        }
    }

    /// 색인의 곡 번호 순서 (정렬 종류별)
    fn index_ids(db: &ITunesDb, typ: u32, sort: u32) -> Vec<u32> {
        let members = db.master_ids(typ).unwrap();
        let mpl = db.mpl(typ).unwrap();
        let idx = mpl.chunks().find(|c| c.mhod_type() == MHOD_LIBINDEX && matches!(&c.body, Body::Leaf(b) if rd32(b, 0) == sort)).unwrap();
        let Body::Leaf(b) = &idx.body else { panic!() };
        parse_libindex(b).unwrap().1.iter().map(|&p| members[p as usize]).collect()
    }

    fn jumps(db: &ITunesDb, typ: u32, sort: u32) -> Vec<(u16, u32, u32)> {
        let mpl = db.mpl(typ).unwrap();
        let t = mpl.chunks().find(|c| c.mhod_type() == MHOD_JUMPTABLE && matches!(&c.body, Body::Leaf(b) if rd32(b, 0) == sort)).unwrap();
        let Body::Leaf(b) = &t.body else { panic!() };
        parse_jumptable(b).unwrap().1
    }

    #[test]
    fn unchanged_db_is_byte_identical() {
        let mut db = ITunesDb::parse(&empty_db(false)).unwrap();
        for (i, t) in ["b", "a", "c"].iter().enumerate() {
            db.add_track(&song(t, "X", "Y", i as u32)).unwrap();
        }
        let bytes = db.to_bytes(None).unwrap();
        let mut again = ITunesDb::parse(&bytes).unwrap();
        assert_eq!(again.to_bytes(None).unwrap(), bytes);
    }

    #[test]
    fn remove_and_add_keep_existing_order() {
        let mut db = ITunesDb::parse(&empty_db(false)).unwrap();
        let mut ids = Vec::new();
        for (i, t) in ["Delta", "alpha", "Charlie", "bravo", "가나"].iter().enumerate() {
            ids.push(db.add_track(&song(t, "X", "Y", i as u32)).unwrap());
        }
        let bytes = db.to_bytes(None).unwrap();
        let mut db = ITunesDb::parse(&bytes).unwrap();
        let title = |db: &ITunesDb, id: u32| db.tracks().into_iter().find(|t| t.id == id).unwrap().title;
        let before: Vec<String> = index_ids(&db, 2, 0x03).iter().map(|&id| title(&db, id)).collect();
        assert_eq!(before, ["가나", "alpha", "bravo", "Charlie", "Delta"]);

        // 재생 목록 하나에 뺄 곡(alpha)과 남길 곡(bravo)을 넣어 둔다
        let pl = {
            let mut c = Chunk::new(b"mhyp", 108, Body::Children(vec![string_mhod(MHOD_TITLE, "Mine")]));
            c.set_u32(0x0C, 1);
            append_mhip(&mut c, ids[1]);
            append_mhip(&mut c, ids[2]);
            c
        };
        db.mhsd_list_mut(2).unwrap().push(Node::Chunk(pl));
        // 재생 횟수 합치기 확인용
        for n in db.mhsd_list_mut(1).unwrap().iter_mut() {
            if let Node::Chunk(c) = n {
                if c.u32_at(0x10) == ids[1] {
                    wr32(&mut c.header, 0x50, 5);
                    c.header[0x1F] = 80;
                }
                if c.u32_at(0x10) == ids[3] {
                    wr32(&mut c.header, 0x50, 2);
                }
            }
        }

        let removed = db.remove_tracks(&[(ids[1], Some(ids[3]))]).unwrap();
        assert_eq!(removed, vec![1]);
        db.add_track(&song("Bingo", "X", "Y", 9)).unwrap();
        let bytes = db.to_bytes(None).unwrap();
        assert_eq!(db.to_bytes(None).unwrap(), bytes, "두 번 써도 같아야 한다");
        let db = ITunesDb::parse(&bytes).unwrap();
        let after: Vec<String> = index_ids(&db, 2, 0x03).iter().map(|&id| title(&db, id)).collect();
        assert_eq!(after, ["가나", "Bingo", "bravo", "Charlie", "Delta"]);
        // 건너뛰기 표: 가나는 A 묶음, B 두 곡
        assert_eq!(jumps(&db, 2, 0x03), vec![(b'A' as u16, 0, 1), (b'B' as u16, 1, 2), (b'C' as u16, 3, 1), (b'D' as u16, 4, 1)]);
        let kept = db.tracks().into_iter().find(|t| t.id == ids[3]).unwrap();
        let kept_mhit = db.mhits().find(|c| c.u32_at(0x10) == kept.id).unwrap();
        assert_eq!(rd32(&kept_mhit.header, 0x50), 7);
        assert_eq!(kept_mhit.header[0x1F], 80);
        // 사용자 재생 목록 [alpha, Charlie]: alpha 자리가 bravo 로 바뀐다
        let mine = db.mhsd(2).unwrap().chunks().next().unwrap().chunks().filter(|c| c.is(b"mhyp")).nth(1).unwrap();
        let items: Vec<u32> = mine.chunks().filter(|c| c.is(b"mhip")).map(|c| c.u32_at(0x18)).collect();
        assert_eq!(items, vec![ids[3], ids[2]]);
        assert_eq!(mine.u32_at(0x10), 2);

        // 남길 곡이 이미 든 재생 목록에서는 뺄 곡 자리를 지운다
        let mut db = db;
        let pl = {
            let mut c = Chunk::new(b"mhyp", 108, Body::Children(vec![string_mhod(MHOD_TITLE, "Two")]));
            c.set_u32(0x0C, 1);
            append_mhip(&mut c, ids[0]);
            append_mhip(&mut c, ids[2]);
            c
        };
        db.mhsd_list_mut(2).unwrap().push(Node::Chunk(pl));
        db.remove_tracks(&[(ids[0], Some(ids[2]))]).unwrap();
        let two = db.mhsd(2).unwrap().chunks().next().unwrap().chunks().filter(|c| c.is(b"mhyp")).nth(2).unwrap();
        assert_eq!(two.chunks().filter(|c| c.is(b"mhip")).map(|c| c.u32_at(0x18)).collect::<Vec<_>>(), vec![ids[2]]);
    }

    #[test]
    fn replace_keeps_id() {
        let mut db = ITunesDb::parse(&empty_db(false)).unwrap();
        let id = db.add_track(&song("Song", "A", "B", 1)).unwrap();
        let mut better = song("Song", "A", "B", 1);
        better.ext = "m4a".into();
        better.alac = true;
        better.location = ":iPod_Control:Music:F01:WXYZ.m4a".into();
        better.bitrate = 900;
        db.replace_file(id, &better).unwrap();
        let bytes = db.to_bytes(None).unwrap();
        let t = &ITunesDb::parse(&bytes).unwrap().tracks()[0];
        assert_eq!(t.id, id);
        assert_eq!(t.location, ":iPod_Control:Music:F01:WXYZ.m4a");
        assert!(t.lossless);
        assert_eq!(t.bitrate, 900);
    }

    #[test]
    fn hash58_needs_id_and_is_stable() {
        let mut db = ITunesDb::parse(&empty_db(true)).unwrap();
        assert_eq!(db.hashing_scheme(), 1);
        assert!(db.to_bytes(None).is_err());
        let fwid = parse_firewire_id("0x000A27001A2B3C4D").unwrap();
        let a = db.to_bytes(Some(&fwid)).unwrap();
        let b = db.to_bytes(Some(&fwid)).unwrap();
        assert_eq!(a, b);
        assert!(a[0x58..0x6C].iter().any(|&x| x != 0));
        // db_id 는 계산 뒤 원래 값으로 돌아와야 한다
        assert_eq!(&a[0x18..0x20], &0x1122334455667788u64.to_le_bytes());
    }

    #[test]
    fn rejects_garbage() {
        assert!(ITunesDb::parse(b"hello").is_err());
        let mut d = empty_db(false);
        d.extend_from_slice(b"junk");
        assert!(ITunesDb::parse(&d).is_err());
    }

    impl ITunesDb {
        fn mhsd_list_len(&self, typ: u32) -> usize {
            self.mhsd(typ).unwrap().chunks().next().unwrap().children().len()
        }
        fn mpl(&self, typ: u32) -> Option<&Chunk> {
            self.mhsd(typ)?.chunks().next()?.chunks().find(|c| c.is(b"mhyp") && c.header[0x14] == 1)
        }
    }
}

/// 점검용: 기본 재생 목록 색인이 곡 구성과 맞는지와, 우리 비교 규칙으로 본 순서 어긋남
#[cfg(test)]
pub struct IndexReport {
    /// (정렬 종류, 항목 수, 우리 규칙으로 앞뒤가 뒤집힌 이웃 쌍 수, 건너뛰기 표 개수 합)
    pub sorts: Vec<(u32, usize, usize, Option<u32>)>,
    pub members: usize,
    /// 재생 목록이 가리키는데 곡 목록에 없는 곡 수
    pub dangling: usize,
}

#[cfg(test)]
impl ITunesDb {
    pub fn index_report(&self) -> IndexReport {
        let tracks: HashMap<u32, DbTrack> = self.tracks().into_iter().map(|t| (t.id, t)).collect();
        let keys: HashMap<u32, SortKey> = tracks.iter().map(|(id, t)| (*id, SortKey::of(t))).collect();
        let members = self.master_ids(2).unwrap_or_default();
        let mut dangling = 0;
        for typ in [2, 3, 5] {
            if let Some(list) = self.mhsd(typ).and_then(|s| s.chunks().next()) {
                for pl in list.chunks().filter(|c| c.is(b"mhyp")) {
                    dangling += pl.chunks().filter(|c| c.is(b"mhip") && !tracks.contains_key(&c.u32_at(0x18))).count();
                }
            }
        }
        let mpl = self.mhsd(2).and_then(|s| s.chunks().next()).and_then(|l| l.chunks().find(|c| c.is(b"mhyp") && c.header.get(0x14) == Some(&1)));
        let mut sorts = Vec::new();
        if let Some(mpl) = mpl {
            let mut jumps: HashMap<u32, u32> = HashMap::new();
            for c in mpl.chunks().filter(|c| c.mhod_type() == MHOD_JUMPTABLE) {
                if let Body::Leaf(b) = &c.body {
                    if let Some((sort, e)) = parse_jumptable(b) {
                        jumps.insert(sort, e.iter().map(|x| x.2).sum());
                    }
                }
            }
            for c in mpl.chunks().filter(|c| c.mhod_type() == MHOD_LIBINDEX) {
                let Body::Leaf(b) = &c.body else { continue };
                let Some((sort, pos)) = parse_libindex(b) else { continue };
                let ids: Vec<u32> = pos.iter().filter_map(|&p| members.get(p as usize).copied()).collect();
                let bad = ids.windows(2).filter(|w| keys[&w[0]].cmp(&keys[&w[1]], sort) == std::cmp::Ordering::Greater).count();
                sorts.push((sort, ids.len(), bad, jumps.get(&sort).copied()));
            }
        }
        IndexReport { sorts, members: members.len(), dangling }
    }

    /// 점검용: 기본 재생 목록 정렬 종류별 곡 번호 순서
    pub fn index_orders(&self) -> HashMap<u32, Vec<u32>> {
        let members = self.master_ids(2).unwrap_or_default();
        let mpl = self.mhsd(2).and_then(|s| s.chunks().next()).and_then(|l| l.chunks().find(|c| c.is(b"mhyp") && c.header.get(0x14) == Some(&1)));
        let mut out = HashMap::new();
        for c in mpl.into_iter().flat_map(|m| m.chunks()).filter(|c| c.mhod_type() == MHOD_LIBINDEX) {
            if let Body::Leaf(b) = &c.body {
                if let Some((sort, pos)) = parse_libindex(b) {
                    out.insert(sort, pos.iter().map(|&p| members[p as usize]).collect());
                }
            }
        }
        out
    }
}

/// libgpod 원본 C 코드와 서명을 맞춰 보기 위해 서명한 DB 를 파일로 남긴다. PODLIFT_HASH_OUT=<파일> 일 때만 돈다.
#[cfg(test)]
#[test]
fn dump_signed_db_for_reference_check() {
    let Ok(out) = std::env::var("PODLIFT_HASH_OUT") else { return };
    let mut db = ITunesDb::parse(&tests::empty_db(true)).unwrap();
    db.add_track(&NewTrack { location: ":iPod_Control:Music:F00:ABCD.mp3".into(), ext: "mp3".into(), title: "테스트".into(), ..Default::default() })
        .unwrap();
    for (i, id) in ["000A27001A2B3C4D", "000A270013B7F1E2"].iter().enumerate() {
        let bytes = db.to_bytes(Some(&parse_firewire_id(id).unwrap())).unwrap();
        std::fs::write(format!("{out}.{i}"), &bytes).unwrap();
        println!("{id} {}", bytes[0x58..0x6C].iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
}
