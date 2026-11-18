//! 아이팟 앨범 아트 DB(iPod_Control/Artwork/ArtworkDB)와 그림 묶음 파일(F1055_1.ithmb 등) 쓰기.
//! 형식은 libgpod(db-artwork-writer.c, ithumb-writer.c)를 따랐다.
//!
//! 구조: mhfd(머리) → mhsd × 3
//!   1: mhli → mhii(그림 하나) → mhod 2 → mhni(형식별 그림 위치) → mhod 3(ithmb 파일 이름)
//!   2: mhla (사진 앨범, 음악에서는 비어 있음)
//!   3: mhlf → mhif(그림 형식: 번호·한 장 바이트 수)
//! 그림 한 장은 형식마다 정해진 크기(아이팟 클래식: 55·128·128·320 정사각)로 줄여
//! 남는 곳을 검은색으로 채운 RGB565(리틀 엔디언) 픽셀로 ithmb 파일 끝에 덧붙인다.
//! 한 줄의 픽셀 수(stride)는 그림 폭보다 클 수 있다(55px 그림은 56px 줄에 담는다).
//! 곡(mhit 0x160)은 mhii 의 그림 번호를 가리킨다. 같은 앨범의 같은 그림은 한 장만 저장해 같이 쓴다.
//!
//! 곡 DB 와 마찬가지로 기존 DB 는 읽은 그대로 두고 그림 항목만 덧붙이며, 다시 쓴 결과가
//! 원본과 같지 않으면 쓰지 않는다. ithmb 는 덧붙이기 전 길이를 기록해 되돌릴 때 잘라 낸다.
use crate::itunesdb::{parse_tree, rd32, wr16, wr32, wr64, write_node, Body, Chunk, Node};
use image::imageops::FilterType;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// ithmb 파일 하나의 최대 크기 (libgpod 와 같음: 너무 크면 아이팟 화면이 느려진다)
const ITHMB_MAX: u64 = 256 * 1000 * 1000;
/// 새 그림 번호의 시작값 (libgpod 의 min_id)
const FIRST_IMAGE_ID: u32 = 0x64;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Format {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// 한 줄의 픽셀 수 (RowBytesAlignment 로 맞춘 폭, 보통 width 와 같다)
    pub stride: u32,
}

impl Format {
    const fn square(id: u32, side: u32, stride: u32) -> Self {
        Format { id, width: side, height: side, stride }
    }

    /// 그림 한 장의 바이트 수 (mhif 0x14, mhni 의 크기)
    pub fn bytes(&self) -> u32 {
        self.stride * self.height * 2
    }
}

/// 아이팟 클래식(그리고 3세대 나노)의 앨범 아트 형식. 실제 클래식(iTunes·Finder 가 쓴 DB)은
/// 1061 을 55x55 로 그리고 한 줄을 4바이트(2픽셀) 단위로 맞춰 56픽셀 폭에 담는다(SysInfoExtended 의
/// RenderWidth 55 + AlignRowBytes). libgpod 의 기본 표(itdb_device.c)는 56x56 이라 그쪽 DB 도 받는다.
pub const CLASSIC_FORMATS: [Format; 4] = [
    Format::square(1061, 55, 56),
    Format::square(1055, 128, 128),
    Format::square(1068, 128, 128),
    Format::square(1060, 320, 320),
];

/// 기존 DB 의 mhif 크기가 위 표와 다를 때 맞춰 볼 다른 꼴 (libgpod 로 쓴 DB)
const ALT_FORMATS: [Format; 1] = [Format::square(1061, 56, 56)];

/// 아이팟 클래식·3세대 나노 모델 번호 (SysInfo 의 ModelNumStr 에서 앞의 x/M 을 뗀 값의 앞부분)
const CLASSIC_MODELS: &[&str] = &[
    "B029", "B147", "B145", "B150", // 6세대 80·160GB
    "B562", "B565", // 6.5세대 120GB
    "C293", "C297", // 7세대 160GB
    "A978", "A980", "B261", "B249", "B253", "B257", "B259", // 3세대 나노
];

