//! 시스템 CJK 폰트 로드. egui 기본 폰트에는 한글·가나·한자가 없으므로
//! 한국어 → 일본어 → 중국어 → 범용(Arial Unicode) 순으로 시스템 폰트를 앞에 넣는다.
//! egui 는 글자마다 가족(family) 목록을 앞에서부터 훑어 글리프가 있는 첫 폰트를 쓰므로,
//! 한글은 한국어 폰트, 가나·일본 한자는 일본어 폰트에서 나온다.
//! 파일은 memmap 으로 열어 필요한 글리프 페이지만 읽는다(CJK 폰트는 수십 MB).
use crate::egui;
use egui::FontFamily;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn font_file(path: &Path) -> Option<egui::epaint::text::Blob> {
    let file = std::fs::File::open(path).ok()?;
    // Safety: 시스템 폰트 파일은 실행 중 바뀌지 않는다.
    match unsafe { memmap2::Mmap::map(&file) } {
        Ok(map) => Some(Arc::new(map)),
        Err(_) => std::fs::read(path)
            .ok()
            .map(|b| Arc::new(b) as egui::epaint::text::Blob),
    }
}

/// 폴더에서 이름 조건에 맞는 첫 파일. macOS 의 일본어 폰트 파일명(ヒラギノ…)은 디스크에
/// 분해형(NFD)으로 저장돼 있어 글자 그대로 비교하면 못 찾을 수 있으므로, 탁점이 없는
/// 글자(ヒラ)와 영문 부분(W4.ttc)만으로 고른다.
fn find_in_dir(dir: &str, pred: impl Fn(&str) -> bool) -> Option<PathBuf> {
    let mut hits: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(&pred))
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// 언어별 후보 목록: (이름, [(경로, 페이스 인덱스)…]). 각 언어에서 처음 열리는 파일 하나를 쓴다.
fn groups() -> Vec<(&'static str, Vec<(PathBuf, u32)>)> {
    let p = |s: &str| PathBuf::from(s);
    #[cfg(target_os = "macos")]
    {
        const SYS: &str = "/System/Library/Fonts";
        const SUP: &str = "/System/Library/Fonts/Supplemental";
        let hiragino = |weight: &'static str| {
            find_in_dir(SYS, move |n| n.starts_with("ヒラ") && !n.contains('丸') && !n.contains('明') && n.ends_with(&format!("{weight}.ttc")))
        };
        let mut ja: Vec<(PathBuf, u32)> = Vec::new();
        for w in ["W4", "W3", "W5", "W6"] {
            if let Some(f) = hiragino(w) {
                ja.push((f, 0));
            }
        }
        ja.push((p(&format!("{SYS}/Hiragino Sans GB.ttc")), 0)); // 가나 포함(중국어판 히라기노)
        vec![
            ("ko", vec![(p(&format!("{SYS}/AppleSDGothicNeo.ttc")), 0), (p(&format!("{SUP}/AppleGothic.ttf")), 0)]),
            ("ja", ja),
            ("zh", vec![(p(&format!("{SYS}/Hiragino Sans GB.ttc")), 0), (p(&format!("{SYS}/STHeiti Light.ttc")), 0)]),
            ("any", vec![(p(&format!("{SUP}/Arial Unicode.ttf")), 0)]),
        ]
    }
    #[cfg(windows)]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        let f = |n: &str| p(&format!("{windir}\\Fonts\\{n}"));
        vec![
            ("ko", vec![(f("malgun.ttf"), 0), (f("gulim.ttc"), 0)]),
            ("ja", vec![(f("YuGothM.ttc"), 0), (f("meiryo.ttc"), 0), (f("msgothic.ttc"), 0)]),
            ("zh", vec![(f("msyh.ttc"), 0), (f("simsun.ttc"), 0)]),
            ("any", vec![(f("arialuni.ttf"), 0), (f("seguisym.ttf"), 0)]),
        ]
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        vec![
            ("ko", vec![(p("/usr/share/fonts/truetype/nanum/NanumGothic.ttf"), 0), (p("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"), 0)]),
            ("ja", vec![(p("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"), 2)]),
            ("zh", vec![(p("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"), 1)]),
            ("any", vec![(p("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"), 0)]),
        ]
    }
}

/// 제목용 굵은 글꼴 후보 (경로, 페이스 인덱스)
fn bold_candidates() -> Vec<(PathBuf, u32)> {
    #[cfg(target_os = "macos")]
    {
        vec![(PathBuf::from("/System/Library/Fonts/AppleSDGothicNeo.ttc"), 6)]
    }
    #[cfg(windows)]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        vec![(PathBuf::from(format!("{windir}\\Fonts\\malgunbd.ttf")), 0)]
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        vec![(PathBuf::from("/usr/share/fonts/truetype/nanum/NanumGothicBold.ttf"), 0)]
    }
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut order: Vec<String> = Vec::new();
    let mut used: Vec<PathBuf> = Vec::new();
    for (name, cands) in groups() {
        let Some((file, path, index)) = cands
            .iter()
            .filter(|(path, _)| !used.contains(path))
            .find_map(|(path, index)| font_file(path).map(|f| (f, path.clone(), *index)))
        else {
            continue;
        };
        used.push(path);
        let key = format!("system-{name}");
        fonts
            .font_data
            .insert(key.clone(), Arc::new(egui::FontData::from_blob(file, index)));
        order.push(key);
    }
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for (i, key) in order.iter().enumerate() {
            list.insert(i, key.clone());
        }
    }
    // "bold" 가족: 굵은 페이스를 맨 앞에, 그 뒤는 일반 가족과 같은 폴백
    let mut bold_family: Vec<String> = Vec::new();
    if let Some((file, index)) = bold_candidates()
        .iter()
        .find_map(|(path, index)| font_file(path).map(|f| (f, *index)))
    {
        fonts
            .font_data
            .insert("system-bold".into(), Arc::new(egui::FontData::from_blob(file, index)));
        bold_family.push("system-bold".into());
    }
    bold_family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name("bold".into()), bold_family);
    ctx.set_fonts(fonts);
}
