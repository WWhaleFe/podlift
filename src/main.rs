//! Podlift - 아이팟 클래식 음악 관리 앱. macOS 전용.
//! - 가져오기: 아이팟은 곡을 iPod_Control/Music/Fxx/ABCD.m4a 처럼 뒤섞인 이름으로 저장하므로,
//!   파일마다 태그를 읽어 '가수/앨범/트랙 제목' 구조로 되살려 복사한다.
//! - 아이팟에 넣기: iTunes·Finder 없이 곡을 복사하고 아이팟 곡 DB(iTunesDB)에 직접 등록한다.
//! - 중복 정리: 앨범·제목이 같은 곡 중 음질(그다음 앨범 아트)이 나은 것만 남긴다.

pub(crate) use eframe::egui;

mod app;
mod artsearch;
mod artworkdb;
mod audio;
mod config;
mod copier;
mod csvlist;
mod dedupe;
mod dedupe_ui;
mod fonts;
mod ipod;
mod ipod_art;
mod ipod_dedupe;
mod itunesdb;
mod library;
mod listkeys;
mod organize;
mod organize_ui;
mod popup;
mod sync;
mod sync_ui;
#[allow(dead_code)]
mod theme;
#[cfg(test)]
mod real_ipod_check;

fn main() -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_title("Podlift")
        .with_icon(
            eframe::icon_data::from_png_bytes(include_bytes!("../icon.png"))
                .expect("icon.png 디코딩 실패"),
        )
        .with_inner_size([1040.0, 980.0])
        .with_min_inner_size([760.0, 600.0])
        // 제목 표시줄을 숨기고 신호등 단추만 남겨 유리 배경이 창 끝까지 이어지게 한다.
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "podlift",
        options,
        Box::new(|cc| {
            fonts::install(&cc.egui_ctx);
            theme::install(&cc.egui_ctx);
            Ok(Box::new(app::PodliftApp::new()))
        }),
    )
}