pub fn artwork_dir(mount: &Path) -> PathBuf {
    mount.join("iPod_Control").join("Artwork")
}

pub fn db_path(mount: &Path) -> PathBuf {
    artwork_dir(mount).join("ArtworkDB")
}

/// 이 아이팟이 쓰는 앨범 아트 형식. SysInfoExtended 에 적혀 있으면 그것(RGB565 만),
/// 없으면 모델 번호가 아이팟 클래식·3세대 나노일 때 정해진 표. 모르면 None.
fn device_formats(mount: &Path) -> Option<Vec<Format>> {
    let device = mount.join("iPod_Control").join("Device");
    if let Ok(v) = plist::from_file::<_, plist::Value>(device.join("SysInfoExtended")) {
        let list = v.as_dictionary().and_then(|d| d.get("AlbumArt")).and_then(|a| a.as_array());
        if let Some(list) = list {
            let int = |d: &plist::Dictionary, k: &str| d.get(k).and_then(|x| x.as_unsigned_integer()).map(|x| x as u32);
            let formats: Vec<Format> = list
                .iter()
                .filter_map(|f| {
                    let d = f.as_dictionary()?;
                    // 'L565' = RGB565 리틀 엔디언
                    if int(d, "PixelFormat") != Some(0x4C35_3635) {
                        return None;
                    }
                    let width = int(d, "RenderWidth")?;
                    // libgpod: AlignRowBytes 만 있으면 4바이트, RowBytesAlignment 가 있으면 그 바이트 수로 줄을 맞춘다
                    let align_bytes = int(d, "RowBytesAlignment").filter(|&a| a > 0).or_else(|| {
                        d.get("AlignRowBytes").and_then(|x| x.as_boolean()).filter(|&b| b).map(|_| 4)
                    });
                    let align = align_bytes.map(|a| (a / 2).max(1)).unwrap_or(1);
                    let stride = width.div_ceil(align) * align;
                    Some(Format { id: int(d, "FormatId")?, width, height: int(d, "RenderHeight")?, stride })
                })
                .collect();
            if !formats.is_empty() {
                return Some(formats);
            }
        }
    }
    let model = std::fs::read_to_string(device.join("SysInfo")).ok().and_then(|s| {
        s.lines().find_map(|l| l.strip_prefix("ModelNumStr:").map(|v| v.trim().trim_start_matches(['x', 'M', 'P']).to_string()))
    })?;
    CLASSIC_MODELS.iter().any(|m| model.starts_with(m)).then(|| CLASSIC_FORMATS.to_vec())
}

/// 덧붙일 그림 한 장 (형식마다 픽셀)
struct Pending {
    image_id: u32,
    song_dbid: u64,
    source_size: u32,
    /// (형식, RGB565 픽셀, 가로 여백, 세로 여백, 여백 포함 가로, 여백 포함 세로)
    thumbs: Vec<(Format, Vec<u8>, u16, u16, u16, u16)>,
}

/// 덧붙이기 전 ithmb 파일 길이 (되돌릴 때 이 길이로 자른다. None = 새로 만든 파일)
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IthmbMark {
    /// 마운트 기준 상대 경로
    pub path: String,
    pub original_len: Option<u64>,
}

pub struct ArtworkDb {
    root: Chunk,
    formats: Vec<Format>,
    next_id: u32,
    pending: Vec<Pending>,
    /// (앨범 열쇠 + 그림 바이트) 해시 → 그림 번호
    seen: HashMap<[u8; 20], u32>,
}

impl ArtworkDb {
    /// 그림 DB 를 연다. 없으면 이 아이팟 형식으로 빈 DB 를 만든다.
    /// 그림을 넣으면 안 되는 사정(모르는 기종·모르는 그림 형식·되쓰기 불일치)이 있으면 Err.
    pub fn open(mount: &Path) -> Result<Self, String> {
        let path = db_path(mount);
        let root = match std::fs::read(&path) {
            Ok(data) => parse_tree(&data, b"mhfd").map_err(|e| format!("ArtworkDB: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let formats = device_formats(mount).ok_or("이 아이팟 모델의 앨범 아트 형식을 알 수 없습니다")?;
                return Ok(Self::empty(formats));
            }
            Err(e) => return Err(format!("ArtworkDB 를 읽지 못했습니다: {e}")),
        };
        // 이미 있는 DB 의 그림 형식(mhif)을 따른다. 크기를 모르는 형식이 있으면 쓰지 않는다.
        let known: Vec<Format> = device_formats(mount).unwrap_or_else(|| CLASSIC_FORMATS.to_vec());
        let mut formats = Vec::new();
        for mhif in list_children(&root, 3).iter().filter(|c| c.is(b"mhif")) {
            let id = mhif.u32_at(0x10);
            let size = mhif.u32_at(0x14);
            if !known.iter().any(|f| f.id == id) {
                return Err(format!("모르는 앨범 아트 형식({id})이 있어 그림을 넣지 않습니다"));
            }
            let f = known
                .iter()
                .chain(ALT_FORMATS.iter())
                .find(|f| f.id == id && f.bytes() == size)
                .ok_or(format!("앨범 아트 형식 {id} 의 크기({size}바이트)가 예상과 달라 그림을 넣지 않습니다"))?;
            formats.push(*f);
        }
        if formats.is_empty() {
            formats = device_formats(mount).ok_or("이 아이팟 모델의 앨범 아트 형식을 알 수 없습니다")?;
        }
        let max_id = list_children(&root, 1).iter().filter(|c| c.is(b"mhii")).map(|c| c.u32_at(0x10)).max().unwrap_or(0);
        let next_id = root.u32_at(0x1C).max(max_id + 1).max(FIRST_IMAGE_ID);
        Ok(Self { root, formats, next_id, pending: Vec::new(), seen: HashMap::new() })
    }

    /// 빈 그림 DB (libgpod write_mhfd 와 같은 꼴)
    fn empty(formats: Vec<Format>) -> Self {
        let mut mhfd = Chunk::new(b"mhfd", 0x84, Body::Children(Vec::new()));
        mhfd.set_u32(0x10, 2);
        mhfd.set_u32(0x14, 3);
        mhfd.header[0x30] = 2;
        let mhsd = |index: u16, list: Chunk| {
            let mut c = Chunk::new(b"mhsd", 0x60, Body::Children(vec![Node::Chunk(list)]));
            wr16(&mut c.header, 0x0C, index);
            Node::Chunk(c)
        };
        let mhifs = formats
            .iter()
            .map(|f| {
                let mut c = Chunk::new(b"mhif", 0x7C, Body::Children(Vec::new()));
                c.set_u32(0x10, f.id);
                c.set_u32(0x14, f.bytes());
                Node::Chunk(c)
            })
            .collect();
        mhfd.body = Body::Children(vec![
            mhsd(1, Chunk::new(b"mhli", 0x5C, Body::List(Vec::new()))),
            mhsd(2, Chunk::new(b"mhla", 0x5C, Body::List(Vec::new()))),
            mhsd(3, Chunk::new(b"mhlf", 0x5C, Body::List(mhifs))),
        ]);
        Self { root: mhfd, formats, next_id: FIRST_IMAGE_ID, pending: Vec::new(), seen: HashMap::new() }
    }

    /// 그림 한 장을 덧붙일 목록에 넣고 그림 번호를 돌려준다. 같은 앨범에 같은 그림이면 앞의 번호를 준다.
    pub fn add(&mut self, cover: &[u8], album_key: &str, song_dbid: u64) -> Result<u32, String> {
        let hash: [u8; 20] = Sha1::new().chain_update(album_key.as_bytes()).chain_update(cover).finalize().into();
        if let Some(&id) = self.seen.get(&hash) {
            return Ok(id);
        }
        let img = image::load_from_memory(cover).map_err(|e| format!("앨범 아트를 읽지 못했습니다: {e}"))?;
        let thumbs = self.formats.iter().map(|f| render(&img, *f)).collect();
        let id = self.next_id;
        self.next_id += 1;
        self.pending.push(Pending { image_id: id, song_dbid, source_size: cover.len() as u32, thumbs });
        self.seen.insert(hash, id);
        Ok(id)
    }

    #[allow(dead_code)] // 점검 테스트에서 쓴다
    pub fn formats(&self) -> &[Format] {
        &self.formats
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 픽셀을 ithmb 파일 끝에 덧붙이고 그림 DB 를 쓴다. 실패하면 덧붙인 것을 잘라 내고 Err.
    /// 성공하면 되돌리기용으로 ithmb 의 원래 길이를 돌려준다.
    pub fn commit(&mut self, mount: &Path) -> Result<Vec<IthmbMark>, String> {
        let dir = artwork_dir(mount);
        std::fs::create_dir_all(&dir).map_err(|e| format!("Artwork 폴더를 만들지 못했습니다: {e}"))?;
        let mut marks: Vec<IthmbMark> = Vec::new();
        let result = self.write_all(mount, &dir, &mut marks);
        if let Err(e) = result {
            rollback_ithmb(mount, &marks);
            return Err(e);
        }
        Ok(marks)
    }

    fn write_all(&mut self, mount: &Path, dir: &Path, marks: &mut Vec<IthmbMark>) -> Result<(), String> {
        // 형식마다 ithmb 파일 하나를 골라(가장 큰 번호, 가득 찼으면 다음 번호) 끝에 덧붙인다.
        // (그림 번호, 형식 번호) → (파일 이름, 위치)
        let mut placed: HashMap<(u32, u32), (String, u32)> = HashMap::new();
        for (fi, f) in self.formats.iter().enumerate() {
            let (path, index) = pick_ithmb(dir, f.id);
            let original = std::fs::metadata(&path).ok().map(|m| m.len());
            marks.push(IthmbMark { path: path.strip_prefix(mount).unwrap_or(&path).to_string_lossy().into_owned(), original_len: original });
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("ithmb 파일을 열지 못했습니다: {e}"))?;
            let mut offset = original.unwrap_or(0);
            for p in &self.pending {
                let pixels = &p.thumbs[fi].1;
                file.write_all(pixels).map_err(|e| format!("ithmb 쓰기 실패: {e}"))?;
                placed.insert((p.image_id, f.id), (format!(":F{}_{}.ithmb", f.id, index), offset as u32));
                offset += pixels.len() as u64;
            }
            file.sync_all().map_err(|e| format!("ithmb 쓰기 실패: {e}"))?;
        }

        // mhii 를 만들어 그림 목록 끝에 덧붙인다
        let mut new_items: Vec<Node> = Vec::new();
        for p in &self.pending {
            let children = p
                .thumbs
                .iter()
                .map(|(f, pixels, hpad, vpad, w, h)| {
                    let (name, offset) = &placed[&(p.image_id, f.id)];
                    location_mhod(f.id, name, *offset, pixels.len() as u32, *hpad, *vpad, *w, *h)
                })
                .collect::<Vec<_>>();
            let mut mhii = Chunk::new(b"mhii", 0x98, Body::Children(Vec::new()));
            mhii.set_u32(0x0C, children.len() as u32);
            mhii.set_u32(0x10, p.image_id);
            wr64(&mut mhii.header, 0x14, p.song_dbid);
            // libgpod: 그림 DB 쪽 원본 크기는 곡 쪽 값 + 그림 수(1)
            mhii.set_u32(0x30, p.source_size + 1);
            mhii.body = Body::Children(children);
            new_items.push(Node::Chunk(mhii));
        }
        list_children_mut(&mut self.root, 1).ok_or("ArtworkDB 에 그림 목록(mhsd 1)이 없습니다")?.extend(new_items);
        self.root.set_u32(0x1C, self.next_id);

        // 다시 읽어 확인한 뒤 임시 이름으로 쓰고 바꿔 끼운다
        let mut bytes = Vec::new();
        write_node(&Node::Chunk(self.root.clone()), &mut bytes);
        parse_tree(&bytes, b"mhfd").map_err(|e| format!("새 ArtworkDB 확인 실패: {e}"))?;
        let tmp = dir.join("ArtworkDB.podlift-writing");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("ArtworkDB 쓰기 실패: {e}"))?;
        std::fs::rename(&tmp, db_path(mount)).map_err(|e| format!("ArtworkDB 바꿔 끼우기 실패: {e}"))?;
        self.pending.clear();
        Ok(())
    }
}

/// ithmb 를 덧붙이기 전 길이로 되돌린다 (새로 만든 파일은 지운다)
pub fn rollback_ithmb(mount: &Path, marks: &[IthmbMark]) {
    for m in marks {
        let path = mount.join(&m.path);
        match m.original_len {
            Some(len) => {
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
                    let _ = f.set_len(len);
                }
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// mhsd(index) 아래 목록의 자식 청크들
fn list_children(root: &Chunk, index: u16) -> Vec<&Chunk> {
    root.chunks()
        .find(|c| c.is(b"mhsd") && (rd32(&c.header, 0x0C) & 0xFFFF) as u16 == index)
        .and_then(|s| s.chunks().next())
        .map(|l| l.chunks().collect())
        .unwrap_or_default()
}

fn list_children_mut(root: &mut Chunk, index: u16) -> Option<&mut Vec<Node>> {
    let mhsd = root.children_mut()?.iter_mut().find_map(|n| match n {
        Node::Chunk(c) if c.is(b"mhsd") && (rd32(&c.header, 0x0C) & 0xFFFF) as u16 == index => Some(c),
        _ => None,
    })?;
    let list = mhsd.children_mut()?.iter_mut().find_map(|n| match n {
        Node::Chunk(c) if matches!(c.body, Body::List(_)) => Some(c),
        _ => None,
    })?;
    list.children_mut()
}

/// 형식마다 덧붙일 ithmb 파일: 가장 큰 번호의 파일, 가득 찼으면 다음 번호 (번호는 1부터)
fn pick_ithmb(dir: &Path, format_id: u32) -> (PathBuf, u32) {
    let prefix = format!("F{format_id}_");
    let max = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.strip_prefix(&prefix)?.strip_suffix(".ithmb")?.parse::<u32>().ok()
                })
                .max()
        })
        .ok()
        .flatten();
    let mut index = max.unwrap_or(1);
    let mut path = dir.join(format!("F{format_id}_{index}.ithmb"));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() >= ITHMB_MAX) {
        index += 1;
        path = dir.join(format!("F{format_id}_{index}.ithmb"));
    }
    (path, index)
}

/// 그림을 형식 크기 안에 비율을 지켜 줄이고, 남는 곳을 검은색으로 채워 가운데에 둔 RGB565 픽셀.
/// 돌려주는 값: (형식, 픽셀, 가로 여백, 세로 여백, 여백 포함 가로, 여백 포함 세로) - libgpod 와 같은 뜻
fn render(img: &image::DynamicImage, f: Format) -> (Format, Vec<u8>, u16, u16, u16, u16) {
    let scaled = img.resize(f.width, f.height, FilterType::Lanczos3).to_rgb8();
    let (w, h) = (scaled.width().min(f.width), scaled.height().min(f.height));
    let hpad = (f.width - w) / 2;
    let vpad = (f.height - h) / 2;
    let mut out = vec![0u8; f.bytes() as usize];
    for y in 0..h {
        for x in 0..w {
            let p = scaled.get_pixel(x, y).0;
            let v = rgb565(p[0], p[1], p[2]);
            let at = (((y + vpad) * f.stride + (x + hpad)) * 2) as usize;
            out[at..at + 2].copy_from_slice(&v.to_le_bytes());
        }
    }
    (f, out, hpad as u16, vpad as u16, (hpad + w) as u16, (vpad + h) as u16)
}

fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3)
}

/// mhod 2(그림 위치) → mhni(형식·위치·크기) → mhod 3(ithmb 파일 이름, UTF-16LE)
#[allow(clippy::too_many_arguments)]
fn location_mhod(format_id: u32, file: &str, offset: u32, size: u32, hpad: u16, vpad: u16, w: u16, h: u16) -> Node {
    // mhod 3: 머리 24 + 문자열 머리 12 + 글자 + 4바이트 맞춤
    let utf16: Vec<u8> = file.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let padding = (4 - (36 + utf16.len()) % 4) % 4;
    let mut body3 = vec![0u8; 12];
    wr32(&mut body3, 0, utf16.len() as u32);
    body3[4] = 2; // UTF-16LE
    body3.extend_from_slice(&utf16);
    body3.extend(std::iter::repeat(0).take(padding));
    let mut mhod3 = Chunk::new(b"mhod", 24, Body::Leaf(body3));
    wr16(&mut mhod3.header, 0x0C, 3);
    mhod3.header[0x0F] = padding as u8;

    let mut mhni = Chunk::new(b"mhni", 0x4C, Body::Children(vec![Node::Chunk(mhod3)]));
    mhni.set_u32(0x0C, 1);
    mhni.set_u32(0x10, format_id);
    mhni.set_u32(0x14, offset);
    mhni.set_u32(0x18, size);
    wr16(&mut mhni.header, 0x1C, vpad);
    wr16(&mut mhni.header, 0x1E, hpad);
    wr16(&mut mhni.header, 0x20, h);
    wr16(&mut mhni.header, 0x22, w);

    // mhod 2 는 자식(mhni)을 품은 청크지만 곡 DB 와 같은 해석기는 mhod 를 잎으로 보므로 바이트로 넣는다
    let mut inner = Vec::new();
    write_node(&Node::Chunk(mhni), &mut inner);
    let mut mhod2 = Chunk::new(b"mhod", 24, Body::Leaf(inner));
    wr16(&mut mhod2.header, 0x0C, 2);
    Node::Chunk(mhod2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(rgb));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn rgb565_packing() {
        assert_eq!(rgb565(255, 0, 0), 0xF800);
        assert_eq!(rgb565(0, 255, 0), 0x07E0);
        assert_eq!(rgb565(0, 0, 255), 0x001F);
    }

    #[test]
    fn render_pads_wide_image() {
        let img = image::load_from_memory(&png(200, 100, [255, 255, 255])).unwrap();
        let (_, px, hpad, vpad, w, h) = render(&img, Format::square(1, 56, 56));
        assert_eq!((hpad, vpad, w, h), (0, 14, 56, 42));
        assert_eq!(px.len(), 56 * 56 * 2);
        assert_eq!(&px[0..2], &[0, 0]); // 위 여백은 검은색
        let mid = ((20 * 56 + 10) * 2) as usize;
        assert_eq!(&px[mid..mid + 2], &0xFFFFu16.to_le_bytes());
    }

    #[test]
    fn render_aligned_rows() {
        // 실제 클래식의 1061: 55x55 그림을 56픽셀 줄에 담아 6160 바이트
        let img = image::load_from_memory(&png(300, 300, [255, 255, 255])).unwrap();
        let f = CLASSIC_FORMATS[0];
        let (_, px, hpad, vpad, w, h) = render(&img, f);
        assert_eq!(px.len(), 6160);
        assert_eq!((hpad, vpad, w, h), (0, 0, 55, 55));
        let at = |x: u32, y: u32| ((y * 56 + x) * 2) as usize;
        assert_eq!(&px[at(54, 3)..at(54, 3) + 2], &0xFFFFu16.to_le_bytes());
        assert_eq!(&px[at(55, 3)..at(55, 3) + 2], &[0, 0]); // 줄 끝 맞춤 칸은 검은색
    }

    #[test]
    fn commit_new_db_and_append() {
        let mount = std::env::temp_dir().join(format!("podlift-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&mount);
        std::fs::create_dir_all(mount.join("iPod_Control/Device")).unwrap();
        std::fs::write(mount.join("iPod_Control/Device/SysInfo"), "ModelNumStr: xB029\n").unwrap();

        // 1) 빈 아이팟: 새 DB 를 만들고 그림 2장 (같은 앨범 같은 그림은 한 장으로)
        let mut db = ArtworkDb::open(&mount).unwrap();
        let red = png(300, 300, [255, 0, 0]);
        let a = db.add(&red, "album", 1).unwrap();
        let b = db.add(&red, "album", 2).unwrap();
        let c = db.add(&png(500, 400, [0, 0, 255]), "other", 3).unwrap();
        assert_eq!((a, b, c), (100, 100, 101));
        let marks = db.commit(&mount).unwrap();
        assert_eq!(marks.len(), 4);
        assert!(marks.iter().all(|m| m.original_len.is_none()));
        let f1060 = artwork_dir(&mount).join("F1060_1.ithmb");
        assert_eq!(std::fs::metadata(&f1060).unwrap().len(), 2 * 320 * 320 * 2);

        // 2) 다시 열어 덧붙이기: 번호가 이어지고 ithmb 는 원래 길이가 기록된다
        let mut again = ArtworkDb::open(&mount).unwrap();
        assert_eq!(again.next_id, 102);
        let d = again.add(&png(64, 64, [0, 255, 0]), "third", 4).unwrap();
        assert_eq!(d, 102);
        let marks2 = again.commit(&mount).unwrap();
        let m1060 = marks2.iter().find(|m| m.path.ends_with("F1060_1.ithmb")).unwrap();
        assert_eq!(m1060.original_len, Some(2 * 320 * 320 * 2));
        let reread = ArtworkDb::open(&mount).unwrap();
        let items = list_children(&reread.root, 1);
        assert_eq!(items.len(), 3);
        // 세 번째 그림의 320 형식 위치는 앞 두 장 뒤
        let Body::Children(kids) = &items[2].body else { panic!() };
        let mhni_off = kids
            .iter()
            .find_map(|n| match n {
                Node::Chunk(c) => match &c.body {
                    Body::Leaf(b) if rd32(b, 0x10) == 1060 => Some(rd32(b, 0x14)),
                    _ => None,
                },
                _ => None,
            })
            .unwrap();
        assert_eq!(mhni_off, 2 * 320 * 320 * 2);

        // 3) 되돌리기: 두 번째 덧붙이기 전 길이로
        rollback_ithmb(&mount, &marks2);
        assert_eq!(std::fs::metadata(&f1060).unwrap().len(), 2 * 320 * 320 * 2);
        std::fs::remove_dir_all(&mount).unwrap();
    }

    #[test]
    fn unknown_model_refuses() {
        let mount = std::env::temp_dir().join(format!("podlift-art-unk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&mount);
        std::fs::create_dir_all(mount.join("iPod_Control/Device")).unwrap();
        std::fs::write(mount.join("iPod_Control/Device/SysInfo"), "ModelNumStr: xA002\n").unwrap();
        assert!(ArtworkDb::open(&mount).is_err());
        std::fs::remove_dir_all(&mount).unwrap();
    }
}
